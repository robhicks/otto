//! vps RemoteTarget: POST /promote handler gating + status codes, VpsTarget round-trip,
//! teardown no-op, and handover-mode behavior. All in-process on ephemeral loopback ports.

use std::sync::Arc;

use otto_engine::{
    EngineService, PromoteBundle, RemoteTarget, VpsTarget, build_default_registry,
    build_tool_registry, serve_app, serve_run,
};
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStatus, SqliteStore};
use otto_protocol::{CapabilitiesManifest, SessionId};
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use std::path::PathBuf;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "vps-token";

/// Build a `PromoteBundle` with the given session id and workspace files (typed → serialized,
/// so the wire shape always matches the real serde derives).
fn sample_bundle(id: SessionId, files: Vec<(&str, &[u8])>) -> PromoteBundle {
    PromoteBundle {
        session: SessionState {
            id,
            goal: "g".to_string(),
            status: SessionStatus::Active,
            config: serde_json::json!({}),
            events: vec![],
            turns: vec![],
        },
        workspace: WorkspaceSnapshot {
            files: files
                .into_iter()
                .map(|(p, b)| (PathBuf::from(p), b.to_vec()))
                .collect(),
        },
    }
}

fn caps() -> CapabilitiesManifest {
    CapabilitiesManifest {
        engine_remote: false,
        local_llm: false,
        remote_llm: false,
        sandbox: false,
    }
}

/// Start a receiver `otto serve` on an ephemeral port. Returns its `http://127.0.0.1:<port>` base.
async fn start_receiver(accept_promotions: bool) -> (String, tempfile::TempDir, tempfile::TempDir) {
    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, ws_dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(SqliteStore::open(db_dir.path().join("r.db")).await.unwrap());
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        Arc::from(otto_engine::build_router()),
        workspace,
        tools,
    );
    let app = serve_app(service, TOKEN.to_string(), caps(), None, accept_promotions);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), ws_dir, db_dir)
}

