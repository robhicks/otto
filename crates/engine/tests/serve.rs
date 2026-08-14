//! End-to-end: the axum WebSocket server streams a turn's events to a connected client,
//! supports Last-Event-ID reconnect, and rejects unauthenticated connections. Runs on a
//! loopback ephemeral port — no external network.

use std::sync::Arc;

use axum_server::tls_rustls::RustlsConfig;
use futures_util::{SinkExt, StreamExt};
use otto_auth::testing::FakeAuthenticator;
use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, build_tool_registry_approving,
    serve_app, serve_run,
};
use otto_engine_core::auth::AuthConfig;
use otto_engine_core::traits::Workspace;
use otto_extensions::{CustomAgentDef, CustomCommandDef, Extensions};
use otto_protocol::AuthMode;
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "test-token";

/// The `SingleUser` auth posture every plain command-flow server in this file serves: no
/// credential (the `Authorization` header is ignored), no authenticator, no promotion secret.
fn single_user() -> AuthConfig {
    AuthConfig {
        mode: AuthMode::SingleUser,
        authenticator: None,
        promotion_secret: None,
        handshake_deadline: std::time::Duration::from_secs(10),
    }
}

/// A fixed manifest the test server reports, so the assertion below is deterministic and
/// also proves non-default values are threaded through (not hardcoded false).
fn test_capabilities() -> otto_protocol::CapabilitiesManifest {
    otto_protocol::CapabilitiesManifest {
        engine_remote: false,
        local_llm: true,
        remote_llm: false,
        sandbox: true,
    }
}

/// Start the serve app on 127.0.0.1:0 and return the bound port. Keeps the tempdir alive.
async fn start_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );

    let app = serve_app(service, single_user(), test_capabilities(), None, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

/// Start the serve app with one discovered command: `greet`, template `hi $1`, narrowed to
/// `fs.read` only (excludes `fs.write` — proves narrowing reaches a served RunCommand turn).
/// The router's ScriptedProvider ignores the exact goal text, so any expansion is acceptable;
/// what's asserted is the narrowing (no file write) and the recorded per-turn goal.
async fn start_server_with_command() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "should not land"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let extensions = Extensions {
        commands: vec![CustomCommandDef {
            name: "greet".to_string(),
            description: None,
            argument_hint: None,
            model: None,
            allowed_tools: Some(vec!["fs.read".to_string()]),
            template: "hi $1".to_string(),
        }],
        ..Default::default()
    };
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    )
    .with_extensions(Arc::new(extensions));

    let app = serve_app(service, single_user(), test_capabilities(), None, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

/// Start the serve app with one discovered custom agent: `reviewer`, system prompt
/// `"SYSTEM-PROMPT"`. Uses the deterministic offline router (no ScriptedProvider needed — the
/// dispatched `MarkdownAgent` goes through `build_router_with_model`, not the service's own
/// router).
async fn start_server_with_agent() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "should not land"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let extensions = Extensions {
        agents: vec![CustomAgentDef {
            name: "reviewer".to_string(),
            description: "d".to_string(),
            tools: None,
            model: None,
            system_prompt: "SYSTEM-PROMPT".to_string(),
        }],
        ..Default::default()
    };
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    )
    .with_extensions(Arc::new(extensions));

    let app = serve_app(service, single_user(), test_capabilities(), None, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

/// Start a serve app in **approval mode** (ordinary writes gated `Ask`). Returns the bound
/// port and the tempdir (whose path is the workspace root the Coder edits: `out.txt`).
async fn start_approval_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "approved contents"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry_approving(
        tools_ws,
        dir.path().to_path_buf(),
    ));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
    let app = serve_app(service, single_user(), test_capabilities(), None, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

/// Start a serve app whose router reports token usage (so meter events fire) with ordinary
/// (auto-allowed) writes. Returns the bound port and the tempdir.
async fn start_metering_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#)
        .with_usage(10, 20);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
    let app = serve_app(service, single_user(), test_capabilities(), None, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

#[tokio::test]
async fn streams_token_cost_meter_events() {
    let (port, _dir) = start_metering_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut saw_meter = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(m) = kind.get("TokenCostMeter") {
                assert!(m["input_tokens"].as_u64().unwrap() > 0);
                saw_meter = true;
            }
            if kind.get("TurnComplete").is_some() {
                break;
            }
        }
    }
    assert!(saw_meter, "expected at least one TokenCostMeter event");
}

