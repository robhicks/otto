//! End-to-end: a RemoteWorkspace client drives a remote engine's workspace over the
//! bearer-authed POST /workspace RPC, on a loopback ephemeral port. Asserts read/list/
//! apply_edit/snapshot parity with the backing LocalWorkspace, the server-side gate floor,
//! and auth rejection. No external network.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_engine::{EngineService, build_default_registry, build_tool_registry, serve_app, serve_run};
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::Edit;
use otto_providers::LocalProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::{LocalWorkspace, RemoteWorkspace};

const TOKEN: &str = "test-token";

/// Start the serve app backed by a LocalWorkspace over `dir` on 127.0.0.1:0; return the port.
async fn start_server(dir: &Path) -> u16 {
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.to_path_buf()));
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(LocalProvider::new())));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.join("s.db")).await.unwrap(),
    );
    let service = EngineService::new(store, Arc::new(build_default_registry()), router, workspace, tools);
    let app = serve_app(service, TOKEN.to_string());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    port
}

#[tokio::test]
async fn remote_workspace_round_trips_against_local() {
    let dir = tempfile::tempdir().unwrap();
    let port = start_server(dir.path()).await;
    let remote = RemoteWorkspace::new(format!("http://127.0.0.1:{port}"), TOKEN);
    let local = LocalWorkspace::new(dir.path());

    // Write via the remote, observe it both via the remote and directly on disk.
    let n = remote
        .apply_edit(&Edit { path: PathBuf::from("a.txt"), new_contents: "hello".to_string() })
        .await
        .unwrap();
    assert_eq!(n, 5);
    assert_eq!(remote.read(Path::new("a.txt")).await.unwrap(), b"hello");
    assert_eq!(local.read(Path::new("a.txt")).await.unwrap(), b"hello");

    // A nested write, then list + snapshot parity with the backing LocalWorkspace.
    remote
        .apply_edit(&Edit { path: PathBuf::from("src/lib.rs"), new_contents: "L".to_string() })
        .await
        .unwrap();
    let listing = remote.list("**").await.unwrap();
    assert!(listing.contains(&PathBuf::from("a.txt")));
    assert!(listing.contains(&PathBuf::from("src/lib.rs")));

    let mut remote_files = remote.snapshot().await.unwrap().files;
    remote_files.sort();
    let mut local_files = local.snapshot().await.unwrap().files;
    local_files.sort();
    assert_eq!(remote_files, local_files);
}

#[tokio::test]
async fn remote_write_to_sensitive_path_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let port = start_server(dir.path()).await;
    let remote = RemoteWorkspace::new(format!("http://127.0.0.1:{port}"), TOKEN);

    let result = remote
        .apply_edit(&Edit { path: PathBuf::from(".env"), new_contents: "SECRET=x".to_string() })
        .await;
    assert!(result.is_err(), "writing a sensitive path over the RPC must be denied");

    // Nothing was written: a direct read of .env on disk fails (file absent).
    let local = LocalWorkspace::new(dir.path());
    assert!(local.read(Path::new(".env")).await.is_err());
}

#[tokio::test]
async fn remote_workspace_rejects_wrong_token() {
    let dir = tempfile::tempdir().unwrap();
    let port = start_server(dir.path()).await;
    let remote = RemoteWorkspace::new(format!("http://127.0.0.1:{port}"), "wrong-token");
    assert!(remote.list("**").await.is_err(), "a wrong token must be rejected");
}
