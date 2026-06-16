//! Session lifecycle over the persistence store. A `Session` owns the durable session
//! identity and the per-session monotonic `seq`/turn counters; running a prompt drives one
//! orchestrator turn and persists its events, turn record, and status. This maps the
//! protocol commands onto store operations: `CreateSession` -> `create`, `SendPrompt` ->
//! `run_prompt`, `Abort` -> `abort`. (Wire-level `Command` dispatch arrives with `serve`.)

use std::sync::{Arc, Mutex};

use otto_engine_core::traits::Workspace;
use otto_engine_core::{AgentRegistry, Orchestrator, Router, TurnOutcome};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, SessionId};

/// Persistent state for one session. Borrows the store; the engine deps for running a turn
/// are passed to `run_prompt` rather than held, so a `Session` is cheap to create and test.
pub struct Session<'a> {
    store: &'a dyn SessionStore,
    id: SessionId,
    next_seq: u64,
    next_turn: u32,
}

impl<'a> Session<'a> {
    /// Create and persist a new session for `goal` with `config` (provider selection as
    /// JSON). Status starts `Active`. (≙ `Command::CreateSession`.)
    pub async fn create(
        store: &'a dyn SessionStore,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Session<'a>> {
        let id = store.create_session(goal, config).await?;
        Ok(Session {
            store,
            id,
            next_seq: 0,
            next_turn: 0,
        })
    }

    /// The session's id.
    pub fn id(&self) -> SessionId {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SqliteStore;

    async fn store_in(dir: &tempfile::TempDir) -> SqliteStore {
        SqliteStore::open(dir.path().join("sessions.db"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_persists_active_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;
        let session = Session::create(&store, "do a thing", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            store.session_status(session.id()).await.unwrap(),
            SessionStatus::Active
        );
    }
}
