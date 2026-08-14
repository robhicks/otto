//! Wire types shared between the engine and any frontend.
//! This crate has no I/O and no engine logic.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod sensitive;
pub use sensitive::{SENSITIVE_MARKERS, is_sensitive};

mod user;
pub use user::{InvalidUserId, UserId};

/// The credentials a client presents to authenticate. Hand-`Debug`ged to redact the secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Credentials {
    Totp { user: UserId, code: String },
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::Totp { user, code: _ } => f
                .debug_struct("Totp")
                .field("user", user)
                .field("code", &"<redacted>")
                .finish(),
        }
    }
}

/// The server's authentication posture, announced in `ServerMessage::Hello` on connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Loopback-only serve with **no credential**: every connection is the reserved `local`
    /// principal, so there is no authentication frame and no session ownership to resolve. The
    /// desktop sidecar's mode (`otto serve --single-user`). Not the default.
    SingleUser,
    /// Enrolled principals authenticate with TOTP; sessions are owned per-user. The default
    /// `otto serve` posture.
    Users,
    /// A promotion receiver (`otto serve --promotion-receiver`): the promotion secret
    /// authenticates the connection and it adopts the attached session's owner. No enrolled
    /// principals.
    Machine,
}

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
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    CreateSession,
    SendPrompt {
        session: SessionId,
        text: String,
    },
    /// Run a discovered `.claude/commands/*.md` command by name: template-expand
    /// `$ARGUMENTS`/`$1..$9` from `args`, resolve `!bash`/`@file` injections through a tool
    /// registry narrowed to the command's `allowed-tools`, then run the result as a normal
    /// turn with the router pinned to the command's `model`. Unknown `name` or an injection
    /// failure surfaces as `ServerMessage::Error` — no turn starts, no `seq` is consumed.
    RunCommand {
        session: SessionId,
        name: String,
        args: Vec<String>,
    },
    /// Dispatch a discovered `.claude/agents/*.md` custom agent by name as a single,
    /// non-interruptible request/response (no orchestrator turn): compose its system prompt with
    /// `prompt` and run it through `TaskTool`/`MarkdownAgent`. Emits the existing
    /// `AgentStarted`/`Log`/`AgentFinished`/`TurnComplete` `EventKind`s — no new wire variant.
    /// Unknown `name` surfaces as `ServerMessage::Error` — no turn starts, no `seq` is consumed.
    RunAgent {
        session: SessionId,
        name: String,
        prompt: String,
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
    /// `ServerMessage::Promoted { endpoint, token }`; the client reconnects there (using `token`,
    /// plus session + last_seq). Handled only between turns.
    PromoteToRemote {
        session: SessionId,
    },
    /// Hand this session back to a freshly-provisioned local engine (the reverse of
    /// `PromoteToRemote`). The engine replies with `ServerMessage::Demoted { endpoint }`.
    DemoteToLocal {
        session: SessionId,
    },
    /// Authenticate as `user` using the provided credentials. The engine replies with
    /// `ServerMessage::LoggedIn` (or `ServerMessage::Error`). Not applicable in `SingleUser` /
    /// `Machine` modes — a `SingleUser` connection needs no credential and a `Machine` one
    /// authenticates with the promotion secret, so `Login` is rejected with the opaque error.
    Login {
        credentials: Credentials,
    },
    /// Attach to an already-authenticated connection using `token` — a bearer minted by a prior
    /// `LoggedIn` (in `Users` mode) or the promotion secret (in `Machine` mode). Not applicable
    /// in `SingleUser` mode. Replied with `ServerMessage::LoggedIn`.
    Attach {
        token: String,
    },
    /// Exchange `refresh_token` for a fresh `LoggedIn` (rotating the access token). Replied with
    /// `ServerMessage::LoggedIn` (or `ServerMessage::Error`).
    Refresh {
        refresh_token: String,
    },
    /// Revoke the current session's tokens. The engine replies with `ServerMessage::LoggedOut`.
    Logout,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::CreateSession => f.write_str("CreateSession"),
            Command::SendPrompt { session, text } => f
                .debug_struct("SendPrompt")
                .field("session", session)
                .field("text", text)
                .finish(),
            Command::RunCommand {
                session,
                name,
                args,
            } => f
                .debug_struct("RunCommand")
                .field("session", session)
                .field("name", name)
                .field("args", args)
                .finish(),
            Command::RunAgent {
                session,
                name,
                prompt,
            } => f
                .debug_struct("RunAgent")
                .field("session", session)
                .field("name", name)
                .field("prompt", prompt)
                .finish(),
            Command::Abort { session } => {
                f.debug_struct("Abort").field("session", session).finish()
            }
            Command::ApproveDiff {
                session,
                id,
                approved,
            } => f
                .debug_struct("ApproveDiff")
                .field("session", session)
                .field("id", id)
                .field("approved", approved)
                .finish(),
            Command::Pause { session } => {
                f.debug_struct("Pause").field("session", session).finish()
            }
            Command::Resume { session } => {
                f.debug_struct("Resume").field("session", session).finish()
            }
            Command::PromoteToRemote { session } => f
                .debug_struct("PromoteToRemote")
                .field("session", session)
                .finish(),
            Command::DemoteToLocal { session } => f
                .debug_struct("DemoteToLocal")
                .field("session", session)
                .finish(),
            Command::Login { credentials } => f
                .debug_struct("Login")
                .field("credentials", credentials)
                .finish(),
            Command::Attach { token: _ } => f
                .debug_struct("Attach")
                .field("token", &"<redacted>")
                .finish(),
            Command::Refresh { refresh_token: _ } => f
                .debug_struct("Refresh")
                .field("refresh_token", &"<redacted>")
                .finish(),
            Command::Logout => f.write_str("Logout"),
        }
    }
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
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    /// `endpoint` (a `ws://host:port` base). The client reconnects there, session and last_seq
    /// unchanged. `token` is a fresh opaque per-session secret the target minted at provision
    /// time and delivered to the remote over the provisioning channel — never the client's own
    /// source credential. The client must switch to it before reconnecting. Always present: a
    /// frame with a `null` or missing `token` fails to deserialize. Not a sequenced `Event` —
    /// never persisted/replayed from the store.
    Promoted {
        session: SessionId,
        endpoint: String,
        token: String,
    },
    /// Handover framing for the reverse trip: the session is now on a local engine at `endpoint`.
    Demoted {
        session: SessionId,
        endpoint: String,
    },
    /// Transport framing announcing the server's auth posture on connect.
    Hello {
        auth_mode: AuthMode,
    },
    /// Sent in reply to `Command::Login`/`Command::Refresh` once the client is authenticated, and
    /// on `Command::Attach` when the provided token is accepted.
    LoggedIn {
        user: UserId,
        access_token: String,
        expires_at: u64,
        refresh_token: String,
    },
    /// Sent in reply to `Command::Logout`: the session's tokens are revoked.
    LoggedOut,
}

