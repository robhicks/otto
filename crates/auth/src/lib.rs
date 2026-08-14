//! The identity backend: the sqlite `AuthStore` that holds users, TOTP secrets,
//! signing keys, refresh-token hashes, and the `jti` denylist; the RFC 6238 TOTP
//! verifier with its replay floor and lockout; and the HS256 JWT issuer with
//! refresh rotation.

mod jwt;
mod store;
mod totp;

pub use jwt::{ACCESS_TTL, JwtError, JwtIssuer, KID, MintedPair, REFRESH_TTL};
pub use store::{AuthStore, ConsumeRefresh, SqliteAuthStore};
pub use totp::{
    Clock, FAILURE_WINDOW, FixedClock, MAX_FAILURES, SKEW, STEP_SECS, SystemClock, TotpError,
    TotpVerifier, totp_at, truncate,
};
