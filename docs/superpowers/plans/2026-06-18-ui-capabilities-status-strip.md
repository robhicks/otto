# UI Capabilities + Status Strip (Sub-project B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the running engine's capabilities visible in the UI by emitting a `CapabilitiesManifest` in the `Ready` frame and replacing the app-shell status line with a strip that surfaces engine/LLM/sandbox state and renders degradation (offline-deterministic LLM, absent sandbox) visibly.

**Architecture:** Additive protocol change (extend `CapabilitiesManifest` with `remote_llm`; carry the manifest in `ServerMessage::Ready`). A wiring-layer `build_capabilities()` derives the manifest from the same env that `build_router`/`session_config` read, threaded transport-side through `serve_app` → `ServeState` → the `Ready` frame (the embedded `EngineService::new` path is untouched). The UI stores the manifest in a signal set on `Ready` and renders an extended status strip whose capability segments derive from a pure, host-tested function.

**Tech Stack:** Rust (edition 2024), `serde`/`serde_json`, axum WebSocket transport, Leptos CSR (Rust→WASM, built with `trunk`), `tokio_tungstenite` for the serve integration tests.

**Spec:** `docs/superpowers/specs/2026-06-17-ui-capabilities-status-strip-design.md`

---

## File Structure

**Protocol (`crates/protocol/src/lib.rs`)** — wire types. Add `remote_llm` to `CapabilitiesManifest`; add `capabilities` to `ServerMessage::Ready`. Update/extend the existing `ServerMessage` tests.

**Engine wiring (`crates/engine/src/`)**
- `lib.rs` — add `capabilities_from_env()` (pure) + `build_capabilities()` (env reader), beside `build_router`/`session_config`. Pure unit test for the mapping (no process-env mutation → no race with the existing env test).
- `serve.rs` — `ServeState` gains a `capabilities` field; `app()`/`serve_app()` gain a `capabilities` parameter; `handle_socket` frames it into `Ready`.
- `main.rs` — the `serve` path computes `build_capabilities()` and passes it to `serve_app`.

**Engine tests (`crates/engine/tests/serve.rs`)** — thread a fixed test manifest through the two `serve_app` call sites; add a test asserting the `Ready` frame carries the manifest.

**UI (`ui/src/`)**
- `view_model.rs` — `CapSegment` struct + pure `capability_segments()` derivation + host tests.
- `components/status_line.rs` — `StatusLine` takes the new `capabilities` signal and renders the capability segments after the transport half.
- `app.rs` — a `capabilities: RwSignal<Option<CapabilitiesManifest>>` signal, set on `Ready`, cleared on connect-start and disconnect, passed to `StatusLine`.
- `style.css` — `.cap` / `.cap-degraded` classes + a `--warn` color var.

**Docs** — roadmap row B and the CLAUDE.md UI paragraph updated at the end.

---

## Task 1: Protocol — extend `CapabilitiesManifest` and `Ready`

**Files:**
- Modify: `crates/protocol/src/lib.rs` (the `CapabilitiesManifest` struct ~line 95, the `ServerMessage` enum ~line 67, and the `#[cfg(test)] mod tests` block ~line 102)

> **Note on build state:** This task changes a type that `crates/engine` consumes, so `cargo build --workspace` will be RED until Task 2 fixes the engine producer. That is expected. The commit gate for *this* task is the protocol crate's own tests: `cargo test -p otto-protocol`. Task 2 follows immediately and restores the workspace to green.

- [ ] **Step 1: Update the `Ready` tag test and add a manifest round-trip test (expect compile failure)**

In `crates/protocol/src/lib.rs`, replace the existing `server_message_ready_has_snake_case_tag` test with the version below, and add the new manifest round-trip test right after it. `CapabilitiesManifest` is already in scope via `use super::*;`.

```rust
    #[test]
    fn server_message_ready_has_snake_case_tag_and_capabilities() {
        let session = SessionId::new();
        let msg = ServerMessage::Ready {
            session,
            capabilities: CapabilitiesManifest {
                engine_remote: false,
                local_llm: true,
                remote_llm: false,
                sandbox: true,
            },
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(v["type"], "ready");
        // SessionId is a newtype over Uuid → serializes as a bare string.
        assert_eq!(v["session"], serde_json::json!(session.0.to_string()));
        // The manifest is a nested sibling object; lock its shape.
        assert_eq!(v["capabilities"]["engine_remote"], false);
        assert_eq!(v["capabilities"]["local_llm"], true);
        assert_eq!(v["capabilities"]["remote_llm"], false);
        assert_eq!(v["capabilities"]["sandbox"], true);
    }

    #[test]
    fn capabilities_manifest_round_trips_with_remote_llm() {
        let m = CapabilitiesManifest {
            engine_remote: true,
            local_llm: false,
            remote_llm: true,
            sandbox: false,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: CapabilitiesManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }
```

