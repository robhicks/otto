# `remote` crate split-out Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the engine-axis remote seam (`RemoteTarget`, `RemoteHandle`, `PromoteBundle`, `promote()`, `VpsTarget`, `UnsupportedTarget`, `PromoteConfig`/`PromoteMode`) from `crates/engine/src/remote.rs` into a new `otto-remote` crate, leaving the engine-booting `LoopbackTarget` in `engine`, with no behavior change.

**Architecture:** `otto-remote` is a new crate whose dependencies flow strictly inward (`protocol`, `engine-core`, `persistence`). `LoopbackTarget` stays in `engine` (it constructs an `EngineService` and serves it, so it must depend on `engine`), implementing `otto_remote::RemoteTarget` — keeping the dependency one-directional (`engine → otto-remote`). `RemoteHandle` gains public constructors (`new`/`with_task`/`abort`) so the engine-side `LoopbackTarget` can build it across the crate boundary. `otto_engine` re-exports every moved name, so the integration tests and `main.rs` need zero changes.

**Tech Stack:** Rust 2024, cargo workspace, `async-trait`, `tokio`, `reqwest`, `serde`. See `docs/superpowers/specs/2026-06-22-remote-crate-split-design.md`.

---

### Task 1: Scaffold the empty `otto-remote` crate

**Files:**
- Create: `crates/remote/Cargo.toml`
- Create: `crates/remote/src/lib.rs`
- Modify: `Cargo.toml` (root workspace members)

- [ ] **Step 1: Create the crate manifest**

Create `crates/remote/Cargo.toml`:

```toml
[package]
name = "otto-remote"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-protocol = { path = "../protocol" }
otto-engine-core = { path = "../engine-core" }
otto-persistence = { path = "../persistence" }
anyhow.workspace = true
async-trait.workspace = true
serde = { workspace = true }
serde_json.workspace = true
tokio = { workspace = true }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

- [ ] **Step 2: Create a placeholder lib**

Create `crates/remote/src/lib.rs`:

```rust
//! The engine-axis remote seam, split out of `otto-engine`.
```

- [ ] **Step 3: Register the crate in the workspace**

In root `Cargo.toml`, add `"crates/remote",` to `members` (immediately after `"crates/engine",`):

```toml
    "crates/engine",
    "crates/remote",
    "crates/persistence",
```

- [ ] **Step 4: Verify the workspace still builds**

Run: `cargo build --workspace`
Expected: PASS (new empty crate compiles; nothing else changed).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/remote/
git commit -m "build(remote): scaffold empty otto-remote crate"
```

---

### Task 2: Move the seam + network targets into `otto-remote`

This fills `otto-remote` with the target-agnostic seam and the two network-facing targets, copied
from `crates/engine/src/remote.rs` with `RemoteHandle` gaining public constructors and `VpsTarget`
using `RemoteHandle::new`. `engine` is untouched in this task (it keeps its own `remote.rs`), so the
workspace keeps compiling with the types defined in two places temporarily.

**Files:**
- Modify: `crates/remote/src/lib.rs`

- [ ] **Step 1: Write the full crate body**

Replace the entire contents of `crates/remote/src/lib.rs` with:

```rust
//! The engine-axis remote seam, split out of `otto-engine`. `promote()` snapshots a session + its
//! workspace into a `PromoteBundle` and hands it to a `RemoteTarget`. `VpsTarget` pushes the bundle
//! to an already-running `otto serve --accept-promotions`; `UnsupportedTarget` honestly refuses,
//! marking the machine-provisioning boundary. The in-process `LoopbackTarget` lives in `otto-engine`
//! (it boots an engine), implementing this crate's `RemoteTarget`.

use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStore};
use otto_protocol::SessionId;

/// Enables session handover on a served engine. `token` is the bearer the target requires (reused
/// from the source, by design); `mode` selects which `RemoteTarget` a handover provisions onto.
/// `ServeState` holds this as `Option`: `Some` ⟺ `--promote-loopback` or `--promote-vps`.
#[derive(Clone)]
pub struct PromoteConfig {
    pub token: String,
    pub mode: PromoteMode,
}

/// Which kind of remote a promote provisions onto.
#[derive(Clone)]
pub enum PromoteMode {
    /// Provision a fresh in-process engine, restoring under `base_dir` (loopback round-trip).
    Loopback { base_dir: PathBuf },
    /// Push to an already-running remote `otto serve` at `endpoint` (`ws://…` / `wss://…`).
    Vps { endpoint: String },
}

