//! Public types for the session store: session status and the turn record written
//! per orchestrator turn.

/// Lifecycle status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRecord {
    pub turn_index: u32,
    pub goal: String,
    pub outcome: serde_json::Value,
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
}
