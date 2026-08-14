//! HS256 JWTs with refresh rotation and a `jti` denylist, over the [`AuthStore`].
//!
//! The [`JwtIssuer`] mints access tokens with `sub`/`iat`/`exp`/`jti` claims and a `kid` header
//! (no `aud` — it is reserved but not emitted, spec §6.2), looks the 32-byte signing key up by
//! `kid` so a second key can be introduced before the first is retired, and verifies in the spec's
//! order: **signature → `exp` → denylist**. Refresh tokens are 32 random bytes, stored only as
//! SHA-256 hashes by the store, and rotated single-use: presenting an already-consumed token is
//! the classic theft signal, so it revokes the user's whole outstanding refresh set.
//!
//! HS256-only enforcement is the `Validation` config at the verify call site, **not** the crate's
//! feature set: jsonwebtoken 10.3's `rust_crypto` backend (its only crypto backend usable here) is
//! algorithm-agnostic, and the signer only ever emits HS256, so a `Validation::new(HS256)` — the
//! only algorithm accepted — closes the algorithm-confusion door.

use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use otto_engine_core::Principal;
use otto_protocol::UserId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::store::{AuthStore, ConsumeRefresh};
use crate::totp::Clock;

/// The single key id in use. Verification looks the key up by `kid`, so a second key can be
/// introduced under a fresh id before the first is retired.
pub const KID: &str = "otto-1";
/// Access tokens live for 15 minutes (spec A7). A short TTL is what keeps the denylist bounded.
pub const ACCESS_TTL: Duration = Duration::from_secs(15 * 60);
/// Refresh tokens live for 30 days (spec A7).
pub const REFRESH_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The claims otto mints into every access token. `aud` is deliberately absent (reserved, not
/// emitted — spec §6.2: a claim nothing validates is worse than omitting it).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    /// The principal (`UserId`) this token belongs to.
    sub: String,
    /// Issued-at, unix seconds.
    iat: u64,
    /// Expiry, unix seconds.
    exp: u64,
    /// Unique token id (uuid v4) — the denylist key.
    jti: String,
}

/// The HS256 issuer and verifier, backed by the [`AuthStore`].
#[derive(Clone)]
pub struct JwtIssuer {
    store: Arc<dyn AuthStore>,
    clock: Arc<dyn Clock>,
}