impl std::fmt::Debug for ServerMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerMessage::Ready {
                session,
                capabilities,
            } => f
                .debug_struct("Ready")
                .field("session", session)
                .field("capabilities", capabilities)
                .finish(),
            ServerMessage::Event { event } => {
                f.debug_struct("Event").field("event", event).finish()
            }
            ServerMessage::Error { message } => {
                f.debug_struct("Error").field("message", message).finish()
            }
            ServerMessage::Promoted {
                session,
                endpoint,
                token: _,
            } => f
                .debug_struct("Promoted")
                .field("session", session)
                .field("endpoint", endpoint)
                .field("token", &"<redacted>")
                .finish(),
            ServerMessage::Demoted { session, endpoint } => f
                .debug_struct("Demoted")
                .field("session", session)
                .field("endpoint", endpoint)
                .finish(),
            ServerMessage::Hello { auth_mode } => f
                .debug_struct("Hello")
                .field("auth_mode", auth_mode)
                .finish(),
            ServerMessage::LoggedIn {
                user,
                access_token: _,
                expires_at,
                refresh_token: _,
            } => f
                .debug_struct("LoggedIn")
                .field("user", user)
                .field("expires_at", expires_at)
                .field("access_token", &"<redacted>")
                .field("refresh_token", &"<redacted>")
                .finish(),
            ServerMessage::LoggedOut => f.write_str("LoggedOut"),
        }
    }
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
    fn run_command_command_round_trips() {
        let cmd = Command::RunCommand {
            session: SessionId::new(),
            name: "git:commit".to_string(),
            args: vec!["fix bug".to_string()],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        // External tagging matches the rest of Command (e.g. {"RunCommand":{...}}).
        assert!(json.contains("\"RunCommand\""));
    }

    #[test]
    fn run_agent_command_round_trips() {
        let cmd = Command::RunAgent {
            session: SessionId::new(),
            name: "reviewer".to_string(),
            prompt: "look at auth.rs".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        // External tagging matches the rest of Command (e.g. {"RunAgent":{...}}).
        assert!(json.contains("\"RunAgent\""));
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
                token: "".into(),
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
            token: "".into(),
        })
        .unwrap();
        assert!(json.contains("\"type\":\"promoted\""));
    }

    #[test]
    fn promoted_with_token_round_trips() {
        let s = SessionId::new();
        let msg = ServerMessage::Promoted {
            session: s,
            endpoint: "ws://127.0.0.1:9000".into(),
            token: "abc".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
        match back {
            ServerMessage::Promoted { token, .. } => assert_eq!(&token, "abc"),
            _ => panic!("expected Promoted"),
        }
    }

    #[test]
    fn promoted_with_null_token_fails_to_deserialize() {
        let json = r#"{"type":"promoted","session":"00000000-0000-0000-0000-000000000001","endpoint":"ws://x","token":null}"#;
        assert!(
            serde_json::from_str::<ServerMessage>(json).is_err(),
            "a null `token` must fail to deserialize"
        );
    }

    #[test]
    fn promoted_with_missing_token_fails_to_deserialize() {
        let json = r#"{"type":"promoted","session":"00000000-0000-0000-0000-000000000001","endpoint":"ws://x"}"#;
        assert!(
            serde_json::from_str::<ServerMessage>(json).is_err(),
            "a missing `token` field must fail to deserialize"
        );
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

    #[test]
    fn login_command_round_trips_externally_tagged() {
        let cmd = Command::Login {
            credentials: Credentials::Totp {
                user: UserId::parse("alice").unwrap(),
                code: "123456".into(),
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(
            json.starts_with(
                r#"{"Login":{"credentials":{"Totp":{"user":"alice","code":"123456"}}}}"#
            )
        );
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
    }

    #[test]
    fn logged_in_frame_is_internally_tagged_and_redacts_debug() {
        let frame = ServerMessage::LoggedIn {
            user: UserId::parse("alice").unwrap(),
            access_token: "access-secret-token".into(),
            expires_at: 1_700_000_000,
            refresh_token: "refresh-secret-token".into(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.starts_with(r#"{"type":"logged_in","user":"alice""#));
        assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), frame);

        let dbg = format!("{frame:?}");
        assert!(
            !dbg.contains("access-secret-token") && !dbg.contains("refresh-secret-token"),
            "tokens leaked in Debug: {dbg}"
        );
    }

    #[test]
    fn command_debug_redacts_attach_and_refresh() {
        let attach = format!(
            "{:?}",
            Command::Attach {
                token: "secret-token".into()
            }
        );
        assert!(!attach.contains("secret-token"));
        let refresh = format!(
            "{:?}",
            Command::Refresh {
                refresh_token: "secret-refresh".into()
            }
        );
        assert!(!refresh.contains("secret-refresh"));
        let login = format!(
            "{:?}",
            Command::Login {
                credentials: Credentials::Totp {
                    user: UserId::parse("alice").unwrap(),
                    code: "654321".into()
                }
            }
        );
        assert!(!login.contains("654321"));
    }

    #[test]
    fn server_message_debug_redacts_promoted_token() {
        // The hand-written `Debug` for the whole enum must also close the pre-existing
        // `Promoted.token` leak in the derived impl it replaces.
        let promoted = ServerMessage::Promoted {
            session: SessionId::new(),
            endpoint: "ws://x".into(),
            token: "fly-secret".into(),
        };
        let dbg = format!("{promoted:?}");
        assert!(
            !dbg.contains("fly-secret"),
            "Promoted.token leaked in Debug: {dbg}"
        );
    }

    #[test]
    fn hello_frame_carries_the_mode_snake_cased() {
        let hello = ServerMessage::Hello {
            auth_mode: AuthMode::SingleUser,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(json, r#"{"type":"hello","auth_mode":"single_user"}"#);
        assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), hello);
    }
}
