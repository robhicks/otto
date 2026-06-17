# RemoteWorkspace + Workspace RPC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the `Workspace` seam over a bearer-authed `POST /workspace` unary RPC (gating read+write through the permission floor server-side) and add a `RemoteWorkspace` client that implements the `Workspace` trait over it.

**Architecture:** `WorkspaceRequest`/`WorkspaceResponse` wire enums in `protocol`. `EngineService::workspace_rpc` dispatches a request against the service's workspace, running `read`/`apply_edit` through `ToolRegistry::check` (Allow-only). A `/workspace` axum route (shared bearer auth with `/ws`) calls it. `RemoteWorkspace` (workspace crate, `reqwest`) implements `Workspace` by POSTing requests. The seam is built + tested in isolation (loopback round-trip); wiring it into a remote orchestrator is the next sub-project.

**Tech Stack:** Rust (edition 2024), `axum` (existing), `reqwest` 0.12 (rustls), `serde`/`serde_json`, tokio/tempfile for tests.

**Spec:** `docs/superpowers/specs/2026-06-17-remote-workspace-design.md`.

---

### Task 1: Workspace RPC wire types (`protocol`)

**Files:**
- Modify: `crates/protocol/src/lib.rs`

- [ ] **Step 1: Write the failing serde round-trip test**

In `crates/protocol/src/lib.rs`, add to the existing `#[cfg(test)] mod tests` block:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-protocol workspace_rpc_types_round_trip_through_json`
Expected: FAIL to compile — `WorkspaceRequest`/`WorkspaceResponse` do not exist.

- [ ] **Step 3: Add the types**

In `crates/protocol/src/lib.rs`, add (after the `Event` struct; `PathBuf` and `serde::{Deserialize, Serialize}` are already imported and used by existing types):

```rust
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-protocol workspace_rpc_types_round_trip_through_json`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/protocol/src/lib.rs
git commit -m "feat(protocol): WorkspaceRequest/WorkspaceResponse wire types"
```

---

### Task 2: `EngineService::workspace_rpc` (dispatch + server-side gating)

**Files:**
- Modify: `crates/engine/src/service.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/engine/src/service.rs`, add to the `#[cfg(test)] mod tests` block (the `service_in` helper, which builds an `EngineService` over a `LocalWorkspace` tempdir with the real `DefaultPermissionGate` tools, already exists):

```rust
    #[tokio::test]
    async fn workspace_rpc_write_read_list_snapshot() {
        use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;

        // Write
        match service
            .workspace_rpc(WorkspaceRequest::ApplyEdit {
                path: std::path::PathBuf::from("a.txt"),
                contents: "hi".to_string(),
            })
            .await
        {
            WorkspaceResponse::ApplyEdit { bytes_written } => assert_eq!(bytes_written, 2),
            other => panic!("unexpected: {other:?}"),
        }
        // Read it back
        match service
            .workspace_rpc(WorkspaceRequest::Read {
                path: std::path::PathBuf::from("a.txt"),
            })
            .await
        {
            WorkspaceResponse::Read { bytes } => assert_eq!(bytes, b"hi".to_vec()),
            other => panic!("unexpected: {other:?}"),
        }
        // List + Snapshot return Ok variants
        assert!(matches!(
            service.workspace_rpc(WorkspaceRequest::List { glob: "**".to_string() }).await,
            WorkspaceResponse::List { .. }
        ));
        assert!(matches!(
            service.workspace_rpc(WorkspaceRequest::Snapshot).await,
            WorkspaceResponse::Snapshot { .. }
        ));
    }

    #[tokio::test]
    async fn workspace_rpc_gates_sensitive_write_and_read() {
        use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;

        // Writing a sensitive path is denied by the gate floor (nothing written).
        assert!(matches!(
            service
                .workspace_rpc(WorkspaceRequest::ApplyEdit {
                    path: std::path::PathBuf::from(".env"),
                    contents: "SECRET=x".to_string(),
                })
                .await,
            WorkspaceResponse::Error { .. }
        ));
        // Reading a sensitive path is denied too.
        assert!(matches!(
            service
                .workspace_rpc(WorkspaceRequest::Read {
                    path: std::path::PathBuf::from(".env"),
                })
                .await,
            WorkspaceResponse::Error { .. }
        ));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine service::tests::workspace_rpc`
Expected: FAIL to compile — `workspace_rpc` is not defined on `EngineService`.

- [ ] **Step 3: Implement `workspace_rpc`**

In `crates/engine/src/service.rs`, add these imports to the top-of-file `use` block:

```rust
use otto_engine_core::tool::Decision;
use otto_engine_core::types::Edit;
use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
use serde_json::json;
```

(`Event`, `EventKind`, `SessionId` are already imported from `otto_protocol`; add `WorkspaceRequest`, `WorkspaceResponse` to that line or as a separate `use` — either is fine.)

Add the method to `impl EngineService` (after `run_prompt`):

