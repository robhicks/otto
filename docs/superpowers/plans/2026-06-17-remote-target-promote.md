# RemoteTarget + Promote Flow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `RemoteTarget` seam + `promote()` orchestration, a `LoopbackTarget` that provisions a real second in-process engine (restore session + workspace, serve on loopback), and an `UnsupportedTarget` that honestly stubs the external cloud provisioner — the remote-axis capstone.

**Architecture:** New `crates/engine/src/remote.rs`. `promote()` snapshots a session (`store.snapshot`) + its workspace (`workspace.snapshot`) into a `PromoteBundle` and calls `target.provision`. `LoopbackTarget::provision` restores the bundle into a fresh `SqliteStore` + `LocalWorkspace` under a caller-provided base dir, builds an `EngineService`, and `serve_run`s it on `127.0.0.1:0`, returning a `RemoteHandle { endpoint, token }` whose private state aborts the serve task on `teardown`. Everything reuses existing primitives; no new deps.

**Tech Stack:** Rust (edition 2024); reuses `persistence`/`workspace`/`serve`/`EngineService`; tests use `tokio-tungstenite` + `RemoteWorkspace` (existing dev/normal deps).

**Spec:** `docs/superpowers/specs/2026-06-17-remote-target-promote-design.md`.

---

### Task 1: Seam + `UnsupportedTarget` + `promote()`

**Files:**
- Create: `crates/engine/src/remote.rs`
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Create the module with the failing test**

Create `crates/engine/src/remote.rs`:

```rust
//! Promote a session to another engine. `promote()` snapshots a session + its workspace into a
//! `PromoteBundle` and hands it to a `RemoteTarget`. `LoopbackTarget` provisions a real second
//! in-process engine (testable on loopback); `UnsupportedTarget` honestly refuses, marking the
//! boundary where a real VPS provisioner (external infra, manual-only) would go.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::Router;
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStore};
use otto_protocol::SessionId;
use otto_workspace::LocalWorkspace;

use crate::service::EngineService;

/// A captured session ready to move to another engine: persisted session state + workspace files.
pub struct PromoteBundle {
    pub session: SessionState,
    pub workspace: WorkspaceSnapshot,
}

/// A reachable, provisioned remote engine. `endpoint` is a `ws://host:port` base; `token` is the
/// bearer token it requires. Impl-private shutdown state tears it down on `teardown`.
pub struct RemoteHandle {
    pub endpoint: String,
    pub token: String,
    shutdown: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
pub trait RemoteTarget: Send + Sync {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle>;
    async fn teardown(&self, handle: RemoteHandle) -> anyhow::Result<()>;
}

/// Snapshot `session` and its `workspace` and provision the result onto `target`. Does not stop
/// the source engine — handover (drop local, reconnect remote) is a client concern.
pub async fn promote(
    store: &dyn SessionStore,
    workspace: &dyn Workspace,
    session: SessionId,
    target: &dyn RemoteTarget,
) -> anyhow::Result<RemoteHandle> {
    let bundle = PromoteBundle {
        session: store.snapshot(session).await?,
        workspace: workspace.snapshot().await?,
    };
    target.provision(&bundle).await
}

/// A `RemoteTarget` that refuses to provision: a real cloud/VPS provisioner needs external
/// infrastructure (a machine, SSH, a deployed engine) and cannot run in-tree or in CI.
pub struct UnsupportedTarget;

#[async_trait]
impl RemoteTarget for UnsupportedTarget {
    async fn provision(&self, _bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle> {
        anyhow::bail!("real VPS provisioning requires external infrastructure; not available in-tree")
    }
    async fn teardown(&self, _handle: RemoteHandle) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SessionStatus;

    #[tokio::test]
    async fn unsupported_target_refuses_to_provision() {
        let bundle = PromoteBundle {
            session: SessionState {
                id: SessionId::new(),
                goal: "g".to_string(),
                status: SessionStatus::Active,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot { files: vec![] },
        };
        assert!(UnsupportedTarget.provision(&bundle).await.is_err());
    }
}
```

Note: `PathBuf`, `Arc`, `Router`, `LocalWorkspace`, `EngineService` are imported for `LoopbackTarget` in Task 2; they are unused in Task 1. If an unused-import warning blocks the build before Task 2, it is resolved there — do not delete them.

- [ ] **Step 2: Register the module**

In `crates/engine/src/lib.rs`, add (alongside the other `mod`/`pub use` lines):

```rust
mod remote;

pub use remote::{PromoteBundle, RemoteHandle, RemoteTarget, UnsupportedTarget, promote};
```

(`LoopbackTarget` is added to this re-export in Task 2.)

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p otto-engine remote::tests::unsupported_target_refuses_to_provision`
Expected: PASS. (The crate compiles; Task-2 imports are unused-but-present — if `-D warnings` is on and this fails to build on the unused imports, add a temporary `#[allow(unused_imports)]` on the `use` block and remove it in Task 2. Prefer just proceeding to Task 2.)

- [ ] **Step 4: Commit**

```bash
git add crates/engine/src/remote.rs crates/engine/src/lib.rs
git commit -m "feat(engine): RemoteTarget seam + promote() + UnsupportedTarget"
```

---

### Task 2: `LoopbackTarget` + the capstone integration test

**Files:**
- Create: `crates/engine/tests/promote.rs`
- Modify: `crates/engine/src/remote.rs`
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Write the failing capstone test**

Create `crates/engine/tests/promote.rs`:

```rust
//! Capstone: run a turn on a source engine, promote the session to a LoopbackTarget (a real
//! second in-process engine), then reconnect a WS client to the provisioned remote and confirm
//! the session resumed (same id, replayed event gap) and the workspace transferred. Loopback only.

use std::path::Path;
use std::sync::Arc;

use futures_util::StreamExt;
use otto_engine::{
    CollectingSink, EngineService, LoopbackTarget, RemoteTarget, build_default_registry,
    build_tool_registry, promote, serve_run,
};
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_persistence::{SessionStore, SqliteStore};
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::{LocalWorkspace, RemoteWorkspace};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "promote-token";

async fn next_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(_)) => return None,
        }
    }
}

#[tokio::test]
async fn promote_resumes_session_and_workspace_on_a_loopback_remote() {
    // --- Source engine: run a turn that writes a file. ---
    let src_dir = tempfile::tempdir().unwrap();
    let promote_base = tempfile::tempdir().unwrap();

    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "PROMOTED"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(src_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(src_dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, src_dir.path().to_path_buf()));
    let store: Arc<dyn SessionStore> =
        Arc::new(SqliteStore::open(src_dir.path().join("a.db")).await.unwrap());
    let service = EngineService::new(
        store.clone(),
        Arc::new(build_default_registry()),
        router,
        workspace.clone(),
        tools,
    );

    let session = service.create_session("g", &serde_json::json!({})).await.unwrap();
    let mut sink = CollectingSink::default();
    service.run_prompt(session, "add a greeting", &mut sink).await.unwrap();
    let src_events = store.replay_since(session, None).await.unwrap();
    assert!(src_events.len() >= 2, "the source turn should emit several events");
    let last_seq = src_events.last().unwrap().seq;

    // --- Promote to a loopback remote. ---
    let target = LoopbackTarget::new(TOKEN, promote_base.path().to_path_buf());
    let handle = promote(&*store, &*workspace, session, &target).await.unwrap();

    // --- Reconnect to the remote: same session, replayed gap after seq 0. ---
    let url = format!("{}/ws?session={}&last_seq=0", handle.endpoint, session.0);
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.expect("connect to remote");

    let ready = next_json(&mut ws).await.expect("ready frame");
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
    // The gap after seq 0 is every source event with seq > 0.
    let expected: Vec<u64> = src_events.iter().map(|e| e.seq).filter(|s| *s > 0).collect();
    assert_eq!(replayed, expected);
    drop(ws);

    // --- The workspace transferred: read the promoted file via the remote's /workspace RPC. ---
    let http_base = handle.endpoint.replace("ws://", "http://");
    let remote_ws = RemoteWorkspace::new(http_base, TOKEN);
    assert_eq!(remote_ws.read(Path::new("out.txt")).await.unwrap(), b"PROMOTED");

    // --- Teardown stops the remote: a subsequent connect fails. ---
    target.teardown(handle).await.unwrap();
    // Give the aborted server a moment to release the port, then confirm it's down.
    let url2 = format!("ws://127.0.0.1:{}/ws", port_of(&remote_endpoint_after_teardown()));
    let _ = url2; // see note below
}

// Helper stubs referenced above are intentionally simple; see Step note about the teardown check.
fn remote_endpoint_after_teardown() -> String { String::new() }
fn port_of(_s: &str) -> u16 { 0 }
```

The teardown-connect-fails check needs the endpoint *before* `teardown` consumes the handle. Replace the messy tail (everything from `// --- Teardown` to the end of the test) with this clean version that captures the endpoint first:

```rust
    // --- Teardown stops the remote: a subsequent connect fails. ---
    let endpoint = handle.endpoint.clone();
    target.teardown(handle).await.unwrap();
    let down_url = format!("{endpoint}/ws");
    let mut down_req = down_url.into_client_request().unwrap();
    down_req
        .headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(down_req).await.is_err(),
        "the remote must be unreachable after teardown"
    );
}
```

(Delete the `remote_endpoint_after_teardown`/`port_of` stub helpers — they were only placeholders to make the structure clear; the clean tail above replaces them.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine --test promote`
Expected: FAIL to compile — `LoopbackTarget` is not defined / not exported.

- [ ] **Step 3: Implement `LoopbackTarget`**

In `crates/engine/src/remote.rs`, add (after `UnsupportedTarget`):

```rust
/// Provisions a real second engine in-process: restores the bundle into a fresh sqlite store +
/// workspace under `base_dir` (one subdir per session id) and serves it on `127.0.0.1:0`.
pub struct LoopbackTarget {
    token: String,
    base_dir: PathBuf,
}

impl LoopbackTarget {
    /// `token` is the bearer token the provisioned remote will require; `base_dir` is where the
    /// restored store + workspace are written (the caller owns its lifetime, e.g. a tempdir).
    pub fn new(token: impl Into<String>, base_dir: PathBuf) -> Self {
        Self {
            token: token.into(),
            base_dir,
        }
    }
}

#[async_trait]
impl RemoteTarget for LoopbackTarget {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle> {
        // One isolated directory per provisioned session.
        let dir = self.base_dir.join(bundle.session.id.0.to_string());
        let ws_dir = dir.join("workspace");
        tokio::fs::create_dir_all(&ws_dir).await?;

        // Restore the session into a fresh store.
        let store = SqliteStore::open(dir.join("sessions.db")).await?;
        store.restore(&bundle.session).await?;

        // Restore the workspace files.
        LocalWorkspace::new(&ws_dir).restore(&bundle.workspace).await?;

        // Build the remote engine over the restored state.
        let store: Arc<dyn SessionStore> = Arc::new(store);
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_dir));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_dir));
        let tools = Arc::new(build_tool_registry(tools_ws, ws_dir.clone()));
        let router: Arc<dyn Router> = Arc::from(build_router());
        let service = EngineService::new(
            store,
            Arc::new(build_default_registry()),
            router,
            workspace,
            tools,
        );