- [ ] **Step 2: Run the protocol tests to verify they fail**

Run: `cargo test -p otto-protocol`
Expected: FAIL — compile error, `ServerMessage::Ready` has no field `capabilities` and `CapabilitiesManifest` has no field `remote_llm`.

- [ ] **Step 3: Add `remote_llm` to the manifest and `capabilities` to `Ready`**

In `crates/protocol/src/lib.rs`, change the `ServerMessage::Ready` variant:

```rust
pub enum ServerMessage {
    Ready {
        session: SessionId,
        capabilities: CapabilitiesManifest,
    },
    Event { event: Event },
    Error { message: String },
}
```

And add `remote_llm` to `CapabilitiesManifest` (between `local_llm` and `sandbox`):

```rust
pub struct CapabilitiesManifest {
    pub engine_remote: bool,
    pub local_llm: bool,
    /// A remote provider (Anthropic) is configured. Distinct from `local_llm` (Ollama);
    /// with both false the engine is on its deterministic offline path (no real LLM).
    pub remote_llm: bool,
    pub sandbox: bool,
}
```

- [ ] **Step 4: Run the protocol tests to verify they pass**

Run: `cargo test -p otto-protocol`
Expected: PASS (all protocol tests, including the two edited/added).

- [ ] **Step 5: Commit**

```bash
git add crates/protocol/src/lib.rs
git commit -m "feat(protocol): carry CapabilitiesManifest in Ready; add remote_llm"
```

---

## Task 2: Engine — `build_capabilities()` and thread the manifest into `Ready`

**Files:**
- Modify: `crates/engine/src/lib.rs` (add the helpers + a unit test in the existing `#[cfg(test)] mod tests`)
- Modify: `crates/engine/src/serve.rs` (`ServeState`, `app()`, `handle_socket`, the `otto_protocol` import)
- Modify: `crates/engine/src/main.rs` (the `serve` path, ~lines 206-216)
- Modify: `crates/engine/tests/serve.rs` (both `serve_app` call sites; add a manifest assertion test)

- [ ] **Step 1: Write the failing `capabilities_from_env` unit test**

In `crates/engine/src/lib.rs`, inside the existing `#[cfg(test)] mod tests { ... }` block (after the `default_build_router_is_offline_and_deterministic` test), add:

```rust
    #[test]
    fn capabilities_from_env_maps_flags() {
        // Pure mapping — takes raw env inputs, touches no process-global env, so it does
        // NOT race the env-reading router test in this same binary.
        // Nothing set → fully offline, local engine, no sandbox.
        assert_eq!(
            capabilities_from_env(None, None, false),
            CapabilitiesManifest {
                engine_remote: false,
                local_llm: false,
                remote_llm: false,
                sandbox: false,
            }
        );
        // OTTO_OLLAMA must equal exactly "1" to count as a local LLM.
        assert!(capabilities_from_env(Some("1"), None, false).local_llm);
        assert!(!capabilities_from_env(Some("0"), None, false).local_llm);
        // A non-empty ANTHROPIC_API_KEY means a remote LLM; an empty one does not.
        assert!(capabilities_from_env(None, Some("sk-xyz"), false).remote_llm);
        assert!(!capabilities_from_env(None, Some(""), false).remote_llm);
        // sandbox passes through unchanged.
        assert!(capabilities_from_env(None, None, true).sandbox);
    }
```

`CapabilitiesManifest` is available in the test module via `use super::*;` once Step 2's import is added; the test still won't compile until Step 2 defines `capabilities_from_env`.

- [ ] **Step 2: Run the engine lib test to verify it fails**

Run: `cargo test -p otto-engine --lib capabilities_from_env_maps_flags`
Expected: FAIL — compile error, `capabilities_from_env` not found (and possibly `CapabilitiesManifest` unresolved).

- [ ] **Step 3: Implement `capabilities_from_env` + `build_capabilities`**

