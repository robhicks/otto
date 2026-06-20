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
    SendPrompt {
        session: SessionId,
        text: String,
    },
    Abort {
        session: SessionId,
    },
    ApproveDiff {
        session: SessionId,
        id: Uuid,
        approved: bool,
    },
    Pause {
        session: SessionId,
    },
    Resume {
        session: SessionId,
    },
    /// Hand this session off to a freshly-provisioned remote engine. The engine replies with
    /// `ServerMessage::Promoted { endpoint }`; the client reconnects there (same token + session
    /// + last_seq). Handled only between turns.
    PromoteToRemote {
        session: SessionId,
    },
    /// Hand this session back to a freshly-provisioned local engine (the reverse of
    /// `PromoteToRemote`). The engine replies with `ServerMessage::Demoted { endpoint }`.
    DemoteToLocal {
        session: SessionId,
    },
}

/// The body of an event emitted by the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    AgentStarted {
        role: Role,
    },
    AgentFinished {
        role: Role,
    },
    FileEdit {
        path: PathBuf,
        bytes_written: u64,
    },
    /// The Coder proposes an edit that needs human approval. `old` is the file's current
    /// contents (`None` if it does not exist yet); `new` is the proposed contents. The UI
    /// renders the diff and replies with `Command::ApproveDiff { id, approved }`.
    ApprovalRequest {
        id: Uuid,
        path: PathBuf,
        old: Option<String>,
        new: String,
    },
    VerifyResult {
        ok: bool,
        detail: String,
    },
    Log {
        message: String,
    },
    TurnComplete {
        ok: bool,
    },
    /// Cumulative token usage for the current turn, emitted as the turn progresses. Only fires
    /// when a metered provider reported usage (the offline path emits none). The UI renders the
    /// counts and derives an approximate cost from the remote model in the capabilities manifest.
    TokenCostMeter {
        input_tokens: u64,
        output_tokens: u64,
    },
}

/// A sequenced, session-scoped event in the engine -> frontend stream.
/// `seq` is monotonic per session so reconnecting clients can replay via Last-Event-ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub session: SessionId,
    pub kind: EventKind,
}

