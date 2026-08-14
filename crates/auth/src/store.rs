//! Sqlite-backed `AuthStore`. The schema is created at open time with
//! `CREATE TABLE IF NOT EXISTS`, so no migrations dir or compile-time DB is needed and the
//! build/test path stays fully offline.
//!
//! Open is idempotent *within* a schema generation but not *across* one: since there is no
//! migration mechanism, a database written by a different `SCHEMA_VERSION` is refused rather
//! than silently used with the wrong table shape. Unlike `persistence`'s session store there
//! is no legacy arm — this database is new in this slice, so no pre-existing shape can exist
//! to bail on; the guard keeps only the create-and-stamp arm and the forward-version refusal.
//!
//! Secrets are stored defensively: TOTP secrets are base32-encoded (RFC 4648, unpadded — the
//! encoding authenticator apps expect) and refresh tokens are stored only as SHA-256 hashes,
//! so a stolen database yields no usable refresh token.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use otto_protocol::UserId;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// Wall-clock bound on one `SqliteAuthStore::open`, enforced with a `timeout` around the whole
/// retry loop rather than a deadline checked between attempts — a single attempt can block this
/// long by itself, so a between-attempts check bounds nothing. See `persistence`'s `sqlite.rs`
/// for the measured failure mode this retries (two otto processes racing on a fresh DB).
const BUSY_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// True if `e` is sqlite's SQLITE_BUSY ("database is locked").
///
/// Matched on the primary result code, so extended codes (`SQLITE_BUSY_SNAPSHOT` = 517 etc.)
/// count too — they all mean "another connection holds a lock; try again".
fn is_busy(e: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = e else {
        return false;
    };
    db.code()
        .and_then(|c| c.parse::<i32>().ok())
        .is_some_and(|c| c & 0xff == 5)
}

/// True if `e` is, or was caused by, a SQLITE_BUSY.
///
/// `anyhow::Error::downcast_ref` already searches the source chain, so `.context()` layers added
/// on the way up do not hide the `sqlx::Error`.
fn busy_somewhere(e: &anyhow::Error) -> bool {
    e.downcast_ref::<sqlx::Error>().is_some_and(is_busy)
}

/// Bumped whenever the on-disk schema changes shape. Stamped into `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

/// What consuming one refresh token found and did, so the caller can react to reuse.
///
/// `consume_refresh` performs the single-use consume atomically (`UPDATE ... WHERE
/// consumed_at IS NULL`), so exactly one caller ever observes [`ConsumeRefresh::Consumed`]
/// for a given token. The reuse-detection matters: a token presented twice is the classic
/// theft signal, and the caller revokes the reported user's whole outstanding set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumeRefresh {
    /// No such token — never issued (or revoked since).
    Unknown,
    /// This call atomically consumed the token; the pair can now be re-issued.
    Consumed { user: UserId },
    /// The token existed but was already consumed — a reuse. The caller should treat it as
    /// a theft signal and revoke `user`'s whole outstanding refresh set.
    Reused { user: UserId },
}

/// The persistence layer behind otto's authentication: enrolled users and their TOTP secrets,
/// the replay/lockout bookkeeping, HS256 signing keys, refresh-token hashes, and the `jti`
/// denylist. Implementations are `Send + Sync` so the engine can hold one behind a trait object.
///
/// All timestamps are unix **seconds**. `now` is passed in rather than read from the wall
/// clock so time-dependent behavior (window expiry, denylist pruning) is directly testable.
#[async_trait]
pub trait AuthStore: Send + Sync {
    /// Enroll `user` with `secret` as their TOTP secret, stored base32-encoded (RFC 4648,
    /// unpadded). Errors if the user is already enrolled — re-provisioning is `revoke_user`
    /// followed by a fresh `enroll_user`, which also resets replay/lockout state.
    async fn enroll_user(&self, user: &UserId, secret: &[u8]) -> anyhow::Result<()>;

    /// The stored TOTP secret for `user` (base32-encoded), or `None` if not enrolled.
    async fn totp_secret(&self, user: &UserId) -> anyhow::Result<Option<String>>;