In `crates/engine/src/lib.rs`, ensure `CapabilitiesManifest` is imported. Add it to the existing `otto_protocol` use (or add a new `use`):

```rust
use otto_protocol::CapabilitiesManifest;
```

Then add the two functions next to `session_config` (after it):

```rust
/// Pure capability derivation from raw env inputs. Kept separate from `build_capabilities`
/// so the mapping is unit-testable without mutating process-global env (which would race the
/// env-reading tests in this binary). `engine_remote` is always false here: `otto serve` is
/// the local engine; the promote path (sub-project F) provisions a separate remote engine
/// that computes its own manifest with `engine_remote = true`.
fn capabilities_from_env(
    otto_ollama: Option<&str>,
    anthropic_key: Option<&str>,
    sandbox: bool,
) -> CapabilitiesManifest {
    CapabilitiesManifest {
        engine_remote: false,
        local_llm: otto_ollama == Some("1"),
        remote_llm: anthropic_key.map(|k| !k.is_empty()).unwrap_or(false),
        sandbox,
    }
}

/// Derive the running engine's capabilities from the environment `build_router` reads, plus
/// the OS sandbox probe. Lives in the wiring layer (not core) because it reads `OTTO_*` /
/// `ANTHROPIC_API_KEY`. Mirrors `session_config`'s predicates so a session's recorded config
/// and its reported capabilities stay consistent.
pub fn build_capabilities() -> CapabilitiesManifest {
    capabilities_from_env(
        std::env::var("OTTO_OLLAMA").ok().as_deref(),
        std::env::var("ANTHROPIC_API_KEY").ok().as_deref(),
        os_sandbox_available(),
    )
}
```

`os_sandbox_available` is already imported in `lib.rs` (it is re-exported there). If the compiler reports it unresolved in this scope, import it: `use otto_tools::os_sandbox_available;`.

- [ ] **Step 4: Run the engine lib test to verify it passes**

Run: `cargo test -p otto-engine --lib capabilities_from_env_maps_flags`
Expected: PASS.

- [ ] **Step 5: Thread the manifest through `serve.rs`**

In `crates/engine/src/serve.rs`:

Add `CapabilitiesManifest` to the protocol import (line ~16):

```rust
use otto_protocol::{
    CapabilitiesManifest, Command, Event, ServerMessage, SessionId, WorkspaceRequest,
};
```

Add the field to `ServeState` (~line 35):

```rust
struct ServeState {
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
}
```

Add the parameter to `app` and store it (~line 69):

```rust
pub fn app(
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
) -> AxumRouter {
    assert!(!token.is_empty(), "serve token must not be empty");
    let state = Arc::new(ServeState {
        service,
        token,
        capabilities,
    });
    AxumRouter::new()
        .route("/ws", get(ws_handler))
        .route("/workspace", post(workspace_handler))
        .with_state(state)
}
```

Frame it in `handle_socket` (~line 170):

```rust
    if send_msg(
        &mut socket,
        &ServerMessage::Ready {
            session,
            capabilities: state.capabilities.clone(),
        },
    )
    .await
    .is_err()
    {
        return;
    }
```

- [ ] **Step 6: Pass `build_capabilities()` in the `serve` binary path**

In `crates/engine/src/main.rs`, in the `serve` path right after the `EngineService::new(...)` line (~line 215):

```rust
    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools);
    let capabilities = otto_engine::build_capabilities();
    let app = serve_app(service, token, capabilities);
```

- [ ] **Step 7: Update the serve integration tests for the new arity + assert the manifest**

In `crates/engine/tests/serve.rs`:

Add a fixed test-manifest helper (place it near `start_server`, after the `TOKEN` const):

```rust
/// A fixed manifest the test server reports, so the assertion below is deterministic and
/// also proves non-default values are threaded through (not hardcoded false).
fn test_capabilities() -> otto_protocol::CapabilitiesManifest {
    otto_protocol::CapabilitiesManifest {
        engine_remote: false,
        local_llm: true,
        remote_llm: false,
        sandbox: true,
    }
}
```

Update **both** `serve_app` call sites to pass it — in `start_server` (~line 49) and `start_tls_server` (~line 215):

```rust
    let app = serve_app(service, TOKEN.to_string(), test_capabilities());
```

Add a new test (place it after `streams_a_turn_then_reconnects_with_replay`):

