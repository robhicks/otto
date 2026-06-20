# Promote-to-remote UX (Sub-project F) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the otto UI hand a live session off to a remote engine and back — `PromoteToRemote`/`DemoteToLocal` commands wired to the existing `promote()` + `LoopbackTarget`, with `Last-Event-ID` reconnect.

**Architecture:** Demote is promote in the other direction — the same snapshot→provision→handover, differing only in the provisioned engine's `engine_remote` flag. New `Command` + `ServerMessage` variants (additive). `otto serve --promote-loopback` opts a `PromoteConfig` into `ServeState`, which the serve loop uses to build a `LoopbackTarget` per command; a connection-scoped registry retains the `RemoteHandle` so the provisioned engine outlives the local connection. The UI shows Promote/Demote buttons and reconnects to the handed-back endpoint on `Promoted`/`Demoted`.

**Tech Stack:** Rust (axum + tokio + tungstenite tests), Leptos CSR (Rust→WASM), serde. Design spec: `docs/superpowers/specs/2026-06-20-ui-promote-to-remote-design.md`.

**Conventions:** Commit messages are plain — NO Claude attribution (no `Co-Authored-By: Claude`, no "Generated with Claude Code", no 🤖). Work happens on branch `feat/ui-promote-to-remote`.

---

## File map

| File | Change |
|---|---|
| `crates/protocol/src/lib.rs` | Add `Command::{PromoteToRemote,DemoteToLocal}` + `ServerMessage::{Promoted,Demoted}` (+ serde tests) |
| `crates/engine/src/service.rs` | Add `EngineService::workspace()` accessor (+ test) |
| `crates/engine/src/remote.rs` | Add `PromoteConfig`; `LoopbackTarget` gains `engine_remote` + nested promote-config wiring |
| `crates/engine/src/lib.rs` | Re-export `PromoteConfig` |
| `crates/engine/src/serve.rs` | `ServeState` gains `promote` + `remotes`; `app()` grows an `Option<PromoteConfig>` param; route Promote/Demote in the outer loop |
| `crates/engine/src/main.rs` | Parse `--promote-loopback`; pass `Some(PromoteConfig)` |
| `crates/engine/tests/{serve,cors,remote_workspace,promote}.rs` | Update call sites; add E2E + unsupported tests |
| `ui/src/view_model.rs` | Pure `can_promote`/`can_demote` predicates (+ tests) |
| `ui/src/app.rs` | `turn_running` signal; promote/demote closures; `Promoted`/`Demoted` reconnect; buttons |

---

## Task 1: Protocol variants