        // Serve it on a loopback ephemeral port.
        let app = crate::serve::app(service, self.token.clone());
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let task = tokio::spawn(async move {
            let _ = crate::serve::run(listener, app, None).await;
        });

        Ok(RemoteHandle {
            endpoint: format!("ws://127.0.0.1:{port}"),
            token: self.token.clone(),
            shutdown: Some(task),
        })
    }

    async fn teardown(&self, mut handle: RemoteHandle) -> anyhow::Result<()> {
        if let Some(task) = handle.shutdown.take() {
            task.abort();
        }
        Ok(())
    }
}
```

Add the needed imports to the top of `remote.rs` (most are already present from Task 1; add what is missing):

```rust
use otto_persistence::SqliteStore;
use crate::{build_default_registry, build_router, build_tool_registry};
```

(`SessionStore`, `Workspace`, `Router`, `LocalWorkspace`, `EngineService`, `Arc`, `PathBuf` are already imported.)

- [ ] **Step 4: Export `LoopbackTarget`**

In `crates/engine/src/lib.rs`, extend the remote re-export:

```rust
pub use remote::{LoopbackTarget, PromoteBundle, RemoteHandle, RemoteTarget, UnsupportedTarget, promote};
```

- [ ] **Step 5: Run the capstone test to verify it passes**

Run: `cargo test -p otto-engine --test promote`
Expected: PASS — the session resumes on the loopback remote (same id, replayed gap), the workspace transferred (`out.txt` == `PROMOTED` via the remote RPC), and teardown makes it unreachable.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/remote.rs crates/engine/src/lib.rs crates/engine/tests/promote.rs
git commit -m "feat(engine): LoopbackTarget + promote capstone integration test"
```

---

### Task 3: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, and test the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: fmt clean (or trivial changes you then include), clippy clean, all tests green — the `remote` unit test, the promote capstone, and every other crate unchanged.

- [ ] **Step 2: If `cargo fmt` changed anything, commit it**

```bash
git add -A
git commit -m "style: cargo fmt after remote target"
```

(If fmt made no changes, skip. Do NOT `git add` any stray build artifact — only source files fmt touched.)

---

## Done criteria

- `RemoteTarget` trait + `PromoteBundle`/`RemoteHandle` + `promote()` glue in `crates/engine/src/remote.rs`.
- `UnsupportedTarget::provision` errors clearly (the real-VPS boundary); `LoopbackTarget` provisions a real second in-process engine (restore session + workspace, serve loopback) and `teardown` stops it.
- Capstone test: a promoted session resumes on the loopback remote with the same id and the replayed event gap, the workspace transferred (file readable via the remote `/workspace` RPC), and the remote is unreachable after teardown.
- `cargo test --workspace` green; clippy/fmt clean.

**Remote axis complete after this plan.** Remaining (out of scope, manual/integration-only): a real `vps`/`microvm` provisioner behind the `RemoteTarget` seam, the client-side handover UX, and splitting a dedicated `remote` crate.
