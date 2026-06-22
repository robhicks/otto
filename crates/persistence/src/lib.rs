//! Durable session store for the engine: persists sessions, their seq-ordered event
//! log, and turn records to sqlite, with gap-correct event replay. The `engine` layer
//! depends on this crate directly and holds a `Box<dyn SessionStore>`.

mod sqlite;
mod types;

use async_trait::async_trait;
use otto_protocol::{Event, SessionId};
use serde_json::Value;

pub use sqlite::SqliteStore;
pub use types::{SessionState, SessionStatus, TurnRecord};

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

    /// Replay events for `session` in ascending seq order. `None` returns the full
    /// log; `Some(n)` returns only events with `seq > n` (strictly after n) — a
    /// client that has seen up through seq n passes `Some(n)` to get the gap.
    /// Returns an empty Vec for an unknown session.
    async fn replay_since(
        &self,
        session: SessionId,
        after_seq: Option<u64>,
    ) -> anyhow::Result<Vec<Event>>;

    /// Read a session's current status. Errors if the session does not exist.
    async fn session_status(&self, session: SessionId) -> anyhow::Result<SessionStatus>;

    /// The next event seq for `session` (`MAX(seq) + 1`, or 0 if none). Lets a long-lived
    /// or reconnected writer continue the seq sequence without holding an in-memory counter.
    async fn next_seq(&self, session: SessionId) -> anyhow::Result<u64>;

    /// The next turn index for `session` (`MAX(turn_index) + 1`, or 0 if none).
    async fn next_turn(&self, session: SessionId) -> anyhow::Result<u32>;

    /// Capture the full state of `session` — metadata, config, the complete event log, and
    /// turn history — as a serializable `SessionState`. Errors if the session does not
    /// exist. (The workspace patch-bundle is deferred until `RemoteWorkspace`.)
    async fn snapshot(&self, session: SessionId) -> anyhow::Result<SessionState>;

    /// Write a previously captured `SessionState` into this store, preserving its id, seqs,
    /// status, config, and turn history. Intended for a fresh store (e.g. a remote engine);
    /// errors if the session id already exists. Returns the (preserved) session id.
    async fn restore(&self, state: &SessionState) -> anyhow::Result<SessionId>;

    /// Like `restore`, but overwrites any existing rows for the session id (delete-then-insert in
    /// one transaction) instead of failing on a duplicate. This is the demote primitive: the source
    /// engine refreshes its own stale copy with the advanced state pulled back from the receiver.
    /// `restore` stays fail-on-conflict — only an explicit demote uses this.
    async fn restore_over(&self, state: &SessionState) -> anyhow::Result<SessionId>;
}