/// A captured session ready to move to another engine: persisted session state + workspace files.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PromoteBundle {
    pub session: SessionState,
    pub workspace: WorkspaceSnapshot,
}

/// A reachable, provisioned remote engine. `endpoint` is a `ws://host:port` base; `token` is the
/// bearer token it requires. An impl-private `shutdown` task tears down an in-process engine on
/// `teardown`/drop (set via `with_task`); network targets that own no task use `new`.
pub struct RemoteHandle {
    pub endpoint: String,
    pub token: String,
    shutdown: Option<tokio::task::JoinHandle<()>>,
}

impl RemoteHandle {
    /// A handle to a remote this process does not own (e.g. `VpsTarget`): nothing to abort.
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            shutdown: None,
        }
    }

    /// A handle to an in-process provisioned engine (`LoopbackTarget`): `task` is aborted on
    /// `teardown`/drop.
    pub fn with_task(
        endpoint: impl Into<String>,
        token: impl Into<String>,
        task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            shutdown: Some(task),
        }
    }

    /// Abort the backing task if any (idempotent). `Drop` also calls this.
    pub fn abort(&mut self) {
        if let Some(task) = self.shutdown.take() {
            task.abort();
        }
    }
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        self.abort();
    }
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
        anyhow::bail!(
            "real VPS provisioning requires external infrastructure; not available in-tree"
        )
    }
    async fn teardown(&self, _handle: RemoteHandle) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A `RemoteTarget` that promotes onto an already-running, operator-managed `otto serve` over the
/// network. Unlike `LoopbackTarget`, it does not create or own the receiver — `teardown` is a
/// no-op so it never aborts the operator's long-lived server.
pub struct VpsTarget {
    /// `ws://host:port` or `wss://host:port` — what the client reconnects to after promote.
    endpoint: String,
    token: String,
    client: reqwest::Client,
}

impl VpsTarget {
    /// `endpoint` is the receiver's `ws://`/`wss://` base; `token` is its bearer (reused from the
    /// source, by design — source and receiver share a trust domain in v1).
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("build reqwest client"),
        }
    }

    /// Map the ws endpoint to the HTTP base for the `/promote` POST (`ws→http`, `wss→https`).
    fn http_base(&self) -> String {
        if let Some(rest) = self.endpoint.strip_prefix("wss://") {
            format!("https://{rest}")
        } else if let Some(rest) = self.endpoint.strip_prefix("ws://") {
            format!("http://{rest}")
        } else {
            self.endpoint.clone()
        }
    }

    /// Pull a session's `PromoteBundle` back from the receiver (the demote primitive). POSTs the
    /// session id to `/export`; surfaces the receiver's status + body on a non-2xx, symmetric with
    /// how `provision` reports a rejected push.
    pub async fn export(&self, session: SessionId) -> anyhow::Result<PromoteBundle> {
        let url = format!("{}/export", self.http_base());
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "session": session.0.to_string() }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("export rejected by remote: HTTP {status}: {body}");
        }
        Ok(resp.json().await?)
    }
}

