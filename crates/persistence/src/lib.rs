//! Durable session store for the engine: persists sessions, their seq-ordered event
//! log, and turn records to sqlite, with gap-correct event replay. The `engine` layer
//! depends on this crate directly and holds a `Box<dyn SessionStore>`.

mod sqlite;
mod types;

use async_trait::async_trait;
use otto_protocol::{Event, SessionId};
use serde_json::Value;

pub use sqlite::SqliteStore;
pub use types::{SessionStatus, TurnRecord};

/// Persists sessions and their event/turn history. Implementations are `Send + Sync`
/// so the engine can hold one as `Box<dyn SessionStore>` across await points.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Create a new session for `goal`, storing `config` (router/model selection) as
    /// JSON. Returns the new session id.
    async fn create_session(&self, goal: &str, config: &Value) -> anyhow::Result<SessionId>;

    /// Append one already-sequenced event to the session's log. `(session, seq)` is a
    /// primary key, so re-appending the same seq is an error.
    async fn append_event(&self, session: SessionId, event: &Event) -> anyhow::Result<()>;

    /// Record one completed orchestrator turn.
    async fn record_turn(&self, session: SessionId, turn: &TurnRecord) -> anyhow::Result<()>;

    /// Update a session's lifecycle status. Errors if the session does not exist.
    async fn set_status(&self, session: SessionId, status: SessionStatus) -> anyhow::Result<()>;

    /// Replay the session's events with `seq > after_seq`, in ascending seq order.
    /// Pass `0` to get the full log (seqs are 0-based, so this returns everything).
    async fn replay_since(
        &self,
        session: SessionId,
        after_seq: u64,
    ) -> anyhow::Result<Vec<Event>>;
}