#[tokio::test]
async fn pause_before_prompt_parks_turn_until_resume() {
    let (port, _dir) = start_metering_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    // Pause BEFORE prompting → the turn's first checkpoint parks deterministically.
    let pause = serde_json::json!({ "Pause": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&pause).unwrap()))
        .await
        .unwrap();
    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Read until "turn paused"; assert TurnComplete did NOT arrive first.
    let mut paused = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(msg) = kind.get("Log").and_then(|l| l["message"].as_str()) {
                if msg == "turn paused" {
                    paused = true;
                    break;
                }
            }
            assert!(
                kind.get("TurnComplete").is_none(),
                "turn completed before pausing"
            );
        }
    }
    assert!(paused, "expected a 'turn paused' log");

    // Resume → expect "turn resumed" then TurnComplete.
    let resume = serde_json::json!({ "Resume": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&resume).unwrap()))
        .await
        .unwrap();
    let mut resumed = false;
    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(msg) = kind.get("Log").and_then(|l| l["message"].as_str()) {
                if msg == "turn resumed" {
                    resumed = true;
                }
            }
            if kind.get("TurnComplete").is_some() {
                completed = true;
                break;
            }
        }
    }
    assert!(resumed, "expected a 'turn resumed' log");
    assert!(completed, "expected the turn to complete after resume");
}

#[tokio::test]
async fn disconnect_while_paused_does_not_hang() {
    let (port, _dir) = start_metering_server().await;

    // Connection 1: pause before prompting so the turn parks at its first checkpoint.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let pause = serde_json::json!({ "Pause": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&pause).unwrap()))
        .await
        .unwrap();
    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Wait until the turn has actually parked, then drop the socket mid-pause.
    let mut paused = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if kind.get("Log").and_then(|l| l["message"].as_str()) == Some("turn paused") {
                paused = true;
                break;
            }
        }
    }
    assert!(paused, "expected the turn to park before disconnect");
    drop(ws); // disconnect while paused → the release path must unwedge the server

    // The server must still serve: a fresh connection on a NEW session runs a turn to completion.
    let (mut ws2, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("server still accepts connections after a paused-disconnect");
    let ready2: Value = ready_frame(&mut ws2).await;
    let session2 = ready2["session"].as_str().unwrap().to_string();
    let cmd2 =
        serde_json::json!({ "SendPrompt": { "session": session2, "text": "add a greeting" } });
    ws2.send(Message::Text(serde_json::to_string(&cmd2).unwrap()))
        .await
        .unwrap();

    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws2).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(
        completed,
        "server wedged: a new turn did not complete after a paused-disconnect"
    );
}

/// Read frames until an `ApprovalRequest` event arrives; return its `id`. Panics on TurnComplete
/// or stream end first (means no approval was requested).
async fn next_approval_id(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> String {
    while let Some(frame) = next_json_opt(ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(req) = kind.get("ApprovalRequest") {
                return req["id"].as_str().unwrap().to_string();
            }
            if kind.get("TurnComplete").is_some() {
                panic!("turn completed before any ApprovalRequest");
            }
        }
    }
    panic!("stream ended before any ApprovalRequest");
}

