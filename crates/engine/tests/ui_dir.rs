//! `otto serve --ui-dir <path>` serves a pre-built web UI bundle as the router's fallback.
//!
//! The route is deliberately unauthenticated (a browser must fetch index.html and the wasm before
//! it has a token) and deliberately absent when `--ui-dir` is not passed. Both properties are
//! asserted here — the second is the regression guard that keeps this feature inert by default.

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, serve_app, serve_with_ui_dir,
};
use otto_engine_core::traits::Workspace;
use otto_providers::LocalProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use tower::ServiceExt; // for `oneshot`

const TOKEN: &str = "test-token";

/// Build the serve router (unbound), optionally with a `--ui-dir` bundle directory layered on.
async fn build_app(ui_dir: Option<PathBuf>) -> (axum::Router, tempfile::TempDir) {
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
        None,
        false,
    );
    let app = match ui_dir {
        Some(d) => serve_with_ui_dir(app, d),
        None => app,
    };
    (app, dir)
}

/// A throwaway "bundle": an index and one hashed asset, the shape `dx build` emits.
fn write_bundle() -> tempfile::TempDir {
    let bundle = tempfile::tempdir().unwrap();
    std::fs::write(bundle.path().join("index.html"), b"<html>otto ui</html>").unwrap();
    std::fs::create_dir_all(bundle.path().join("assets")).unwrap();
    std::fs::write(bundle.path().join("assets/app-abc123.wasm"), b"\0asm-fake").unwrap();
    bundle
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn serves_index_at_root_without_a_token() {
    let bundle = write_bundle();
    let (app, _dir) = build_app(Some(bundle.path().to_path_buf())).await;
    let req = axum::http::Request::builder()
        .uri("/")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(
        resp.status().is_success(),
        "GET / must succeed with no Authorization header — a browser has no token on first \
         load, got {}",
        resp.status()
    );
    assert!(body_string(resp).await.contains("otto ui"));
}

#[tokio::test]
async fn serves_a_hashed_asset_without_a_token() {
    let bundle = write_bundle();
    let (app, _dir) = build_app(Some(bundle.path().to_path_buf())).await;
    let req = axum::http::Request::builder()
        .uri("/assets/app-abc123.wasm")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(resp.status().is_success(), "got {}", resp.status());
}

#[tokio::test]
async fn unknown_path_falls_back_to_index() {
    let bundle = write_bundle();
    let (app, _dir) = build_app(Some(bundle.path().to_path_buf())).await;
    let req = axum::http::Request::builder()
        .uri("/no/such/path")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(resp.status().is_success(), "got {}", resp.status());
    assert!(body_string(resp).await.contains("otto ui"));
}

/// The regression guard: with no `--ui-dir`, the feature must be completely inert.
#[tokio::test]
async fn without_ui_dir_root_is_not_served() {
    let (app, _dir) = build_app(None).await;
    let req = axum::http::Request::builder()
        .uri("/")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::NOT_FOUND,
        "with no --ui-dir there must be no static route at all"
    );
}

/// The existing API routes must behave identically with the layer installed. `/ws` without an
/// upgrade is the cheapest probe that proves the fallback did not swallow a real route.
#[tokio::test]
async fn existing_routes_are_unaffected_by_the_fallback() {
    let bundle = write_bundle();
    let (app, _dir) = build_app(Some(bundle.path().to_path_buf())).await;
    let req = axum::http::Request::builder()
        .uri("/ws")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_ne!(
        resp.status(),
        axum::http::StatusCode::OK,
        "/ws must still be handled by the ws route (rejecting a non-upgrade request), not \
         served as a static file"
    );
    assert!(
        !body_string(resp).await.contains("otto ui"),
        "/ws must never fall through to index.html"
    );
}

/// `ServeDir` must not escape the bundle directory. This matters more here than in a normal
/// static server: the sensitive-path floor that guards every other file access in otto does not
/// apply to this route.
///
/// The bundle is nested one level down inside the tempdir so the "outside" file has somewhere
/// real to live — putting it in the shared system temp dir under a fixed name would collide
/// between concurrent test runs.
#[tokio::test]
async fn path_traversal_does_not_escape_the_bundle_dir() {
    let outer = tempfile::tempdir().unwrap();
    let bundle_dir = outer.path().join("public");
    std::fs::create_dir_all(&bundle_dir).unwrap();
    std::fs::write(bundle_dir.join("index.html"), b"<html>otto ui</html>").unwrap();
    std::fs::write(outer.path().join("outside-secret.txt"), b"TOP SECRET").unwrap();

    let (app, _dir) = build_app(Some(bundle_dir)).await;
    let req = axum::http::Request::builder()
        .uri("/../outside-secret.txt")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(
        !body_string(resp).await.contains("TOP SECRET"),
        "traversal must not read outside the bundle dir"
    );
}
