//! Sqlite-backed `SessionStore`. Schema is created idempotently at open time with
//! `CREATE TABLE IF NOT EXISTS`, so no migrations dir or compile-time DB is needed and
//! the build/test path stays fully offline.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// A session store backed by a single sqlite database file.
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if absent) the sqlite database at `path` and ensure the schema
    /// exists.
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // WAL lets one writer proceed concurrently with readers.
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new().connect_with(opts).await?;
        let store = Self { pool };
        store.init_schema().await?;
        Ok(store)
    }

    async fn init_schema(&self) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                goal TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                config TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (session_id, seq)
            )",
        )
        .execute(&self.pool)
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::SessionStore for SqliteStore {
    async fn create_session(
        &self,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<otto_protocol::SessionId> {
        let id = otto_protocol::SessionId::new();
        let now = now_millis();
        sqlx::query(
            "INSERT INTO sessions (id, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(id.0.to_string())
        .bind(goal)
        .bind(crate::SessionStatus::Active.as_db_str())
        .bind(now)
        .bind(now)
        .bind(serde_json::to_string(config)?)
        .execute(&self.pool)
        .await?;
        Ok(id)
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
        session: otto_protocol::SessionId,
        after_seq: Option<u64>,
    ) -> anyhow::Result<Vec<otto_protocol::Event>> {
        let bound = match after_seq {
            None => -1i64,
            Some(n) => n as i64,
        };
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT seq, kind FROM events
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC",
        )
        .bind(session.0.to_string())
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
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<crate::SessionStatus> {
        let row: Option<(String,)> = sqlx::query_as("SELECT status FROM sessions WHERE id = ?1")
            .bind(session.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        let (status,) =
            row.ok_or_else(|| anyhow::anyhow!("session_status: no session {}", session.0))?;
        crate::SessionStatus::from_db_str(&status)
    }

    async fn snapshot(
        &self,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<crate::SessionState> {
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT goal, status, config FROM sessions WHERE id = ?1")
                .bind(session.0.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (goal, status, config) =
            row.ok_or_else(|| anyhow::anyhow!("snapshot: no session {}", session.0))?;
        let status = crate::SessionStatus::from_db_str(&status)?;
        let config: serde_json::Value = serde_json::from_str(&config)?;

        let events = self.replay_since(session, None).await?;

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
            "INSERT INTO sessions (id, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(state.id.0.to_string())
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
            .create_session("add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            store.session_status(id).await.unwrap(),
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
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        for (seq, msg) in [(0u64, "a"), (1, "b"), (2, "c")] {
            store
                .append_event(id, &log_event(id, seq, msg))
                .await
                .unwrap();
        }
        let replayed = store.replay_since(id, None).await.unwrap();
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
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        for (seq, msg) in [(0u64, "a"), (1, "b"), (2, "c")] {
            store
                .append_event(id, &log_event(id, seq, msg))
                .await
                .unwrap();
        }
        let gap = store.replay_since(id, Some(1)).await.unwrap();
        assert_eq!(gap, vec![log_event(id, 2, "c")]);
    }

    #[tokio::test]
    async fn append_duplicate_seq_is_error() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("g", &serde_json::json!({}))
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
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        store.set_status(id, SessionStatus::Done).await.unwrap();
        assert_eq!(store.session_status(id).await.unwrap(), SessionStatus::Done);
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
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
    }

    #[tokio::test]
    async fn record_turn_rejects_duplicate_index() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
        assert!(store.record_turn(id, &turn(0, false)).await.is_err());
    }

    #[tokio::test]
    async fn replay_since_unknown_session_is_empty() {
        let (store, _dir) = temp_store().await;
        let missing = otto_protocol::SessionId::new();
        assert!(store.replay_since(missing, None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn snapshot_captures_metadata_events_and_turns() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("the goal", &serde_json::json!({ "k": 1 }))
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

        let snap = store.snapshot(id).await.unwrap();
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
        assert!(store.snapshot(missing).await.is_err());
    }

    #[tokio::test]
    async fn snapshot_restore_round_trips_into_a_fresh_store() {
        let (source, _d1) = temp_store().await;
        let id = source
            .create_session("g", &serde_json::json!({ "m": "x" }))
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
        let snap = source.snapshot(id).await.unwrap();

        let (target, _d2) = temp_store().await;
        let restored_id = target.restore(&snap).await.unwrap();
        assert_eq!(restored_id, id);

        // Re-snapshotting the target yields an identical SessionState (preserved id/seqs).
        assert_eq!(target.snapshot(id).await.unwrap(), snap);
        assert_eq!(target.replay_since(id, None).await.unwrap(), snap.events);
        assert_eq!(
            target.session_status(id).await.unwrap(),
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
            goal: "g".to_string(),
            status: SessionStatus::Done,
            config: serde_json::json!({}),
            events: vec![log_event(id, 0, "a"), log_event(id, 0, "dup")],
            turns: vec![],
        };
        assert!(store.restore(&state).await.is_err());
        // The transaction rolled back: no stranded session row.
        assert!(store.session_status(id).await.is_err());
    }

    #[tokio::test]
    async fn restore_into_existing_session_is_error() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        let snap = store.snapshot(id).await.unwrap();
        // Restoring into the same store collides on the sessions primary key.
        assert!(store.restore(&snap).await.is_err());
    }

    #[tokio::test]
    async fn cursors_advance_with_events_and_turns() {
        let (store, _dir) = temp_store().await;
        let id = store.create_session("g", &serde_json::json!({})).await.unwrap();
        assert_eq!(store.next_seq(id).await.unwrap(), 0);
        assert_eq!(store.next_turn(id).await.unwrap(), 0);
        store.append_event(id, &log_event(id, 0, "a")).await.unwrap();
        store.append_event(id, &log_event(id, 1, "b")).await.unwrap();
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
            .create_session("a", &serde_json::json!({}))
            .await
            .unwrap();
        let b = store
            .create_session("b", &serde_json::json!({}))
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
            store.replay_since(a, None).await.unwrap(),
            vec![log_event(a, 0, "for-a")]
        );
        assert_eq!(
            store.replay_since(b, None).await.unwrap(),
            vec![log_event(b, 0, "for-b")]
        );
    }
}