#[tokio::test]
async fn approved_edit_is_written() {
    let (port, dir) = start_approval_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Approve every ApprovalRequest until the turn completes (the repair loop may re-propose).
    let id = next_approval_id(&mut ws).await;
    let approve =
        serde_json::json!({ "ApproveDiff": { "session": session, "id": id, "approved": true } });
    ws.send(Message::Text(serde_json::to_string(&approve).unwrap()))
        .await
        .unwrap();

    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(req) = kind.get("ApprovalRequest") {
                let id = req["id"].as_str().unwrap().to_string();
                let a = serde_json::json!({ "ApproveDiff": { "session": session, "id": id, "approved": true } });
                ws.send(Message::Text(serde_json::to_string(&a).unwrap()))
                    .await
                    .unwrap();
            } else if kind.get("TurnComplete").is_some() {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete);
    let written = std::fs::read_to_string(dir.path().join("out.txt")).expect("out.txt written");
    assert_eq!(written, "approved contents");
}

#[tokio::test]
async fn rejected_edit_is_not_written() {
    let (port, dir) = start_approval_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Reject every ApprovalRequest until the turn completes.
    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(req) = kind.get("ApprovalRequest") {
                let id = req["id"].as_str().unwrap().to_string();
                let r = serde_json::json!({ "ApproveDiff": { "session": session, "id": id, "approved": false } });
                ws.send(Message::Text(serde_json::to_string(&r).unwrap()))
                    .await
                    .unwrap();
            } else if kind.get("TurnComplete").is_some() {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete);
    assert!(
        !dir.path().join("out.txt").exists(),
        "a rejected edit must not be written"
    );
}

#[tokio::test]
async fn disconnect_mid_approval_fails_closed() {
    let (port, dir) = start_approval_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Wait until the server is blocked on an approval, then drop the socket without replying.
    let _id = next_approval_id(&mut ws).await;
    drop(ws);

    // The edit is only ever written on approval; a disconnect resolves the pending request to
    // false (fail-closed), so out.txt must never appear. Give the server a moment to settle.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !dir.path().join("out.txt").exists(),
        "a disconnect mid-approval must not write the edit"
    );
}