impl JwtIssuer {
    pub fn new(store: Arc<dyn AuthStore>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    /// Mint an access/refresh pair for `user` at the clock's current time.
    pub async fn mint(&self, user: &UserId) -> Result<MintedPair, JwtError> {
        let now = self.clock.now();
        self.mint_at(user, now).await
    }

    /// Mint at an explicit `now` (unix seconds) — deterministic for tests.
    pub async fn mint_at(&self, user: &UserId, now: u64) -> Result<MintedPair, JwtError> {
        let key = self.signing_key().await?;
        let jti = Uuid::new_v4().to_string();
        let exp = now + ACCESS_TTL.as_secs();
        let claims = Claims {
            sub: user.as_str().to_owned(),
            iat: now,
            exp,
            jti,
        };
        let header = Header {
            kid: Some(KID.to_owned()),
            ..Default::default()
        };
        let access_token = encode(&header, &claims, &EncodingKey::from_secret(&key))
            .map_err(|e| anyhow::anyhow!("failed to sign access token: {e}"))?;

        let refresh_token = random_token();
        self.store
            .insert_refresh(&refresh_token, user, (now + REFRESH_TTL.as_secs()) as i64)
            .await?;
        Ok(MintedPair {
            access_token,
            refresh_token,
            expires_at: exp,
        })
    }

    /// Verify an access token (signature → `exp` → denylist) and return its principal.
    pub async fn verify_access(&self, token: &str) -> Result<Principal, JwtError> {
        let now = self.clock.now();
        self.verify_access_at(token, now).await
    }

    /// Verify at an explicit `now` for the denylist check. The signature and `exp` are enforced
    /// by jsonwebtoken against the wall clock, before the denylist is consulted — so a token that
    /// is expired is rejected without ever touching the denylist.
    pub async fn verify_access_at(&self, token: &str, now: u64) -> Result<Principal, JwtError> {
        let claims = self.decode(token).await?;
        if self.store.is_denylisted(&claims.jti, now as i64).await? {
            return Err(JwtError::Unverifiable);
        }
        let user = UserId::parse(&claims.sub)
            .map_err(|e| anyhow::anyhow!("access token carries an invalid sub: {e}"))?;
        Ok(Principal { user })
    }

    /// Rotate a refresh token: consume it atomically and issue a fresh pair. A token already
    /// consumed is a reuse — the theft signal — and revokes the user's whole outstanding set.
    pub async fn rotate_refresh(&self, refresh_token: &str) -> Result<MintedPair, JwtError> {
        let now = self.clock.now();
        self.rotate_refresh_at(refresh_token, now).await
    }

    /// Rotate at an explicit `now` (unix seconds) — deterministic for tests.
    pub async fn rotate_refresh_at(
        &self,
        refresh_token: &str,
        now: u64,
    ) -> Result<MintedPair, JwtError> {
        match self
            .store
            .consume_refresh(refresh_token, now as i64)
            .await?
        {
            ConsumeRefresh::Consumed { user } => self.mint_at(&user, now).await,
            ConsumeRefresh::Unknown => Err(JwtError::Unverifiable),
            ConsumeRefresh::Reused { user } => {
                // Presenting an already-consumed token is the classic theft signal: revoke the
                // user's whole outstanding refresh set so the stolen token and its siblings die.
                self.store.revoke_user_refresh(&user).await?;
                Err(JwtError::Unverifiable)
            }
        }
    }

    /// Denylist an access token's `jti` until its own `exp`, pruning expired rows along the way.
    pub async fn logout(&self, access_token: &str) -> Result<(), JwtError> {
        let now = self.clock.now();
        self.logout_at(access_token, now).await
    }

    /// Denylist at an explicit `now` (unix seconds) — deterministic for tests.
    pub async fn logout_at(&self, access_token: &str, now: u64) -> Result<(), JwtError> {
        let claims = self.decode(access_token).await?;
        self.store
            .denylist_insert(&claims.jti, claims.exp as i64)
            .await?;
        // Opportunistic prune on this write keeps the table bounded by logouts within one
        // access TTL rather than growing without limit (spec §6.2).
        self.store.prune_denylist(now as i64).await?;
        Ok(())
    }

    /// Decode and signature-verify a token, returning its claims. `exp` is validated here too
    /// (jsonwebtoken's `Validation::new(HS256)` against the wall clock), so this is both the
    /// verify path and the logout claim-extraction path.
    async fn decode(&self, token: &str) -> Result<Claims, JwtError> {
        let key = self.signing_key().await?;
        decode::<Claims>(
            token,
            &DecodingKey::from_secret(&key),
            &Validation::new(Algorithm::HS256),
        )
        .map(|data| data.claims)
        .map_err(|_| JwtError::Unverifiable)
    }

    /// The 32-byte signing key for [`KID`], generated and stored on first use. Concurrent
    /// first-mints race on `insert_signing_key` (it refuses duplicates); the loser falls back to
    /// the winner's key so every mint signs with the same stored key.
    async fn signing_key(&self) -> Result<Vec<u8>, JwtError> {
        if let Some(key) = self.store.signing_key(KID).await? {
            return Ok(key);
        }
        let key = random_bytes();
        match self.store.insert_signing_key(KID, &key).await {
            Ok(()) => Ok(key),
            Err(_) => {
                // Another caller won the first-use race; use their key.
                let winner =
                    self.store.signing_key(KID).await?.ok_or_else(|| {
                        anyhow::anyhow!("signing key vanished after the insert race")
                    })?;
                Ok(winner)
            }
        }
    }
}

/// A freshly minted access/refresh pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedPair {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds; the access token's `exp`.
    pub expires_at: u64,
}