#[async_trait]
impl RemoteTarget for VpsTarget {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle> {
        let url = format!("{}/promote", self.http_base());
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(bundle)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            // Surface the receiver's reason (e.g. "restore refused sensitive path: …",
            // "session already exists") instead of a bare status, for operator diagnostics.
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("promote rejected by remote: HTTP {status}: {body}");
        }
        Ok(RemoteHandle::new(self.endpoint.clone(), self.token.clone()))
    }

    async fn teardown(&self, _handle: RemoteHandle) -> anyhow::Result<()> {
        // No-op: VpsTarget does not own the operator's server, so it must never abort it.
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

    #[test]
    fn vps_http_base_maps_ws_schemes() {
        // wss → https (the production path; otherwise the promote POST silently downgrades to
        // plaintext), ws → http (loopback), and an unrecognized scheme passes through verbatim.
        assert_eq!(
            VpsTarget::new("wss://host:9000", "t").http_base(),
            "https://host:9000"
        );
        assert_eq!(
            VpsTarget::new("ws://127.0.0.1:7878", "t").http_base(),
            "http://127.0.0.1:7878"
        );
        assert_eq!(
            VpsTarget::new("http://host:1", "t").http_base(),
            "http://host:1"
        );
    }
}
```

- [ ] **Step 2: Run the new crate's tests to verify they pass**

Run: `cargo test -p otto-remote`
Expected: PASS — both `unsupported_target_refuses_to_provision` and `vps_http_base_maps_ws_schemes`.

- [ ] **Step 3: Verify the whole workspace still builds**

Run: `cargo build --workspace`
Expected: PASS. `engine` still defines its own copy of these types in `remote.rs` and does not yet
depend on `otto-remote`, so there is no conflict.

- [ ] **Step 4: Commit**

```bash
git add crates/remote/src/lib.rs
git commit -m "feat(remote): move RemoteTarget seam + Vps/Unsupported targets into otto-remote"
```

---

### Task 3: Re-home `LoopbackTarget` in `engine` and rewire onto `otto-remote`

This deletes `crates/engine/src/remote.rs`, adds the `otto-remote` dependency, moves
`LoopbackTarget` into a new `crates/engine/src/loopback.rs`, and repoints every internal
`crate::remote::…` reference at `otto_remote::…`. The `otto_engine` public re-exports are preserved,
so `main.rs` and the integration tests (`promote.rs`, `vps_promote.rs`, `serve.rs`) are untouched.

**Files:**
- Modify: `crates/engine/Cargo.toml`
- Create: `crates/engine/src/loopback.rs`
- Delete: `crates/engine/src/remote.rs`
- Modify: `crates/engine/src/lib.rs:23` and `:32-35`
- Modify: `crates/engine/src/serve.rs:33` (+ inline `crate::remote::` refs)
- Modify: `crates/engine/src/service.rs` (inline `crate::remote::PromoteBundle` refs)

- [ ] **Step 1: Add the `otto-remote` dependency to `engine`**

In `crates/engine/Cargo.toml`, under `[dependencies]`, add the path dep next to the other otto
crates (e.g. after `otto-persistence`):

```toml
otto-persistence = { path = "../persistence" }
otto-remote = { path = "../remote" }
```

Leave `reqwest` in `[dependencies]` as-is — `vps_promote.rs` tests still use `reqwest::Response`.

- [ ] **Step 2: Create `crates/engine/src/loopback.rs`**

```rust
//! `LoopbackTarget` — an in-process `RemoteTarget` that boots a real second engine. It lives in
//! `otto-engine` (not `otto-remote`) because it constructs an `EngineService` and serves it; keeping
//! it here makes the crate dependency one-directional (`engine → otto-remote`, never the reverse).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::Router;
use otto_engine_core::traits::Workspace;
use otto_persistence::{SessionStore, SqliteStore};
use otto_remote::{PromoteBundle, PromoteConfig, PromoteMode, RemoteHandle, RemoteTarget};
use otto_workspace::LocalWorkspace;

use crate::service::EngineService;
use crate::{build_default_registry, build_router, build_tool_registry};

/// Provisions a real second engine in-process: restores the bundle into a fresh sqlite store +
/// workspace under `base_dir` (one subdir per session id) and serves it on `127.0.0.1:0`.
pub struct LoopbackTarget {
    token: String,
    base_dir: PathBuf,
    /// The `engine_remote` capability the provisioned engine reports: `true` for promote
    /// (it's now "remote"), `false` for demote (back to "local").
    engine_remote: bool,
}

