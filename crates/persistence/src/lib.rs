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
///
/// # Which methods are owner-scoped, and why not all of them
///
/// `create_session`, `owner_of`, `replay_since`, `session_status`, and `snapshot` take a
/// principal: they are the methods a client can reach with a session id it does not own, and the
/// ones that would return another tenant's data. Their ownership predicate lives inside the SQL
/// statement, so no caller can forget it.
///
/// `append_event`, `record_turn`, `next_seq`, `next_turn`, and `set_status` are deliberately
/// UNSCOPED. They are reachable only from inside a turn that `EngineService` has already
/// authorized, none of them returns another tenant's data, and scoping them would roughly triple
/// the churn for no gain. This is a deliberate trade, not an oversight.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Create a session owned by `owner`.
    async fn create_session(
        &self,
        owner: &otto_protocol::UserId,
        goal: &str,
        config: &Value,
    ) -> anyhow::Result<SessionId>;

    /// The owner of `session`, or `Err` if there is no such session.
    ///
    /// UNSCOPED, and deliberately a reverse existence oracle: it distinguishes "exists, owned by
    /// someone else" from "does not exist" — exactly the distinction the scoped reads below
    /// hide. `EngineService::authorize` needs that comparison, so it cannot be removed. It must
    /// therefore NEVER back a client-facing path.
    async fn owner_of(&self, session: SessionId) -> anyhow::Result<otto_protocol::UserId>;

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
    /// Returns an empty Vec for an unknown session — and, identically, for a session `owner`
    /// does not own.
    async fn replay_since(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
        after_seq: Option<u64>,
    ) -> anyhow::Result<Vec<Event>>;

    /// Read a session's current status. Errors if the session does not exist — or, with a
    /// byte-identical message, if `owner` does not own it.
    async fn session_status(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
    ) -> anyhow::Result<SessionStatus>;

    /// The next event seq for `session` (`MAX(seq) + 1`, or 0 if none). Lets a long-lived
    /// or reconnected writer continue the seq sequence without holding an in-memory counter.
    async fn next_seq(&self, session: SessionId) -> anyhow::Result<u64>;

    /// The next turn index for `session` (`MAX(turn_index) + 1`, or 0 if none).
    async fn next_turn(&self, session: SessionId) -> anyhow::Result<u32>;

    /// Capture the full state of `session` — metadata, config, the complete event log, and
    /// turn history — as a serializable `SessionState`. Errors if the session does not
    /// exist — or, with a byte-identical message, if `owner` does not own it. (The workspace
    /// patch-bundle is deferred until `RemoteWorkspace`.)
    async fn snapshot(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
    ) -> anyhow::Result<SessionState>;

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
