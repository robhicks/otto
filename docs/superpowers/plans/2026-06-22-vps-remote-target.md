# vps RemoteTarget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a real `VpsTarget` that promotes a session onto an already-running, bearer-authed remote `otto serve` over the network, restoring it there so a client can reconnect via `Last-Event-ID`.

**Architecture:** The receiver `otto serve --accept-promotions` exposes a new bearer-authed `POST /promote` RPC that restores a serialized `PromoteBundle` into its store + workspace. The sender `otto serve --promote-vps <ws-endpoint>` makes a UI's `PromoteToRemote` push to that endpoint via a new `VpsTarget` (reqwest client) instead of looping back. Target choice is generalized with a `PromoteMode { Loopback | Vps }` enum on `PromoteConfig`. Both flags are opt-in and fail-closed; the default offline determinism suite is untouched.

**Tech Stack:** Rust (edition 2024), axum 0.8, reqwest 0.12 (rustls), async-trait, serde/serde_json, sqlx (sqlite), tokio. Tests use `tokio_tungstenite` (WS client) and in-process `127.0.0.1:0` servers — no external infra.

---

## Spec deviation (read first — important)

The approved design spec (`docs/superpowers/specs/2026-06-20-vps-remote-target-design.md`) component **#1** says: promote `LocalWorkspace::restore` onto the `Workspace` trait, and relies on the claim that *"it still writes through the gated `apply_edit`, so the inviolable sensitive-path floor still applies."*

**That claim is factually wrong.** `LocalWorkspace::apply_edit` (`crates/workspace/src/lib.rs:139`) only does path containment (`contain()`); it does **not** enforce the sensitive-path floor. The floor lives in `DefaultPermissionGate` / `ToolRegistry` (`crates/tools/src/gate.rs`), applied by *callers* (the orchestrator's gated-edit rule and `EngineService::workspace_rpc`'s `ApplyEdit` branch). A naive trait-`restore` would happily write a `.env` entry from a malicious bundle — failing the spec's own security test (#2, ".env entry refused").

