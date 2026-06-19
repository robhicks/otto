//! The /workspace endpoint must advertise CORS so the browser UI (served from a different
//! origin by trunk) can make its preflighted cross-origin POST. Tested in-process via
//! tower's oneshot — no port binding, no network.

use std::sync::Arc;

use otto_engine::{EngineService, build_default_registry, build_tool_registry, serve_app};
use otto_engine_core::traits::Workspace;
use otto_providers::LocalProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use tower::ServiceExt; // for `oneshot`

const TOKEN: &str = "test-token";

/// Build the serve router (unbound) over a temp workspace.
async fn build_app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(LocalProvider::new())));
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
    let app = serve_app(
        service,
        TOKEN.to_string(),
        otto_protocol::CapabilitiesManifest {
            engine_remote: false,
            local_llm: false,
            remote_llm: false,
            sandbox: false,
        },
    );
    (app, dir)
}

#[tokio::test]
async fn workspace_preflight_advertises_cors() {
    let (app, _dir) = build_app().await;
    let req = axum::http::Request::builder()
        .method(axum::http::Method::OPTIONS)
        .uri("/workspace")
        .header(axum::http::header::ORIGIN, "http://127.0.0.1:8080")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization,content-type",
        )
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(
        resp.status().is_success(),
        "preflight should be answered 2xx, got {}",
        resp.status()
    );
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*"),
    );
    let methods = resp
        .headers()
        .get("access-control-allow-methods")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(methods.contains("POST"), "allow-methods was {methods:?}");
    let allow_headers = resp
        .headers()
        .get("access-control-allow-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        allow_headers.contains("authorization"),
        "allow-headers was {allow_headers:?}"
    );
    assert!(
        allow_headers.contains("content-type"),
        "allow-headers must include content-type, was {allow_headers:?}"
    );
}

#[tokio::test]
async fn workspace_post_response_carries_cors_origin() {
    let (app, _dir) = build_app().await;
    let body = serde_json::to_vec(&otto_protocol::WorkspaceRequest::List {
        glob: "**/*".to_string(),
    })
    .unwrap();
    let req = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/workspace")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::ORIGIN, "http://127.0.0.1:8080")
        .body(axum::body::Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status().is_success(),
        "POST should succeed, got {}",
        resp.status()
    );
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*"),
    );
}