```rust
#[tokio::test]
async fn ready_frame_carries_capabilities() {
    let (port, _dir) = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");

    let ready: Value = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let caps = &ready["capabilities"];
    assert!(caps.is_object(), "Ready must carry a capabilities object");
    assert_eq!(caps["engine_remote"], false);
    assert_eq!(caps["local_llm"], true);
    assert_eq!(caps["remote_llm"], false);
    assert_eq!(caps["sandbox"], true);
}
```

- [ ] **Step 8: Build the workspace and run the engine + protocol tests to verify green**

Run: `cargo build --workspace`
Expected: PASS (the workspace compiles again).

Run: `cargo test -p otto-protocol -p otto-engine`
Expected: PASS, including `ready_frame_carries_capabilities`, `capabilities_from_env_maps_flags`, and the unchanged streaming/replay/TLS tests.

- [ ] **Step 9: Commit**

```bash
git add crates/engine/src/lib.rs crates/engine/src/serve.rs crates/engine/src/main.rs crates/engine/tests/serve.rs
git commit -m "feat(engine): derive CapabilitiesManifest and emit it in the Ready frame"
```

---

## Task 3: UI — capabilities signal + status strip + degradation styling

**Files:**
- Modify: `ui/src/view_model.rs` (add `CapSegment` + `capability_segments` + host tests)
- Modify: `ui/src/components/status_line.rs` (render the capability segments)
- Modify: `ui/src/app.rs` (the `capabilities` signal; set/clear; pass to `StatusLine`)
- Modify: `ui/style.css` (capability classes + `--warn`)

All UI commands run from inside `ui/`.

- [ ] **Step 1: Write the failing `capability_segments` host tests**

In `ui/src/view_model.rs`, add `use otto_protocol::CapabilitiesManifest;` at the top (next to the existing `use otto_protocol::EventKind;`). Then add these tests inside the existing `#[cfg(test)] mod tests` block:

```rust
    fn manifest(engine_remote: bool, local_llm: bool, remote_llm: bool, sandbox: bool) -> CapabilitiesManifest {
        CapabilitiesManifest { engine_remote, local_llm, remote_llm, sandbox }
    }

    #[test]
    fn offline_engine_marks_llm_segment_degraded() {
        let segs = capability_segments(&manifest(false, false, false, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "offline (deterministic)");
        assert!(llm.degraded);
        let engine = segs.iter().find(|s| s.label == "engine").unwrap();
        assert_eq!(engine.value, "local");
        assert!(!engine.degraded);
        let sandbox = segs.iter().find(|s| s.label == "sandbox").unwrap();
        assert!(!sandbox.degraded);
    }

    #[test]
    fn remote_llm_is_not_degraded() {
        let segs = capability_segments(&manifest(false, false, true, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "remote");
        assert!(!llm.degraded);
    }

    #[test]
    fn local_and_remote_llm_labels_both() {
        let segs = capability_segments(&manifest(false, true, true, true));
        let llm = segs.iter().find(|s| s.label == "LLM").unwrap();
        assert_eq!(llm.value, "local+remote");
        assert!(!llm.degraded);
    }

    #[test]
    fn sandbox_off_is_degraded() {
        let segs = capability_segments(&manifest(false, true, false, false));
        let sandbox = segs.iter().find(|s| s.label == "sandbox").unwrap();
        assert_eq!(sandbox.value, "off");
        assert!(sandbox.degraded);
    }

    #[test]
    fn engine_remote_labels_remote() {
        let segs = capability_segments(&manifest(true, true, false, true));
        let engine = segs.iter().find(|s| s.label == "engine").unwrap();
        assert_eq!(engine.value, "remote");
        assert!(!engine.degraded);
    }
```

- [ ] **Step 2: Run the UI host tests to verify they fail**

Run: `cd ui && cargo test capability`
Expected: FAIL — compile error, `CapSegment` / `capability_segments` not found.

- [ ] **Step 3: Implement `CapSegment` + `capability_segments`**

In `ui/src/view_model.rs`, add (after the `LogRow` definitions, before or after `status_label` — any top-level position):