impl LoopbackTarget {
    /// `token` is the bearer the provisioned remote requires; `base_dir` is where the restored
    /// store + workspace are written; `engine_remote` is the capability flag it reports.
    pub fn new(token: impl Into<String>, base_dir: PathBuf, engine_remote: bool) -> Self {
        Self {
            token: token.into(),
            base_dir,
            engine_remote,
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
        LocalWorkspace::new(&ws_dir)
            .restore(&bundle.workspace)
            .await?;

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

        // This provisioned engine reports the configured capability and is itself promote-capable
        // (so the round-trip — demote, re-promote — works), rooted at a nested base dir.
        let capabilities = otto_protocol::CapabilitiesManifest {
            engine_remote: self.engine_remote,
            ..crate::build_capabilities()
        };
        let promote = Some(PromoteConfig {
            token: self.token.clone(),
            mode: PromoteMode::Loopback {
                base_dir: dir.join("promote"),
            },
        });
        let app = crate::serve::app(service, self.token.clone(), capabilities, promote, false);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let task = tokio::spawn(async move {
            let _ = crate::serve::run(listener, app, None).await;
        });

        Ok(RemoteHandle::with_task(
            format!("ws://127.0.0.1:{port}"),
            self.token.clone(),
            task,
        ))
    }

    async fn teardown(&self, mut handle: RemoteHandle) -> anyhow::Result<()> {
        handle.abort();
        Ok(())
    }
}
```

- [ ] **Step 3: Delete the old `remote.rs`**

Run: `git rm crates/engine/src/remote.rs`

- [ ] **Step 4: Rewire the module + re-exports in `lib.rs`**

In `crates/engine/src/lib.rs`, change the module declaration on line 23 from `mod remote;` to:

```rust
mod loopback;
```

(Keep alphabetical-ish ordering tidy; the existing list is `approval, mcp, loopback, serve, service` — exact order doesn't matter to the compiler.)

Then replace the re-export block at lines 32-35:

```rust
pub use remote::{
    LoopbackTarget, PromoteBundle, PromoteConfig, PromoteMode, RemoteHandle, RemoteTarget,
    UnsupportedTarget, VpsTarget, promote,
};
```

with:

```rust
pub use loopback::LoopbackTarget;
pub use otto_remote::{
    PromoteBundle, PromoteConfig, PromoteMode, RemoteHandle, RemoteTarget, UnsupportedTarget,
    VpsTarget, promote,
};
```

- [ ] **Step 5: Repoint `serve.rs`**

In `crates/engine/src/serve.rs`, replace the import on line 33:

```rust
use crate::remote::{LoopbackTarget, PromoteConfig, RemoteHandle, promote};
```

with:

```rust
use crate::loopback::LoopbackTarget;
use otto_remote::{PromoteConfig, RemoteHandle, promote};
```

Then replace every remaining inline `crate::remote::` with `otto_remote::` in this file. There are
six such references (the `PromoteBundle` deserialize, the `PromoteMode::Vps`/`PromoteMode::Loopback`
matches, the `VpsTarget::new` calls, and the `Box<dyn ... RemoteTarget>`):

```bash
sed -i 's/crate::remote::/otto_remote::/g' crates/engine/src/serve.rs
```

(The `LoopbackTarget` references in `serve.rs` are via the `use crate::loopback::LoopbackTarget;`
import — unqualified `LoopbackTarget::new` — so the `sed` does not touch them.)

- [ ] **Step 6: Repoint `service.rs`**

`service.rs` references `crate::remote::PromoteBundle` only. Replace them all:

```bash
sed -i 's/crate::remote::/otto_remote::/g' crates/engine/src/service.rs
```

- [ ] **Step 7: Verify the workspace builds**

Run: `cargo build --workspace`
Expected: PASS. If the compiler reports a leftover `crate::remote::` path or a missing
`LoopbackTarget` import, fix the named site and re-run.

- [ ] **Step 8: Run the full test suite**

Run: `cargo test --workspace`
Expected: PASS — in particular `promote.rs` (loopback round-trip), `vps_promote.rs` (vps
promote/demote), and `serve.rs` integration tests, all unchanged and green.

- [ ] **Step 9: Commit**

```bash
git add crates/engine/
git commit -m "refactor(engine): consume otto-remote; keep LoopbackTarget in engine"
```

---

### Task 4: Update docs + final lint/format gate

**Files:**
- Modify: `CLAUDE.md` (crate table)
- Modify: `docs/ARCHITECTURE.md` (`remote` crate description)

- [ ] **Step 1: Add the `remote` crate row to `CLAUDE.md`**

In the crate table in `CLAUDE.md`, add a new row after the `persistence` row (and before
`engine`):

```markdown
| `remote` | The engine-axis handover seam: `RemoteTarget` (trait) + `RemoteHandle` + `PromoteBundle` + `promote()`, the network-facing `VpsTarget` (promote via `POST /promote`; `export` pulls a bundle back for demote) and `UnsupportedTarget` (marks the machine-provisioning boundary), and the `PromoteConfig`/`PromoteMode` handover config. Dependencies flow strictly inward (`protocol`, `engine-core`, `persistence`). `LoopbackTarget` is **not** here — it boots an in-process engine, so it stays in `engine`. |
```

- [ ] **Step 2: Update the `engine` row's `remote.rs` description in `CLAUDE.md`**

In the `engine` crate row, replace the sentence describing `remote.rs`:

```
`remote.rs` is the `RemoteTarget` seam + `promote()` + a `LoopbackTarget` (provisions a real second in-process engine) + a `VpsTarget` (promotes over the wire to a running `otto serve --accept-promotions`, and `VpsTarget::export` pulls a session bundle back from the receiver for demote; `PromoteConfig` selects the target via `PromoteMode::{Loopback,Vps}`) + `UnsupportedTarget` (now marks only the machine-provisioning boundary).
```

with:

```
The `RemoteTarget` seam, `promote()`, `VpsTarget`/`UnsupportedTarget`, and `PromoteConfig`/`PromoteMode` now live in the `remote` crate; `engine` keeps `loopback.rs` — the `LoopbackTarget` (provisions a real second in-process engine), which implements `otto_remote::RemoteTarget` and is re-exported from `otto_engine` alongside the rest of the seam.
```

- [ ] **Step 3: Update `docs/ARCHITECTURE.md`**

The crate-tree line (`docs/ARCHITECTURE.md:37`) already reads
`│   ├── remote           # RemoteTarget impls: vps (v1-ready), microvm (v2).` — update it to note
loopback's home:

```
│   ├── remote           # RemoteTarget seam + vps (shipped) / microvm (v2). LoopbackTarget stays in engine.
```

- [ ] **Step 4: Format**

Run: `cargo fmt --all`
Expected: no diff beyond any incidental reformatting of the moved code; re-stage if it touches files.

- [ ] **Step 5: Lint**

Run: `cargo clippy --workspace --all-targets`
Expected: PASS with no new warnings. (If clippy flags `reqwest` as an unused dependency of
`engine`, that is a false positive — the tests use it — but clippy does not error on unused deps, so
no action is needed.)

- [ ] **Step 6: Final full test run**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md
git commit -m "docs: record remote crate split-out (LoopbackTarget stays in engine)"
```

---

## Spec coverage check

- New `otto-remote` crate with inward-only deps — Tasks 1–2.
- Seam + `VpsTarget`/`UnsupportedTarget` + config + `promote()` moved — Task 2.
- `RemoteHandle` public constructors (`new`/`with_task`/`abort`) — Task 2 (used cross-crate in Task 3).
- `LoopbackTarget` stays in `engine`, implements `otto_remote::RemoteTarget` — Task 3.
- One-directional dependency (`engine → otto-remote`) — Task 3 (no `otto-remote` import of `otto-engine`).
- Backward-compatible `otto_engine` re-exports; tests + `main.rs` unchanged — Task 3 Steps 4–8.
- Moved unit tests stay green; full suite green — Tasks 2–4.
- Docs (`CLAUDE.md`, `ARCHITECTURE.md`) updated — Task 4.
- No behavior change; fmt + clippy clean — Task 4.
