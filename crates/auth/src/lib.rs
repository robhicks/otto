//! The identity backend: the sqlite `AuthStore` that holds users, TOTP secrets,
//! signing keys, refresh-token hashes, and the `jti` denylist; the RFC 6238 TOTP
//! verifier with its replay floor and lockout; the HS256 JWT issuer with
//! refresh rotation; and the [`TotpAuthenticator`] that glues them all behind
//! the engine-core [`Authenticator`] seam. [`testing`] carries the always-compiled
//! `FakeAuthenticator` test double the serve integration harnesses use.

mod jwt;
mod store;
mod totp;

pub use jwt::{ACCESS_TTL, JwtError, JwtIssuer, KID, MintedPair, REFRESH_TTL};
pub use store::{AuthStore, ConsumeRefresh, SqliteAuthStore};
pub use totp::{
    Clock, FAILURE_WINDOW, FixedClock, MAX_FAILURES, SKEW, STEP_SECS, SystemClock, TotpError,
    TotpVerifier, totp_at, truncate,
};

use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::{AuthError, Authenticator, Principal, TokenPair};
use otto_protocol::Credentials;

/// The production [`Authenticator`]: TOTP login through the verifier's replay floor and
/// lockout, then the HS256 token lifecycle through the [`JwtIssuer`]. `serve.rs` reaches it
/// only as `Arc<dyn Authenticator>`.
#[derive(Clone)]
pub struct TotpAuthenticator {
    verifier: TotpVerifier,
    jwt: JwtIssuer,
}

impl TotpAuthenticator {
    /// Build the authenticator over `store` (users, signing keys, refresh hashes, denylist)
    /// reading time from `clock` — [`SystemClock`] in production, a [`FixedClock`] in tests
    /// so the whole suite stays deterministic.
    pub fn new(store: Arc<dyn AuthStore>, clock: Arc<dyn Clock>) -> Self {
        Self {
            verifier: TotpVerifier::new(store.clone(), clock.clone()),
            jwt: JwtIssuer::new(store, clock),
        }
    }
}

#[async_trait]
impl Authenticator for TotpAuthenticator {
    async fn authenticate(&self, creds: &Credentials) -> Result<Principal, AuthError> {
        match creds {
            Credentials::Totp { user, code } => {
                self.verifier
                    .verify(user, code)
                    .await
                    .map_err(totp_to_auth)?;
                Ok(Principal { user: user.clone() })
            }
        }
    }

    async fn mint(&self, principal: &Principal) -> Result<TokenPair, AuthError> {
        self.jwt
            .mint(&principal.user)
            .await
            .map_err(jwt_to_auth)
            .map(minted_pair_to_token_pair)
    }

    async fn verify_access(&self, token: &str) -> Result<Principal, AuthError> {
        self.jwt.verify_access(token).await.map_err(jwt_to_auth)
    }

    async fn rotate_refresh(&self, refresh_token: &str) -> Result<TokenPair, AuthError> {
        self.jwt
            .rotate_refresh(refresh_token)
            .await
            .map_err(jwt_to_auth)
            .map(minted_pair_to_token_pair)
    }

    async fn logout(&self, access_token: &str) -> Result<(), AuthError> {
        self.jwt.logout(access_token).await.map_err(jwt_to_auth)
    }
}

/// Map the verifier's verdict onto the seam's. A replayed code folds into
/// `InvalidCredentials` — replay, an unknown user, and a wrong code are deliberately one
/// variant (spec A11), so no caller can branch on the distinction and leak it.
fn totp_to_auth(e: TotpError) -> AuthError {
    match e {
        TotpError::Invalid | TotpError::Replay => AuthError::InvalidCredentials,
        TotpError::RateLimited { retry_after_secs } => AuthError::RateLimited { retry_after_secs },
        TotpError::Backend(inner) => AuthError::Backend(inner),
    }
}

