//! Wire types shared between the engine and any frontend.
//! This crate has no I/O and no engine logic.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies a single agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// The role an atomic agent plays in the orchestrator spine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Planner,
    ContextFinder,
    Coder,
    Verifier,
    Custom(String),
}

/// Commands sent from a frontend to the engine (request/response channel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    CreateSession,
    SendPrompt { session: SessionId, text: String },
    Abort { session: SessionId },
}

/// The body of an event emitted by the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    AgentStarted { role: Role },
    AgentFinished { role: Role },
    FileEdit { path: PathBuf, bytes_written: u64 },
    VerifyResult { ok: bool, detail: String },
    Log { message: String },
    TurnComplete { ok: bool },
}

/// A sequenced, session-scoped event in the engine -> frontend stream.
/// `seq` is monotonic per session so reconnecting clients can replay via Last-Event-ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub session: SessionId,
    pub kind: EventKind,
}

/// A unary workspace operation, sent to a remote engine's `POST /workspace`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceRequest {
    Read { path: PathBuf },
    List { glob: String },
    ApplyEdit { path: PathBuf, contents: String },
    Snapshot,
}

/// The response to a `WorkspaceRequest`. `Error` carries an application-level failure
/// (the HTTP status is still 200); the client maps it to an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceResponse {
    Read { bytes: Vec<u8> },
    List { paths: Vec<PathBuf> },
    ApplyEdit { bytes_written: u64 },
    Snapshot { files: Vec<(PathBuf, Vec<u8>)> },
    Error { message: String },
}

/// What the running engine environment can do. The frontend composes its behavior
/// from the intersection of this manifest and its own form factor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilitiesManifest {
    pub engine_remote: bool,
    pub local_llm: bool,
    pub sandbox: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trips_through_json() {
        let event = Event {
            seq: 7,
            session: SessionId::new(),
            kind: EventKind::FileEdit {
                path: PathBuf::from("otto_output.txt"),
                bytes_written: 42,
            },
        };

        let json = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(event, back);
    }

    #[test]
    fn command_round_trips_through_json() {
        let cmd = Command::SendPrompt {
            session: SessionId::new(),
            text: "add a greeting".to_string(),
        };

        let json = serde_json::to_string(&cmd).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(cmd, back);
    }

    #[test]
    fn workspace_rpc_types_round_trip_through_json() {
        let req = WorkspaceRequest::ApplyEdit {
            path: PathBuf::from("src/a.rs"),
            contents: "fn main() {}".to_string(),
        };
        let back: WorkspaceRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(req, back);

        let resp = WorkspaceResponse::Snapshot {
            files: vec![(PathBuf::from("a.txt"), vec![1, 2, 3])],
        };
        let back: WorkspaceResponse =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(resp, back);
    }
}