```rust
    /// Handle one unary workspace RPC against this service's workspace. `read` and
    /// `apply_edit` are routed through the permission gate (Allow-only), so the
    /// network-exposed primitive cannot read/write sensitive paths even though it bypasses
    /// the orchestrator. `list`/`snapshot` rely on the list walk's dotfile/`.git` exclusion.
    pub async fn workspace_rpc(&self, req: WorkspaceRequest) -> WorkspaceResponse {
        match req {
            WorkspaceRequest::Read { path } => {
                if self.tools.check("fs.read", &json!({ "path": path })) != Decision::Allow {
                    return WorkspaceResponse::Error {
                        message: format!("read denied by permission gate: {}", path.display()),
                    };
                }
                match self.workspace.read(&path).await {
                    Ok(bytes) => WorkspaceResponse::Read { bytes },
                    Err(e) => WorkspaceResponse::Error { message: e.to_string() },
                }
            }
            WorkspaceRequest::List { glob } => match self.workspace.list(&glob).await {
                Ok(paths) => WorkspaceResponse::List { paths },
                Err(e) => WorkspaceResponse::Error { message: e.to_string() },
            },
            WorkspaceRequest::ApplyEdit { path, contents } => {
                if self.tools.check("fs.write", &json!({ "path": path })) != Decision::Allow {
                    return WorkspaceResponse::Error {
                        message: format!("write denied by permission gate: {}", path.display()),
                    };
                }
                let edit = Edit {
                    path,
                    new_contents: contents,
                };
                match self.workspace.apply_edit(&edit).await {
                    Ok(bytes_written) => WorkspaceResponse::ApplyEdit { bytes_written },
                    Err(e) => WorkspaceResponse::Error { message: e.to_string() },
                }
            }
            WorkspaceRequest::Snapshot => match self.workspace.snapshot().await {
                Ok(snap) => WorkspaceResponse::Snapshot { files: snap.files },
                Err(e) => WorkspaceResponse::Error { message: e.to_string() },
            },
        }
    }
```

`Decision` derives `PartialEq` (used by `!=`); confirm — it is `#[derive(... PartialEq ...)]` in `tool.rs`. If it is not, compare with a `matches!(..., Decision::Allow)` instead.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine service::tests::workspace_rpc`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "feat(engine): EngineService::workspace_rpc with server-side gating"
```

---

### Task 3: The `/workspace` route + shared bearer auth

**Files:**
- Modify: `crates/engine/src/serve.rs`

- [ ] **Step 1: Factor the bearer check into a helper and add the route + handler**

In `crates/engine/src/serve.rs`:

Add `WorkspaceRequest` to the `otto_protocol` import and `post` to the routing import:

```rust
use axum::routing::{get, post};
use otto_protocol::{Command, Event, SessionId, WorkspaceRequest};
```

Add a shared auth helper (near the top of the module body):

```rust
/// True if `headers` carry `Authorization: Bearer <token>` matching `token`.
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        == Some(token)
}
```

Refactor the existing check in `ws_handler` to use it — replace the `let presented = ...; if presented != Some(state.token.as_str()) { ... }` block with:

```rust
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
```

Add the route in `app()` (after the `/ws` route, before `.with_state`):

```rust
        .route("/workspace", post(workspace_handler))
```

Add the handler (after `ws_handler`). It reads the raw body so auth is checked before parsing:

```rust
async fn workspace_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    let req: WorkspaceRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response(),
    };
    let resp = state.service.workspace_rpc(req).await;
    axum::Json(resp).into_response()
}
```

(`axum::Json` is in scope via the `axum` crate; if not already imported, add `use axum::Json;` and use `Json(resp)`. `serde_json` is a dependency of `otto-engine`.)

- [ ] **Step 2: Verify build + existing serve tests still pass**

Run: `cargo test -p otto-engine --test serve`
Expected: PASS — the `/ws` tests (now using the factored `authorized` helper) are unaffected; the new route compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/serve.rs
git commit -m "feat(serve): POST /workspace route (shared bearer auth)"
```

---

### Task 4: `RemoteWorkspace` client (workspace crate)

**Files:**
- Modify: `crates/workspace/Cargo.toml`
- Create: `crates/workspace/src/remote.rs`
- Modify: `crates/workspace/src/lib.rs`

- [ ] **Step 1: Add the client dependencies**

In `crates/workspace/Cargo.toml`, under `[dependencies]`, add:

```toml
otto-protocol = { path = "../protocol" }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde_json = { workspace = true }
```

- [ ] **Step 2: Create the `RemoteWorkspace` client**

Create `crates/workspace/src/remote.rs`:

```rust
//! `RemoteWorkspace`: a `Workspace` implemented over the bearer-authed `POST /workspace` RPC
//! of a remote engine. Each trait method is one unary request. The server enforces the
//! permission floor and path containment, so this client is a thin proxy.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::{Edit, WorkspaceSnapshot};
use otto_protocol::{WorkspaceRequest, WorkspaceResponse};