    /// The highest TOTP time-step accepted so far for `user` (0 if none, or not enrolled).
    /// The replay guard compares candidate steps against this.
    async fn last_step(&self, user: &UserId) -> anyhow::Result<u64>;

    /// Advance `user`'s replay floor to `step` **only if** the stored floor is strictly
    /// below it (`UPDATE ... WHERE last_step < ?`). Returns whether this call won; two
    /// concurrent logins presenting the same code cannot both succeed. A success also
    /// clears the failure counter — this is the successful-login path.
    async fn set_last_step(&self, user: &UserId, step: u64) -> anyhow::Result<bool>;

    /// Record one failed login for `user`. Failures within `window` of each other
    /// accumulate; a failure after the window has passed restarts the count at 1.
    async fn record_failure(
        &self,
        user: &UserId,
        now: i64,
        window: std::time::Duration,
    ) -> anyhow::Result<()>;

    /// How many failures for `user` are inside the trailing `window` from `now` — the count
    /// the caller compares against the lockout threshold. 0 for an unknown user.
    async fn failures_within(
        &self,
        user: &UserId,
        now: i64,
        window: std::time::Duration,
    ) -> anyhow::Result<u32>;

    /// The HS256 signing key for `kid`, or `None`. Looked up by the presented `kid` at
    /// verification time so a second key can be introduced before the first is retired.
    async fn signing_key(&self, kid: &str) -> anyhow::Result<Option<Vec<u8>>>;

    /// Store an HS256 signing key under `kid`. Errors on a duplicate `kid`.
    async fn insert_signing_key(&self, kid: &str, key: &[u8]) -> anyhow::Result<()>;

    /// Store a refresh token for `user` expiring at `expires_at`, keeping only its SHA-256
    /// hash — a stolen database yields no usable token.
    async fn insert_refresh(
        &self,
        token: &str,
        user: &UserId,
        expires_at: i64,
    ) -> anyhow::Result<()>;

    /// Atomically consume `token` (single-use). See [`ConsumeRefresh`].
    async fn consume_refresh(&self, token: &str, now: i64) -> anyhow::Result<ConsumeRefresh>;

    /// Revoke every outstanding refresh token for `user` (the response to a reuse).
    async fn revoke_user_refresh(&self, user: &UserId) -> anyhow::Result<()>;

    /// Denylist an access token's `jti` until `expires_at` (its own `exp`). Idempotent:
    /// re-inserting the same `jti` is not an error.
    async fn denylist_insert(&self, jti: &str, expires_at: i64) -> anyhow::Result<()>;

    /// Whether `jti` is denylisted and not yet past its `expires_at`.
    async fn is_denylisted(&self, jti: &str, now: i64) -> anyhow::Result<bool>;

    /// Delete denylist rows past their `expires_at`; returns how many were removed. Called
    /// opportunistically on write and at startup so the table stays bounded.
    async fn prune_denylist(&self, now: i64) -> anyhow::Result<u64>;

    /// How many users are currently enrolled (used by `--promotion-receiver`'s
    /// zero-principal refusal and by the CLI).
    async fn enrolled_count(&self) -> anyhow::Result<u64>;

    /// All enrolled users, in id order.
    async fn list_users(&self) -> anyhow::Result<Vec<UserId>>;

    /// Remove `user` and revoke all of their refresh tokens, in one transaction.
    async fn revoke_user(&self, user: &UserId) -> anyhow::Result<()>;
}

/// An auth store backed by a single sqlite database file.
#[derive(Debug, Clone)]
pub struct SqliteAuthStore {
    pool: SqlitePool,
}

impl SqliteAuthStore {
    /// Open (creating if absent) the sqlite database at `path` and ensure the schema exists.
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();