**Resolution (this plan):** Do **not** modify the `Workspace` trait. Instead, `EngineService::accept_promotion` performs a **gated** restore using the `ToolRegistry` the service already holds — checking each snapshot entry with `tools.check("fs.write", …) == Deny` (exactly mirroring `workspace_rpc`'s `ApplyEdit` Deny check) before writing. This:
- makes the sensitive-floor refusal honest and real (reuses the one security spine),
- needs **no** changes to `crates/engine-core/src/traits.rs`, `crates/workspace/src/lib.rs`, or `crates/workspace/src/remote.rs` (the spec's "files touched" list shrinks),
- keeps `LocalWorkspace::restore` as the existing inherent method (still used by `LoopbackTarget`, unchanged).

Everything else in the spec is implemented as written.

---

## File structure

| File | Change |
|---|---|
| `crates/engine/Cargo.toml` | Add `reqwest` dependency (for `VpsTarget`). |
| `crates/engine/src/remote.rs` | `PromoteConfig` gains `mode: PromoteMode`; new `PromoteMode` enum; `PromoteBundle` gains serde derives; new `VpsTarget`; `LoopbackTarget::provision` builds a `Loopback`-mode nested config. |
| `crates/engine/src/service.rs` | New `EngineService::accept_promotion` + `AcceptError` enum (gated workspace restore + store restore). |
| `crates/engine/src/serve.rs` | `ServeState.accept_promotions`; `app()` gains `accept_promotions: bool`; new `POST /promote` route + `promote_handler`; `handle_handover` selects target by `PromoteMode`. |
| `crates/engine/src/lib.rs` | Re-export `PromoteMode` (and `AcceptError` if needed by tests). |
| `crates/engine/src/main.rs` | CLI: `--accept-promotions`, `--promote-vps <endpoint>`, mutual exclusion with `--promote-loopback`; build `PromoteConfig` with `mode`; pass `accept_promotions` to `serve_app`. |
| `crates/engine/tests/serve.rs`, `tests/cors.rs`, `tests/remote_workspace.rs` | Update `serve_app(…)` call sites for the new `accept_promotions` param; update the one `PromoteConfig` construction. |
| `crates/engine/tests/vps_promote.rs` | New: end-to-end VpsTarget round-trip + handler unit tests + teardown-no-op + handover-mode tests. |
| `docs/ARCHITECTURE.md` + a roadmap/spec note | Record `vps` shipped; shrink the `UnsupportedTarget` boundary note. |

---

### Task 1: Refactor `PromoteConfig` → `PromoteMode`; derive serde on `PromoteBundle`

**Files:**
- Modify: `crates/engine/src/remote.rs:23-33` (PromoteConfig, PromoteBundle), `:147-150` (LoopbackTarget nested config)
- Modify: `crates/engine/src/serve.rs:516` (handle_handover target build)
- Modify: `crates/engine/src/main.rs:228-238` (CLI config build)
- Modify: `crates/engine/src/lib.rs:32-35` (re-export PromoteMode)
- Modify: `crates/engine/tests/serve.rs:704-707` (test PromoteConfig)

This is a mechanical type refactor. There is no new behavior, so the existing tests (`tests/promote.rs`, `tests/serve.rs`) are the regression check — they must stay green.

- [ ] **Step 1: Change `PromoteConfig` and add `PromoteMode` in `remote.rs`**

Replace the current `PromoteConfig` struct (lines 20-27) with:

```rust
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
```

- [ ] **Step 2: Add serde derives to `PromoteBundle` in `remote.rs`**

The `/promote` RPC sends this over the wire. Both fields already derive serde. Change the struct (lines 29-33) to:

```rust
/// A captured session ready to move to another engine: persisted session state + workspace files.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PromoteBundle {
    pub session: SessionState,
    pub workspace: WorkspaceSnapshot,
}
```

- [ ] **Step 3: Update `LoopbackTarget::provision`'s nested config in `remote.rs`**

At lines 147-150, the provisioned engine is itself promote-capable (loopback). Replace:

```rust
        let promote = Some(PromoteConfig {
            token: self.token.clone(),
            base_dir: dir.join("promote"),
        });
```

with:

```rust
        let promote = Some(PromoteConfig {
            token: self.token.clone(),
            mode: PromoteMode::Loopback {
                base_dir: dir.join("promote"),
            },
        });
```

Also update the `crate::serve::app(...)` call on line 151 to pass `false` for the new `accept_promotions` param (added in Task 3) — note this here so it's not forgotten, but it will not compile until Task 3 changes the signature. For Task 1's green checkpoint, leave line 151 as-is; Task 3 updates it.

- [ ] **Step 4: Update `handle_handover` target build in `serve.rs`**

At `crates/engine/src/serve.rs` around line 516, replace:

```rust
            let target = LoopbackTarget::new(cfg.token.clone(), cfg.base_dir.clone(), to_remote);
```

with a mode match (the `Vps` arm's `VpsTarget` type lands in Task 4 — for Task 1, stub the `Vps` arm to `unreachable!()` so it compiles, then Task 5 replaces the whole function properly):

```rust
            let base_dir = match &cfg.mode {
                crate::remote::PromoteMode::Loopback { base_dir } => base_dir.clone(),
                crate::remote::PromoteMode::Vps { .. } => unreachable!("vps wired in Task 5"),
            };
            let target = LoopbackTarget::new(cfg.token.clone(), base_dir, to_remote);
```

(Task 5 rewrites `handle_handover` to select `VpsTarget`; this stub just keeps the loopback path compiling and tested meanwhile.)

- [ ] **Step 5: Update the CLI config build in `main.rs`**

At `crates/engine/src/main.rs:228-238`, replace the `base_dir: root.join(".otto-remotes")` config with the `mode` form (keep the load-bearing dot-prefix comment):

```rust
    let promote = if promote_loopback {
        Some(otto_engine::PromoteConfig {
            token: token.clone(),
            // The dot-prefix is load-bearing: `LocalWorkspace::list` skips dot-directories, so a
            // provisioned engine's restored store/workspace under here is never recursively
            // captured by a later `workspace.snapshot()`. Do not rename without that guarantee.
            mode: otto_engine::PromoteMode::Loopback {
                base_dir: root.join(".otto-remotes"),
            },
        })
    } else {
        None
    };
```

- [ ] **Step 6: Re-export `PromoteMode` from `lib.rs`**

At `crates/engine/src/lib.rs:32-35`, add `PromoteMode` to the `remote::{…}` re-export list:

```rust
pub use remote::{
    LoopbackTarget, PromoteBundle, PromoteConfig, PromoteMode, RemoteHandle, RemoteTarget,
    UnsupportedTarget, promote,
};
```

- [ ] **Step 7: Update the test `PromoteConfig` in `tests/serve.rs`**

At `crates/engine/tests/serve.rs:704-707`, replace:

```rust
    let promote = Some(otto_engine::PromoteConfig {
        token: TOKEN.to_string(),
        base_dir: dir.path().join("remotes"),
    });
```

with:

```rust
    let promote = Some(otto_engine::PromoteConfig {
        token: TOKEN.to_string(),
        mode: otto_engine::PromoteMode::Loopback {
            base_dir: dir.path().join("remotes"),
        },
    });
```

- [ ] **Step 8: Build and run the existing suite to confirm the refactor is green**

Run: `cargo test -p otto-engine`
Expected: PASS — in particular `tests/promote.rs::promote_resumes_session_and_workspace_on_a_loopback_remote` and the `tests/serve.rs` promote test still pass. (The `unreachable!()` in the Vps arm is never hit on these paths.)

- [ ] **Step 9: Commit**

```bash
git add crates/engine/src/remote.rs crates/engine/src/serve.rs crates/engine/src/main.rs crates/engine/src/lib.rs crates/engine/tests/serve.rs
git commit -m "refactor(remote): generalize PromoteConfig with PromoteMode; derive serde on PromoteBundle"
```

---

### Task 2: `EngineService::accept_promotion` (gated restore)

**Files:**
- Modify: `crates/engine/src/service.rs` (add `AcceptError` + `accept_promotion`; tests in the existing `#[cfg(test)] mod tests`)
- Test: `crates/engine/src/service.rs` (inline unit tests)

See the **Spec deviation** section: this restores the workspace through the gate, not via a raw trait `restore`.

- [ ] **Step 1: Write the failing tests**

Add these to the `#[cfg(test)] mod tests` block in `crates/engine/src/service.rs`. They reuse the test scaffolding already present in that module (look at `workspace_rpc_*` tests for how a service is built — replicate that local helper inline if there is no shared constructor). The bundle type is `crate::remote::PromoteBundle`; `SessionState`/`SessionStatus` come from `otto_persistence`.

```rust
#[tokio::test]
async fn accept_promotion_restores_session_and_workspace() {
    use crate::remote::PromoteBundle;
    use otto_engine_core::types::WorkspaceSnapshot;
    use otto_persistence::{SessionState, SessionStatus};
    use std::path::PathBuf;

    // A receiver service over a fresh empty store + temp workspace.
    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

    let id = SessionId::new();
    let bundle = PromoteBundle {
        session: SessionState {
            id,
            goal: "g".to_string(),
            status: SessionStatus::Active,
            config: serde_json::json!({}),
            events: vec![],
            turns: vec![],
        },
        workspace: WorkspaceSnapshot {
            files: vec![(PathBuf::from("out.txt"), b"HELLO".to_vec())],
        },
    };

    let restored = service.accept_promotion(&bundle).await.unwrap();
    assert_eq!(restored, id);
    // Session is now present in the receiver's store.
    assert!(service.store().session_status(id).await.is_ok());
    // Workspace file landed.
    assert_eq!(
        service.workspace().read(std::path::Path::new("out.txt")).await.unwrap(),
        b"HELLO"
    );
}

#[tokio::test]
async fn accept_promotion_refuses_sensitive_workspace_entry() {
    use crate::remote::PromoteBundle;
    use otto_engine_core::types::WorkspaceSnapshot;
    use otto_persistence::{SessionState, SessionStatus};
    use std::path::PathBuf;

    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

    let id = SessionId::new();
    let bundle = PromoteBundle {
        session: SessionState {
            id,
            goal: "g".to_string(),
            status: SessionStatus::Active,
            config: serde_json::json!({}),
            events: vec![],
            turns: vec![],
        },
        workspace: WorkspaceSnapshot {
            files: vec![(PathBuf::from(".env"), b"SECRET=1".to_vec())],
        },
    };

    let err = service.accept_promotion(&bundle).await;
    assert!(matches!(err, Err(AcceptError::Failed(_))));
    // Fail-closed: nothing landed — neither the file nor the session.
    assert!(service.workspace().read(std::path::Path::new(".env")).await.is_err());
    assert!(service.store().session_status(id).await.is_err());
}

#[tokio::test]
async fn accept_promotion_duplicate_session_is_already_exists() {
    use crate::remote::PromoteBundle;
    use otto_engine_core::types::WorkspaceSnapshot;

    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

    // Create a session, snapshot it, then try to accept a bundle re-using its id.
    let id = service.create_session("g", &serde_json::json!({})).await.unwrap();
    let state = service.store().snapshot(id).await.unwrap();
    let bundle = PromoteBundle { session: state, workspace: WorkspaceSnapshot { files: vec![] } };

    assert!(matches!(
        service.accept_promotion(&bundle).await,
        Err(AcceptError::AlreadyExists)
    ));
}
```

Add a local test helper near the top of the test module (if an equivalent doesn't already exist — check first and reuse):

```rust
async fn build_test_service(ws_root: &std::path::Path, db_path: std::path::PathBuf) -> EngineService {
    use std::sync::Arc;
    let workspace: Arc<dyn otto_engine_core::traits::Workspace> =
        Arc::new(otto_workspace::LocalWorkspace::new(ws_root));
    let tools_ws: Arc<dyn otto_engine_core::traits::Workspace> =
        Arc::new(otto_workspace::LocalWorkspace::new(ws_root));
    let tools = Arc::new(crate::build_tool_registry(tools_ws, ws_root.to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(db_path).await.unwrap());
    let router: Arc<dyn otto_engine_core::Router> = Arc::from(crate::build_router());
    EngineService::new(store, Arc::new(crate::build_default_registry()), router, workspace, tools)
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine --lib accept_promotion`
Expected: FAIL to compile — `accept_promotion` and `AcceptError` do not exist yet.

- [ ] **Step 3: Implement `AcceptError` and `accept_promotion`**

In `crates/engine/src/service.rs`, add the error type near the top (after the imports / before `EngineService`):

```rust
/// Outcome of a failed `accept_promotion`, mapped to HTTP status by the `/promote` handler.
pub enum AcceptError {
    /// The session id already exists in the receiver store → `409 Conflict` (no silent overwrite).
    AlreadyExists,
    /// Any other failure (sensitive-path refusal, non-UTF-8 entry, store/IO error) → `500`.
    Failed(anyhow::Error),
}
```

Add the method inside `impl EngineService` (the `Decision`, `Edit`, and `json!` imports are already used by `workspace_rpc`):

```rust
    /// Restore a promoted bundle into this (receiver) engine: write each workspace file through
    /// the permission gate, then restore the session into the store. Fail-closed and validated
    /// up front — a sensitive-path entry (`.env`/`.ssh`/…) is refused before anything is written,
    /// and a duplicate session id is reported (never overwritten).
    pub async fn accept_promotion(
        &self,
        bundle: &crate::remote::PromoteBundle,
    ) -> Result<SessionId, AcceptError> {
        let id = bundle.session.id;

        // Duplicate probe: a present session is a 409, not a silent overwrite.
        if self.store.session_status(id).await.is_ok() {
            return Err(AcceptError::AlreadyExists);
        }

        // Validate the WHOLE workspace snapshot through the gate before writing anything: a
        // sensitive-path entry is refused (fail-closed) and nothing lands. This is where the
        // inviolable sensitive-path floor is enforced on restore (see the design note).
        let mut edits = Vec::with_capacity(bundle.workspace.files.len());
        for (path, bytes) in &bundle.workspace.files {
            if self
                .tools
                .check("fs.write", &json!({ "path": path.to_string_lossy() }))
                == Decision::Deny
            {
                return Err(AcceptError::Failed(anyhow::anyhow!(
                    "restore refused sensitive path: {}",
                    path.display()
                )));
            }
            let new_contents = String::from_utf8(bytes.clone()).map_err(|_| {
                AcceptError::Failed(anyhow::anyhow!(
                    "restore: non-UTF-8 contents for {}",
                    path.display()
                ))
            })?;
            edits.push(Edit {
                path: path.clone(),
                new_contents,
            });
        }

        // Session first, then the pre-validated workspace files.
        self.store
            .restore(&bundle.session)
            .await
            .map_err(AcceptError::Failed)?;
        for edit in &edits {
            self.workspace
                .apply_edit(edit)
                .await
                .map_err(AcceptError::Failed)?;
        }
        Ok(id)
    }
```

If `Decision`, `Edit`, or `json!` are not already imported at module scope, add `use` statements (they are used in `workspace_rpc` — confirm the existing imports cover them; `json!` is `serde_json::json`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine --lib accept_promotion`
Expected: PASS (all three tests).

- [ ] **Step 5: Export `AcceptError` from `lib.rs`**

At `crates/engine/src/lib.rs:37`, add `AcceptError` to the `service::{…}` re-export so `serve.rs` and tests can name it:

```rust
pub use service::{AcceptError, CollectingSink, EngineService, EventSink, TurnControls};
```

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/service.rs crates/engine/src/lib.rs
git commit -m "feat(engine): EngineService::accept_promotion with gated workspace restore"
```

---

### Task 3: `POST /promote` route + `--accept-promotions` gate

**Files:**
- Modify: `crates/engine/src/serve.rs` (ServeState field, `app()` signature, route, `promote_handler`)
- Modify call sites: `crates/engine/src/remote.rs:151`, `crates/engine/src/main.rs:239`, `crates/engine/tests/serve.rs` (4 + 1 sites), `crates/engine/tests/cors.rs:36`, `crates/engine/tests/remote_workspace.rs:39`
- Test: `crates/engine/tests/vps_promote.rs` (new — handler status-code tests)

- [ ] **Step 1: Write the failing handler tests**

Create `crates/engine/tests/vps_promote.rs` with the handler tests (the e2e round-trip is added in Task 7; keep them in one file). Use a helper that serves a receiver app on `127.0.0.1:0`:

```rust
//! vps RemoteTarget: POST /promote handler gating + status codes, VpsTarget round-trip,
//! teardown no-op, and handover-mode behavior. All in-process on ephemeral loopback ports.

use std::sync::Arc;

use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, serve_app, serve_run,
};
use otto_engine_core::traits::Workspace;
use otto_persistence::{SessionState, SessionStatus, SqliteStore};
use otto_protocol::{CapabilitiesManifest, SessionId};
use otto_workspace::LocalWorkspace;
use serde_json::json;

const TOKEN: &str = "vps-token";

fn caps() -> CapabilitiesManifest {
    CapabilitiesManifest { engine_remote: false, local_llm: false, remote_llm: false, sandbox: false }
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
    let service =
        EngineService::new(store, Arc::new(build_default_registry()), Arc::from(otto_engine::build_router()), workspace, tools);
    let app = serve_app(service, TOKEN.to_string(), caps(), None, accept_promotions);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { serve_run(listener, app, None).await.unwrap(); });
    (format!("http://127.0.0.1:{port}"), ws_dir, db_dir)
}

fn sample_bundle_json(id: SessionId, files: Vec<(&str, &[u8])>) -> serde_json::Value {
    json!({
        "session": {
            "id": id.0.to_string(),
            "goal": "g",
            "status": "active",
            "config": {},
            "events": [],
            "turns": []
        },
        "workspace": { "files": files.iter().map(|(p, b)| json!([p, b])).collect::<Vec<_>>() }
    })
}

async fn post_promote(base: &str, token: Option<&str>, body: &serde_json::Value) -> reqwest::Response {
    let mut req = reqwest::Client::new().post(format!("{base}/promote")).json(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn promote_without_accept_flag_is_forbidden() {
    let (base, _w, _d) = start_receiver(false).await;
    let body = sample_bundle_json(SessionId::new(), vec![]);
    let resp = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn promote_without_bearer_is_unauthorized() {
    let (base, _w, _d) = start_receiver(true).await;
    let body = sample_bundle_json(SessionId::new(), vec![]);
    let resp = post_promote(&base, None, &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn promote_with_wrong_bearer_is_unauthorized() {
    let (base, _w, _d) = start_receiver(true).await;
    let body = sample_bundle_json(SessionId::new(), vec![]);
    let resp = post_promote(&base, Some("nope"), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn promote_valid_bundle_is_ok_and_restores() {
    let (base, _w, _d) = start_receiver(true).await;
    let id = SessionId::new();
    let body = sample_bundle_json(id, vec![("out.txt", b"HELLO")]);
    let resp = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["session"].as_str().unwrap(), id.0.to_string());
}

#[tokio::test]
async fn promote_sensitive_entry_is_refused() {
    let (base, _w, _d) = start_receiver(true).await;
    let body = sample_bundle_json(SessionId::new(), vec![(".env", b"SECRET=1")]);
    let resp = post_promote(&base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn promote_duplicate_session_is_conflict() {
    let (base, _w, _d) = start_receiver(true).await;
    let id = SessionId::new();
    let body = sample_bundle_json(id, vec![]);
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
```

Note: confirm `SessionStatus::Active` serializes to `"active"` (it derives serde and `as_db_str()` uses `"active"` — verify the serde rename matches; if `SessionStatus` serializes differently, adjust the JSON `status` value to match its serde form). If unsure, construct the bundle with a real `serde_json::to_value(&PromoteBundle{…})` instead of hand-written JSON. `reqwest` is a dev-dependency of `otto-engine` already (used by `tests/remote_workspace.rs` via `RemoteWorkspace`); if not present as a direct dev-dep, add it in Task 4's Cargo change.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p otto-engine --test vps_promote`
Expected: FAIL to compile — `serve_app` does not yet take an `accept_promotions` arg and `/promote` does not exist.

- [ ] **Step 3: Add `accept_promotions` to `ServeState` and `app()`**

In `crates/engine/src/serve.rs`, add the field to `ServeState` (after `promote`):

```rust
    /// `true` when `--accept-promotions` is set; enables the inbound `POST /promote` restore RPC.
    accept_promotions: bool,
```

Change `app()`'s signature and body. New signature (add the param last):

```rust
pub fn app(
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
    promote: Option<PromoteConfig>,
    accept_promotions: bool,
) -> AxumRouter {
```

Set it in the `ServeState` construction:

```rust
    let state = Arc::new(ServeState {
        service,
        token,
        capabilities,
        promote,
        accept_promotions,
        remotes: Mutex::new(HashMap::new()),
    });
```

Register the route alongside `/workspace`:

```rust
        .route("/workspace", post(workspace_handler))
        .route("/promote", post(promote_handler))
```

- [ ] **Step 4: Add the `promote_handler`**

After `workspace_handler` in `serve.rs`, add (import `AcceptError` and `PromoteBundle`):

```rust
/// Inbound restore RPC (receiver role). Fail-closed: `403` unless `--accept-promotions`, `401`
/// without a valid bearer. Restores the bundle into this engine's store + workspace (gated).
async fn promote_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    if !state.accept_promotions {
        return (StatusCode::FORBIDDEN, "promotion acceptance disabled").into_response();
    }
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    let bundle: crate::remote::PromoteBundle = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response(),
    };
    match state.service.accept_promotion(&bundle).await {
        Ok(session) => {
            axum::Json(serde_json::json!({ "session": session.0.to_string() })).into_response()
        }
        Err(crate::service::AcceptError::AlreadyExists) => {
            (StatusCode::CONFLICT, "session already exists").into_response()
        }
        Err(crate::service::AcceptError::Failed(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
```

- [ ] **Step 5: Update every `app()` / `serve_app()` call site to pass `accept_promotions`**

Pass `false` everywhere except `main.rs` (Task 6 passes the parsed flag there):

- `crates/engine/src/remote.rs:151` — `crate::serve::app(service, self.token.clone(), capabilities, promote, false)`
- `crates/engine/src/main.rs:239` — leave for Task 6, but to compile now pass `false`: `serve_app(service, token, capabilities, promote, false)`
- `crates/engine/tests/serve.rs:61, 101, 139, 601, 708` — append `, false`
- `crates/engine/tests/cors.rs:36` — append `, false` (after the `None,`)
- `crates/engine/tests/remote_workspace.rs:39` — append `, false` (after the `None,`)

- [ ] **Step 6: Run the handler tests + the full engine suite**

Run: `cargo test -p otto-engine --test vps_promote`
Expected: PASS for the seven handler tests (the e2e/teardown/handover tests don't exist yet).

Run: `cargo test -p otto-engine`
Expected: PASS — existing serve/cors/remote_workspace/promote tests still green with the new param.

- [ ] **Step 7: Commit**

```bash
git add crates/engine/src/serve.rs crates/engine/src/remote.rs crates/engine/src/main.rs crates/engine/tests/serve.rs crates/engine/tests/cors.rs crates/engine/tests/remote_workspace.rs crates/engine/tests/vps_promote.rs
git commit -m "feat(serve): POST /promote restore RPC gated by --accept-promotions"
```

---

### Task 4: `VpsTarget` client target

**Files:**
- Modify: `crates/engine/Cargo.toml` (add `reqwest`)
- Modify: `crates/engine/src/remote.rs` (add `VpsTarget`)
- Modify: `crates/engine/src/lib.rs` (re-export `VpsTarget`)
- Test: `crates/engine/tests/vps_promote.rs` (round-trip + teardown-no-op)

- [ ] **Step 1: Write the failing VpsTarget tests**

Append to `crates/engine/tests/vps_promote.rs`:

```rust
#[tokio::test]
async fn vps_target_provisions_against_a_receiver() {
    use otto_engine::{PromoteBundle, RemoteTarget, VpsTarget};
    use otto_engine_core::types::WorkspaceSnapshot;

    let (http_base, _w, _d) = start_receiver(true).await;
    let ws_endpoint = http_base.replace("http://", "ws://");

    let id = SessionId::new();
    let bundle = PromoteBundle {
        session: SessionState {
            id,
            goal: "g".to_string(),
            status: SessionStatus::Active,
            config: serde_json::json!({}),
            events: vec![],
            turns: vec![],
        },
        workspace: WorkspaceSnapshot { files: vec![(std::path::PathBuf::from("out.txt"), b"HI".to_vec())] },
    };

    let target = VpsTarget::new(ws_endpoint.clone(), TOKEN);
    let handle = target.provision(&bundle).await.unwrap();
    // The handle points back at the ws endpoint the client reconnects to.
    assert_eq!(handle.endpoint, ws_endpoint);
}

#[tokio::test]
async fn vps_target_teardown_does_not_stop_the_receiver() {
    use otto_engine::{PromoteBundle, RemoteTarget, VpsTarget};
    use otto_engine_core::types::WorkspaceSnapshot;

    let (http_base, _w, _d) = start_receiver(true).await;
    let ws_endpoint = http_base.replace("http://", "ws://");

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
    let target = VpsTarget::new(ws_endpoint, TOKEN);
    let handle = target.provision(&bundle).await.unwrap();
    target.teardown(handle).await.unwrap();

    // The receiver is still up: a second valid POST /promote (new session id) succeeds.
    let body = sample_bundle_json(SessionId::new(), vec![]);
    let resp = post_promote(&http_base, Some(TOKEN), &body).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn vps_target_provision_errors_on_non_2xx() {
    use otto_engine::{PromoteBundle, RemoteTarget, VpsTarget};
    use otto_engine_core::types::WorkspaceSnapshot;

    // Receiver with acceptance DISABLED → /promote returns 403 → provision must Err.
    let (http_base, _w, _d) = start_receiver(false).await;
    let ws_endpoint = http_base.replace("http://", "ws://");
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
    let target = VpsTarget::new(ws_endpoint, TOKEN);
    assert!(target.provision(&bundle).await.is_err());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p otto-engine --test vps_promote vps_target`
Expected: FAIL to compile — `VpsTarget` does not exist.

- [ ] **Step 3: Add the `reqwest` dependency**

In `crates/engine/Cargo.toml`, under `[dependencies]`, add (mirror the workspace crate's posture):

```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

If `reqwest` is not already a `[dev-dependencies]` entry (the tests use it directly), it is now available transitively via the main dep — but a direct `[dev-dependencies]` reqwest is cleaner for tests. Check `crates/engine/Cargo.toml [dev-dependencies]`; if absent, add `reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }` there too.

- [ ] **Step 4: Implement `VpsTarget` in `remote.rs`**

Add to `crates/engine/src/remote.rs` (after `LoopbackTarget`). Note `RemoteHandle`'s `shutdown` field is private to this module, so `VpsTarget` can build `RemoteHandle { … shutdown: None }`.

```rust
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
        if !resp.status().is_success() {
            anyhow::bail!("promote rejected by remote: HTTP {}", resp.status());
        }
        Ok(RemoteHandle {
            endpoint: self.endpoint.clone(),
            token: self.token.clone(),
            shutdown: None,
        })
    }

    async fn teardown(&self, _handle: RemoteHandle) -> anyhow::Result<()> {
        // No-op: VpsTarget does not own the operator's server, so it must never abort it.
        Ok(())
    }
}
```

- [ ] **Step 5: Re-export `VpsTarget` from `lib.rs`**

Add `VpsTarget` to the `remote::{…}` re-export in `crates/engine/src/lib.rs`:

```rust
pub use remote::{
    LoopbackTarget, PromoteBundle, PromoteConfig, PromoteMode, RemoteHandle, RemoteTarget,
    UnsupportedTarget, VpsTarget, promote,
};
```

- [ ] **Step 6: Run the VpsTarget tests**

Run: `cargo test -p otto-engine --test vps_promote vps_target`
Expected: PASS (provision, teardown-no-op, non-2xx error).

- [ ] **Step 7: Commit**

```bash
git add crates/engine/Cargo.toml crates/engine/src/remote.rs crates/engine/src/lib.rs crates/engine/tests/vps_promote.rs
git commit -m "feat(remote): VpsTarget — promote over the wire to a running otto serve"
```

---

### Task 5: `handle_handover` target selection by mode

**Files:**
- Modify: `crates/engine/src/serve.rs` (`handle_handover`)
- Test: `crates/engine/tests/vps_promote.rs` (handover-via-WS tests)

- [ ] **Step 1: Write the failing handover tests**

These drive `handle_handover` through a real WS connection to a *source* serve started in `Vps` mode. Append to `crates/engine/tests/vps_promote.rs` (add the `futures_util` + `tokio_tungstenite` imports at the top; they are dev-deps used by `tests/promote.rs`):

```rust
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

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

/// Start a SOURCE serve configured to promote in vps mode at `vps_endpoint`. Returns its ws base.
async fn start_source_vps(vps_endpoint: String) -> (String, tempfile::TempDir, tempfile::TempDir) {
    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, ws_dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(SqliteStore::open(db_dir.path().join("s.db")).await.unwrap());
    let service =
        EngineService::new(store, Arc::new(build_default_registry()), Arc::from(otto_engine::build_router()), workspace, tools);
    let promote = Some(otto_engine::PromoteConfig {
        token: TOKEN.to_string(),
        mode: otto_engine::PromoteMode::Vps { endpoint: vps_endpoint },
    });
    let app = serve_app(service, TOKEN.to_string(), caps(), promote, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { serve_run(listener, app, None).await.unwrap(); });
    (format!("ws://127.0.0.1:{port}"), ws_dir, db_dir)
}

fn authed_ws_request(url: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = url.to_string().into_client_request().unwrap();
    req.headers_mut().insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}

#[tokio::test]
async fn handover_vps_promote_points_at_receiver() {
    // Receiver accepts promotions; source promotes to it in vps mode.
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");
    let (src_ws, _sw, _sd) = start_source_vps(recv_ws.clone()).await;

    // Connect to the source, which creates a fresh session on connect.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws"))).await.unwrap();
    let ready = next_json(&mut ws).await.unwrap();
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    // Send PromoteToRemote and expect a Promoted frame pointing at the receiver ws endpoint.
    ws.send(Message::text(json!({ "type": "promote_to_remote", "session": session }).to_string())).await.unwrap();
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

    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws"))).await.unwrap();
    let ready = next_json(&mut ws).await.unwrap();
    let session = ready["session"].as_str().unwrap().to_string();

    ws.send(Message::text(json!({ "type": "demote_to_local", "session": session }).to_string())).await.unwrap();
    loop {
        let frame = next_json(&mut ws).await.expect("a frame");
        if frame["type"] == "error" {
            assert!(frame["message"].as_str().unwrap().contains("demote-from-remote not supported"));
            break;
        }
        assert_ne!(frame["type"], "demoted", "demote must not succeed in vps mode");
    }
}
```

Note: confirm the exact serde tag for `Command::PromoteToRemote`/`DemoteToLocal` and `ServerMessage::Promoted`/`Error` (the `"type"` values `promote_to_remote`, `demote_to_local`, `promoted`, `error`). Check `crates/protocol/src/lib.rs` for the `#[serde(tag = "type", rename_all = "snake_case")]` (or similar) on these enums and match the JSON exactly. Adjust the string literals if the rename differs.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p otto-engine --test vps_promote handover`
Expected: FAIL — the `unreachable!()` stub from Task 1 panics on the Vps arm (or demote is mishandled).

- [ ] **Step 3: Rewrite `handle_handover` to select the target by mode**

Replace the body of `handle_handover` in `crates/engine/src/serve.rs` (lines ~486-553) with:

```rust
async fn handle_handover(
    state: &ServeState,
    writer: &mut WsWriter,
    session: SessionId,
    to_remote: bool,
) {
    let Some(cfg) = state.promote.as_ref() else {
        let _ = send_msg(
            writer,
            &ServerMessage::Error {
                message:
                    "remote provisioning unavailable (start otto serve with --promote-loopback or --promote-vps)"
                        .to_string(),
            },
        )
        .await;
        return;
    };

    // Vps mode cannot demote: pulling a session back off the operator's server is a separate,
    // larger piece (a reverse snapshot-export RPC). Refuse honestly. Loopback demote is unchanged.
    if !to_remote && matches!(cfg.mode, crate::remote::PromoteMode::Vps { .. }) {
        let _ = send_msg(
            writer,
            &ServerMessage::Error {
                message: "demote-from-remote not supported in vps mode".to_string(),
            },
        )
        .await;
        return;
    }

    // Reuse an existing handover for this session+direction (idempotent): re-provisioning would
    // drop the prior RemoteHandle. Bind to a local so the Mutex guard releases before the await.
    let existing = state
        .remotes
        .lock()
        .unwrap()
        .get(&(session, to_remote))
        .map(|h| h.endpoint.clone());
    let endpoint = match existing {
        Some(endpoint) => endpoint,
        None => {
            let target: Box<dyn crate::remote::RemoteTarget> = match &cfg.mode {
                crate::remote::PromoteMode::Loopback { base_dir } => Box::new(
                    LoopbackTarget::new(cfg.token.clone(), base_dir.clone(), to_remote),
                ),
                crate::remote::PromoteMode::Vps { endpoint } => {
                    Box::new(crate::remote::VpsTarget::new(endpoint.clone(), cfg.token.clone()))
                }
            };
            let handle = match promote(
                state.service.store(),
                state.service.workspace(),
                session,
                &*target,
            )
            .await
            {
                Ok(h) => h,
                Err(e) => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                    return;
                }
            };
            let endpoint = handle.endpoint.clone();
            // Retain BEFORE replying: for loopback, dropping the handle aborts the engine; for
            // vps the handle's shutdown is None, so retention is cheap and harmless.
            state
                .remotes
                .lock()
                .unwrap()
                .insert((session, to_remote), handle);
            endpoint
        }
    };
    let msg = if to_remote {
        ServerMessage::Promoted { session, endpoint }
    } else {
        ServerMessage::Demoted { session, endpoint }
    };
    let _ = send_msg(writer, &msg).await;
}
```

Remove the now-dead Task-1 stub lines (the `unreachable!` match). Ensure `LoopbackTarget` is still imported (it is, line 33).

- [ ] **Step 4: Run the handover tests + full engine suite**

Run: `cargo test -p otto-engine --test vps_promote handover`
Expected: PASS (Promoted points at receiver; demote errors).

Run: `cargo test -p otto-engine`
Expected: PASS — the loopback `tests/promote.rs` and `tests/serve.rs` handover tests still pass (loopback path unchanged in behavior).

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/serve.rs crates/engine/tests/vps_promote.rs
git commit -m "feat(serve): select promote target by PromoteMode (loopback vs vps)"
```

---

### Task 6: CLI wiring — `--accept-promotions`, `--promote-vps`, mutual exclusion

**Files:**
- Modify: `crates/engine/src/main.rs` (flag parsing, config build, serve_app call, usage strings)

- [ ] **Step 1: Add flag parsing**

In `crates/engine/src/main.rs`, near the other `let mut` flags (lines 173-176), add:

```rust
    let mut accept_promotions = false;
    let mut promote_vps: Option<String> = None;
```

In the arg `match` (after `--promote-loopback`, line 202), add:

```rust
            "--accept-promotions" => accept_promotions = true,
            "--promote-vps" => match it.next() {
                Some(e) => promote_vps = Some(e.clone()),
                None => {
                    eprintln!("error: --promote-vps requires a ws://… endpoint");
                    std::process::exit(2);
                }
            },
```

- [ ] **Step 2: Build the `PromoteConfig` with mutual exclusion**

Replace the `let promote = if promote_loopback { … } else { None };` block (lines 228-238, already changed in Task 1) with:

```rust
    let promote = match (promote_loopback, promote_vps) {
        (true, Some(_)) => {
            eprintln!("error: --promote-loopback and --promote-vps are mutually exclusive");
            std::process::exit(2);
        }
        (true, None) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            // The dot-prefix is load-bearing: `LocalWorkspace::list` skips dot-directories, so a
            // provisioned engine's restored store/workspace under here is never recursively
            // captured by a later `workspace.snapshot()`. Do not rename without that guarantee.
            mode: otto_engine::PromoteMode::Loopback {
                base_dir: root.join(".otto-remotes"),
            },
        }),
        (false, Some(endpoint)) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Vps { endpoint },
        }),
        (false, None) => None,
    };