/// Map the issuer's verdict onto the seam's. A bad signature, an expired token, and a
/// denylisted `jti` are deliberately one variant (spec A11).
fn jwt_to_auth(e: JwtError) -> AuthError {
    match e {
        JwtError::Unverifiable => AuthError::InvalidCredentials,
        JwtError::Backend(inner) => AuthError::Backend(inner),
    }
}

/// The seam's crypto-free token pair, filled from the issuer's minted pair.
fn minted_pair_to_token_pair(pair: MintedPair) -> TokenPair {
    TokenPair {
        access_token: pair.access_token,
        refresh_token: pair.refresh_token,
        expires_at: pair.expires_at,
    }
}

/// Test doubles for the [`Authenticator`] seam. [`FakeAuthenticator`] lives here, always
/// compiled and behind no feature gate, because every serve integration harness — `cargo test
/// --workspace` — constructs it unconditionally (plan-critique resolution A9). Like the
/// `ScriptedProvider` precedent, it is a test double in an impl crate's public API: it does no
/// I/O and no crypto, so shipping it inside the binary is harmless.
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use otto_engine_core::{AuthError, Authenticator, Principal, TokenPair};
    use otto_protocol::{Credentials, UserId};

    /// A deterministic [`Authenticator`]: every credential authenticates as the fixed
    /// `principal`, and tokens it mints verify back to that principal until `logout` revokes
    /// them. The whole store is an in-memory map of token → user, so two fakes built with
    /// distinct principals are fully isolated — the cross-tenant shape the serve harnesses
    /// need. The stored user is the principal `mint` was asked for, so a fake can also mint
    /// for a tenant that is not its own when a test wants that.
    ///
    /// `logout` models the real backend's whole-set semantics (spec §9): denylisting the
    /// presented access token also revokes that user's **entire** outstanding token set — the
    /// store offers only whole-set refresh revocation, so every concurrent session of the
    /// user is signed out. A test that needs only the access token dead while the refresh set
    /// stays live (the one real way that state arises — the access token's own `exp`
    /// passing) uses [`FakeAuthenticator::expire_access`] instead.
    pub struct FakeAuthenticator {
        principal: UserId,
        state: Mutex<FakeState>,
    }

    /// The live token map plus a monotonic mint counter. The counter is deliberately
    /// independent of the map's live size, so a `logout`/`rotate_refresh` that shrinks the
    /// map can never make a later `mint` reuse a token string — a byte-identical token
    /// that maps to the wrong principal is the worst failure a cross-tenant test double
    /// could produce.
    struct FakeState {
        tokens: HashMap<String, UserId>,
        next: u64,
    }

    impl FakeAuthenticator {
        pub fn new(user: UserId) -> Self {
            Self {
                principal: user,
                state: Mutex::new(FakeState {
                    tokens: HashMap::new(),
                    next: 1,
                }),
            }
        }

        /// Mark a single access token dead **without** touching the user's refresh set —
        /// models the access token's own 15-minute `exp` passing (spec A7), the one real way
        /// a live refresh token outlives its access token now that `logout` revokes the whole
        /// refresh set. The serve recovery tests (§8) use this to prove a `Refresh` restores
        /// a connection whose access token can no longer re-verify.
        pub async fn expire_access(&self, access_token: &str) {
            self.state.lock().unwrap().tokens.remove(access_token);
        }
    }

    #[async_trait]
    impl Authenticator for FakeAuthenticator {
        async fn authenticate(&self, _creds: &Credentials) -> Result<Principal, AuthError> {
            Ok(Principal {
                user: self.principal.clone(),
            })
        }

        async fn mint(&self, principal: &Principal) -> Result<TokenPair, AuthError> {
            // Numbered by a monotonic per-fake counter, never by the map's live size, so
            // every token string is unique across the fake's whole lifetime even after a
            // `logout`/`rotate_refresh` shrinks the map. One lock hold keeps the counter
            // increment and the insert atomic under concurrent mints.
            let mut state = self.state.lock().unwrap();
            let n = state.next;
            state.next += 1;
            let access_token = format!("fake-access-{n}");
            let refresh_token = format!("fake-refresh-{n}");
            let user = principal.user.clone();
            state.tokens.insert(access_token.clone(), user.clone());
            state.tokens.insert(refresh_token.clone(), user);
            Ok(TokenPair {
                access_token,
                refresh_token,
                expires_at: 0,
            })
        }

        async fn verify_access(&self, token: &str) -> Result<Principal, AuthError> {
            match self.state.lock().unwrap().tokens.get(token) {
                Some(user) => Ok(Principal { user: user.clone() }),
                None => Err(AuthError::InvalidCredentials),
            }
        }

        async fn rotate_refresh(&self, refresh_token: &str) -> Result<TokenPair, AuthError> {
            // Consume the old refresh token and mint a fresh pair for whoever owned it.
            let user = self
                .state
                .lock()
                .unwrap()
                .tokens
                .remove(refresh_token)
                .ok_or(AuthError::InvalidCredentials)?;
            self.mint(&Principal { user }).await
        }

        async fn logout(&self, access_token: &str) -> Result<(), AuthError> {
            // Whole-set revocation, mirroring the real backend (spec §9): the store offers
            // only whole-set refresh revocation, so denylisting the access token also signs
            // out every concurrent session of the same principal. Looking the user up first
            // means an unknown token is a no-op (idempotent, like `denylist_insert`).
            let mut state = self.state.lock().unwrap();
            let Some(user) = state.tokens.get(access_token).cloned() else {
                return Ok(());
            };
            state.tokens.retain(|_, owner| *owner != user);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otto_engine_core::{AuthError, Principal};
    use otto_protocol::{Credentials, UserId};

    use super::*;
    use crate::store::SqliteAuthStore;

    /// The RFC 6238 Appendix B test secret: the ASCII bytes of "12345678901234567890".
    const RFC_SECRET: &[u8] = b"12345678901234567890";
    /// The fixed "now" the whole suite runs at. It must sit comfortably ahead of the wall clock,
    /// because the JWT issuer mints `exp` from the injected clock while jsonwebtoken validates
    /// `exp` against the **real** clock — a token minted at a 1970s-era fixed time is already
    /// expired on arrival. The exact step is irrelevant: every TOTP assertion computes its own
    /// expected code via `totp_at` at this step.
    const NOW: u64 = 2_000_000_000;

    fn alice() -> UserId {
        UserId::parse("alice").unwrap()
    }

    fn bob() -> UserId {
        UserId::parse("bob").unwrap()
    }

    /// A `TotpAuthenticator` over a fresh store with a fixed clock. The returned `TempDir`
    /// must be kept alive for the duration of the test so the database file is not deleted.
    async fn authenticator() -> (TotpAuthenticator, SqliteAuthStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteAuthStore::open(dir.path().join("auth.db"))
            .await
            .unwrap();
        let auth = TotpAuthenticator::new(Arc::new(store.clone()), Arc::new(FixedClock(NOW)));
        (auth, store, dir)
    }

    /// The step-`NOW` code — the correct code at [`NOW`] — computed from the RFC secret.
    fn correct_code() -> String {
        totp_at(RFC_SECRET, NOW / STEP_SECS)
    }

    /// A code no candidate step (T-1..T+1 around [`NOW`]) produces, so a "wrong code" assertion
    /// cannot accidentally be satisfied by the skew window.
    fn wrong_code() -> String {
        let now = NOW / STEP_SECS;
        let candidates: Vec<String> = ((now - 1)..=(now + 1))
            .map(|s| totp_at(RFC_SECRET, s))
            .collect();
        (0..6u32)
            .map(|n| format!("{n:06}"))
            .find(|c| !candidates.contains(c))
            .expect("three six-digit codes cannot cover 000000..000005")
    }

    #[tokio::test]
    async fn a_wrong_code_is_invalid_credentials_never_backend() {
        let (auth, store, _dir) = authenticator().await;
        store.enroll_user(&alice(), RFC_SECRET).await.unwrap();

        assert!(matches!(
            auth.authenticate(&Credentials::Totp {
                user: alice(),
                code: wrong_code(),
            })
            .await,
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn a_correct_code_yields_the_principal() {
        let (auth, store, _dir) = authenticator().await;
        store.enroll_user(&alice(), RFC_SECRET).await.unwrap();

        let principal = auth
            .authenticate(&Credentials::Totp {
                user: alice(),
                code: correct_code(),
            })
            .await
            .unwrap();
        assert_eq!(principal.user, alice());
    }

    #[tokio::test]
    async fn a_locked_store_is_rate_limited() {
        let (auth, store, _dir) = authenticator().await;
        store.enroll_user(&alice(), RFC_SECRET).await.unwrap();

        for _ in 0..MAX_FAILURES {
            assert!(matches!(
                auth.authenticate(&Credentials::Totp {
                    user: alice(),
                    code: wrong_code(),
                })
                .await,
                Err(AuthError::InvalidCredentials)
            ));
        }
        // Locked: even the correct code is rejected without being computed against.
        assert!(matches!(
            auth.authenticate(&Credentials::Totp {
                user: alice(),
                code: correct_code(),
            })
            .await,
            Err(AuthError::RateLimited {
                retry_after_secs: _
            })
        ));
    }

    #[tokio::test]
    async fn verify_access_on_a_minted_token_yields_the_principal() {
        let (auth, _store, _dir) = authenticator().await;
        let pair = auth.mint(&Principal { user: alice() }).await.unwrap();

        assert_eq!(
            auth.verify_access(&pair.access_token).await.unwrap().user,
            alice()
        );
    }

    #[tokio::test]
    async fn verify_access_on_a_logged_out_token_is_invalid_credentials() {
        let (auth, _store, _dir) = authenticator().await;
        let pair = auth.mint(&Principal { user: alice() }).await.unwrap();
        auth.logout(&pair.access_token).await.unwrap();

        assert!(matches!(
            auth.verify_access(&pair.access_token).await,
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn rotate_refresh_consumes_the_old_token_and_mints_a_fresh_pair() {
        let (auth, _store, _dir) = authenticator().await;
        let pair = auth.mint(&Principal { user: alice() }).await.unwrap();

        let rotated = auth.rotate_refresh(&pair.refresh_token).await.unwrap();
        assert_ne!(rotated.access_token, pair.access_token);
        assert_ne!(rotated.refresh_token, pair.refresh_token);
        assert_eq!(
            auth.verify_access(&rotated.access_token)
                .await
                .unwrap()
                .user,
            alice()
        );

        // The old refresh token is single-use: a second rotation is refused. The old access
        // token, by contrast, is still valid — rotation mints a new pair, it does not log out.
        assert!(matches!(
            auth.rotate_refresh(&pair.refresh_token).await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(auth.verify_access(&pair.access_token).await.is_ok());
    }

    #[tokio::test]
    async fn the_fake_returns_its_fixed_principal_for_any_input() {
        let fake = testing::FakeAuthenticator::new(alice());

        let principal = fake
            .authenticate(&Credentials::Totp {
                user: bob(),
                code: "000000".into(),
            })
            .await
            .unwrap();
        assert_eq!(principal.user, alice());
    }

    #[tokio::test]
    async fn the_fake_verifies_only_the_tokens_it_minted() {
        let alice_fake = testing::FakeAuthenticator::new(alice());
        let bob_fake = testing::FakeAuthenticator::new(bob());
        let pair = alice_fake.mint(&Principal { user: alice() }).await.unwrap();

        assert_eq!(
            alice_fake
                .verify_access(&pair.access_token)
                .await
                .unwrap()
                .user,
            alice()
        );
        // A token minted by alice's fake is unknown to bob's fake.
        assert!(matches!(
            bob_fake.verify_access(&pair.access_token).await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            alice_fake.verify_access("never-minted").await,
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn the_fake_logout_denylists_a_minted_token() {
        let fake = testing::FakeAuthenticator::new(alice());
        let pair = fake.mint(&Principal { user: alice() }).await.unwrap();

        fake.logout(&pair.access_token).await.unwrap();
        assert!(matches!(
            fake.verify_access(&pair.access_token).await,
            Err(AuthError::InvalidCredentials)
        ));
    }

    /// Finding 1's fake-side model: `logout` revokes the user's whole outstanding token set,
    /// not just the presented access token — a refresh token minted to another concurrent
    /// session of the same user must not survive it.
    #[tokio::test]
    async fn the_fake_logout_revokes_the_whole_refresh_set() {
        let fake = testing::FakeAuthenticator::new(alice());
        let first = fake.mint(&Principal { user: alice() }).await.unwrap();
        let second = fake.mint(&Principal { user: alice() }).await.unwrap();

        fake.logout(&first.access_token).await.unwrap();

        // The denylisted access token is rejected...
        assert!(matches!(
            fake.verify_access(&first.access_token).await,
            Err(AuthError::InvalidCredentials)
        ));
        // ...and neither outstanding refresh token of the user can rotate — logout signs
        // every concurrent session out, matching the real backend's whole-set revocation.
        assert!(matches!(
            fake.rotate_refresh(&first.refresh_token).await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            fake.rotate_refresh(&second.refresh_token).await,
            Err(AuthError::InvalidCredentials)
        ));

        // Another principal's tokens are untouched by alice's logout.
        let bob_pair = fake.mint(&Principal { user: bob() }).await.unwrap();
        assert!(fake.verify_access(&bob_pair.access_token).await.is_ok());
    }

    #[tokio::test]
    async fn the_fake_rotate_consumes_the_old_refresh_token() {
        let fake = testing::FakeAuthenticator::new(alice());
        let pair = fake.mint(&Principal { user: alice() }).await.unwrap();

        let rotated = fake.rotate_refresh(&pair.refresh_token).await.unwrap();
        assert_ne!(rotated.refresh_token, pair.refresh_token);
        assert_eq!(
            fake.verify_access(&rotated.access_token)
                .await
                .unwrap()
                .user,
            alice()
        );
        assert!(matches!(
            fake.rotate_refresh(&pair.refresh_token).await,
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn the_fake_mint_counter_is_monotonic_across_logout() {
        let fake = testing::FakeAuthenticator::new(alice());

        // Two mints, then log out alice's first pair. Under the old map-size-derived numbering
        // (`len + 1`, which skips to odd numbers 1, 3, ...), the third mint would reuse
        // `fake-access-3` — byte-identical to bob's still-live token — and remint it for alice,
        // misattributing bob's live token. The fake's `logout` revokes alice's whole set (both
        // tokens of the pair), which is exactly what a test of the counter needs.
        let alice1 = fake.mint(&Principal { user: alice() }).await.unwrap();
        let bob1 = fake.mint(&Principal { user: bob() }).await.unwrap();
        assert_eq!(bob1.access_token, "fake-access-2");
        fake.logout(&alice1.access_token).await.unwrap();

        // The fresh mint must NOT reuse any token string ever issued, and must verify as the
        // principal it was minted for.
        let alice2 = fake.mint(&Principal { user: alice() }).await.unwrap();
        assert_ne!(alice2.access_token, bob1.access_token);
        assert_ne!(alice2.access_token, alice1.access_token);
        assert_ne!(alice2.refresh_token, bob1.refresh_token);
        assert_eq!(
            fake.verify_access(&alice2.access_token).await.unwrap().user,
            alice()
        );
        // Bob's still-live token is untouched: no cross-principal misattribution.
        assert_eq!(
            fake.verify_access(&bob1.access_token).await.unwrap().user,
            bob()
        );
    }
}
