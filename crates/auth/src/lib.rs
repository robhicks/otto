//! The identity backend: the sqlite `AuthStore` that holds users, TOTP secrets,
//! signing keys, refresh-token hashes, and the `jti` denylist. TOTP verification and
//! JWT minting build on top of this store in the surrounding slices.

mod store;

pub use store::{AuthStore, ConsumeRefresh, SqliteAuthStore};