```

- [ ] **Step 3: Pass `accept_promotions` to `serve_app`**

At line 239, change:

```rust
    let app = serve_app(service, token, capabilities, promote, accept_promotions);
```

- [ ] **Step 4: Update the usage strings**

Update the two usage strings (lines 2 and 26) to mention the new flags, e.g.:

```
otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint>] [--accept-promotions]
```

- [ ] **Step 5: Build to confirm it compiles**

Run: `cargo build -p otto-engine`
Expected: success.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(cli): otto serve --accept-promotions / --promote-vps (mutually exclusive with --promote-loopback)"
```

---

### Task 7: End-to-end promote round-trip through `VpsTarget`

**Files:**
- Test: `crates/engine/tests/vps_promote.rs` (capstone e2e)

This is the spec's crux test (#1): a real `VpsTarget` pointed at a second in-process `otto serve --accept-promotions` exercises the network restore RPC end to end, then a WS client reconnects to the receiver and resumes via `last_seq` replay.

- [ ] **Step 1: Write the failing e2e test**

Append to `crates/engine/tests/vps_promote.rs`. This runs a turn on a source engine (a `ScriptedProvider` writing a file), promotes via `VpsTarget` to a receiver, then reconnects to the receiver and asserts the event gap replays and the workspace transferred. Model it on `tests/promote.rs`:

```rust
#[tokio::test]
async fn vps_promote_resumes_session_and_workspace_on_receiver() {
    use otto_engine::{CollectingSink, PromoteBundle, RemoteTarget, VpsTarget};
    use otto_engine_core::traits::WorkspaceRead;
    use otto_providers::ScriptedProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::RemoteWorkspace;

    // --- Receiver serve B, acceptance enabled. ---
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");

    // --- Source engine A: run a turn that writes out.txt. ---
    let src_ws_dir = tempfile::tempdir().unwrap();
    let src_db_dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on("edits", r#"{"edits": [{"path": "out.txt", "contents": "PROMOTED"}]}"#)
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(src_ws_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(src_ws_dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, src_ws_dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(SqliteStore::open(src_db_dir.path().join("a.db")).await.unwrap());
    let service =
        EngineService::new(store.clone(), Arc::new(build_default_registry()), router, workspace.clone(), tools);

    let session = service.create_session("g", &serde_json::json!({})).await.unwrap();
    let mut sink = CollectingSink::default();
    service.run_prompt(session, "add a greeting", &mut sink).await.unwrap();
    let src_events = store.replay_since(session, None).await.unwrap();
    let last_seq = src_events.last().unwrap().seq;

    // --- Promote to the receiver via VpsTarget. ---
    let target = VpsTarget::new(recv_ws.clone(), TOKEN);
    let handle = otto_engine::promote(&*store, &*workspace, session, &target).await.unwrap();
    assert_eq!(handle.endpoint, recv_ws);

    // --- Reconnect to the receiver: same session, replayed gap after seq 0. ---
    let (mut ws, _) = tokio_tungstenite::connect_async(
        authed_ws_request(&format!("{recv_ws}/ws?session={}&last_seq=0", session.0)),
    )
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
    let expected: Vec<u64> = src_events.iter().map(|e| e.seq).filter(|s| *s > 0).collect();
    assert_eq!(replayed, expected);
    drop(ws);

    // --- The workspace transferred: read out.txt via the receiver's /workspace RPC. ---
    let remote_ws = RemoteWorkspace::new(recv_http, TOKEN);
    assert_eq!(remote_ws.read(std::path::Path::new("out.txt")).await.unwrap(), b"PROMOTED");
}
```

