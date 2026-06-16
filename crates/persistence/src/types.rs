//! Public types for the session store: session status and the turn record written
//! per orchestrator turn.

use otto_protocol::{Event, SessionId};
use serde::{Deserialize, Serialize};

/// Lifecycle status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Active,
    Done,
    Aborted,
    Failed,
}

impl SessionStatus {
    /// The string stored in the `sessions.status` column.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Done => "done",
            SessionStatus::Aborted => "aborted",
            SessionStatus::Failed => "failed",
        }
    }

    /// Parse a status back from its `sessions.status` column value.
    pub fn from_db_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "active" => SessionStatus::Active,
            "done" => SessionStatus::Done,
            "aborted" => SessionStatus::Aborted,
            "failed" => SessionStatus::Failed,
            other => anyhow::bail!("unknown session status: {other:?}"),
        })
    }
}

/// One orchestrator turn's record. `outcome` is a JSON value so the store stays
/// decoupled from `engine-core`'s `TurnOutcome` (the engine layer serializes it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub turn_index: u32,
    pub goal: String,
    pub outcome: serde_json::Value,
}

/// The full, serializable state of a session — its metadata, config, complete event log,
/// and turn history — derived from the store's tables. Used to move a session between
/// engines (snapshot on one, restore on another). The workspace patch-bundle is deferred
/// until `RemoteWorkspace`; timestamps are storage metadata and are not captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub id: SessionId,
    pub goal: String,
    pub status: SessionStatus,
    pub config: serde_json::Value,
    pub events: Vec<Event>,
    pub turns: Vec<TurnRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_round_trips_through_db_str() {
        for s in [
            SessionStatus::Active,
            SessionStatus::Done,
            SessionStatus::Aborted,
            SessionStatus::Failed,
        ] {
            assert_eq!(SessionStatus::from_db_str(s.as_db_str()).unwrap(), s);
        }
    }

    #[test]
    fn session_status_rejects_unknown() {
        assert!(SessionStatus::from_db_str("bogus").is_err());
    }

    #[test]
    fn session_state_round_trips_through_json() {
        use otto_protocol::EventKind;
        let id = SessionId::new();
        let state = SessionState {
            id,
            goal: "the goal".to_string(),
            status: SessionStatus::Done,
            config: serde_json::json!({ "ollama": false }),
            events: vec![Event {
                seq: 0,
                session: id,
                kind: EventKind::Log {
                    message: "hi".to_string(),
                },
            }],
            turns: vec![TurnRecord {
                turn_index: 0,
                goal: "the goal".to_string(),
                outcome: serde_json::json!({ "ok": true }),
            }],
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }
}