**Files:**
- Modify: `crates/protocol/src/lib.rs` (the `Command` enum ~37-57, the `ServerMessage` enum ~111+, and the `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing serde tests**

Add to the `#[cfg(test)] mod tests` block in `crates/protocol/src/lib.rs`:

```rust
    #[test]
    fn promote_commands_round_trip() {
        let s = SessionId::new();
        for cmd in [
            Command::PromoteToRemote { session: s },
            Command::DemoteToLocal { session: s },
        ] {
            let json = serde_json::to_string(&cmd).unwrap();
            assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
        }
        // External tagging matches the rest of Command (e.g. {"PromoteToRemote":{...}}).
        let json = serde_json::to_string(&Command::PromoteToRemote { session: s }).unwrap();
        assert!(json.contains("PromoteToRemote"));
    }

    #[test]
    fn handover_server_messages_round_trip() {
        let s = SessionId::new();
        for msg in [
            ServerMessage::Promoted { session: s, endpoint: "ws://127.0.0.1:9000".into() },
            ServerMessage::Demoted { session: s, endpoint: "ws://127.0.0.1:9001".into() },
        ] {
            let json = serde_json::to_string(&msg).unwrap();
            assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), msg);
        }
        // ServerMessage is `#[serde(tag="type", rename_all="snake_case")]`.
        let json = serde_json::to_string(
            &ServerMessage::Promoted { session: s, endpoint: "x".into() }
        ).unwrap();
        assert!(json.contains("\"type\":\"promoted\""));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p otto-protocol promote_commands_round_trip handover_server_messages_round_trip`
Expected: FAIL — `no variant named PromoteToRemote` / `Promoted` (compile error).

- [ ] **Step 3: Add the Command variants**

In `crates/protocol/src/lib.rs`, inside `enum Command` (after the `Resume { session: SessionId }` variant):

```rust
    /// Hand this session off to a freshly-provisioned remote engine. The engine replies with
    /// `ServerMessage::Promoted { endpoint }`; the client reconnects there (same token + session
    /// + last_seq). Handled only between turns.
    PromoteToRemote {
        session: SessionId,
    },
    /// Hand this session back to a freshly-provisioned local engine (the reverse of
    /// `PromoteToRemote`). The engine replies with `ServerMessage::Demoted { endpoint }`.
    DemoteToLocal {
        session: SessionId,
    },
```

- [ ] **Step 4: Add the ServerMessage variants**

In `crates/protocol/src/lib.rs`, inside `enum ServerMessage` (after the existing variants, before the closing brace):

```rust
    /// Handover framing: the session has been provisioned onto a remote engine reachable at
    /// `endpoint` (a `ws://host:port` base). The client reconnects there reusing its token,
    /// session, and last_seq. Not a sequenced `Event` — never persisted/replayed from the store.
    Promoted {
        session: SessionId,
        endpoint: String,
    },
    /// Handover framing for the reverse trip: the session is now on a local engine at `endpoint`.
    Demoted {
        session: SessionId,
        endpoint: String,
    },
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p otto-protocol`
Expected: PASS (all protocol tests, including the two new ones).

- [ ] **Step 6: Commit**

```bash
git add crates/protocol/src/lib.rs
git commit -m "feat(protocol): Promote/Demote commands + Promoted/Demoted handover frames"
```

---

## Task 2: `EngineService::workspace()` accessor

**Files:**
- Modify: `crates/engine/src/service.rs` (the `impl EngineService` block near `store()`, ~88; tests `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/engine/src/service.rs`:

```rust
    #[tokio::test]
    async fn workspace_accessor_reads_written_file() {
        use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        // Write through the RPC, then read back through the accessor.
        assert!(matches!(
            service
                .workspace_rpc(WorkspaceRequest::ApplyEdit {
                    path: std::path::PathBuf::from("a.txt"),
                    contents: "hi".to_string(),
                })
                .await,
            WorkspaceResponse::ApplyEdit { .. }
        ));
        let bytes = service
            .workspace()
            .read(std::path::Path::new("a.txt"))
            .await
            .unwrap();
        assert_eq!(bytes, b"hi".to_vec());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p otto-engine workspace_accessor_reads_written_file`
Expected: FAIL — `no method named workspace` on `EngineService`.

- [ ] **Step 3: Add the accessor**

In `crates/engine/src/service.rs`, in `impl EngineService`, right after the `store()` accessor:

```rust
    /// The workspace this service edits, for operations that need it directly (e.g. `promote`,
    /// which snapshots the workspace). Agents never get this — they see only the read-only view.
    pub fn workspace(&self) -> &dyn Workspace {
        &*self.workspace
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p otto-engine workspace_accessor_reads_written_file`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "feat(engine): EngineService::workspace() accessor for promote"
```

---

## Task 3: `PromoteConfig` + thread `Option<PromoteConfig>` through serve

This task only wires the config through; routing comes in Task 5. It keeps everything compiling and existing tests green.

**Files:**
- Modify: `crates/engine/src/remote.rs` (add `PromoteConfig`)
- Modify: `crates/engine/src/lib.rs` (re-export)
- Modify: `crates/engine/src/serve.rs` (`ServeState` fields, `app()` signature)
- Modify (call sites → pass `None`): `crates/engine/src/main.rs:226`, `crates/engine/src/remote.rs:134`, `crates/engine/tests/serve.rs` (4 sites), `crates/engine/tests/cors.rs:36`, `crates/engine/tests/remote_workspace.rs:39`

- [ ] **Step 1: Add `PromoteConfig` in remote.rs**

In `crates/engine/src/remote.rs`, after the `use` block (~18):

```rust
/// Enables session handover on a served engine. `token` is the bearer the provisioned engine
/// requires (reused from the source, by design); `base_dir` is where restored stores/workspaces
/// are written. `ServeState` holds this as `Option`: `Some` ⟺ `--promote-loopback`.
#[derive(Clone)]
pub struct PromoteConfig {
    pub token: String,
    pub base_dir: PathBuf,
}
```

- [ ] **Step 2: Re-export from lib.rs**

In `crates/engine/src/lib.rs`, extend the `pub use remote::{...}` block (~32) to include `PromoteConfig`:

```rust
pub use remote::{
    LoopbackTarget, PromoteBundle, PromoteConfig, RemoteHandle, RemoteTarget, UnsupportedTarget,
    promote,
};
```

(Keep whatever names are already listed; just add `PromoteConfig`. Verify the exact set with `cargo build` in Step 5.)

- [ ] **Step 3: Extend `ServeState` and `app()`**

In `crates/engine/src/serve.rs`:

Add imports near the top `use` block:

```rust
use crate::remote::{LoopbackTarget, PromoteConfig, RemoteHandle, promote};
```

Change `struct ServeState` (~49) to:

```rust
struct ServeState {
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
    /// `Some` when `--promote-loopback` is set; enables the handover commands.
    promote: Option<PromoteConfig>,
    /// Provisioned engines, retained so they outlive the local connection that created them
    /// (a dropped `RemoteHandle` aborts its engine task).
    remotes: std::sync::Mutex<std::collections::HashMap<SessionId, RemoteHandle>>,
}
```

Change `pub fn app(...)` (~84) to take the config and initialize the new fields:

```rust
pub fn app(
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
    promote: Option<PromoteConfig>,
) -> AxumRouter {
    assert!(!token.is_empty(), "serve token must not be empty");
    let state = Arc::new(ServeState {
        service,
        token,
        capabilities,
        promote,
        remotes: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    // ... rest unchanged (CORS layer + routes) ...
```

(Confirm `SessionId` is in scope in serve.rs — it is used elsewhere in the file. If `HashMap`/`Mutex` are already imported, use the short names instead of the fully-qualified paths.)

- [ ] **Step 4: Update every `app()` / `serve_app()` caller to pass `None`**

- `crates/engine/src/main.rs:226` → `let app = serve_app(service, token, capabilities, None);`
- `crates/engine/src/remote.rs:134` → `let app = crate::serve::app(service, self.token.clone(), capabilities, None);`
- `crates/engine/tests/serve.rs` — all four `serve_app(service, TOKEN.to_string(), test_capabilities())` calls → add `, None)`.
- `crates/engine/tests/cors.rs:36` and `crates/engine/tests/remote_workspace.rs:39` → add `, None` as the final arg (read each call to match its exact formatting).

- [ ] **Step 5: Build and run the full engine suite**

Run: `cargo build -p otto-engine && cargo test -p otto-engine`
Expected: PASS — everything compiles and existing tests are green with the new (unused-for-now) fields.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/remote.rs crates/engine/src/lib.rs crates/engine/src/serve.rs crates/engine/src/main.rs crates/engine/tests/serve.rs crates/engine/tests/cors.rs crates/engine/tests/remote_workspace.rs
git commit -m "feat(engine): PromoteConfig threaded through serve app (no routing yet)"
```

---

## Task 4: `LoopbackTarget` engine_remote flag + nested promote-config

**Files:**
- Modify: `crates/engine/src/remote.rs` (`LoopbackTarget` struct, `new`, `provision`)
- Modify: `crates/engine/tests/promote.rs:88` (call site)

- [ ] **Step 1: Update the existing promote test call site (the failing-build driver)**

In `crates/engine/tests/promote.rs:88`, change:

```rust
    let target = LoopbackTarget::new(TOKEN, promote_base.path().to_path_buf());
```
to:
```rust
    let target = LoopbackTarget::new(TOKEN, promote_base.path().to_path_buf(), true);
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p otto-engine --test promote`
Expected: FAIL — `LoopbackTarget::new` takes 2 arguments, not 3 (compile error).

- [ ] **Step 3: Add the `engine_remote` field + nested config**

In `crates/engine/src/remote.rs`, change the struct and `new`:

```rust
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
```

In `provision()`, change the capabilities line to use the flag, and pass a nested `PromoteConfig` to the inner `app()` so the provisioned engine can itself hand the session on. Replace the `capabilities` + `app` lines (~130-134) with:

```rust
        // This provisioned engine reports the configured capability and is itself promote-capable
        // (so the round-trip — demote, re-promote — works), rooted at a nested base dir.
        let capabilities = otto_protocol::CapabilitiesManifest {
            engine_remote: self.engine_remote,
            ..crate::build_capabilities()
        };
        let promote = Some(PromoteConfig {
            token: self.token.clone(),
            base_dir: dir.join("promote"),
        });
        let app = crate::serve::app(service, self.token.clone(), capabilities, promote);
```

(`dir` is the per-session directory already computed at the top of `provision()`. `PromoteConfig` is defined in this same module.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p otto-engine --test promote`
Expected: PASS — the library-level promote+reconnect test still works with the new flag.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/remote.rs crates/engine/tests/promote.rs
git commit -m "feat(engine): LoopbackTarget engine_remote flag + promote-capable provisioned engine"
```

---

## Task 5: Route Promote/Demote in the serve loop

**Files:**
- Modify: `crates/engine/src/serve.rs` (add a handover helper; add match arms in the outer command loop ~449-454)

- [ ] **Step 1: Add the handover helper**

In `crates/engine/src/serve.rs`, add this free function (near `send_msg`, after the `handle_socket` fn or above it):

```rust
/// Provision the session onto a fresh engine (remote for promote, local for demote), retain the
/// handle so it outlives this connection, and tell the client where to reconnect. A no-op error
/// reply when promotion is not enabled (the `UnsupportedTarget` posture). Handled between turns.
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
                message: "remote provisioning unavailable (start otto serve with --promote-loopback)"
                    .to_string(),
            },
        )
        .await;
        return;
    };
    let target = LoopbackTarget::new(cfg.token.clone(), cfg.base_dir.clone(), to_remote);
    let handle = match promote(
        state.service.store(),
        state.service.workspace(),
        session,
        &target,
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            let _ = send_msg(writer, &ServerMessage::Error { message: e.to_string() }).await;
            return;
        }
    };
    let endpoint = handle.endpoint.clone();
    // Retain BEFORE replying: dropping the handle aborts the provisioned engine.
    state.remotes.lock().unwrap().insert(session, handle);
    let msg = if to_remote {
        ServerMessage::Promoted { session, endpoint }
    } else {
        ServerMessage::Demoted { session, endpoint }
    };
    let _ = send_msg(writer, &msg).await;
}
```

- [ ] **Step 2: Add the outer-loop match arms**

In `crates/engine/src/serve.rs`, in the outer `match command { ... }` (the one with `Command::Pause`/`Command::Resume` ~449-454, NOT the in-turn `select!`), add:

```rust
            Command::PromoteToRemote { .. } => {
                handle_handover(&state, &mut writer, session, true).await;
            }
            Command::DemoteToLocal { .. } => {
                handle_handover(&state, &mut writer, session, false).await;
            }