- [ ] **Step 2: Run to verify it passes**

Run: `cargo test -p otto-engine --test vps_promote vps_promote_resumes`
Expected: PASS — the session resumes on the receiver and the promoted file is readable.

(If it fails on the `ScriptedProvider` `.on(...)` keys, confirm the prompt-substring keys against `tests/promote.rs`, which is the known-good template.)

- [ ] **Step 3: Commit**

```bash
git add crates/engine/tests/vps_promote.rs
git commit -m "test(remote): end-to-end VpsTarget promote round-trip to a receiver serve"
```

---

### Task 8: Docs + full-suite verification

**Files:**
- Modify: `docs/ARCHITECTURE.md` (and a short note in the design spec / roadmap)
- Modify: `CLAUDE.md` (the remote-axis paragraph mentions `UnsupportedTarget` as the VPS boundary — update it)

- [ ] **Step 1: Update `docs/ARCHITECTURE.md`**

Find the section describing the remote axis / `RemoteTarget` / `UnsupportedTarget` (search for `UnsupportedTarget` and `vps`). Record that `vps` is shipped: a `VpsTarget` promotes onto an already-running `otto serve --accept-promotions` via `POST /promote`; the residual `UnsupportedTarget` boundary is now **only machine-provisioning** (SSH/cloud-SDK VM creation); demote-from-remote is the next follow-up.

