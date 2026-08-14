//! vps RemoteTarget: POST /promote handler gating + status codes, VpsTarget round-trip,
//! teardown no-op, and handover-mode behavior. All in-process on ephemeral loopback ports.

use std::sync::Arc;

use otto_engine::{
    EngineService, PromoteBundle, RemoteTarget, VpsTarget, build_default_registry,
    build_tool_registry, serve_app, serve_run,
};
use otto_engine_core::auth::AuthConfig;
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStatus, SessionStore, SqliteStore};
use otto_protocol::{AuthMode, CapabilitiesManifest, SessionId};
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
            owner: otto_protocol::UserId::local(),
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

/// Start a `--promotion-receiver` serve (`AuthMode::Machine`) on an ephemeral port. Returns its
/// `http://127.0.0.1:<port>` base. `TOKEN` doubles as the promotion secret — the credential the
/// `/promote`/`/export`/`/workspace` RPCs and a WS `Bearer` header all carry.
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
    let auth = AuthConfig {
        mode: AuthMode::Machine,
        authenticator: None,
        promotion_secret: Some(TOKEN.to_string()),
        handshake_deadline: std::time::Duration::from_secs(10),
    };
    let app = serve_app(service, auth, caps(), None, accept_promotions);
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

async fn post_export(base: &str, token: Option<&str>, session: &str) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/export"))
        .json(&serde_json::json!({ "session": session }));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn export_without_accept_flag_is_forbidden() {
    let (base, _w, _d) = start_receiver(false).await;
    let resp = post_export(&base, Some(TOKEN), &SessionId::new().0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn export_without_bearer_is_unauthorized() {
    let (base, _w, _d) = start_receiver(true).await;
    let resp = post_export(&base, None, &SessionId::new().0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn export_unknown_session_is_not_found() {
    let (base, _w, _d) = start_receiver(true).await;
    let resp = post_export(&base, Some(TOKEN), &SessionId::new().0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn export_existing_session_returns_a_bundle() {
    // Promote a session onto the receiver, then export it back out.
    let (base, _w, _d) = start_receiver(true).await;
    let id = SessionId::new();
    let body = sample_bundle(id, vec![("out.txt", b"HI")]);
    assert_eq!(
        post_promote(&base, Some(TOKEN), &body).await.status(),
        reqwest::StatusCode::OK
    );
    let resp = post_export(&base, Some(TOKEN), &id.0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let bundle: PromoteBundle = resp.json().await.unwrap();
    assert_eq!(bundle.session.id, id);
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
    // A hostile/malformed bundle is a client fault → 400, not a receiver error (500).
    let (base, _w, _d) = start_receiver(true).await;
    let body = sample_bundle(SessionId::new(), vec![(".env", b"SECRET=1")]);
    let resp = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
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
async fn export_with_wrong_bearer_is_unauthorized() {
    let (base, _w, _d) = start_receiver(true).await;
    let resp = post_export(&base, Some("nope"), &SessionId::new().0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn export_malformed_body_is_bad_request() {
    let (base, _w, _d) = start_receiver(true).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/export"))
        .bearer_auth(TOKEN)
        .body("{ not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn export_malformed_session_id_is_bad_request() {
    let (base, _w, _d) = start_receiver(true).await;
    let resp = post_export(&base, Some(TOKEN), "not-a-uuid").await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn demote_without_promote_config_is_unavailable() {
    let (recv_http, _w, db_dir) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");
    // A `Machine` receiver creates no sessions on connect, so seed the session the connection
    // adopts (the promotion-secret header pre-resolves; `?session=` names the promoted copy).
    let store = SqliteStore::open(db_dir.path().join("r.db")).await.unwrap();
    let session = SessionStore::create_session(
        &store,
        &otto_protocol::UserId::local(),
        "promoted",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!(
        "{recv_ws}/ws?session={}",
        session.0
    )))
    .await
    .unwrap();
    ready_frame(&mut ws).await;
    let demote = serde_json::json!({ "DemoteToLocal": { "session": session.0.to_string() } });
    ws.send(Message::Text(serde_json::to_string(&demote).unwrap()))
        .await
        .unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "error" {
            assert!(
                f["message"]
                    .as_str()
                    .unwrap()
                    .contains("remote provisioning unavailable"),
                "{f:?}"
            );
            break;
        }
        assert_ne!(
            f["type"], "demoted",
            "demote must not succeed without promote config"
        );
    }
}

#[tokio::test]
async fn demote_with_rejected_export_surfaces_error() {
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");
    let (src_ws, _sw, _sd) = start_source_vps(recv_ws).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();
    // NOTE: session was created on the source but never promoted, so the receiver has no copy.
    let demote = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&demote).unwrap()))
        .await
        .unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "error" {
            break;
        }
        assert_ne!(
            f["type"], "demoted",
            "demote must not succeed when the receiver has no copy: {f:?}"
        );
    }
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

/// Read the `Hello` greeting (always the first frame, in every mode).
async fn hello_frame(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) {
    let hello = next_json(ws).await.expect("hello frame");
    assert_eq!(hello["type"], "hello");
}

/// Read `Hello` then `Ready`, returning the `Ready` frame.
async fn ready_frame(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> serde_json::Value {
    hello_frame(ws).await;
    let ready = next_json(ws).await.expect("ready frame");
    assert_eq!(ready["type"], "ready");
    ready
}

/// Start a SOURCE serve configured to promote in vps mode at `vps_endpoint`. Returns its ws base.
/// The source's own posture is `SingleUser` — this harness drives it only through its WS
/// handover commands, never a credential, and its handover token is the receiver's promotion
/// secret carried by the `PromoteConfig` below (spec §6.5 keeps the per-mode credentials apart).
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
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("ws://127.0.0.1:{port}");
    let auth = AuthConfig {
        mode: AuthMode::SingleUser,
        authenticator: None,
        promotion_secret: None,
        handshake_deadline: std::time::Duration::from_secs(10),
    };
    let app = otto_engine::serve_app_with_base(service, auth, caps(), promote, false, base.clone());
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (base, ws_dir, db_dir)
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
    let ready = ready_frame(&mut ws).await;
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
async fn handover_vps_demote_pulls_session_back_to_source() {
    use otto_engine_core::traits::Workspace as _;
    use otto_workspace::RemoteWorkspace;

    // Receiver accepts promotions; source promotes to it in vps mode.
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");
    let (src_ws, src_w, _sd) = start_source_vps(recv_ws.clone()).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    // Promote to the receiver.
    let promote = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&promote).unwrap()))
        .await
        .unwrap();
    loop {
        let frame = next_json(&mut ws).await.expect("a frame");
        if frame["type"] == "promoted" {
            break;
        }
        assert_ne!(frame["type"], "error", "promote must not error: {frame:?}");
    }

    // Advance the session on the RECEIVER: write a file via its /workspace RPC.
    let recv_remote_ws = RemoteWorkspace::new(recv_http.clone(), TOKEN);
    recv_remote_ws
        .apply_edit(&otto_engine_core::types::Edit {
            path: std::path::PathBuf::from("remote_only.txt"),
            new_contents: "FROM_RECEIVER".to_string(),
        })
        .await
        .unwrap();

    // Demote: the source pulls the session + workspace back to local.
    let demote = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&demote).unwrap()))
        .await
        .unwrap();
    loop {
        let frame = next_json(&mut ws).await.expect("a frame");
        if frame["type"] == "demoted" {
            assert_eq!(frame["endpoint"].as_str().unwrap(), src_ws);
            break;
        }
        assert_ne!(frame["type"], "error", "demote must not error: {frame:?}");
    }

    // The receiver-only file now exists in the SOURCE's on-disk workspace.
    assert_eq!(
        std::fs::read(src_w.path().join("remote_only.txt")).unwrap(),
        b"FROM_RECEIVER"
    );
}

#[tokio::test]
async fn vps_demote_round_trip_brings_advanced_state_back_to_source() {
    use otto_engine_core::traits::Workspace as _;

    // Receiver, acceptance enabled; source serve in vps mode pointed at it.
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");
    let (src_ws, src_w, _sd) = start_source_vps(recv_ws.clone()).await;

    // Connect to the source, create + promote a session.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();
    let promote = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&promote).unwrap()))
        .await
        .unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "promoted" {
            break;
        }
        assert_ne!(f["type"], "error", "{f:?}");
    }

    // Advance the session ON THE RECEIVER: write a file via its /workspace RPC.
    let recv_remote_ws = otto_workspace::RemoteWorkspace::new(recv_http.clone(), TOKEN);
    recv_remote_ws
        .apply_edit(&otto_engine_core::types::Edit {
            path: std::path::PathBuf::from("advanced.txt"),
            new_contents: "ADVANCED_ON_RECEIVER".to_string(),
        })
        .await
        .unwrap();

    // Demote back to the source.
    let demote = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&demote).unwrap()))
        .await
        .unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "demoted" {
            break;
        }
        assert_ne!(f["type"], "error", "{f:?}");
    }
    drop(ws);

    // The source's on-disk workspace now has the receiver's advanced file.
    assert_eq!(
        std::fs::read(src_w.path().join("advanced.txt")).unwrap(),
        b"ADVANCED_ON_RECEIVER"
    );

    // INCREMENTAL VALUE: the source can reconnect to its now-local session — a fresh /ws connection
    // with the id yields a Ready for the same session (it lives in the source store again).
    let (mut ws2, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!(
        "{src_ws}/ws?session={session}&last_seq=0"
    )))
    .await
    .unwrap();
    let ready2 = ready_frame(&mut ws2).await;
    assert_eq!(ready2["session"].as_str().unwrap(), session);

    // Copy semantics: demote is non-destructive — the receiver still holds the session.
    let resp = post_export(&recv_http, Some(TOKEN), &session).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
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
        .create_session(&otto_protocol::UserId::local(), "g", &serde_json::json!({}))
        .await
        .unwrap();
    let mut sink = CollectingSink::default();
    service
        .run_prompt(
            &otto_protocol::UserId::local(),
            session,
            "add a greeting",
            &mut sink,
        )
        .await
        .unwrap();
    let src_events = store
        .replay_since(&otto_protocol::UserId::local(), session, None)
        .await
        .unwrap();
    let last_seq = src_events.last().unwrap().seq;

    // --- Promote to the receiver via VpsTarget. ---
    let target = VpsTarget::new(recv_ws.clone(), TOKEN);
    let handle = otto_engine::promote(&*store, &*workspace, session, &target)
        .await
        .unwrap();
    assert_eq!(handle.endpoint, recv_ws);

    // --- Reconnect to the receiver: same session, replayed gap after seq 0. The `Machine`
    // receiver pre-resolves from the promotion-secret header and adopts the promoted session. ---
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!(
        "{recv_ws}/ws?session={}&last_seq=0",
        session.0
    )))
    .await
    .unwrap();
    let ready = ready_frame(&mut ws).await;
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