fn authed_request(
    port: u16,
    query: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let url = format!("ws://127.0.0.1:{port}/ws{query}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A ws request with no credential — the auth-posture tests connect bare so the server must
/// reach its own verdict (deadline on `Users`, `Ready` on `SingleUser`).
fn request(port: u16, query: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let url = format!("ws://127.0.0.1:{port}/ws{query}");
    url.into_client_request().unwrap()
}

async fn connect(req: tokio_tungstenite::tungstenite::handshake::client::Request) -> Ws {
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("connect");
    ws
}

/// Assert the first frame is `Hello` (the greeting that precedes every handshake, in every mode),
/// returning it.
async fn hello(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Value {
    let hello = next_json(ws).await;
    assert_eq!(hello["type"], "hello");
    hello
}

/// Read `Hello` then `Ready`, returning the `Ready` frame.
async fn ready_frame(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Value {
    hello(ws).await;
    let ready = next_json(ws).await;
    assert_eq!(ready["type"], "ready");
    ready
}

/// A `Users`-mode server over the `FakeAuthenticator` (every `Login` authenticates as `alice`)
/// with a short handshake deadline — the posture where a socket presenting only `?token=` (no
/// header, no auth frame) must hit the deadline and get the opaque Error.
async fn start_users_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}");
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
    let fake = Arc::new(FakeAuthenticator::new(
        otto_protocol::UserId::parse("alice").unwrap(),
    ));
    let auth = AuthConfig {
        mode: AuthMode::Users,
        authenticator: Some(fake as Arc<dyn otto_engine_core::Authenticator>),
        promotion_secret: None,
        handshake_deadline: std::time::Duration::from_millis(500),
    };
    let app = serve_app(service, auth, test_capabilities(), None, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

#[tokio::test]
async fn streams_a_turn_then_reconnects_with_replay() {
    let (port, _dir) = start_server().await;

    // First connection: new session, send a prompt, collect streamed frames.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");

    // First frame is Ready { session }.
    let ready: Value = ready_frame(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    // Send a prompt for that session.
    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Collect event frames until TurnComplete.
    let mut seqs = Vec::new();
    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            seqs.push(frame["event"]["seq"].as_u64().unwrap());
            if kind.get("TurnComplete").is_some() {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete, "expected a TurnComplete event");
    assert_eq!(seqs.first(), Some(&0), "events start at seq 0");
    let last_seq = *seqs.last().unwrap();
    drop(ws);

    // Reconnect with last_seq = 0: expect the gap (events with seq > 0) replayed.
    let (mut ws2, _) = tokio_tungstenite::connect_async(authed_request(
        port,
        &format!("?session={session}&last_seq=0"),
    ))
    .await
    .expect("reconnect");
    let ready2: Value = ready_frame(&mut ws2).await;
    assert_eq!(ready2["type"], "ready");
    assert_eq!(ready2["session"].as_str().unwrap(), session);

    let mut replayed = Vec::new();
    // The gap is finite; read until we've seen the last seq, then stop.
    while let Some(frame) = next_json_opt(&mut ws2).await {
        if frame["type"] == "event" {
            let seq = frame["event"]["seq"].as_u64().unwrap();
            replayed.push(seq);
            if seq == last_seq {
                break;
            }
        }
    }
    assert!(replayed.iter().all(|&s| s > 0), "replay gap excludes seq 0");
    assert_eq!(replayed.last(), Some(&last_seq));
}

#[tokio::test]
async fn run_command_expands_and_narrows_tools() {
    let (port, dir) = start_server_with_command().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({
        "RunCommand": { "session": session, "name": "greet", "args": ["world"] }
    });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(completed, "expected the RunCommand turn to complete");

    // Narrowing worked: fs.write was excluded from the command's tools, so the Coder's edit
    // (which the scripted provider always proposes) was never applied.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !dir.path().join("out.txt").exists(),
        "allowed-tools must have excluded fs.write"
    );
}

#[tokio::test]
async fn run_command_unknown_name_reports_error_and_keeps_connection_open() {
    let (port, _dir) = start_server_with_command().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({
        "RunCommand": { "session": session.clone(), "name": "nope", "args": [] }
    });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert!(
        frame["message"]
            .as_str()
            .unwrap()
            .contains("no command named 'nope'"),
        "got: {frame}"
    );

    // The connection is still usable afterward.
    let cmd2 =
        serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd2).unwrap()))
        .await
        .unwrap();
    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(completed, "connection must survive an unknown RunCommand");
}

#[tokio::test]
async fn run_agent_dispatches_and_reports_turn_complete() {
    let (port, dir) = start_server_with_agent().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({
        "RunAgent": { "session": session, "name": "reviewer", "prompt": "look at auth.rs" }
    });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut saw_started = false;
    let mut saw_log_with_prompt = false;
    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] != "event" {
            continue;
        }
        let kind = &frame["event"]["kind"];
        if kind.get("AgentStarted").is_some() {
            saw_started = true;
        }
        if let Some(log) = kind.get("Log") {
            if log["message"]
                .as_str()
                .unwrap_or("")
                .contains("look at auth.rs")
            {
                saw_log_with_prompt = true;
            }
        }
        if kind.get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(saw_started, "expected an AgentStarted event");
    assert!(
        saw_log_with_prompt,
        "expected a Log event carrying the dispatched prompt"
    );
    assert!(completed, "expected the RunAgent call to complete");

    // A custom-agent dispatch never touches the workspace via fs.write — no orchestrator edit
    // was ever proposed for this call.
    assert!(!dir.path().join("out.txt").exists());
}

#[tokio::test]
async fn run_agent_unknown_name_reports_error_and_keeps_connection_open() {
    let (port, _dir) = start_server_with_agent().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({
        "RunAgent": { "session": session.clone(), "name": "ghost", "prompt": "x" }
    });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert!(
        frame["message"]
            .as_str()
            .unwrap()
            .contains("no custom agent named 'ghost'"),
        "got: {frame}"
    );

    // The connection is still usable afterward — SendPrompt still completes a turn.
    let cmd2 =
        serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd2).unwrap()))
        .await
        .unwrap();
    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(completed, "connection must survive an unknown RunAgent");
}