/// A workspace backed by a remote engine's `POST {base_url}/workspace` endpoint.
pub struct RemoteWorkspace {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl RemoteWorkspace {
    /// `base_url` is the engine origin (e.g. `http://127.0.0.1:7878` or `https://host:port`);
    /// `token` is the bearer token the server requires.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client: reqwest::Client::new(),
        }
    }

    async fn rpc(&self, req: &WorkspaceRequest) -> anyhow::Result<WorkspaceResponse> {
        let url = format!("{}/workspace", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("workspace rpc failed: HTTP {}", resp.status());
        }
        let parsed: WorkspaceResponse = resp.json().await?;
        if let WorkspaceResponse::Error { message } = &parsed {
            anyhow::bail!("workspace rpc error: {message}");
        }
        Ok(parsed)
    }
}

#[async_trait]
impl WorkspaceRead for RemoteWorkspace {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        match self.rpc(&WorkspaceRequest::Read { path: path.to_path_buf() }).await? {
            WorkspaceResponse::Read { bytes } => Ok(bytes),
            other => anyhow::bail!("unexpected response to Read: {other:?}"),
        }
    }

    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>> {
        match self.rpc(&WorkspaceRequest::List { glob: glob.to_string() }).await? {
            WorkspaceResponse::List { paths } => Ok(paths),
            other => anyhow::bail!("unexpected response to List: {other:?}"),
        }
    }
}

#[async_trait]
impl Workspace for RemoteWorkspace {
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
        match self
            .rpc(&WorkspaceRequest::ApplyEdit {
                path: edit.path.clone(),
                contents: edit.new_contents.clone(),
            })
            .await?
        {
            WorkspaceResponse::ApplyEdit { bytes_written } => Ok(bytes_written),
            other => anyhow::bail!("unexpected response to ApplyEdit: {other:?}"),
        }
    }

    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot> {
        match self.rpc(&WorkspaceRequest::Snapshot).await? {
            WorkspaceResponse::Snapshot { files } => Ok(WorkspaceSnapshot { files }),
            other => anyhow::bail!("unexpected response to Snapshot: {other:?}"),
        }
    }
}
```

- [ ] **Step 3: Register the module**

In `crates/workspace/src/lib.rs`, add near the top (after the existing items / alongside the `LocalWorkspace` definition — place the `mod`/`pub use` at the top of the file):

```rust
mod remote;
pub use remote::RemoteWorkspace;
```

(If `lib.rs` has no module declarations yet because `LocalWorkspace` is defined inline, add these two lines at the very top below the file doc comment.)

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p otto-workspace`
Expected: PASS (pulls `reqwest`; `RemoteWorkspace` compiles, untested until Task 5).

- [ ] **Step 5: Commit**

```bash
git add crates/workspace/Cargo.toml crates/workspace/src/remote.rs crates/workspace/src/lib.rs Cargo.lock
git commit -m "feat(workspace): RemoteWorkspace client over the workspace RPC"
```

---

### Task 5: Loopback integration test (round-trip + gate + auth)

**Files:**
- Create: `crates/engine/tests/remote_workspace.rs`

- [ ] **Step 1: Write the integration test**

Create `crates/engine/tests/remote_workspace.rs`. It stands up the serve app (with `/workspace`) backed by a `LocalWorkspace` tempdir, and drives it through a `RemoteWorkspace` client. (`otto-engine` depends on `otto-workspace`, so `RemoteWorkspace` is usable here.)

```rust
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
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p otto-engine --test remote_workspace`
Expected: PASS (3 tests).

- [ ] **Step 3: Commit**

```bash
git add crates/engine/tests/remote_workspace.rs
git commit -m "test(engine): RemoteWorkspace loopback round-trip + gate + auth"
```

---

### Task 6: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, and test the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: fmt clean (or trivial changes you then include), clippy clean, all tests green — the protocol round-trip, the `workspace_rpc` unit tests, the existing serve tests through the factored auth, the RemoteWorkspace integration tests, and every other crate unchanged.

- [ ] **Step 2: If `cargo fmt` changed anything, commit it**

```bash
git add -A
git commit -m "style: cargo fmt after remote workspace"
```

(If fmt made no changes, skip this commit. Do NOT `git add` any stray build artifacts — only source files fmt touched.)

---

## Done criteria

- `WorkspaceRequest`/`WorkspaceResponse` in `protocol` (serde round-trip).
- `EngineService::workspace_rpc` dispatches read/list/apply_edit/snapshot, routing read+write through the gate (Allow-only; sensitive paths denied); unit-tested.
- `POST /workspace` route shares the bearer-auth helper with `/ws`; auth checked before body parse.
- `RemoteWorkspace` (workspace crate) implements `Workspace` over the RPC; loopback test shows read/list/apply_edit/snapshot parity with the backing `LocalWorkspace`, a sensitive-path write denied over the wire, and a wrong token rejected.
- `cargo test --workspace` green; clippy/fmt clean.

**Next in the remote axis:** `RemoteTarget` + the promote flow (snapshot `SessionState` + `Workspace::snapshot` → provision → reconnect via `Last-Event-ID`), where `RemoteWorkspace` and `WorkspaceSnapshot` get wired into a real handover — and where the genuinely-external VPS provisioner is flagged as manual/integration-only.