- [ ] **Step 2: Update `CLAUDE.md`**

In the "distribution axis" paragraph, update the sentence that frames `UnsupportedTarget` / the VPS boundary to reflect that `vps` promote now works against a running receiver; the boundary is machine provisioning + demote-from-remote.

- [ ] **Step 3: Add a "shipped" note to the design spec**

At the top of `docs/superpowers/specs/2026-06-20-vps-remote-target-design.md`, change **Status** to `Shipped 2026-06-22` and add a one-line note that component #1 was implemented as a gated `accept_promotion` restore (not a `Workspace::restore` trait move) — see this plan's Spec deviation section for why.

- [ ] **Step 4: Run the full workspace suite (determinism invariant)**

Run: `cargo test --workspace`
Expected: PASS — no new default behavior; both flags are opt-in, all paths offline. The default offline determinism suite is unchanged.

Run: `cargo fmt --all && cargo clippy --workspace --all-targets`
Expected: clean (no warnings introduced).

- [ ] **Step 5: Commit**

```bash
git add docs/ARCHITECTURE.md CLAUDE.md docs/superpowers/specs/2026-06-20-vps-remote-target-design.md
git commit -m "docs: record vps RemoteTarget shipped; shrink UnsupportedTarget boundary"
```

---

## Spec coverage check

| Spec requirement | Task |
|---|---|
| Receiver role `--accept-promotions` + `POST /promote` (403/401/400/409/500/200) | 3 |
| Sender role `--promote-vps`; token reused from source | 4, 6 |
| Mutual exclusion with `--promote-loopback` | 6 |
| Sensitive-path floor preserved on restore | 2 (gated restore — see Spec deviation) |
| `VpsTarget::teardown` is a no-op | 4 |
| `PromoteConfig` + `PromoteMode` refactor | 1 |
| `handle_handover` mode selection; demote-vps → honest error | 5 |
| `PromoteBundle` serde derives | 1 |
| End-to-end round-trip against in-process receiver | 7 |
| Determinism suite untouched | 8 |
| Docs: record shipped, shrink `UnsupportedTarget` boundary | 8 |

**Intentionally not implemented (spec non-goals):** machine provisioning, demote-from-remote (returns an honest error), multi-session receivers, the `remote` crate split-out.

**Intentional deviation:** spec component #1 (`Workspace::restore` trait move) is replaced by a gated `EngineService::accept_promotion` — the `Workspace` trait, `crates/workspace/src/lib.rs`, and `crates/workspace/src/remote.rs` are **not** touched. See the Spec deviation section.