#[tokio::test]
async fn ready_frame_carries_capabilities() {
    let (port, _dir) = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");

    let ready: Value = ready_frame(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let caps = &ready["capabilities"];
    assert!(caps.is_object(), "Ready must carry a capabilities object");
    assert_eq!(caps["engine_remote"], false);
    assert_eq!(caps["local_llm"], true);
    assert_eq!(caps["remote_llm"], false);
    assert_eq!(caps["sandbox"], true);
}

/// `?token=` is gone (spec §9): a `Users`-mode socket with no credential of any kind — no header,
/// no query token, no auth frame — is not authenticated. It hits the handshake deadline and gets
/// the single opaque Error.
#[tokio::test]
async fn rejects_missing_token() {
    let (port, _dir) = start_users_server().await;
    let mut ws = connect(request(port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after the failed handshake"
    );
}

/// A wrong bearer header is not a credential on a `Users` app: it falls through to the frame
/// handshake, and a socket that sends no `Login`/`Attach` hits the deadline (opaque Error).
#[tokio::test]
async fn rejects_wrong_token() {
    let (port, _dir) = start_users_server().await;
    let mut req = request(port, "");
    req.headers_mut()
        .insert("Authorization", "Bearer wrong-token".parse().unwrap());
    let mut ws = connect(req).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after the failed handshake"
    );
}

/// The query token is inert, not authorizing: a `SingleUser` app ignores `?token=` entirely and
/// reaches `Ready` with no credential (the old browser path neither grants nor interferes).
#[tokio::test]
async fn authorizes_via_query_token() {
    let (port, _dir) = start_server().await;
    let mut ws = connect(request(port, &format!("?token={TOKEN}"))).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "single_user");
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
}

/// `?token=` is inert on a `Users` app too: a socket presenting only a wrong query token (no
/// header, no auth frame) hits the handshake deadline and gets the opaque Error.
#[tokio::test]
async fn rejects_wrong_query_token() {
    let (port, _dir) = start_users_server().await;
    let mut ws = connect(request(port, "?token=wrong-token")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after the failed handshake"
    );
}

/// An empty `?token=` is as inert as any other query value: no header, no auth frame → the
/// handshake deadline fires and the connection is closed.
#[tokio::test]
async fn rejects_empty_query_token() {
    let (port, _dir) = start_users_server().await;
    let mut ws = connect(request(port, "?token=")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after the failed handshake"
    );
}

/// Start the serve app over TLS on 127.0.0.1:0 with a self-signed cert for "localhost".
/// Returns (port, cert_der, tempdir). The client must trust `cert_der`.
async fn start_tls_server() -> (u16, Vec<u8>, tempfile::TempDir) {
    // rustls 0.23 requires an installed crypto provider; ring is the default when
    // both ring and aws-lc-rs are available — ignore the error if already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
    let app = serve_app(service, single_user(), test_capabilities(), None, false);

    // Self-signed cert for "localhost" (connect via wss://localhost so the SAN matches).
    // rcgen 0.13 uses CertifiedKey { cert, key_pair }.
    // SAN is "localhost" (not 127.0.0.1) so the client connects via wss://localhost and gets a real hostname match; localhost resolves to the loopback the server bound.
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    let cert_der = cert.der().to_vec();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();
    let cfg = RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, Some(cfg)).await.unwrap();
    });
    (port, cert_der, dir)
}

#[tokio::test]
async fn streams_a_turn_over_wss() {
    let (port, cert_der, _dir) = start_tls_server().await;

    // Install the ring crypto provider so rustls 0.23 can function.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build a rustls client that trusts only the generated cert.
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert_der))
        .unwrap();
    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_tungstenite::Connector::Rustls(Arc::new(client_config));

    // Connect via wss://localhost (loopback) so the cert SAN matches.
    let url = format!("wss://localhost:{port}/ws");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) =
        tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(connector))
            .await
            .expect("wss connect");

    let ready: Value = ready_frame(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            saw_turn_complete = true;
            break;
        }
    }
    assert!(saw_turn_complete, "expected a TurnComplete event over wss");
}