```

(In the in-turn `select!` arm, `PromoteToRemote`/`DemoteToLocal` fall into the existing `_ => {}` no-op — promoting mid-turn is intentionally ignored. Leave that as is.)

- [ ] **Step 3: Build**

Run: `cargo build -p otto-engine`
Expected: PASS (full E2E coverage is added in Task 9; this step just confirms it compiles).

- [ ] **Step 4: Commit**

```bash
git add crates/engine/src/serve.rs
git commit -m "feat(engine): serve routes PromoteToRemote/DemoteToLocal via handover + remotes registry"
```

---

## Task 6: `--promote-loopback` flag

**Files:**
- Modify: `crates/engine/src/main.rs` (`cmd_serve` arg parse ~175-203 and the `serve_app` call ~226)

- [ ] **Step 1: Parse the flag**

In `crates/engine/src/main.rs`, near `let mut approve_edits = false;` (~175) add:

```rust
    let mut promote_loopback = false;
```

In the arg-match loop, alongside `"--approve-edits" => approve_edits = true,`:

```rust
            "--promote-loopback" => promote_loopback = true,
```

- [ ] **Step 2: Build the config and pass it**

In `crates/engine/src/main.rs`, change the `serve_app` call (~226) to:

```rust
    let promote = if promote_loopback {
        Some(otto_engine::PromoteConfig {
            token: token.clone(),
            base_dir: root.join(".otto-remotes"),
        })
    } else {
        None
    };
    let app = serve_app(service, token, capabilities, promote);