```rust
/// One capability segment in the status strip: a label, its current value, and whether it
/// represents a degraded/lost capability (rendered in the warning style).
#[derive(Clone, PartialEq, Debug)]
pub struct CapSegment {
    pub label: &'static str,
    pub value: String,
    pub degraded: bool,
}

/// Derive the engine/LLM/sandbox segments from the manifest. The two degradations the strip
/// exists to surface: a fully-offline (deterministic) LLM, and an absent sandbox (bash off).
pub fn capability_segments(m: &CapabilitiesManifest) -> Vec<CapSegment> {
    let engine = CapSegment {
        label: "engine",
        value: if m.engine_remote { "remote" } else { "local" }.to_string(),
        degraded: false,
    };
    let llm_value = match (m.local_llm, m.remote_llm) {
        (true, true) => "local+remote",
        (false, true) => "remote",
        (true, false) => "local",
        (false, false) => "offline (deterministic)",
    };
    let llm = CapSegment {
        label: "LLM",
        value: llm_value.to_string(),
        degraded: !m.local_llm && !m.remote_llm,
    };
    let sandbox = CapSegment {
        label: "sandbox",
        value: if m.sandbox { "on" } else { "off" }.to_string(),
        degraded: !m.sandbox,
    };
    vec![engine, llm, sandbox]
}
```

- [ ] **Step 4: Run the UI host tests to verify they pass**

Run: `cd ui && cargo test`
Expected: PASS — the new `capability_segments` tests plus the existing `view_model`/`url` tests.

- [ ] **Step 5: Render the segments in `StatusLine`**

Replace the entire contents of `ui/src/components/status_line.rs` with:

```rust
use leptos::prelude::*;
use otto_protocol::CapabilitiesManifest;

use crate::view_model::{capability_segments, short_session, status_label, ConnState};

/// Status strip: the transport half (connection state · session · seq) plus the engine/LLM/
/// sandbox capability segments. Capability segments render only while connected with a
/// manifest; degraded segments carry the `cap-degraded` class so lost capability is visible.
#[component]
pub fn StatusLine(
    conn: RwSignal<ConnState>,
    last_seq: RwSignal<Option<u64>>,
    capabilities: RwSignal<Option<CapabilitiesManifest>>,
) -> impl IntoView {
    view! {
        <div class="status">
            {move || {
                let c = conn.get();
                let sess = match &c {
                    ConnState::Connected { session } => short_session(session),
                    _ => "-".to_string(),
                };
                let seq = last_seq.get().map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());
                format!("status: {} · {} · seq {}", status_label(&c), sess, seq)
            }}
            {move || {
                // Only while connected AND a manifest is present — never show a stale one.
                let connected = matches!(conn.get(), ConnState::Connected { .. });
                capabilities.get().filter(|_| connected).map(|m| {
                    let segs = capability_segments(&m);
                    view! {
                        <span class="cap-group">
                            <span class="cap-sep">" | "</span>
                            {segs
                                .into_iter()
                                .enumerate()
                                .map(|(i, s)| {
                                    let cls = if s.degraded { "cap cap-degraded" } else { "cap" };
                                    let text = format!("{}: {}", s.label, s.value);
                                    let sep = if i == 0 { "" } else { " · " };
                                    view! { <span class="cap-sep">{sep}</span><span class=cls>{text}</span> }
                                })
                                .collect_view()}
                        </span>
                    }
                })
            }}
        </div>
    }
}
```

- [ ] **Step 6: Wire the `capabilities` signal in `app.rs`**

In `ui/src/app.rs`:

Add `CapabilitiesManifest` to the protocol import (line 2):

```rust
use otto_protocol::{CapabilitiesManifest, Command, ServerMessage, SessionId};
```

Add the signal next to the other connection signals (after `let socket = ...`, ~line 22):

```rust
    let capabilities = RwSignal::new(None::<CapabilitiesManifest>); // set on Ready, cleared on (re)connect/disconnect
```

In `connect`, clear it when a fresh attempt starts. Change the existing `conn.set(ConnState::Connecting);` line to:

```rust
        conn.set(ConnState::Connecting);
        capabilities.set(None);
```

In the `on_msg` `Ready` arm, capture and store the manifest. Replace:

```rust
            Ok(ServerMessage::Ready { session: s }) => {
                let id = s.0.to_string();
                session.set(Some(id.clone()));
                conn.set(ConnState::Connected { session: id });
            }
```

with:

```rust
            Ok(ServerMessage::Ready { session: s, capabilities: caps }) => {
                let id = s.0.to_string();
                session.set(Some(id.clone()));
                capabilities.set(Some(caps));
                conn.set(ConnState::Connected { session: id });
            }
```

