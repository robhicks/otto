//! Sqlite-backed `SessionStore`. The schema is created at open time with
//! `CREATE TABLE IF NOT EXISTS`, so no migrations dir or compile-time DB is needed and the
//! build/test path stays fully offline.
//!
//! Open is idempotent *within* a schema generation but not *across* one: since there is no
//! migration mechanism, a database written by a different `SCHEMA_VERSION` is refused rather
//! than silently used with the wrong table shape. See [`SqliteStore::init_schema`].

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// Bumped whenever the on-disk schema changes shape. Stamped into `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

/// A session store backed by a single sqlite database file.
#[derive(Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if absent) the sqlite database at `path` and ensure the schema
    /// exists.
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // WAL lets one writer proceed concurrently with readers.
            .journal_mode(SqliteJournalMode::Wal)
            // Two otto processes can open the same default database at once (`otto serve &`
            // then `otto run` both resolve `otto-sessions.db` in the cwd). Wait for the other
            // one's schema transaction instead of failing with SQLITE_BUSY.
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = SqlitePoolOptions::new().connect_with(opts).await?;
        let store = Self { pool };
        store.init_schema(path).await?;
        Ok(store)
    }

    /// Create the schema on a fresh database, or verify an existing one is the shape we expect.
    ///
    /// The crate has no migration mechanism — `CREATE TABLE IF NOT EXISTS` cannot alter an
    /// existing table — so a pre-ownership file would silently keep its old shape and then fail
    /// at query time with "no such column: owner". The probe turns that into one clear error at
    /// open. `PRAGMA user_version` is 0 on both a brand-new file and every pre-ownership one;
    /// the two are told apart by whether `sessions` already exists.
    ///
    /// Takes `path` solely so the error can name the file an operator has to delete.
    ///
    /// Creation runs inside one `BEGIN IMMEDIATE` transaction. That is load-bearing rather than
    /// tidiness: each `execute` auto-commits on its own, so without the transaction a second
    /// process opening the same fresh file could probe in the window after `CREATE TABLE
    /// sessions` committed but before the version was stamped — seeing `user_version == 0`
    /// *and* an existing `sessions` table, which is the pre-ownership signature. It would then
    /// refuse a perfectly good brand-new database and tell the operator to delete it.
    /// `BEGIN IMMEDIATE` takes the write lock up front, so the second process blocks until the
    /// first commits and then re-probes to find the stamped version.
    async fn init_schema(&self, path: &Path) -> anyhow::Result<()> {
        // Fast path, and the overwhelmingly common one: an already-stamped database needs no
        // schema work, so probe read-only and take **no write lock at all**. This matters —
        // several processes can share one file (`otto run` and `otto serve` both default to
        // `otto-sessions.db` in the cwd), and taking a write lock on every open serializes
        // unrelated processes against each other for no benefit.
        {
            let mut conn = self.pool.acquire().await?;
            if Self::schema_is_current(&mut conn, path).await? {
                return Ok(());
            }
        }

        // Slow path, taken once per database. `begin_with` rather than a hand-rolled
        // BEGIN/COMMIT/ROLLBACK: `Transaction`'s `Drop` rolls back on every path that does not
        // reach `commit()`, *including a failed commit*. Rolling that by hand leaks an open
        // transaction back into the pool if COMMIT itself errors, and whichever caller next
        // acquires that connection would then run inside it.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // Re-probe under the write lock: another process may have created and stamped the
        // database while we waited for it.
        if !Self::schema_is_current(&mut tx, path).await? {
            Self::create_schema(&mut tx, path).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// `Ok(true)` when the database is already at `SCHEMA_VERSION`, `Ok(false)` when it is empty
    /// and must be created. Errors on a database this build cannot use.
    ///
    /// Safe to call outside a transaction: every arm either errors or is re-checked under the
    /// write lock before anything is created.
    async fn schema_is_current(
        conn: &mut sqlx::SqliteConnection,
        path: &Path,
    ) -> anyhow::Result<bool> {
        let (user_version,): (i64,) = sqlx::query_as("PRAGMA user_version")
            .fetch_one(&mut *conn)
            .await?;
        let sessions_exists: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name='sessions'")
                .fetch_optional(&mut *conn)
                .await?;

        match (user_version, sessions_exists.is_some()) {
            (0, false) => Ok(false),
            (0, true) => anyhow::bail!(
                "session database at {} predates session ownership (issue #115) and has no \
                 owner column. otto has no installed base, so there is no migration: delete \
                 the file and let otto re-create it.",
                path.display()
            ),
            (v, _) if v == SCHEMA_VERSION => Ok(true),
            // Split by direction: once SCHEMA_VERSION is next bumped, a genuinely older
            // database would otherwise be told it is "newer", which is backwards. Neither
            // direction has a migration story, but the message has to be true.
            (v, _) if v > SCHEMA_VERSION => anyhow::bail!(
                "session database at {} has schema version {v}, newer than this otto build \
                 understands ({SCHEMA_VERSION}); upgrade otto",
                path.display()
            ),
            (v, _) => anyhow::bail!(
                "session database at {} has schema version {v}, older than this otto build \
                 requires ({SCHEMA_VERSION}), and otto has no migration path: delete the file \
                 and let otto re-create it.",
                path.display()
            ),
        }
    }

    /// Create the schema and stamp the version. Runs inside the caller's write transaction.
    async fn create_schema(conn: &mut sqlx::SqliteConnection, _path: &Path) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                owner TEXT NOT NULL,
                goal TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                config TEXT NOT NULL
            )",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS sessions_owner_idx ON sessions (owner)")
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (session_id, seq)
            )",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS turns (
                session_id TEXT NOT NULL,
                turn_index INTEGER NOT NULL,
                goal TEXT NOT NULL,
                outcome TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, turn_index)
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