/// Outbound WS framing for the engine→frontend stream. Reuses the core `Event`;
/// `Ready`/`Error` are transport framing. Shared so browser clients can deserialize it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Ready {
        session: SessionId,
        #[serde(default)]
        capabilities: CapabilitiesManifest,
    },
    Event {
        event: Event,
    },
    Error {
        message: String,
    },
    /// Handover framing: the session has been provisioned onto a remote engine reachable at
    /// `endpoint` (a `ws://host:port` base). The client reconnects there reusing its token,
    /// session, and last_seq. Not a sequenced `Event` — never persisted/replayed from the store.
    Promoted {
        session: SessionId,
        endpoint: String,
    },
    /// Handover framing for the reverse trip: the session is now on a local engine at `endpoint`.
    Demoted {
        session: SessionId,
        endpoint: String,
    },
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
///
/// `#[serde(default)]`: a missing field deserializes to `false` ("capability absent"),
/// so adding a capability stays a semver-minor wire change for the separately-built UI.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CapabilitiesManifest {
    pub engine_remote: bool,
    pub local_llm: bool,
    /// A remote provider (Anthropic) is configured. Distinct from `local_llm` (Ollama);
    /// with both false the engine is on its deterministic offline path (no real LLM).
    pub remote_llm: bool,
    pub sandbox: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_message_ready_has_snake_case_tag_and_capabilities() {
        let session = SessionId::new();
        let msg = ServerMessage::Ready {
            session,
            capabilities: CapabilitiesManifest {
                engine_remote: false,
                local_llm: true,
                remote_llm: false,
                sandbox: true,
            },
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(v["type"], "ready");
        // SessionId is a newtype over Uuid → serializes as a bare string.
        assert_eq!(v["session"], serde_json::json!(session.0.to_string()));
        // The manifest is a nested sibling object; lock its shape.
        assert_eq!(v["capabilities"]["engine_remote"], false);
        assert_eq!(v["capabilities"]["local_llm"], true);
        assert_eq!(v["capabilities"]["remote_llm"], false);
        assert_eq!(v["capabilities"]["sandbox"], true);
    }

    #[test]
    fn capabilities_manifest_round_trips_with_remote_llm() {
        let m = CapabilitiesManifest {
            engine_remote: true,
            local_llm: false,
            remote_llm: true,
            sandbox: false,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: CapabilitiesManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn ready_without_capabilities_defaults_to_all_false() {
        // An out-of-step peer (e.g. an older engine) that omits `capabilities` must still
        // deserialize — defaulting every capability to false (absent → shown as degraded).
        let session = SessionId::new();
        let json = format!(r#"{{"type":"ready","session":"{}"}}"#, session.0);
        let msg: ServerMessage = serde_json::from_str(&json).expect("deserialize");
        match msg {
            ServerMessage::Ready { capabilities, .. } => {
                assert_eq!(capabilities, CapabilitiesManifest::default());
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn server_message_event_round_trips() {
        let msg = ServerMessage::Event {
            event: Event {
                seq: 3,
                session: SessionId::new(),
                kind: EventKind::TurnComplete { ok: true },
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ServerMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

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
    fn approve_diff_command_round_trips() {
        let cmd = Command::ApproveDiff {
            session: SessionId::new(),
            id: Uuid::from_u128(7),
            approved: true,
        };
        let back: Command = serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn pause_and_resume_commands_round_trip() {
        let session = SessionId::new();
        for cmd in [Command::Pause { session }, Command::Resume { session }] {
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
            assert_eq!(cmd, back);
        }
    }

    #[test]
    fn token_cost_meter_event_round_trips() {
        let event = Event {
            seq: 5,
            session: SessionId::new(),
            kind: EventKind::TokenCostMeter {
                input_tokens: 1234,
                output_tokens: 567,
            },
        };
        let back: Event = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn approval_request_event_round_trips() {
        let event = Event {
            seq: 4,
            session: SessionId::new(),
            kind: EventKind::ApprovalRequest {
                id: Uuid::from_u128(9),
                path: PathBuf::from("src/a.rs"),
                old: Some("old line\n".to_string()),
                new: "new line\n".to_string(),
            },
        };
        let back: Event = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(event, back);
        // A new file carries `old: None`.
        let new_file = EventKind::ApprovalRequest {
            id: Uuid::from_u128(1),
            path: PathBuf::from("new.rs"),
            old: None,
            new: "x".to_string(),
        };
        let back: EventKind =
            serde_json::from_str(&serde_json::to_string(&new_file).unwrap()).unwrap();
        assert_eq!(new_file, back);
    }

    #[test]
    fn promote_commands_round_trip() {
        let s = SessionId::new();
        for cmd in [
            Command::PromoteToRemote { session: s },
            Command::DemoteToLocal { session: s },
        ] {
            let json = serde_json::to_string(&cmd).unwrap();
            assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
        }
        // External tagging matches the rest of Command (e.g. {"PromoteToRemote":{...}}).
        let json = serde_json::to_string(&Command::PromoteToRemote { session: s }).unwrap();
        assert!(json.contains("PromoteToRemote"));
    }

    #[test]
    fn handover_server_messages_round_trip() {
        let s = SessionId::new();
        for msg in [
            ServerMessage::Promoted {
                session: s,
                endpoint: "ws://127.0.0.1:9000".into(),
            },
            ServerMessage::Demoted {
                session: s,
                endpoint: "ws://127.0.0.1:9001".into(),
            },
        ] {
            let json = serde_json::to_string(&msg).unwrap();
            assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), msg);
        }
        // ServerMessage is `#[serde(tag="type", rename_all="snake_case")]`.
        let json = serde_json::to_string(&ServerMessage::Promoted {
            session: s,
            endpoint: "x".into(),
        })
        .unwrap();
        assert!(json.contains("\"type\":\"promoted\""));
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