/// Why a token was not accepted. `Unverifiable` deliberately covers a bad signature, an expired
/// token, and a denylisted jti in one variant — upstream renders every failure to the same opaque
/// string (spec A11), and separating them here would invite a caller to leak which one happened.
#[derive(Debug)]
pub enum JwtError {
    /// Signature invalid, expired, or denylisted.
    Unverifiable,
    /// The backend itself failed.
    Backend(anyhow::Error),
}

impl From<anyhow::Error> for JwtError {
    fn from(e: anyhow::Error) -> Self {
        JwtError::Backend(e)
    }
}

/// 32 fresh random bytes — the signing key or the raw material for a refresh token.
fn random_bytes() -> Vec<u8> {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.to_vec()
}

/// A refresh token: 32 random bytes, hex-encoded. The store keeps only the SHA-256 hash, so the
/// plaintext lives only with the client (spec §6.2).
fn random_token() -> String {
    data_encoding::HEXLOWER.encode(&random_bytes())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
    use otto_protocol::UserId;

    use super::*;
    use crate::store::SqliteAuthStore;
    use crate::totp::SystemClock;

    fn alice() -> UserId {
        UserId::parse("alice").unwrap()
    }

    /// A known signing key, inserted before any mint so crafted and minted tokens share a key and
    /// every test is deterministic.
    const KEY: [u8; 32] = [7u8; 32];

    async fn temp_store() -> (SqliteAuthStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteAuthStore::open(dir.path().join("auth.db"))
            .await
            .unwrap();
        (store, dir)
    }

    async fn issuer_with_key() -> (JwtIssuer, SqliteAuthStore, tempfile::TempDir) {
        let (store, dir) = temp_store().await;
        store.insert_signing_key(KID, &KEY).await.unwrap();
        let issuer = JwtIssuer::new(Arc::new(store.clone()), Arc::new(SystemClock));
        (issuer, store, dir)
    }

    /// A `Validation` that enforces HS256 and the signature but not `exp` — so the claims of a
    /// token minted at a fixed past timestamp are still inspectable.
    fn claims_validation() -> Validation {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = false;
        validation
    }

    /// Decode a token's claims, checking the signature but not `exp`.
    fn decode_claims(token: &str) -> Claims {
        decode(token, &DecodingKey::from_secret(&KEY), &claims_validation())
            .unwrap()
            .claims
    }

    #[tokio::test]
    async fn mint_and_verify_round_trip() {
        let (issuer, _store, _dir) = issuer_with_key().await;
        let pair = issuer.mint(&alice()).await.unwrap();
        assert!(pair.access_token.starts_with("eyJ"), "not a JWT");
        let principal = issuer.verify_access(&pair.access_token).await.unwrap();
        assert_eq!(principal.user, alice());
    }

    #[tokio::test]
    async fn minted_token_carries_the_expected_claims() {
        let (issuer, _store, _dir) = issuer_with_key().await;
        let now = 1_700_000_000u64;
        let pair = issuer.mint_at(&alice(), now).await.unwrap();

        let data = decode::<Claims>(
            &pair.access_token,
            &DecodingKey::from_secret(&KEY),
            &claims_validation(),
        )
        .unwrap();
        assert_eq!(data.claims.sub, alice().as_str());
        assert_eq!(data.claims.iat, now);
        assert_eq!(data.claims.exp, now + ACCESS_TTL.as_secs());
        assert_eq!(pair.expires_at, data.claims.exp);
        // jti is a uuid v4 — the denylist key.
        assert!(uuid::Uuid::parse_str(&data.claims.jti).is_ok());
        assert_eq!(data.header.kid.as_deref(), Some(KID));
    }

    #[tokio::test]
    async fn an_expired_token_is_rejected() {
        let (issuer, _store, _dir) = issuer_with_key().await;
        let expired = Claims {
            sub: alice().as_str().to_owned(),
            iat: 1_000_000,
            exp: 1_000_000,
            jti: uuid::Uuid::new_v4().to_string(),
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &expired,
            &jsonwebtoken::EncodingKey::from_secret(&KEY),
        )
        .unwrap();
        // Signed correctly with the right key, so the only thing wrong is that exp is long past.
        assert!(matches!(
            issuer.verify_access(&token).await,
            Err(JwtError::Unverifiable)
        ));
    }

    #[tokio::test]
    async fn a_denylisted_token_is_rejected_for_the_rest_of_its_exp() {
        let (issuer, store, _dir) = issuer_with_key().await;
        let pair = issuer.mint(&alice()).await.unwrap();
        let jti = decode_claims(&pair.access_token).jti;

        issuer.logout(&pair.access_token).await.unwrap();

        // The token is still valid and unexpired, but its jti is denylisted.
        assert!(matches!(
            issuer.verify_access(&pair.access_token).await,
            Err(JwtError::Unverifiable)
        ));
        assert!(
            store
                .is_denylisted(&jti, pair.expires_at as i64 - 1)
                .await
                .unwrap()
        );
        assert!(
            !store
                .is_denylisted(&jti, pair.expires_at as i64 + 1)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn denylist_rows_are_pruned_past_the_tokens_exp() {
        let (issuer, store, _dir) = issuer_with_key().await;
        let pair = issuer.mint(&alice()).await.unwrap();
        let jti = decode_claims(&pair.access_token).jti;

        issuer.logout(&pair.access_token).await.unwrap();
        // `is_denylisted` is strict on expiry (`expires_at > now`), so the row is live at
        // exp - 1 and gone once exp itself has passed.
        assert!(
            store
                .is_denylisted(&jti, pair.expires_at as i64 - 1)
                .await
                .unwrap()
        );

        // Once the token's own exp passes, the row is pruned and no longer consulted.
        let pruned = store
            .prune_denylist(pair.expires_at as i64 + 1)
            .await
            .unwrap();
        assert_eq!(pruned, 1);
        assert!(
            !store
                .is_denylisted(&jti, pair.expires_at as i64 + 1)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn refresh_rotation_consumes_the_old_token_and_issues_a_new_pair() {
        let (issuer, _store, _dir) = issuer_with_key().await;
        let pair = issuer.mint(&alice()).await.unwrap();

        let rotated = issuer.rotate_refresh(&pair.refresh_token).await.unwrap();
        assert_ne!(rotated.access_token, pair.access_token);
        assert_ne!(rotated.refresh_token, pair.refresh_token);
        assert_eq!(
            issuer
                .verify_access(&rotated.access_token)
                .await
                .unwrap()
                .user,
            alice()
        );

        // The old refresh token is single-use: a second presentation is rejected.
        assert!(matches!(
            issuer.rotate_refresh(&pair.refresh_token).await,
            Err(JwtError::Unverifiable)
        ));
    }

    #[tokio::test]
    async fn a_reused_refresh_token_revokes_the_users_whole_outstanding_set() {
        let (issuer, store, _dir) = issuer_with_key().await;
        // Two independent logins leave two outstanding refresh tokens for alice.
        let first = issuer.mint(&alice()).await.unwrap();
        let second = issuer.mint(&alice()).await.unwrap();

        // A normal rotation consumes the first and issues a new pair...
        let rotated = issuer.rotate_refresh(&first.refresh_token).await.unwrap();

        // ...but presenting the already-consumed token again is the theft signal: it revokes
        // alice's whole outstanding set, so both the second and the rotated tokens die too.
        assert!(matches!(
            issuer.rotate_refresh(&first.refresh_token).await,
            Err(JwtError::Unverifiable)
        ));
        assert!(matches!(
            issuer.rotate_refresh(&second.refresh_token).await,
            Err(JwtError::Unverifiable)
        ));
        assert!(matches!(
            issuer.rotate_refresh(&rotated.refresh_token).await,
            Err(JwtError::Unverifiable)
        ));
        // The revocation is real at the store: the never-rotated token is no longer known.
        assert_eq!(
            store
                .consume_refresh(&second.refresh_token, 1_700_000_000)
                .await
                .unwrap(),
            crate::store::ConsumeRefresh::Unknown
        );
    }
}