In `disconnect`, clear it. Change the body to also reset capabilities:

```rust
    let disconnect = move || {
        if let Some(ws) = socket.get() {
            let _ = ws.close();
        }
        socket.set(None);
        capabilities.set(None);
        conn.set(ConnState::Disconnected);
    };
```

Pass the signal to `StatusLine` in the `view!`:

```rust
            <StatusLine conn=conn last_seq=last_seq capabilities=capabilities />
```

- [ ] **Step 7: Add the capability styling**

In `ui/style.css`, add `--warn` to `:root` (after `--error`):

```css
  --error: #f7768e;
  --warn: #e0af68;
```

And add the capability classes (after the `.row-error` line):

```css
.cap { color: var(--fg); }
.cap-degraded { color: var(--warn); font-weight: bold; }
.cap-sep { color: var(--dim); }
```

- [ ] **Step 8: Verify the UI builds for wasm and host tests pass**

Run: `cd ui && cargo test`
Expected: PASS (host-side `view_model`/`url` tests).

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: PASS — the WASM bundle compiles with the new component signature and signal.

- [ ] **Step 9: Commit**

```bash
git add ui/src/view_model.rs ui/src/components/status_line.rs ui/src/app.rs ui/style.css
git commit -m "feat(ui): capabilities status strip with visible degradation"
```

---

## Task 4: Manual acceptance + docs sync

**Files:**
- Modify: `docs/superpowers/specs/2026-06-17-ui-roadmap.md` (status line + row B)
- Modify: `CLAUDE.md` (the UI paragraph)

- [ ] **Step 1: Manual acceptance against a running engine**

This slice's component render is verified in the browser (the spec's definition of done). With no model env vars set:

```bash
OTTO_TOKEN=dev cargo run -p otto-engine -- serve --port 8787 &
cd ui && trunk serve
```

Open the UI, Connect, and confirm:
1. The strip shows `engine: local · LLM: offline (deterministic) · sandbox: <on|off>`, with the LLM segment (and sandbox, if no backend) rendered amber.
2. Restart serve with `ANTHROPIC_API_KEY=sk-... OTTO_TOKEN=dev cargo run -p otto-engine -- serve --port 8787` → reconnect → LLM segment shows `remote`, not amber.
3. Restart with `OTTO_OLLAMA=1 OTTO_TOKEN=dev cargo run -p otto-engine -- serve --port 8787` → LLM segment shows `local`.
4. Disconnect → the capability segments disappear; only `status: disconnected · - · seq -` remains.

Stop the background `otto serve` when done (`kill %1` or `fg` then Ctrl-C).

- [ ] **Step 2: Update the roadmap**

In `docs/superpowers/specs/2026-06-17-ui-roadmap.md`:

Update the status line (line 4) to record B shipping, e.g.:

```markdown
**Status:** Approved decomposition — **Sub-projects A–B shipped** (A: PR #47, 2026-06-18; B: capabilities + status strip, 2026-06-18); C–F pending.
```

Update the table row for **B** to mark it shipped (mirror how row A reads), linking the spec and this plan:

```markdown
| **B** ✅ | **Capabilities + status strip** *(shipped — [design](2026-06-17-ui-capabilities-status-strip-design.md) · [plan](../plans/2026-06-18-ui-capabilities-status-strip.md))* | Engine emits `CapabilitiesManifest` on connect; UI status strip shows engine/LLM/sandbox state with **visible** degradation. | **Done:** extended `CapabilitiesManifest` with `remote_llm`; the `Ready` frame now carries the manifest; `build_capabilities()` derives it from the serve environment. |
```

- [ ] **Step 3: Update CLAUDE.md**

In `CLAUDE.md`, update the UI paragraph (the "The **UI has its first slice**" sentence) to note B shipped — that the `Ready` frame now carries a `CapabilitiesManifest` (with the added `remote_llm` field) and the UI renders a capabilities status strip with visible degradation. Keep it to one or two added clauses consistent with the surrounding prose; do not rewrite the paragraph.

- [ ] **Step 4: Final full-workspace verification**

Run: `cargo test --workspace`
Expected: PASS (offline/deterministic, no network).

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets`
Expected: formatting clean; clippy reports no new warnings.

Run: `cd ui && cargo test && cargo build --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-06-17-ui-roadmap.md CLAUDE.md
git commit -m "docs: mark sub-project B (capabilities status strip) shipped"
```