```

- [ ] **Step 3: Update the usage strings**

In `crates/engine/src/main.rs`, update both usage strings (the `main` dispatcher ~26 and the doc comment ~1) that list `serve` flags to include `[--promote-loopback]`. For example:

```rust
    otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback]
```

- [ ] **Step 4: Build**

Run: `cargo build -p otto-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): otto serve --promote-loopback enables session handover"
```

---

## Task 7: UI pure predicates (`can_promote` / `can_demote`)

**Files:**
- Modify: `ui/src/view_model.rs` (helpers + tests)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `ui/src/view_model.rs`:

```rust
    fn caps(engine_remote: bool) -> CapabilitiesManifest {
        CapabilitiesManifest {
            engine_remote,
            local_llm: true,
            remote_llm: false,
            sandbox: true,
        }
    }

    #[test]
    fn can_promote_only_when_connected_local_and_idle() {
        let connected = ConnState::Connected { session: "s".into() };
        assert!(can_promote(&connected, &Some(caps(false)), false));
        // not while a turn runs
        assert!(!can_promote(&connected, &Some(caps(false)), true));
        // not when already remote
        assert!(!can_promote(&connected, &Some(caps(true)), false));
        // not when disconnected / caps unknown
        assert!(!can_promote(&ConnState::Disconnected, &Some(caps(false)), false));
        assert!(!can_promote(&connected, &None, false));
    }

    #[test]
    fn can_demote_only_when_connected_remote_and_idle() {
        let connected = ConnState::Connected { session: "s".into() };
        assert!(can_demote(&connected, &Some(caps(true)), false));
        assert!(!can_demote(&connected, &Some(caps(true)), true));
        assert!(!can_demote(&connected, &Some(caps(false)), false));
        assert!(!can_demote(&ConnState::Disconnected, &Some(caps(true)), false));
        assert!(!can_demote(&connected, &None, false));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd ui && cargo test can_promote can_demote`
Expected: FAIL — `cannot find function can_promote`.

- [ ] **Step 3: Implement the predicates**

Add to `ui/src/view_model.rs` (near `capability_segments`):

```rust
/// True when the Promote button should be enabled: connected, the engine is local, and no turn
/// is running (promoting mid-turn would snapshot partial state, so it is disabled).
pub fn can_promote(
    conn: &ConnState,
    caps: &Option<CapabilitiesManifest>,
    turn_running: bool,
) -> bool {
    matches!(conn, ConnState::Connected { .. })
        && !turn_running
        && matches!(caps, Some(c) if !c.engine_remote)
}

/// True when the Demote button should be enabled: connected, the engine is remote, no turn running.
pub fn can_demote(
    conn: &ConnState,
    caps: &Option<CapabilitiesManifest>,
    turn_running: bool,
) -> bool {
    matches!(conn, ConnState::Connected { .. })
        && !turn_running
        && matches!(caps, Some(c) if c.engine_remote)
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd ui && cargo test can_promote can_demote`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ui/src/view_model.rs
git commit -m "feat(ui): can_promote/can_demote button-visibility predicates"
```

---

## Task 8: UI buttons + Promoted/Demoted reconnect

**Files:**
- Modify: `ui/src/app.rs`

- [ ] **Step 1: Add the `turn_running` and `reconnect_to` signals**

In `ui/src/app.rs`, near `let paused = RwSignal::new(false);` (~41), add:

```rust
    let turn_running = RwSignal::new(false);
    // Set by a Promoted/Demoted frame; an Effect below performs the actual reconnect to the
    // handed-back endpoint (on_msg can't call `connect` directly — it's defined inside it).
    let reconnect_to = RwSignal::new(None::<String>);
```

- [ ] **Step 2: Track turn lifecycle + handle handover frames in `on_msg`**

In `ui/src/app.rs`, in the `Ok(ServerMessage::Event { event })` branch, where `TurnComplete` is handled, set `turn_running` false there and true on `AgentStarted`. Update that block to:

```rust
                    if let EventKind::AgentStarted { .. } = &event.kind {
                        turn_running.set(true);
                    }
                    if let EventKind::TurnComplete { .. } = &event.kind {
                        pending_approval.set(None);
                        paused.set(false);
                        turn_running.set(false);
                    }
```

Add two new arms to the outer `match incoming` (alongside `Ok(ServerMessage::Error { .. })`):

```rust
            Ok(ServerMessage::Promoted { endpoint, .. })
            | Ok(ServerMessage::Demoted { endpoint, .. }) => {
                // Reconnect to the handed-back engine, reusing token + session + last_seq. The new
                // engine's manifest flips the status strip local↔remote. Deferred to an Effect.
                reconnect_to.set(Some(endpoint));
            }
```

- [ ] **Step 3: Reset `turn_running` on the connection-reset paths**

In `ui/src/app.rs`, add `turn_running.set(false);` next to each existing `paused.set(false);` in: connect-start (~65), `on_close`, `on_error`, and `disconnect` (the explicit disconnect). Also set `turn_running.set(true);` in `send_prompt` next to `paused.set(false);` (a new turn is starting).

- [ ] **Step 4: Add the reconnect Effect (after `connect` is defined)**

In `ui/src/app.rs`, after the `connect` closure is defined (and before the `view!`), add:

```rust
    // Perform a handover reconnect: point the URL at the new endpoint and reconnect. `connect`
    // closes the old socket first and reuses session + last_seq for replay.
    Effect::new(move |_| {
        if let Some(endpoint) = reconnect_to.get() {
            reconnect_to.set(None);
            url.set(endpoint);
            connect();
        }
    });
```

(`connect` captures only `Copy` signals, so it is itself `Copy` and can be moved into this Effect.)

- [ ] **Step 5: Add the promote/demote command closures**

In `ui/src/app.rs`, near the `pause`/`resume` closures (~180-207), add:

```rust
    let promote_remote = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(&ws, &Command::PromoteToRemote { session: SessionId(uuid) });
        }
    };
    let demote_local = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(&ws, &Command::DemoteToLocal { session: SessionId(uuid) });
        }
    };
```

- [ ] **Step 6: Render the buttons**

In `ui/src/app.rs`, import the predicates — change the `view_model` use line to include them:

```rust
use crate::view_model::{
    can_demote, can_promote, client_error_row, describe_event, error_row, ConnState, LogRow,
};
```

In the `view!`, add a small control row (e.g. directly above `<PromptBar ...>`):

```rust
            <div class="handover">
                <button
                    on:click=move |_| promote_remote()
                    disabled=move || !can_promote(&conn.get(), &capabilities.get(), turn_running.get())
                >"Promote to remote"</button>
                <button
                    on:click=move |_| demote_local()
                    disabled=move || !can_demote(&conn.get(), &capabilities.get(), turn_running.get())
                >"Demote to local"</button>
            </div>
```

- [ ] **Step 7: Verify wasm build + host tests**

Run: `cd ui && cargo build --target wasm32-unknown-unknown && cargo test`
Expected: PASS — compiles to wasm; all host tests green.

- [ ] **Step 8: Commit**

```bash
git add ui/src/app.rs
git commit -m "feat(ui): Promote/Demote buttons + handover reconnect on Promoted/Demoted"
```

---

## Task 9: Engine E2E — loopback round-trip + unsupported posture

**Files:**
- Modify: `crates/engine/tests/serve.rs` (add a promote-enabled server helper + two tests, reusing `authed_request`/`next_json`/`next_json_opt`)

- [ ] **Step 1: Add a promote-enabled server helper**

In `crates/engine/tests/serve.rs`, add (model it on `start_server`, but pass a `PromoteConfig`):

```rust
/// Start a serve app with `--promote-loopback` enabled. Returns the bound port and the tempdir.
async fn start_promote_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
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
    let promote = Some(otto_engine::PromoteConfig {
        token: TOKEN.to_string(),
        base_dir: dir.path().join("remotes"),
    });
    let app = serve_app(service, TOKEN.to_string(), test_capabilities(), promote);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

/// Build an authed client request to an absolute `ws://…` endpoint (the promoted target), with a
/// session + last_seq query for replay.
fn authed_endpoint_request(
    endpoint: &str,
    session: &str,
    last_seq: u64,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let url = format!("{endpoint}/ws?session={session}&last_seq={last_seq}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}
```

- [ ] **Step 2: Write the unsupported-posture test**

Add to `crates/engine/tests/serve.rs`:

```rust
#[tokio::test]
async fn promote_without_flag_replies_error() {
    let (port, _dir) = start_server().await; // no promote config
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut saw_error = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "error" {
            saw_error = true;
            break;
        }
    }
    assert!(saw_error, "promote without --promote-loopback must reply error");
}
```

- [ ] **Step 3: Run the unsupported test (it should pass already)**

Run: `cargo test -p otto-engine --test serve promote_without_flag_replies_error`
Expected: PASS (routing from Task 5 + no config → error reply).

- [ ] **Step 4: Write the round-trip test**

Add to `crates/engine/tests/serve.rs`:

```rust
#[tokio::test]
async fn promote_then_demote_round_trip_preserves_session() {
    let (port, _dir) = start_promote_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect local");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    // Run a turn on the local engine; track the highest seq seen.
    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();
    let mut last_seq = 0u64;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            last_seq = frame["event"]["seq"].as_u64().unwrap();
            if frame["event"]["kind"]["TurnComplete"].is_object()
                || frame["event"]["kind"] == serde_json::json!("TurnComplete")
            {
                // TurnComplete may serialize with a payload object; either way we've reached it.
            }
        }
        // Stop once the turn is done: TurnComplete is the last emitted event.
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            break;
        }
    }

    // Promote.
    let cmd = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();
    let mut remote_endpoint = String::new();
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "promoted" {
            remote_endpoint = frame["endpoint"].as_str().unwrap().to_string();
            break;
        }
    }
    assert!(remote_endpoint.starts_with("ws://"), "got endpoint {remote_endpoint}");

    // Reconnect to the remote; it must report engine_remote = true and replay the session.
    let (mut ws_r, _) = tokio_tungstenite::connect_async(authed_endpoint_request(
        &remote_endpoint,
        &session,
        last_seq,
    ))
    .await
    .expect("connect remote");
    let ready_r: Value = next_json(&mut ws_r).await;
    assert_eq!(ready_r["type"], "ready");
    assert_eq!(ready_r["capabilities"]["engine_remote"], true);

    // Demote back to local.
    let cmd = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws_r.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();
    let mut local_endpoint = String::new();
    while let Some(frame) = next_json_opt(&mut ws_r).await {
        if frame["type"] == "demoted" {
            local_endpoint = frame["endpoint"].as_str().unwrap().to_string();
            break;
        }
    }
    assert!(local_endpoint.starts_with("ws://"), "got endpoint {local_endpoint}");

    // Reconnect to the demoted-local engine; it must report engine_remote = false.
    let (mut ws_l, _) = tokio_tungstenite::connect_async(authed_endpoint_request(
        &local_endpoint,
        &session,
        0,
    ))
    .await
    .expect("connect demoted-local");
    let ready_l: Value = next_json(&mut ws_l).await;
    assert_eq!(ready_l["capabilities"]["engine_remote"], false);
    // Continuity: replaying from seq 0 yields the original session's events.
    let mut saw_event = false;
    while let Some(frame) = next_json_opt(&mut ws_l).await {
        if frame["type"] == "event" {
            saw_event = true;
            break;
        }
    }
    assert!(saw_event, "demoted-local engine should replay the session history");
}
```

- [ ] **Step 5: Run the round-trip test**

Run: `cargo test -p otto-engine --test serve promote_then_demote_round_trip_preserves_session`
Expected: PASS. If the `TurnComplete` detection loop hangs, inspect a raw frame with `eprintln!("{frame}")` to confirm the exact JSON shape of `event.kind` for `TurnComplete` and adjust the match (the orchestrator emits `EventKind::TurnComplete { ok }`, which serializes as `{"TurnComplete":{"ok":true}}`).

- [ ] **Step 6: Commit**

```bash
git add crates/engine/tests/serve.rs
git commit -m "test(engine): promote/demote round-trip E2E + unsupported posture"
```

---

## Final verification

- [ ] **Full workspace suite:** `cargo test --workspace` → all green.
- [ ] **UI:** `cd ui && cargo test && cargo build --target wasm32-unknown-unknown` → green.
- [ ] **Lint/format:** `cargo fmt --all && cargo clippy --workspace --all-targets` and `cd ui && cargo clippy --all-targets` → clean.
- [ ] **Docs:** update the roadmap row **F** in `docs/superpowers/specs/2026-06-17-ui-roadmap.md` to ✅ with a one-line "what shipped" summary (mirroring the A–E entries), and link this plan + the design spec. Commit: `docs: record sub-project F (promote-to-remote UX) shipped`.

## Spec coverage check

- Protocol `PromoteToRemote`/`DemoteToLocal` + `Promoted`/`Demoted` → Task 1.
- `EngineService::workspace()` → Task 2.
- `PromoteConfig`, `app()` param, `ServeState` registry → Tasks 3, 5.
- `LoopbackTarget` `engine_remote` + nested config → Task 4.
- `--promote-loopback` flag → Task 6.
- UI buttons + reconnect + predicates → Tasks 7, 8.
- Loopback E2E + unsupported posture + determinism (offline never builds a `PromoteConfig`) → Task 9 + Final verification.
- Accepted limitation (handles live until process exit) — documented in the design spec; no teardown task by design.