        // `otto auth enroll` and `otto serve` can open the same database at once, and on a
        // *fresh* file they race to create it. Two distinct steps can then report SQLITE_BUSY
        // despite `busy_timeout`: `connect_with` (the WAL transition takes an exclusive lock
        // busy_timeout cannot wait on) and `init_schema` (`BEGIN IMMEDIATE`). The whole open is
        // retried under one budget, bounded by the `timeout` rather than a between-attempts
        // check — a single attempt can itself block for the full budget.
        tokio::time::timeout(BUSY_BUDGET, async {
            loop {
                match Self::try_open(path).await {
                    Ok(store) => return Ok(store),
                    Err(e) if busy_somewhere(&e) => {
                        // Jitter, so N racing processes do not retry in lockstep.
                        let jitter = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| u64::from(d.subsec_nanos() % 15_000_000))
                            .unwrap_or(0);
                        tokio::time::sleep(
                            std::time::Duration::from_millis(15)
                                + std::time::Duration::from_nanos(jitter),
                        )
                        .await;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "timed out after {}s waiting for another process to release the auth database \
                 at {}; if no other otto is running, the file may be locked by a stale process",
                BUSY_BUDGET.as_secs(),
                path.display()
            ))
        })
    }

    /// One attempt at [`Self::open`]. Any step may fail with SQLITE_BUSY; the caller retries.
    async fn try_open(path: &Path) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(BUSY_BUDGET);
        let pool = SqlitePoolOptions::new().connect_with(opts).await?;
        let store = Self { pool };
        if let Err(e) = store.init_schema(path).await {
            // Close rather than drop: on the retry path, dropping leaves handles and WAL shm
            // mappings open against the very file we are waiting on a lock for.
            store.pool.close().await;
            return Err(e);
        }
        Ok(store)
    }

    /// Create the schema on a fresh database, or verify an existing one is the shape we expect.
    ///
    /// **Both** paths are transactional: creation runs inside one `BEGIN IMMEDIATE` and the
    /// read-only probe inside a deferred `BEGIN`, so a second process opening the same fresh
    /// file cannot observe the window between `CREATE TABLE users` committing and the version
    /// being stamped. See `persistence`'s `sqlite.rs` for the full argument.
    async fn init_schema(&self, path: &Path) -> anyhow::Result<()> {
        {
            let mut tx = self.pool.begin().await?;
            if Self::schema_is_current(&mut tx, path).await? {
                return Ok(());
            }
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // Re-probe under the write lock: another process may have created and stamped the
        // database while we waited for it.
        if !Self::schema_is_current(&mut tx, path).await? {
            Self::create_schema(&mut tx, path).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// `Ok(true)` when the database is already at `SCHEMA_VERSION`, `Ok(false)` when it is
    /// fresh and must be created. Errors on a database this build cannot use.
    ///
    /// **Must be called inside a transaction**: it issues two statements, and in WAL each
    /// implicit transaction takes its own snapshot, so a concurrent creator committing between
    /// them would otherwise read `user_version == 0` *with* `users` already present.
    async fn schema_is_current(
        conn: &mut sqlx::SqliteConnection,
        path: &Path,
    ) -> anyhow::Result<bool> {
        let (user_version,): (i64,) = sqlx::query_as("PRAGMA user_version")
            .fetch_one(&mut *conn)
            .await?;
        let users_exists: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name='users'")
                .fetch_optional(&mut *conn)
                .await?;

        match (user_version, users_exists.is_some()) {
            // Fresh, or tables exist but are unstamped (a hand-crafted file): create-and-stamp.
            // There is deliberately no legacy arm — this database is new in this slice.
            (0, _) => Ok(false),
            (v, _) if v == SCHEMA_VERSION => Ok(true),
            (v, _) if v > SCHEMA_VERSION => anyhow::bail!(
                "auth database at {} has schema version {v}, newer than this otto build \
                 understands ({SCHEMA_VERSION}); upgrade otto",
                path.display()
            ),
            (v, _) => anyhow::bail!(
                "auth database at {} has schema version {v}, older than this otto build \
                 requires ({SCHEMA_VERSION}), and otto has no auth migration path: delete the \
                 file and let otto re-create it.",
                path.display()
            ),
        }
    }

    /// Create the schema and stamp the version. Runs inside the caller's write transaction.
    async fn create_schema(conn: &mut sqlx::SqliteConnection, _path: &Path) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                totp_secret TEXT NOT NULL,
                last_step INTEGER NOT NULL DEFAULT 0,
                failure_count INTEGER NOT NULL DEFAULT 0,
                failure_window INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS signing_keys (
                kid TEXT PRIMARY KEY,
                key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS refresh_tokens (
                hash TEXT PRIMARY KEY,
                user TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                consumed_at INTEGER
            )",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS refresh_tokens_user_idx ON refresh_tokens (user)")
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS denylist (
                jti TEXT PRIMARY KEY,
                expires_at INTEGER NOT NULL
            )",
        )
        .execute(&mut *conn)
        .await?;

        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .execute(&mut *conn)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl AuthStore for SqliteAuthStore {
    async fn enroll_user(&self, user: &UserId, secret: &[u8]) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO users (id, totp_secret, last_step, failure_count, failure_window)
             VALUES (?1, ?2, 0, 0, 0)",
        )
        .bind(user.as_str())
        .bind(BASE32_NOPAD.encode(secret))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn totp_secret(&self, user: &UserId) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT totp_secret FROM users WHERE id = ?1")
            .bind(user.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(secret,)| secret))
    }

    async fn last_step(&self, user: &UserId) -> anyhow::Result<u64> {
        let row: (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(last_step), 0) FROM users WHERE id = ?1")
                .bind(user.as_str())
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0 as u64)
    }

    async fn set_last_step(&self, user: &UserId, step: u64) -> anyhow::Result<bool> {
        // The `AND last_step < ?` is the replay guard itself: two concurrent logins with the
        // same code both try this, and sqlite serializes writes so exactly one sees a floor
        // strictly below `step`. A success also clears the lockout counter.
        let result = sqlx::query(
            "UPDATE users SET last_step = ?1, failure_count = 0, failure_window = 0
             WHERE id = ?2 AND last_step < ?1",
        )
        .bind(step as i64)
        .bind(user.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn record_failure(
        &self,
        user: &UserId,
        now: i64,
        window: std::time::Duration,
    ) -> anyhow::Result<()> {
        // The CASE restarts the count at 1 once the previous failures have fallen out of the
        // window, so a stale count can never stack into a lockout on its own.
        let result = sqlx::query(
            "UPDATE users SET
                failure_count = CASE WHEN ?1 - failure_window < ?2
                    THEN failure_count + 1 ELSE 1 END,
                failure_window = ?1
             WHERE id = ?3",
        )
        .bind(now)
        .bind(window.as_secs() as i64)
        .bind(user.as_str())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!("record_failure: no enrolled user {}", user);
        }
        Ok(())
    }

    async fn failures_within(
        &self,
        user: &UserId,
        now: i64,
        window: std::time::Duration,
    ) -> anyhow::Result<u32> {
        let row: Option<(i64, i64)> =
            sqlx::query_as("SELECT failure_count, failure_window FROM users WHERE id = ?1")
                .bind(user.as_str())
                .fetch_optional(&self.pool)
                .await?;
        let Some((count, window_start)) = row else {
            return Ok(0);
        };
        if now - window_start >= window.as_secs() as i64 {
            return Ok(0);
        }
        Ok(count as u32)
    }

    async fn signing_key(&self, kid: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT key FROM signing_keys WHERE kid = ?1")
            .bind(kid)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|(hex,)| HEXLOWER.decode(hex.as_bytes()).map_err(Into::into))
            .transpose()
    }

    async fn insert_signing_key(&self, kid: &str, key: &[u8]) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO signing_keys (kid, key, created_at) VALUES (?1, ?2, ?3)")
            .bind(kid)
            .bind(HEXLOWER.encode(key))
            .bind(now_secs())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn insert_refresh(
        &self,
        token: &str,
        user: &UserId,
        expires_at: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO refresh_tokens (hash, user, expires_at, consumed_at)
             VALUES (?1, ?2, ?3, NULL)",
        )
        .bind(hash_refresh(token))
        .bind(user.as_str())
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn consume_refresh(&self, token: &str, now: i64) -> anyhow::Result<ConsumeRefresh> {
        let hash = hash_refresh(token);
        // The atomic single-use consume AND the user lookup happen in one statement:
        // `UPDATE ... WHERE consumed_at IS NULL RETURNING user` yields the user's row only if
        // THIS call won the single-use race (sqlite 3.35+ RETURNING, supported by sqlx 0.8).
        // There is no window between the claim and the read, so a concurrent
        // `revoke_user_refresh`/`revoke_user` deleting the just-claimed row can no longer make
        // a follow-up lookup come up empty — the old two-statement version panicked there.
        let user: Option<(String,)> = sqlx::query_as(
            "UPDATE refresh_tokens SET consumed_at = ?1
             WHERE hash = ?2 AND consumed_at IS NULL
             RETURNING user",
        )
        .bind(now)
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((user,)) = user {
            let user = UserId::parse(&user)
                .map_err(|e| anyhow::anyhow!("refresh_tokens: stored user is invalid: {e}"))?;
            return Ok(ConsumeRefresh::Consumed { user });
        }
        // The row is absent (never issued) or already consumed — tell them apart so a reuse
        // can revoke the user's whole set. The lookup tolerates the row having vanished since
        // the losing UPDATE (a concurrent revoke deleting it wholesale), reporting `Unknown`
        // rather than panicking.
        match self.refresh_user(&hash).await? {
            None => Ok(ConsumeRefresh::Unknown),
            Some(user) => Ok(ConsumeRefresh::Reused { user }),
        }
    }

    async fn revoke_user_refresh(&self, user: &UserId) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM refresh_tokens WHERE user = ?1")
            .bind(user.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn denylist_insert(&self, jti: &str, expires_at: i64) -> anyhow::Result<()> {
        // OR IGNORE: logging out a token that was already logged out is not an error.
        sqlx::query("INSERT OR IGNORE INTO denylist (jti, expires_at) VALUES (?1, ?2)")
            .bind(jti)
            .bind(expires_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn is_denylisted(&self, jti: &str, now: i64) -> anyhow::Result<bool> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM denylist WHERE jti = ?1 AND expires_at > ?2")
                .bind(jti)
                .bind(now)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    async fn prune_denylist(&self, now: i64) -> anyhow::Result<u64> {
        let result = sqlx::query("DELETE FROM denylist WHERE expires_at <= ?1")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn enrolled_count(&self) -> anyhow::Result<u64> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 as u64)
    }

    async fn list_users(&self) -> anyhow::Result<Vec<UserId>> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM users ORDER BY id ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|(id,)| {
                UserId::parse(&id)
                    .map_err(|e| anyhow::anyhow!("list_users: stored user id is invalid: {e}"))
            })
            .collect()
    }

    async fn revoke_user(&self, user: &UserId) -> anyhow::Result<()> {
        // Atomic: either the user and all their refresh tokens go, or nothing does.
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM refresh_tokens WHERE user = ?1")
            .bind(user.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM users WHERE id = ?1")
            .bind(user.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

impl SqliteAuthStore {
    /// The user owning the refresh-token row for `hash`, or `None`. The row's own user is
    /// returned (not the argument) so the reuse path never trusts caller-supplied identity.
    async fn refresh_user(&self, hash: &str) -> anyhow::Result<Option<UserId>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT user FROM refresh_tokens WHERE hash = ?1")
                .bind(hash)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(user,)| {
            UserId::parse(&user)
                .map_err(|e| anyhow::anyhow!("refresh_tokens: stored user is invalid: {e}"))
        })
        .transpose()
    }
}

/// The SHA-256 hex digest of a refresh token — the only thing ever stored.
fn hash_refresh(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Seconds since the Unix epoch, for `signing_keys.created_at`.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use otto_protocol::UserId;

    use super::*;

    /// The 15-minute failure window from the spec (§6.1).
    const WINDOW: Duration = Duration::from_secs(15 * 60);
    /// A fixed "now" in unix seconds, so time-dependent assertions are deterministic.
    const NOW: i64 = 1_700_000_000;

    fn alice() -> UserId {
        UserId::parse("alice").unwrap()
    }

    fn bob() -> UserId {
        UserId::parse("bob").unwrap()
    }

    /// A 20-byte secret: TOTP secrets are 20 random bytes (spec §6.1).
    fn secret() -> Vec<u8> {
        (0..20u8).collect()
    }

    /// Opens a fresh store in a temp dir. The returned `TempDir` must be kept alive for
    /// the duration of the test so the database file is not deleted.
    async fn temp_store() -> (SqliteAuthStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteAuthStore::open(dir.path().join("auth.db"))
            .await
            .unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn enroll_then_read_back_the_secret() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        // The secret comes back base32-encoded (RFC 4648, unpadded).
        let stored = store
            .totp_secret(&alice())
            .await
            .unwrap()
            .expect("alice is enrolled");
        assert_eq!(stored, data_encoding::BASE32_NOPAD.encode(&secret()));
    }

    #[tokio::test]
    async fn enrolling_the_same_user_twice_is_an_error() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        assert!(store.enroll_user(&alice(), &secret()).await.is_err());
    }

    #[tokio::test]
    async fn totp_secret_for_an_unknown_user_is_none() {
        let (store, _dir) = temp_store().await;
        assert!(store.totp_secret(&alice()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn last_step_starts_zero_for_an_unknown_user() {
        let (store, _dir) = temp_store().await;
        assert_eq!(store.last_step(&alice()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn last_step_advances_only_via_strictly_greater_steps() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        assert!(store.set_last_step(&alice(), 5).await.unwrap());
        // Same step again: no longer strictly greater, so it must not win.
        assert!(!store.set_last_step(&alice(), 5).await.unwrap());
        // A later step wins, and a step that has already been used can never be
        // re-accepted — that is the replay guard.
        assert!(store.set_last_step(&alice(), 7).await.unwrap());
        assert!(!store.set_last_step(&alice(), 6).await.unwrap());
        assert_eq!(store.last_step(&alice()).await.unwrap(), 7);
    }

    /// Two concurrent logins presenting the same code both attempt
    /// `UPDATE ... WHERE last_step < ?`; exactly one of them can win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_same_step_writes_have_exactly_one_winner() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move { store.set_last_step(&alice(), 10).await.unwrap() })
            })
            .collect();
        let mut winners = 0;
        for handle in handles {
            if handle.await.unwrap() {
                winners += 1;
            }
        }
        assert_eq!(
            winners, 1,
            "exactly one concurrent same-step write must win"
        );
        assert_eq!(store.last_step(&alice()).await.unwrap(), 10);
    }

    #[tokio::test]
    async fn set_last_step_resets_the_failure_counter() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        for i in 0..4 {
            store
                .record_failure(&alice(), NOW + i, WINDOW)
                .await
                .unwrap();
        }
        assert_eq!(
            store
                .failures_within(&alice(), NOW + 4, WINDOW)
                .await
                .unwrap(),
            4
        );
        assert!(store.set_last_step(&alice(), 1).await.unwrap());
        assert_eq!(
            store
                .failures_within(&alice(), NOW + 5, WINDOW)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn failures_accumulate_within_the_window() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        assert_eq!(
            store.failures_within(&alice(), NOW, WINDOW).await.unwrap(),
            0
        );
        store.record_failure(&alice(), NOW, WINDOW).await.unwrap();
        store
            .record_failure(&alice(), NOW + 10, WINDOW)
            .await
            .unwrap();
        store
            .record_failure(&alice(), NOW + 20, WINDOW)
            .await
            .unwrap();
        assert_eq!(
            store
                .failures_within(&alice(), NOW + 21, WINDOW)
                .await
                .unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn failures_expire_outside_the_window() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        for i in 0..4 {
            store
                .record_failure(&alice(), NOW + i, WINDOW)
                .await
                .unwrap();
        }
        // The window is anchored at the LAST failure: all four still count just before
        // the 15 minutes from it elapse...
        assert_eq!(
            store
                .failures_within(&alice(), NOW + 902, WINDOW)
                .await
                .unwrap(),
            4
        );
        // ...and none once it has.
        assert_eq!(
            store
                .failures_within(&alice(), NOW + 903, WINDOW)
                .await
                .unwrap(),
            0
        );
        // The next failure after expiry restarts the count at 1 rather than reaching 5.
        store
            .record_failure(&alice(), NOW + 904, WINDOW)
            .await
            .unwrap();
        assert_eq!(
            store
                .failures_within(&alice(), NOW + 904, WINDOW)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn refresh_tokens_are_inserted_and_consumed_single_use() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        store
            .insert_refresh("rt-1", &alice(), NOW + 3600)
            .await
            .unwrap();

        assert_eq!(
            store.consume_refresh("rt-1", NOW).await.unwrap(),
            ConsumeRefresh::Consumed { user: alice() }
        );
        // A second consume of the same token is a reuse, and reports the user so the
        // caller can revoke their whole outstanding set.
        assert_eq!(
            store.consume_refresh("rt-1", NOW).await.unwrap(),
            ConsumeRefresh::Reused { user: alice() }
        );
    }

    #[tokio::test]
    async fn consuming_an_unknown_refresh_token_is_unknown() {
        let (store, _dir) = temp_store().await;
        assert_eq!(
            store.consume_refresh("never-issued", NOW).await.unwrap(),
            ConsumeRefresh::Unknown
        );
    }

    #[tokio::test]
    async fn revoke_user_refresh_removes_only_that_users_tokens() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        store.enroll_user(&bob(), &secret()).await.unwrap();
        store
            .insert_refresh("alice-1", &alice(), NOW + 3600)
            .await
            .unwrap();
        store
            .insert_refresh("alice-2", &alice(), NOW + 3600)
            .await
            .unwrap();
        store
            .insert_refresh("bob-1", &bob(), NOW + 3600)
            .await
            .unwrap();

        store.revoke_user_refresh(&alice()).await.unwrap();
        assert_eq!(
            store.consume_refresh("alice-1", NOW).await.unwrap(),
            ConsumeRefresh::Unknown
        );
        assert_eq!(
            store.consume_refresh("alice-2", NOW).await.unwrap(),
            ConsumeRefresh::Unknown
        );
        assert_eq!(
            store.consume_refresh("bob-1", NOW).await.unwrap(),
            ConsumeRefresh::Consumed { user: bob() }
        );
    }

    /// The deterministic half of the consume+revoke race: consume a token, revoke the whole
    /// set (which deletes the just-consumed row), then present the token again. The old
    /// two-statement implementation would panic here only if the row vanished between the
    /// winning UPDATE and its follow-up lookup; this sequence guarantees the row is gone by
    /// the time of any re-presentation, and the method must answer `Unknown` — never panic.
    #[tokio::test]
    async fn consuming_a_token_after_its_set_is_revoked_is_unknown() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        store
            .insert_refresh("alice-1", &alice(), NOW + 3600)
            .await
            .unwrap();

        assert_eq!(
            store.consume_refresh("alice-1", NOW).await.unwrap(),
            ConsumeRefresh::Consumed { user: alice() }
        );
        store.revoke_user_refresh(&alice()).await.unwrap();
        assert_eq!(
            store.consume_refresh("alice-1", NOW).await.unwrap(),
            ConsumeRefresh::Unknown
        );
    }

    /// The nondeterministic half: consumers and a whole-set revoker race, so a consume can
    /// land between the revoker's DELETE and... nothing — the atomic `RETURNING` consume and
    /// the revoke are both single statements. Whatever interleaving happens, `consume_refresh`
    /// must return a variant (never panic, never error).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_consume_and_revoke_never_panics() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let store = store.clone();
                tokio::spawn(async move {
                    let token = format!("rt-{i}");
                    store
                        .insert_refresh(&token, &alice(), NOW + 3600)
                        .await
                        .unwrap();
                    let consumer = {
                        let store = store.clone();
                        let token = token.clone();
                        tokio::spawn(async move { store.consume_refresh(&token, NOW).await })
                    };
                    let revoker = {
                        let store = store.clone();
                        tokio::spawn(async move { store.revoke_user_refresh(&alice()).await })
                    };
                    let (consumed, revoked) = (consumer.await, revoker.await);
                    // A panicked task (the old `.expect`) fails here via the JoinError.
                    let consumed = consumed
                        .expect("consume task panicked")
                        .expect("consume_refresh errored");
                    revoked
                        .expect("revoke task panicked")
                        .expect("revoke errored");
                    match consumed {
                        ConsumeRefresh::Consumed { .. }
                        | ConsumeRefresh::Reused { .. }
                        | ConsumeRefresh::Unknown => {}
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.await.expect("task panicked");
        }
    }

    #[tokio::test]
    async fn denylist_insert_check_and_prune() {
        let (store, _dir) = temp_store().await;
        store.denylist_insert("jti-a", NOW + 100).await.unwrap();
        store.denylist_insert("jti-b", NOW + 200).await.unwrap();

        assert!(store.is_denylisted("jti-a", NOW).await.unwrap());
        assert!(store.is_denylisted("jti-b", NOW).await.unwrap());
        // A jti past its own expiry is no longer denylisted.
        assert!(!store.is_denylisted("jti-a", NOW + 101).await.unwrap());
        assert!(!store.is_denylisted("never-inserted", NOW).await.unwrap());

        // Pruning removes only expired rows, and reports how many went.
        assert_eq!(store.prune_denylist(NOW + 150).await.unwrap(), 1);
        assert!(!store.is_denylisted("jti-a", NOW).await.unwrap());
        assert!(store.is_denylisted("jti-b", NOW).await.unwrap());
    }

    #[tokio::test]
    async fn denylist_insert_is_idempotent_for_a_duplicate_jti() {
        let (store, _dir) = temp_store().await;
        store.denylist_insert("jti-a", NOW + 100).await.unwrap();
        // Logging out the same token twice must not error on the primary key.
        store.denylist_insert("jti-a", NOW + 100).await.unwrap();
        assert!(store.is_denylisted("jti-a", NOW).await.unwrap());
    }

    #[tokio::test]
    async fn enrolled_count_and_list_users_are_ordered() {
        let (store, _dir) = temp_store().await;
        assert_eq!(store.enrolled_count().await.unwrap(), 0);
        store.enroll_user(&bob(), &secret()).await.unwrap();
        store.enroll_user(&alice(), &secret()).await.unwrap();
        assert_eq!(store.enrolled_count().await.unwrap(), 2);
        assert_eq!(store.list_users().await.unwrap(), vec![alice(), bob()]);
    }

    #[tokio::test]
    async fn revoke_user_removes_the_user_and_their_refresh_tokens() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), &secret()).await.unwrap();
        store
            .insert_refresh("alice-1", &alice(), NOW + 3600)
            .await
            .unwrap();

        store.revoke_user(&alice()).await.unwrap();
        assert_eq!(store.enrolled_count().await.unwrap(), 0);
        assert!(store.totp_secret(&alice()).await.unwrap().is_none());
        assert_eq!(
            store.consume_refresh("alice-1", NOW).await.unwrap(),
            ConsumeRefresh::Unknown
        );
    }

    #[tokio::test]
    async fn signing_keys_round_trip_by_kid() {
        let (store, _dir) = temp_store().await;
        assert!(store.signing_key("kid-1").await.unwrap().is_none());
        let key = vec![7u8; 32];
        store.insert_signing_key("kid-1", &key).await.unwrap();
        assert_eq!(store.signing_key("kid-1").await.unwrap().unwrap(), key);
        // A duplicate kid is a hard error, not a silent overwrite.
        assert!(store.insert_signing_key("kid-1", &key).await.is_err());
    }

    #[tokio::test]
    async fn fresh_database_is_stamped_with_the_schema_version() {
        let (store, _dir) = temp_store().await;
        let v: (i64,) = sqlx::query_as("PRAGMA user_version")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(v.0, SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn opening_a_forward_version_database_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        {
            let store = SqliteAuthStore::open(&path).await.unwrap();
            sqlx::query(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
                .execute(&store.pool)
                .await
                .unwrap();
            store.pool.close().await;
        }
        let err = SqliteAuthStore::open(&path).await.unwrap_err().to_string();
        assert!(
            err.contains("newer than this otto build"),
            "unexpected: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_opens_of_a_fresh_database_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("racy.db");

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                tokio::spawn(async move { SqliteAuthStore::open(&path).await })
            })
            .collect();

        for (i, handle) in handles.into_iter().enumerate() {
            let store = handle
                .await
                .expect("task panicked")
                .unwrap_or_else(|e| panic!("concurrent open {i} failed: {e}"));
            // ...and every one of them sees a usable, correctly stamped schema.
            let (v,): (i64,) = sqlx::query_as("PRAGMA user_version")
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(v, SCHEMA_VERSION, "open {i} saw an unstamped schema");
        }
    }
}
