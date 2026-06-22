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
use otto_workspace::LocalWorkspace;
use std::path::PathBuf;

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