async fn post_promote(base: &str, token: Option<&str>, body: &PromoteBundle) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/promote"))
        .json(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn promote_without_accept_flag_is_forbidden() {
    let (base, _w, _d) = start_receiver(false).await;
    let body = sample_bundle(SessionId::new(), vec![]);
    let resp = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn promote_without_bearer_is_unauthorized() {
    let (base, _w, _d) = start_receiver(true).await;
    let body = sample_bundle(SessionId::new(), vec![]);
    let resp = post_promote(&base, None, &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn promote_with_wrong_bearer_is_unauthorized() {
    let (base, _w, _d) = start_receiver(true).await;
    let body = sample_bundle(SessionId::new(), vec![]);
    let resp = post_promote(&base, Some("nope"), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn promote_valid_bundle_is_ok_and_restores() {
    let (base, _w, _d) = start_receiver(true).await;
    let id = SessionId::new();
    let body = sample_bundle(id, vec![("out.txt", b"HELLO")]);
    let resp = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["session"].as_str().unwrap(), id.0.to_string());
}

#[tokio::test]
async fn promote_sensitive_entry_is_refused() {
    let (base, _w, _d) = start_receiver(true).await;
    let body = sample_bundle(SessionId::new(), vec![(".env", b"SECRET=1")]);
    let resp = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn promote_duplicate_session_is_conflict() {
    let (base, _w, _d) = start_receiver(true).await;
    let id = SessionId::new();
    let body = sample_bundle(id, vec![]);
    let first = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    let second = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(second.status(), reqwest::StatusCode::CONFLICT);
}

#[tokio::test]
async fn promote_malformed_body_is_bad_request() {
    let (base, _w, _d) = start_receiver(true).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/promote"))
        .bearer_auth(TOKEN)
        .body("{ not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn vps_target_provisions_against_a_receiver() {
    let (http_base, _w, _d) = start_receiver(true).await;
    let ws_endpoint = http_base.replace("http://", "ws://");

    let bundle = sample_bundle(SessionId::new(), vec![("out.txt", b"HI")]);
    let target = VpsTarget::new(ws_endpoint.clone(), TOKEN);
    let handle = target.provision(&bundle).await.unwrap();
    // The handle points back at the ws endpoint the client reconnects to.
    assert_eq!(handle.endpoint, ws_endpoint);
}

#[tokio::test]
async fn vps_target_teardown_does_not_stop_the_receiver() {
    let (http_base, _w, _d) = start_receiver(true).await;
    let ws_endpoint = http_base.replace("http://", "ws://");

    let bundle = sample_bundle(SessionId::new(), vec![]);
    let target = VpsTarget::new(ws_endpoint, TOKEN);
    let handle = target.provision(&bundle).await.unwrap();
    target.teardown(handle).await.unwrap();

    // The receiver is still up: a second valid POST /promote (new session id) succeeds.
    let body = sample_bundle(SessionId::new(), vec![]);
    let resp = post_promote(&http_base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn vps_target_provision_errors_on_non_2xx() {
    // Receiver with acceptance DISABLED → /promote returns 403 → provision must Err.
    let (http_base, _w, _d) = start_receiver(false).await;
    let ws_endpoint = http_base.replace("http://", "ws://");
    let bundle = sample_bundle(SessionId::new(), vec![]);
    let target = VpsTarget::new(ws_endpoint, TOKEN);
    assert!(target.provision(&bundle).await.is_err());
}

async fn next_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<serde_json::Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(_)) => return None,
        }
    }
}

fn authed_ws_request(url: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = url.to_string().into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}

/// Start a SOURCE serve configured to promote in vps mode at `vps_endpoint`. Returns its ws base.
async fn start_source_vps(vps_endpoint: String) -> (String, tempfile::TempDir, tempfile::TempDir) {
    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, ws_dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(SqliteStore::open(db_dir.path().join("s.db")).await.unwrap());
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        Arc::from(otto_engine::build_router()),
        workspace,
        tools,
    );
    let promote = Some(otto_engine::PromoteConfig {
        token: TOKEN.to_string(),
        mode: otto_engine::PromoteMode::Vps {
            endpoint: vps_endpoint,
        },
    });
    let app = serve_app(service, TOKEN.to_string(), caps(), promote, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (format!("ws://127.0.0.1:{port}"), ws_dir, db_dir)
}

#[tokio::test]
async fn handover_vps_promote_points_at_receiver() {
    // Receiver accepts promotions; source promotes to it in vps mode.
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");
    let (src_ws, _sw, _sd) = start_source_vps(recv_ws.clone()).await;

    // Connect to the source, which creates a fresh session on connect.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = next_json(&mut ws).await.unwrap();
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    // Send PromoteToRemote (externally tagged) and expect a Promoted frame at the receiver.
    let cmd = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();
    loop {
        let frame = next_json(&mut ws).await.expect("a frame");
        if frame["type"] == "promoted" {
            assert_eq!(frame["endpoint"].as_str().unwrap(), recv_ws);
            break;
        }
        assert_ne!(frame["type"], "error", "promote must not error: {frame:?}");
    }
}

#[tokio::test]
async fn handover_vps_demote_is_unsupported() {
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");
    let (src_ws, _sw, _sd) = start_source_vps(recv_ws).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = next_json(&mut ws).await.unwrap();
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();
    loop {
        let frame = next_json(&mut ws).await.expect("a frame");
        if frame["type"] == "error" {
            assert!(
                frame["message"]
                    .as_str()
                    .unwrap()
                    .contains("demote-from-remote not supported")
            );
            break;
        }
        assert_ne!(frame["type"], "demoted", "demote must not succeed in vps mode");
    }
}

#[tokio::test]
async fn vps_promote_resumes_session_and_workspace_on_receiver() {
    use otto_engine::CollectingSink;
    use otto_engine_core::traits::WorkspaceRead;
    use otto_workspace::RemoteWorkspace;

    // --- Receiver serve B, acceptance enabled. ---
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");

    // --- Source engine A: run a turn that writes out.txt. ---
    let src_ws_dir = tempfile::tempdir().unwrap();
    let src_db_dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "PROMOTED"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(src_ws_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(src_ws_dir.path()));
    let tools = Arc::new(build_tool_registry(
        tools_ws,
        src_ws_dir.path().to_path_buf(),
    ));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        SqliteStore::open(src_db_dir.path().join("a.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store.clone(),
        Arc::new(build_default_registry()),
        router,
        workspace.clone(),
        tools,
    );

    let session = service
        .create_session("g", &serde_json::json!({}))
        .await
        .unwrap();
    let mut sink = CollectingSink::default();
    service
        .run_prompt(session, "add a greeting", &mut sink)
        .await
        .unwrap();
    let src_events = store.replay_since(session, None).await.unwrap();
    let last_seq = src_events.last().unwrap().seq;

    // --- Promote to the receiver via VpsTarget. ---
    let target = VpsTarget::new(recv_ws.clone(), TOKEN);
    let handle = otto_engine::promote(&*store, &*workspace, session, &target)
        .await
        .unwrap();
    assert_eq!(handle.endpoint, recv_ws);

    // --- Reconnect to the receiver: same session, replayed gap after seq 0. ---
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!(
        "{recv_ws}/ws?session={}&last_seq=0",
        session.0
    )))
    .await
    .unwrap();
    let ready = next_json(&mut ws).await.unwrap();
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["session"].as_str().unwrap(), session.0.to_string());

    let mut replayed = Vec::new();
    while let Some(frame) = next_json(&mut ws).await {
        if frame["type"] == "event" {
            let seq = frame["event"]["seq"].as_u64().unwrap();
            replayed.push(seq);
            if seq == last_seq {
                break;
            }
        }
    }
    let expected: Vec<u64> = src_events
        .iter()
        .map(|e| e.seq)
        .filter(|s| *s > 0)
        .collect();
    assert_eq!(replayed, expected);
    drop(ws);

    // --- The workspace transferred: read out.txt via the receiver's /workspace RPC. ---
    let remote_ws = RemoteWorkspace::new(recv_http, TOKEN);
    assert_eq!(
        remote_ws
            .read(std::path::Path::new("out.txt"))
            .await
            .unwrap(),
        b"PROMOTED"
    );
}
