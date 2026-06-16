//! Sqlite-backed `SessionStore`. Schema is created idempotently at open time with
//! `CREATE TABLE IF NOT EXISTS`, so no migrations dir or compile-time DB is needed and
//! the build/test path stays fully offline.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

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
            .create_if_missing(true);
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

impl SqliteStore {
    /// Read a session's current status. Errors if the session does not exist.
    pub async fn session_status(
        &self,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<crate::SessionStatus> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM sessions WHERE id = ?1")
                .bind(session.0.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (status,) = row
            .ok_or_else(|| anyhow::anyhow!("session_status: no session {}", session.0))?;
        crate::SessionStatus::from_db_str(&status)
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
        _session: otto_protocol::SessionId,
        _event: &otto_protocol::Event,
    ) -> anyhow::Result<()> {
        unimplemented!("append_event lands in Task 5")
    }

    async fn record_turn(
        &self,
        _session: otto_protocol::SessionId,
        _turn: &crate::TurnRecord,
    ) -> anyhow::Result<()> {
        unimplemented!("record_turn lands in Task 7")
    }

    async fn set_status(
        &self,
        _session: otto_protocol::SessionId,
        _status: crate::SessionStatus,
    ) -> anyhow::Result<()> {
        unimplemented!("set_status lands in Task 6")
    }

    async fn replay_since(
        &self,
        _session: otto_protocol::SessionId,
        _after_seq: u64,
    ) -> anyhow::Result<Vec<otto_protocol::Event>> {
        unimplemented!("replay_since lands in Task 5")
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
    use crate::{SessionStatus, SessionStore};

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
        assert_eq!(store.session_status(id).await.unwrap(), SessionStatus::Active);
    }
}