#[async_trait::async_trait]
impl crate::SessionStore for SqliteStore {
    async fn create_session(
        &self,
        owner: &otto_protocol::UserId,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<otto_protocol::SessionId> {
        let id = otto_protocol::SessionId::new();
        let now = now_millis();
        sqlx::query(
            "INSERT INTO sessions (id, owner, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id.0.to_string())
        .bind(owner.as_str())
        .bind(goal)
        .bind(crate::SessionStatus::Active.as_db_str())
        .bind(now)
        .bind(now)
        .bind(serde_json::to_string(config)?)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    async fn owner_of(
        &self,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<otto_protocol::UserId> {
        let row: Option<(String,)> = sqlx::query_as("SELECT owner FROM sessions WHERE id = ?1")
            .bind(session.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        let (owner,) = row.ok_or_else(|| no_such_session("owner_of", session))?;
        // No `Ok(...?)` here — clippy's needless_question_mark fires on it.
        otto_protocol::UserId::parse(&owner)
            .map_err(|e| anyhow::anyhow!("owner_of: stored owner is invalid: {e}"))
    }

    async fn append_event(
        &self,
        session: otto_protocol::SessionId,
        event: &otto_protocol::Event,
    ) -> anyhow::Result<()> {
        let kind = serde_json::to_string(&event.kind)?;
        sqlx::query("INSERT INTO events (session_id, seq, kind) VALUES (?1, ?2, ?3)")
            .bind(session.0.to_string())
            .bind(event.seq as i64)
            .bind(kind)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn record_turn(
        &self,
        session: otto_protocol::SessionId,
        turn: &crate::TurnRecord,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO turns (session_id, turn_index, goal, outcome, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(session.0.to_string())
        .bind(turn.turn_index as i64)
        .bind(&turn.goal)
        .bind(serde_json::to_string(&turn.outcome)?)
        .bind(now_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_status(
        &self,
        session: otto_protocol::SessionId,
        status: crate::SessionStatus,
    ) -> anyhow::Result<()> {
        let result = sqlx::query("UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(status.as_db_str())
            .bind(now_millis())
            .bind(session.0.to_string())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!("set_status: no session {}", session.0);
        }
        Ok(())
    }

    async fn replay_since(
        &self,
        owner: &otto_protocol::UserId,
        session: otto_protocol::SessionId,
        after_seq: Option<u64>,
    ) -> anyhow::Result<Vec<otto_protocol::Event>> {
        let bound = match after_seq {
            None => -1i64,
            Some(n) => i64::try_from(n).unwrap_or(i64::MAX),
        };
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT e.seq, e.kind FROM events e
             JOIN sessions s ON s.id = e.session_id
             WHERE e.session_id = ?1 AND s.owner = ?2 AND e.seq > ?3
             ORDER BY e.seq ASC",
        )
        .bind(session.0.to_string())
        .bind(owner.as_str())
        .bind(bound)
        .fetch_all(&self.pool)
        .await?;
        let mut events = Vec::with_capacity(rows.len());
        for (seq, kind) in rows {
            events.push(otto_protocol::Event {
                seq: seq as u64,
                session,
                kind: serde_json::from_str(&kind)?,
            });
        }
        Ok(events)
    }

    async fn session_status(
        &self,
        owner: &otto_protocol::UserId,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<crate::SessionStatus> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM sessions WHERE id = ?1 AND owner = ?2")
                .bind(session.0.to_string())
                .bind(owner.as_str())
                .fetch_optional(&self.pool)
                .await?;
        let (status,) = row.ok_or_else(|| no_such_session("session_status", session))?;
        crate::SessionStatus::from_db_str(&status)
    }

    async fn snapshot(
        &self,
        owner: &otto_protocol::UserId,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<crate::SessionState> {
        let row: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT owner, goal, status, config FROM sessions WHERE id = ?1 AND owner = ?2",
        )
        .bind(session.0.to_string())
        .bind(owner.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let (_owner_str, goal, status, config) =
            row.ok_or_else(|| no_such_session("snapshot", session))?;
        let status = crate::SessionStatus::from_db_str(&status)?;
        let config: serde_json::Value = serde_json::from_str(&config)?;

        let events = self.replay_since(owner, session, None).await?;

        let turn_rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT turn_index, goal, outcome FROM turns
             WHERE session_id = ?1 ORDER BY turn_index ASC",
        )
        .bind(session.0.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut turns = Vec::with_capacity(turn_rows.len());
        for (turn_index, turn_goal, outcome) in turn_rows {
            turns.push(crate::TurnRecord {
                turn_index: turn_index as u32,
                goal: turn_goal,
                outcome: serde_json::from_str(&outcome)?,
            });
        }

        Ok(crate::SessionState {
            id: session,
            // The row's `owner` column is redundant with the `owner` predicate that just matched
            // it, so the argument is the authority — one fewer parse that can fail.
            owner: owner.clone(),
            goal,
            status,
            config,
            events,
            turns,
        })
    }

    async fn next_seq(&self, session: otto_protocol::SessionId) -> anyhow::Result<u64> {
        let row: (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(seq) + 1, 0) FROM events WHERE session_id = ?1")
                .bind(session.0.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0 as u64)
    }

    async fn next_turn(&self, session: otto_protocol::SessionId) -> anyhow::Result<u32> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(turn_index) + 1, 0) FROM turns WHERE session_id = ?1",
        )
        .bind(session.0.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 as u32)
    }

    async fn restore(
        &self,
        state: &crate::SessionState,
    ) -> anyhow::Result<otto_protocol::SessionId> {
        // Atomic: either the whole session (row + events + turns) lands, or nothing does.
        // Timestamps are regenerated; ids and seqs are preserved.
        let now = now_millis();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO sessions (id, owner, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(state.id.0.to_string())
        .bind(state.owner.as_str())
        .bind(&state.goal)
        .bind(state.status.as_db_str())
        .bind(now)
        .bind(now)
        .bind(serde_json::to_string(&state.config)?)
        .execute(&mut *tx)
        .await?;

        for event in &state.events {
            sqlx::query("INSERT INTO events (session_id, seq, kind) VALUES (?1, ?2, ?3)")
                .bind(state.id.0.to_string())
                .bind(event.seq as i64)
                .bind(serde_json::to_string(&event.kind)?)
                .execute(&mut *tx)
                .await?;
        }

        for turn in &state.turns {
            sqlx::query(
                "INSERT INTO turns (session_id, turn_index, goal, outcome, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(state.id.0.to_string())
            .bind(turn.turn_index as i64)
            .bind(&turn.goal)
            .bind(serde_json::to_string(&turn.outcome)?)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(state.id)
    }

    async fn restore_over(
        &self,
        state: &crate::SessionState,
    ) -> anyhow::Result<otto_protocol::SessionId> {
        // Atomic overwrite: delete any prior rows for this id, then re-insert the whole session.
        // Either the replacement lands completely or nothing changes. Timestamps regenerated.
        let now = now_millis();
        let mut tx = self.pool.begin().await?;

        let id = state.id.0.to_string();
        sqlx::query("DELETE FROM events WHERE session_id = ?1")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM turns WHERE session_id = ?1")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE id = ?1")
            .bind(&id)
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "INSERT INTO sessions (id, owner, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&id)
        .bind(state.owner.as_str())
        .bind(&state.goal)
        .bind(state.status.as_db_str())
        .bind(now)
        .bind(now)
        .bind(serde_json::to_string(&state.config)?)
        .execute(&mut *tx)
        .await?;

        for event in &state.events {
            sqlx::query("INSERT INTO events (session_id, seq, kind) VALUES (?1, ?2, ?3)")
                .bind(&id)
                .bind(event.seq as i64)
                .bind(serde_json::to_string(&event.kind)?)
                .execute(&mut *tx)
                .await?;
        }

        for turn in &state.turns {
            sqlx::query(
                "INSERT INTO turns (session_id, turn_index, goal, outcome, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(&id)
            .bind(turn.turn_index as i64)
            .bind(&turn.goal)
            .bind(serde_json::to_string(&turn.outcome)?)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(state.id)
    }
}

/// The error returned for BOTH "no such session" and "session owned by someone else".
///
/// One constructor, deliberately: if these two ever produced different strings the API would
/// become an existence oracle for other tenants' session ids.
fn no_such_session(op: &str, session: otto_protocol::SessionId) -> anyhow::Error {
    anyhow::anyhow!("{op}: no session {}", session.0)
}

/// Milliseconds since the Unix epoch, for `created_at`/`updated_at`/`started_at`.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionState, SessionStatus, SessionStore, TurnRecord};
    use otto_protocol::{Event, EventKind};

    /// Opens a fresh store in a temp dir. The returned `TempDir` must be kept alive for
    /// the duration of the test so the database file is not deleted.
    async fn temp_store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("test.db")).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        // Opening twice over the same file must not error (schema uses IF NOT EXISTS).
        let _first = SqliteStore::open(&path).await.unwrap();
        let _second = SqliteStore::open(&path).await.unwrap();
    }

    #[tokio::test]
    async fn now_millis_is_positive() {
        let (_store, _dir) = temp_store().await;
        assert!(now_millis() > 0);
    }

    #[tokio::test]
    async fn create_session_starts_active() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(
                &otto_protocol::UserId::local(),
                "add a greeting",
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .session_status(&otto_protocol::UserId::local(), id)
                .await
                .unwrap(),
            SessionStatus::Active
        );
    }

    fn log_event(session: otto_protocol::SessionId, seq: u64, msg: &str) -> Event {
        Event {
            seq,
            session,
            kind: EventKind::Log {
                message: msg.to_string(),
            },
        }
    }

    #[tokio::test]
    async fn append_then_replay_returns_events_in_order() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        for (seq, msg) in [(0u64, "a"), (1, "b"), (2, "c")] {
            store
                .append_event(id, &log_event(id, seq, msg))
                .await
                .unwrap();
        }
        let replayed = store
            .replay_since(&otto_protocol::UserId::local(), id, None)
            .await
            .unwrap();
        assert_eq!(
            replayed,
            vec![
                log_event(id, 0, "a"),
                log_event(id, 1, "b"),
                log_event(id, 2, "c"),
            ]
        );
    }

    #[tokio::test]
    async fn replay_since_returns_only_the_gap() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        for (seq, msg) in [(0u64, "a"), (1, "b"), (2, "c")] {
            store
                .append_event(id, &log_event(id, seq, msg))
                .await
                .unwrap();
        }
        let gap = store
            .replay_since(&otto_protocol::UserId::local(), id, Some(1))
            .await
            .unwrap();
        assert_eq!(gap, vec![log_event(id, 2, "c")]);
    }

    #[tokio::test]
    async fn append_duplicate_seq_is_error() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store
            .append_event(id, &log_event(id, 0, "a"))
            .await
            .unwrap();
        assert!(
            store
                .append_event(id, &log_event(id, 0, "dup"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn set_status_updates_existing_session() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store.set_status(id, SessionStatus::Done).await.unwrap();
        assert_eq!(
            store
                .session_status(&otto_protocol::UserId::local(), id)
                .await
                .unwrap(),
            SessionStatus::Done
        );
    }

    #[tokio::test]
    async fn set_status_on_missing_session_is_error() {
        let (store, _dir) = temp_store().await;
        let missing = otto_protocol::SessionId::new();
        assert!(
            store
                .set_status(missing, SessionStatus::Aborted)
                .await
                .is_err()
        );
    }

    fn turn(idx: u32, ok: bool) -> TurnRecord {
        TurnRecord {
            turn_index: idx,
            goal: "g".to_string(),
            outcome: serde_json::json!({ "ok": ok }),
        }
    }

    #[tokio::test]
    async fn record_turn_succeeds() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
    }

    #[tokio::test]
    async fn record_turn_rejects_duplicate_index() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
        assert!(store.record_turn(id, &turn(0, false)).await.is_err());
    }

    #[tokio::test]
    async fn replay_since_unknown_session_is_empty() {
        let (store, _dir) = temp_store().await;
        let missing = otto_protocol::SessionId::new();
        assert!(
            store
                .replay_since(&otto_protocol::UserId::local(), missing, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn replay_since_huge_after_seq_returns_nothing() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store
            .append_event(id, &log_event(id, 0, "a"))
            .await
            .unwrap();
        store
            .append_event(id, &log_event(id, 1, "b"))
            .await
            .unwrap();
        // A u64 larger than i64::MAX must not wrap to -1 and dump the whole log.
        let gap = store
            .replay_since(&otto_protocol::UserId::local(), id, Some(u64::MAX))
            .await
            .unwrap();
        assert!(gap.is_empty());
    }

    #[tokio::test]
    async fn snapshot_captures_metadata_events_and_turns() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(
                &otto_protocol::UserId::local(),
                "the goal",
                &serde_json::json!({ "k": 1 }),
            )
            .await
            .unwrap();
        store
            .append_event(id, &log_event(id, 0, "a"))
            .await
            .unwrap();
        store
            .append_event(id, &log_event(id, 1, "b"))
            .await
            .unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
        store.set_status(id, SessionStatus::Done).await.unwrap();

        let snap = store
            .snapshot(&otto_protocol::UserId::local(), id)
            .await
            .unwrap();
        assert_eq!(snap.id, id);
        assert_eq!(snap.goal, "the goal");
        assert_eq!(snap.status, SessionStatus::Done);
        assert_eq!(snap.config, serde_json::json!({ "k": 1 }));
        assert_eq!(
            snap.events,
            vec![log_event(id, 0, "a"), log_event(id, 1, "b")]
        );
        assert_eq!(snap.turns, vec![turn(0, true)]);
    }

    #[tokio::test]
    async fn snapshot_unknown_session_is_error() {
        let (store, _dir) = temp_store().await;
        let missing = otto_protocol::SessionId::new();
        assert!(
            store
                .snapshot(&otto_protocol::UserId::local(), missing)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn snapshot_restore_round_trips_into_a_fresh_store() {
        let (source, _d1) = temp_store().await;
        let id = source
            .create_session(
                &otto_protocol::UserId::local(),
                "g",
                &serde_json::json!({ "m": "x" }),
            )
            .await
            .unwrap();
        source
            .append_event(id, &log_event(id, 0, "a"))
            .await
            .unwrap();
        source
            .append_event(id, &log_event(id, 1, "b"))
            .await
            .unwrap();
        source.record_turn(id, &turn(0, true)).await.unwrap();
        source.set_status(id, SessionStatus::Done).await.unwrap();
        let snap = source
            .snapshot(&otto_protocol::UserId::local(), id)
            .await
            .unwrap();

        let (target, _d2) = temp_store().await;
        let restored_id = target.restore(&snap).await.unwrap();
        assert_eq!(restored_id, id);

        // Re-snapshotting the target yields an identical SessionState (preserved id/seqs).
        assert_eq!(
            target
                .snapshot(&otto_protocol::UserId::local(), id)
                .await
                .unwrap(),
            snap
        );
        assert_eq!(
            target
                .replay_since(&otto_protocol::UserId::local(), id, None)
                .await
                .unwrap(),
            snap.events
        );
        assert_eq!(
            target
                .session_status(&otto_protocol::UserId::local(), id)
                .await
                .unwrap(),
            SessionStatus::Done
        );
    }

    #[tokio::test]
    async fn restore_is_atomic_on_inconsistent_state() {
        let (store, _dir) = temp_store().await;
        let id = otto_protocol::SessionId::new();
        // A SessionState with a duplicate seq in its event log is internally inconsistent.
        let state = SessionState {
            id,
            owner: otto_protocol::UserId::local(),
            goal: "g".to_string(),
            status: SessionStatus::Done,
            config: serde_json::json!({}),
            events: vec![log_event(id, 0, "a"), log_event(id, 0, "dup")],
            turns: vec![],
        };
        assert!(store.restore(&state).await.is_err());
        // The transaction rolled back: no stranded session row.
        assert!(
            store
                .session_status(&otto_protocol::UserId::local(), id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn restore_into_existing_session_is_error() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let snap = store
            .snapshot(&otto_protocol::UserId::local(), id)
            .await
            .unwrap();
        // Restoring into the same store collides on the sessions primary key.
        assert!(store.restore(&snap).await.is_err());
    }

    #[tokio::test]
    async fn restore_over_overwrites_an_existing_session() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(
                &otto_protocol::UserId::local(),
                "old goal",
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        // A different snapshot for the SAME id: new goal, fresh event, Done status.
        let advanced = SessionState {
            id,
            owner: otto_protocol::UserId::local(),
            goal: "new goal".to_string(),
            status: SessionStatus::Done,
            config: serde_json::json!({ "m": "y" }),
            events: vec![log_event(id, 0, "fresh")],
            turns: vec![turn(0, true)],
        };
        let returned = store.restore_over(&advanced).await.unwrap();
        assert_eq!(returned, id);
        // The stale row was replaced, not duplicated or rejected.
        assert_eq!(
            store
                .snapshot(&otto_protocol::UserId::local(), id)
                .await
                .unwrap(),
            advanced
        );
        assert_eq!(
            store
                .session_status(&otto_protocol::UserId::local(), id)
                .await
                .unwrap(),
            SessionStatus::Done
        );
    }

    #[tokio::test]
    async fn restore_over_into_a_fresh_store_inserts() {
        let (source, _d1) = temp_store().await;
        let id = source
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        source
            .append_event(id, &log_event(id, 0, "a"))
            .await
            .unwrap();
        let snap = source
            .snapshot(&otto_protocol::UserId::local(), id)
            .await
            .unwrap();

        let (target, _d2) = temp_store().await;
        // No existing row: restore_over behaves like restore (insert).
        assert_eq!(target.restore_over(&snap).await.unwrap(), id);
        assert_eq!(
            target
                .snapshot(&otto_protocol::UserId::local(), id)
                .await
                .unwrap(),
            snap
        );
    }

    #[tokio::test]
    async fn cursors_advance_with_events_and_turns() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(store.next_seq(id).await.unwrap(), 0);
        assert_eq!(store.next_turn(id).await.unwrap(), 0);
        store
            .append_event(id, &log_event(id, 0, "a"))
            .await
            .unwrap();
        store
            .append_event(id, &log_event(id, 1, "b"))
            .await
            .unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
        assert_eq!(store.next_seq(id).await.unwrap(), 2);
        assert_eq!(store.next_turn(id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn cursors_are_zero_for_unknown_session() {
        let (store, _dir) = temp_store().await;
        let missing = otto_protocol::SessionId::new();
        assert_eq!(store.next_seq(missing).await.unwrap(), 0);
        assert_eq!(store.next_turn(missing).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn replay_is_isolated_per_session() {
        let (store, _dir) = temp_store().await;
        let a = store
            .create_session(&otto_protocol::UserId::local(), "a", &serde_json::json!({}))
            .await
            .unwrap();
        let b = store
            .create_session(&otto_protocol::UserId::local(), "b", &serde_json::json!({}))
            .await
            .unwrap();
        store
            .append_event(a, &log_event(a, 0, "for-a"))
            .await
            .unwrap();
        store
            .append_event(b, &log_event(b, 0, "for-b"))
            .await
            .unwrap();
        assert_eq!(
            store
                .replay_since(&otto_protocol::UserId::local(), a, None)
                .await
                .unwrap(),
            vec![log_event(a, 0, "for-a")]
        );
        assert_eq!(
            store
                .replay_since(&otto_protocol::UserId::local(), b, None)
                .await
                .unwrap(),
            vec![log_event(b, 0, "for-b")]
        );
    }

    #[tokio::test]
    async fn create_session_writes_the_owner_column() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let owner: (String,) = sqlx::query_as("SELECT owner FROM sessions WHERE id = ?1")
            .bind(id.0.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(owner.0, "local");
    }

    #[tokio::test]
    async fn snapshot_carries_the_owner() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let state = store
            .snapshot(&otto_protocol::UserId::local(), id)
            .await
            .unwrap();
        assert_eq!(state.owner, otto_protocol::UserId::local());
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

    /// A pre-ownership database must fail loudly at open, not at some later query with a
    /// confusing "no such column: owner". There is no installed base, so there is no migration.
    #[tokio::test]
    async fn opening_a_pre_ownership_database_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");

        // Build the OLD schema by hand: `sessions` with no `owner`, user_version left at 0.
        {
            let opts = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
            sqlx::query(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, goal TEXT NOT NULL, status TEXT NOT NULL,
                    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, config TEXT NOT NULL
                )",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let err = SqliteStore::open(&path).await.unwrap_err().to_string();
        assert!(
            err.contains("predates session ownership"),
            "unexpected: {err}"
        );
        assert!(err.contains("delete the file"), "unexpected: {err}");
        // Spec success criterion 5: the message must name the file, or an operator cannot act
        // on it. This is why the probe takes the path (see Step 4).
        assert!(err.contains("legacy.db"), "error must name the file: {err}");
    }

    /// sqlite will happily let an older binary open a newer file; refuse instead of corrupting it.
    #[tokio::test]
    async fn opening_a_forward_version_database_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        {
            let store = SqliteStore::open(&path).await.unwrap();
            sqlx::query(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
                .execute(&store.pool)
                .await
                .unwrap();
            store.pool.close().await;
        }
        let err = SqliteStore::open(&path).await.unwrap_err().to_string();
        assert!(
            err.contains("newer than this otto build"),
            "unexpected: {err}"
        );
    }

    /// Two otto processes can open the same default database at once — `otto serve &` followed
    /// by `otto run` both resolve `otto-sessions.db` in the cwd. Before the schema probe, the
    /// DDL, and the version stamp were wrapped in one `BEGIN IMMEDIATE` transaction, the second
    /// opener could observe the window after `CREATE TABLE sessions` committed but before the
    /// version was stamped — `user_version == 0` with a `sessions` table present, which is
    /// exactly the pre-ownership signature — and refuse a brand-new database, telling the
    /// operator to delete it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_opens_of_a_fresh_database_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("racy.db");

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                tokio::spawn(async move { SqliteStore::open(&path).await })
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

    fn alice() -> otto_protocol::UserId {
        otto_protocol::UserId::parse("alice").unwrap()
    }
    fn bob() -> otto_protocol::UserId {
        otto_protocol::UserId::parse("bob").unwrap()
    }

    #[tokio::test]
    async fn create_session_records_the_given_owner() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(store.owner_of(id).await.unwrap(), alice());
    }

    #[tokio::test]
    async fn scoped_reads_succeed_for_the_owner() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store
            .append_event(id, &log_event(id, 0, "hi"))
            .await
            .unwrap();

        assert_eq!(
            store.replay_since(&alice(), id, None).await.unwrap().len(),
            1
        );
        assert_eq!(
            store.session_status(&alice(), id).await.unwrap(),
            SessionStatus::Active
        );
        assert_eq!(store.snapshot(&alice(), id).await.unwrap().owner, alice());
    }

    /// The API must not be an existence oracle: "someone else's session" and "no such session"
    /// must be indistinguishable. Asserting string equality, not just `is_err`.
    #[tokio::test]
    async fn wrong_owner_is_byte_identical_to_nonexistent() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let missing = otto_protocol::SessionId::new();

        let wrong = store
            .session_status(&bob(), id)
            .await
            .unwrap_err()
            .to_string();
        let absent = store
            .session_status(&bob(), missing)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            wrong.replace(&id.0.to_string(), "ID"),
            absent.replace(&missing.0.to_string(), "ID"),
            "session_status must not distinguish wrong-owner from nonexistent"
        );

        let wrong = store.snapshot(&bob(), id).await.unwrap_err().to_string();
        let absent = store
            .snapshot(&bob(), missing)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            wrong.replace(&id.0.to_string(), "ID"),
            absent.replace(&missing.0.to_string(), "ID"),
            "snapshot must not distinguish wrong-owner from nonexistent"
        );
    }

    /// replay_since returns an empty vec for a nonexistent session today; a wrong owner must
    /// look exactly the same, and must not leak the events.
    #[tokio::test]
    async fn replay_for_the_wrong_owner_is_empty() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store
            .append_event(id, &log_event(id, 0, "secret"))
            .await
            .unwrap();

        assert!(
            store
                .replay_since(&bob(), id, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .replay_since(&bob(), otto_protocol::SessionId::new(), None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn restore_preserves_the_owner() {
        let (src, _d1) = temp_store().await;
        let id = src
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let state = src.snapshot(&alice(), id).await.unwrap();

        let (dst, _d2) = temp_store().await;
        dst.restore(&state).await.unwrap();
        assert_eq!(dst.owner_of(id).await.unwrap(), alice());
    }
}