/// Start a serve app with `--promote-loopback` enabled. Returns the bound port and the tempdir.
async fn start_promote_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    // Keep the workspace root separate from the DB so workspace snapshots don't capture binary
    // SQLite WAL files (s.db-shm etc.) and fail the UTF-8 restore check in promote().
    let ws_root = dir.path().join("workspace");
    std::fs::create_dir_all(&ws_root).unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_root));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_root));
    let tools = Arc::new(build_tool_registry(tools_ws, ws_root.clone()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
    let promote = Some(otto_engine::PromoteConfig {
        token: TOKEN.to_string(),
        mode: otto_engine::PromoteMode::Loopback {
            base_dir: dir.path().join("remotes"),
        },
    });
    let app = serve_app(service, single_user(), test_capabilities(), promote, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

/// Build an authed client request to an absolute `ws://…` endpoint (the promoted target), with a
/// session + last_seq query for replay.
fn authed_endpoint_request(
    endpoint: &str,
    session: &str,
    last_seq: u64,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let url = format!("{endpoint}/ws?session={session}&last_seq={last_seq}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}

/// The handover arm authorizes before `handle_handover`, and this is its regression test.
/// Promote ships a session's whole event log off-machine and demote overwrites the local row
/// including its owner, so the check must not be quietly droppable.
///
/// With one principal a connection cannot normally reach another owner's session, so the test
/// seeds one directly in the store under a different owner and then attaches to it by id. Since
/// the identity slice the refusal happens at attach time (`resolve_session` — the same shared
/// not-found message), so the handover command is never reachable for a foreign session; the
/// vacuity guard below proves an *owned* session still gets past authorization and fails later,
/// on the absent promote configuration, with a different message.
#[tokio::test]
async fn handover_refuses_a_session_the_connection_does_not_own() {
    let (port, dir) = start_server().await;

    let store = otto_persistence::SqliteStore::open(dir.path().join("s.db"))
        .await
        .unwrap();
    let alice = otto_protocol::UserId::parse("alice").unwrap();
    let victim = otto_persistence::SessionStore::create_session(
        &store,
        &alice,
        "alice's session",
        &serde_json::json!({}),
    )
    .await
    .unwrap();

    for cmd in ["PromoteToRemote", "DemoteToLocal"] {
        let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(
            port,
            &format!("?session={}", victim.0),
        ))
        .await
        .expect("connect");
        // Hello arrives (the greeting precedes every handshake), then the attach-time refusal —
        // never a Ready, so the handover command cannot run against a foreign session.
        hello(&mut ws).await;
        let frame: Value = next_json(&mut ws).await;
        assert_eq!(frame["type"], "error", "{cmd} should be refused");
        // The shared not-found message — never "not yours", which would be an existence oracle.
        assert!(
            frame["message"].as_str().unwrap().contains("no session"),
            "{cmd}: unexpected {frame}"
        );
        assert!(
            next_json_opt(&mut ws).await.is_none(),
            "no Ready and no further frames may follow the attach-time refusal"
        );
    }

    // Vacuity guard. The seeding above reconstructs the server's DB path by hardcoding `s.db`,
    // an unexported detail of `start_server`. If that ever changes, the seeded row would not be
    // in the server's store, the refusal above would become an ordinary not-found, and this test
    // would stay green while testing nothing about ownership. So assert the opposite direction
    // too: a session the connection *does* own gets past `authorize` and fails later, on the
    // absent promote configuration, with a different message.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let owned = ready["session"].as_str().unwrap().to_string();
    let msg = serde_json::json!({ "PromoteToRemote": { "session": owned } });
    ws.send(Message::Text(serde_json::to_string(&msg).unwrap()))
        .await
        .unwrap();
    let frame: Value = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert!(
        !frame["message"].as_str().unwrap().contains("no session"),
        "the owner must clear authorization, not be rejected by it: {frame}"
    );
}

#[tokio::test]
async fn promote_without_flag_replies_error() {
    let (port, _dir) = start_server().await; // no promote config
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut saw_error = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "error" {
            saw_error = true;
            break;
        }
    }
    assert!(
        saw_error,
        "promote without --promote-loopback must reply error"
    );
}

#[tokio::test]
async fn promote_then_demote_round_trip_preserves_session() {
    let (port, _dir) = start_promote_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect local");
    let ready: Value = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    // Run a turn on the local engine; track the highest seq seen.
    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();
    let mut last_seq = 0u64;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            last_seq = frame["event"]["seq"].as_u64().unwrap_or(last_seq);
        }
        // Stop once the turn is done: TurnComplete is the last emitted event.
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            break;
        }
    }
    assert!(
        last_seq > 0,
        "expected at least one event in the local turn before promoting"
    );

    // Promote.
    let cmd = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();
    let mut remote_endpoint = String::new();
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "promoted" {
            remote_endpoint = frame["endpoint"].as_str().unwrap().to_string();
            break;
        }
    }
    assert!(
        remote_endpoint.starts_with("ws://"),
        "got endpoint {remote_endpoint}"
    );

    // Reconnect to the remote; it must report engine_remote = true and replay the session.
    let (mut ws_r, _) = tokio_tungstenite::connect_async(authed_endpoint_request(
        &remote_endpoint,
        &session,
        last_seq,
    ))
    .await
    .expect("connect remote");
    let ready_r: Value = ready_frame(&mut ws_r).await;
    assert_eq!(ready_r["type"], "ready");
    assert_eq!(ready_r["capabilities"]["engine_remote"], true);

    // Demote back to local.
    let cmd = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws_r.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();
    let mut local_endpoint = String::new();
    while let Some(frame) = next_json_opt(&mut ws_r).await {
        if frame["type"] == "demoted" {
            local_endpoint = frame["endpoint"].as_str().unwrap().to_string();
            break;
        }
    }
    assert!(
        local_endpoint.starts_with("ws://"),
        "got endpoint {local_endpoint}"
    );

    // Reconnect to the demoted-local engine; it must report engine_remote = false.
    let (mut ws_l, _) =
        tokio_tungstenite::connect_async(authed_endpoint_request(&local_endpoint, &session, 0))
            .await
            .expect("connect demoted-local");
    let ready_l: Value = ready_frame(&mut ws_l).await;
    assert_eq!(ready_l["capabilities"]["engine_remote"], false);
    // Continuity: replaying from seq 0 yields the original session's events.
    let mut saw_event = false;
    while let Some(frame) = next_json_opt(&mut ws_l).await {
        if frame["type"] == "event" {
            saw_event = true;
            break;
        }
    }
    assert!(
        saw_event,
        "demoted-local engine should replay the session history"
    );
}

/// Receive the next text frame as JSON (panics on close/non-text or stream end).
async fn next_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Value {
    next_json_opt(ws).await.expect("expected a frame")
}

/// Receive the next text frame as JSON, or None if the stream ended / a non-text frame arrived.
async fn next_json_opt(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue, // skip ping/pong/binary
            Some(Err(_)) => return None,
        }
    }
}

/// Ownership is wired end to end: a served session is owned by the reserved local principal,
/// and the store's scoped reads accept it. This is the seam slice 1b flips on — until then
/// there is exactly one principal, so this asserts the plumbing, not isolation.
#[tokio::test]
async fn served_sessions_are_owned_by_the_local_principal() {
    let (port, dir) = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .unwrap();
    let ready: Value = ready_frame(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    let store = otto_persistence::SqliteStore::open(dir.path().join("s.db"))
        .await
        .unwrap();
    let id = otto_protocol::SessionId(session.parse().unwrap());
    assert_eq!(
        otto_persistence::SessionStore::owner_of(&store, id)
            .await
            .unwrap(),
        otto_protocol::UserId::local()
    );
}
