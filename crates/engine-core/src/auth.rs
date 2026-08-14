//! The authentication seam: identity verification plus the full token lifecycle.
//!
//! This is the **one seam that must stay crypto-free** (spec §5, plan-critique resolution A9):
//! the concrete backend lives in the `auth` crate, while `serve.rs` reaches it only as
//! `Arc<dyn Authenticator>` and must mint (`Login`), verify (`Attach` + per-command re-check +
//! `/workspace`), rotate (`Refresh`), and revoke (`Logout`) through that object. Everything
//! crossing the seam is a plain `String`/`u64` or a plain record — never a signature, a hash, or
//! a secret. The choice of HS256 over asymmetric keys (spec §10 risk 4: "the signing key verifies
//! and mints", safe only while the key never leaves the source) is a concern of the `auth` crate,
//! not of this seam; if a future slice ever lets a remote verify user JWTs, the algorithm moves to
//! asymmetric first — recorded here as a condition on that crate's choice, not just a preference.

use std::sync::Arc;
use std::time::Duration;

use otto_protocol::{AuthMode, Credentials, UserId};

/// An authenticated principal. Just an identity today; roles and tenancy attributes
/// attach here when they exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user: UserId,
}

/// Why authentication failed. The variants exist so the server can log precisely and
/// rate-limit correctly; every one of them renders to the client as the same opaque
/// string (Assumption A11).
#[derive(Debug)]
pub enum AuthError {
    /// Unknown user, wrong code, or a replayed time-step — deliberately one variant, so
    /// a caller cannot accidentally branch on the distinction and leak it.
    InvalidCredentials,
    /// Too many recent failures for this principal; retry after the cooldown.
    RateLimited { retry_after_secs: u64 },
    /// The backend itself failed (database unreachable, clock unavailable).
    Backend(anyhow::Error),
}

/// The server's authentication posture for a served engine — the one struct the serve
/// constructors take (plan Tasks 6/8). `authenticator` is `Some` only for `Users`;
/// `promotion_secret` is `Some` only for `Machine`/`--promote-*`; `handshake_deadline` is
/// injectable so the timeout tests do not block 10s each.
///
/// `Debug` is hand-written: a `dyn Authenticator` trait object cannot be `Debug` (and must not be
/// forced to), and the promotion secret is a credential that must not reach the log.
#[derive(Clone)]
pub struct AuthConfig {
    pub mode: AuthMode,
    pub authenticator: Option<Arc<dyn Authenticator>>,
    pub promotion_secret: Option<String>,
    pub handshake_deadline: Duration,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("mode", &self.mode)
            .field(
                "authenticator",
                &self.authenticator.as_ref().map(|_| "<dyn Authenticator>"),
            )
            .field(
                "promotion_secret",
                &self.promotion_secret.as_deref().map(|_| "<redacted>"),
            )
            .field("handshake_deadline", &self.handshake_deadline)
            .finish()
    }
}

impl Default for AuthConfig {
    /// The `Users` posture with the 10s handshake deadline the plan specifies as the default.
    fn default() -> Self {
        Self {
            mode: AuthMode::Users,
            authenticator: None,
            promotion_secret: None,
            handshake_deadline: Duration::from_secs(10),
        }
    }
}

/// The token lifecycle, not just identity verification: `serve.rs` reaches the authenticator
/// only as `Arc<dyn Authenticator>`, so mint/verify/rotate/revoke must all live on the seam. A
/// concrete backend's extra methods would be unreachable through the trait object, and future
/// OIDC/device-flow backends implement the same lifecycle anyway. All of it is expressed in
/// `String`/`u64`/plain records, keeping the no-crypto-in-engine-core rule (§1) intact.
#[async_trait::async_trait]
pub trait Authenticator: Send + Sync {
    /// Verify presented credentials and return the authenticated principal. (Login.)
    async fn authenticate(&self, creds: &Credentials) -> Result<Principal, AuthError>;

    /// Mint an access + refresh token pair for an already-authenticated principal.
    /// Called by the `Login` command after `authenticate` succeeds.
    async fn mint(&self, principal: &Principal) -> Result<TokenPair, AuthError>;

    /// Verify an access token (signature, `exp`, denylist) and return its principal.
    /// Called by `Attach` and by the per-command re-verification in §7.2.
    async fn verify_access(&self, token: &str) -> Result<Principal, AuthError>;

    /// Rotate a refresh token: consume it, issue a fresh pair. Called by `Refresh`; single-use.
    async fn rotate_refresh(&self, refresh_token: &str) -> Result<TokenPair, AuthError>;

    /// Denylist an access token's `jti` and revoke its principal's whole outstanding refresh
    /// set (the store offers only whole-set revocation, so every concurrent session of the
    /// user is signed out — spec §9). Called by `Logout`.
    async fn logout(&self, access_token: &str) -> Result<(), AuthError>;
}

/// A minted access/refresh pair, expressed in plain wire types so the seam stays crypto-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds; the access token's `exp`.
    pub expires_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_seam_is_trait_object_friendly_and_covers_the_token_lifecycle() {
        // The seam must be usable as `Arc<dyn Authenticator>` — this compiles only if the
        // trait is Send + Sync + object-safe. The Stub implements every lifecycle method.
        struct Stub;
        #[async_trait::async_trait]
        impl Authenticator for Stub {
            async fn authenticate(&self, _c: &Credentials) -> Result<Principal, AuthError> {
                Ok(Principal {
                    user: UserId::local(),
                })
            }
            async fn mint(&self, _p: &Principal) -> Result<TokenPair, AuthError> {
                Ok(TokenPair {
                    access_token: "at".into(),
                    refresh_token: "rt".into(),
                    expires_at: 0,
                })
            }
            async fn verify_access(&self, _t: &str) -> Result<Principal, AuthError> {
                Ok(Principal {
                    user: UserId::local(),
                })
            }
            async fn rotate_refresh(&self, _r: &str) -> Result<TokenPair, AuthError> {
                Ok(TokenPair {
                    access_token: "at2".into(),
                    refresh_token: "rt2".into(),
                    expires_at: 0,
                })
            }
            async fn logout(&self, _a: &str) -> Result<(), AuthError> {
                Ok(())
            }
        }
        let a: Arc<dyn Authenticator> = Arc::new(Stub);
        let p = a
            .authenticate(&Credentials::Totp {
                user: UserId::local(),
                code: "0".into(),
            })
            .await
            .unwrap();
        assert_eq!(p.user, UserId::local());
        let pair = a.mint(&p).await.unwrap();
        assert_eq!(
            a.verify_access(&pair.access_token).await.unwrap().user,
            UserId::local()
        );
        assert!(a.logout(&pair.access_token).await.is_ok());
    }
}
