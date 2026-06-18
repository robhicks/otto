# otto UI — Sub-project A: App Shell + Live Session — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up a browser-first Leptos CSR app (`ui/`) that connects to a running `otto serve` over WebSocket, sends a prompt, renders the live event stream, aborts a turn, and reconnects with `last_seq` replay — plus the two small additive engine changes that make a browser client possible.

**Architecture:** Two additive engine changes first (move the `ServerMessage` WS-framing enum into the shared `protocol` crate so the UI can deserialize it; accept the bearer token via a `?token=` query param since a browser `WebSocket` can't set headers). Then a standalone `ui/` crate — **excluded from the workspace** so `cargo build --workspace` and the offline determinism suite stay untouched — depending only on `../crates/protocol`, built to WASM with `trunk`. Pure URL/seq logic lives in a browser-free module unit-tested on the host; web-facing code (WebSocket wrapper, components) is compile-gated against the wasm target and verified manually in a browser.

**Tech Stack:** Rust 2024 workspace (engine side), Leptos 0.8 (`csr` feature) + `trunk` + `web-sys`/`wasm-bindgen` (UI side), `otto-protocol` as the shared wire-types crate.

**Spec:** `docs/superpowers/specs/2026-06-17-ui-shell-live-session-design.md`
**Roadmap:** `docs/superpowers/specs/2026-06-17-ui-roadmap.md` (sub-project A)

---

## File structure

**Engine side (existing workspace crates — modified):**
- `crates/protocol/src/lib.rs` — gains `pub enum ServerMessage` (moved from `serve.rs`) + wire-shape tests.
- `crates/engine/src/serve.rs` — imports `ServerMessage` from `protocol`; `ConnectParams` gains `token`; `/ws` auth accepts header **or** `?token=`.
- `crates/engine/tests/serve.rs` — adds query-token accept/reject integration tests.
- `Cargo.toml` (workspace root) — adds `exclude = ["ui"]`.

**UI side (new standalone crate `ui/`):**
- `ui/Cargo.toml` — standalone package `otto-ui`; deps on `../crates/protocol`, leptos, web-sys, etc.
- `ui/.cargo/config.toml` — sets the getrandom wasm backend cfg for the wasm target.
- `ui/Trunk.toml` — trunk serve config.
- `ui/index.html` — trunk entry; links `style.css`.
- `ui/style.css` — minimal terminal-like styling.
- `ui/src/main.rs` — declares modules; mounts `App`.
- `ui/src/url.rs` — **pure, browser-free** helpers: `build_ws_url`, `should_apply`, `advance_last_seq` (host unit tests).
- `ui/src/view_model.rs` — **pure** view helpers: `ConnState`, `LogRow`, `describe_event`, `status_label`, `short_session` (host unit tests).
- `ui/src/ws.rs` — `web_sys::WebSocket` wrapper: `open_ws`, `send_command`.
- `ui/src/app.rs` — `App` root component: signals, connection logic, composition.
- `ui/src/components/mod.rs` — re-exports the four components.
- `ui/src/components/status_line.rs` — `StatusLine`.
- `ui/src/components/event_log.rs` — `EventLog` (auto-scroll).
- `ui/src/components/prompt_bar.rs` — `PromptBar` (Send/Abort).
- `ui/src/components/connection_form.rs` — `ConnectionForm` (URL/token, Connect/Disconnect).

---

## Task 1: Move `ServerMessage` into `protocol`

The UI may depend only on `protocol` but must deserialize the WS framing enum, which today is private in `serve.rs`. Move it, add `Deserialize`, and lock the wire shape with a test. No JSON change — existing clients/tests stay green.

**Files:**
- Modify: `crates/protocol/src/lib.rs`
- Modify: `crates/engine/src/serve.rs:21-28` (remove local enum), `:16` (import)
- Test: `crates/protocol/src/lib.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the failing wire-shape test to `protocol`**

In `crates/protocol/src/lib.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn server_message_ready_has_snake_case_tag() {
        let session = SessionId::new();
        let msg = ServerMessage::Ready { session };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(v["type"], "ready");
        // SessionId is a newtype over Uuid → serializes as a bare string.
        assert_eq!(v["session"], serde_json::json!(session.0.to_string()));
    }

    #[test]
    fn server_message_event_round_trips() {
        let msg = ServerMessage::Event {
            event: Event {
                seq: 3,
                session: SessionId::new(),
                kind: EventKind::TurnComplete { ok: true },
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ServerMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }
```

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cargo test -p otto-protocol server_message`
Expected: FAIL — `cannot find type ServerMessage in this scope`.

- [ ] **Step 3: Add `ServerMessage` to `protocol`**

In `crates/protocol/src/lib.rs`, after the `Event` struct (around line 61), add:

```rust
/// Outbound WS framing for the engine→frontend stream. Reuses the core `Event`;
/// `Ready`/`Error` are transport framing. Shared so browser clients can deserialize it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Ready { session: SessionId },
    Event { event: Event },
    Error { message: String },
}
```

- [ ] **Step 4: Run the protocol tests to verify they pass**

Run: `cargo test -p otto-protocol`
Expected: PASS (all tests, including the two new ones).

- [ ] **Step 5: Make `serve.rs` import the moved enum**

In `crates/engine/src/serve.rs`, delete the local definition (lines 21-28):

```rust
/// Outbound WS frame. Reuses the core `Event`; `Ready`/`Error` are transport framing.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Ready { session: SessionId },
    Event { event: Event },
    Error { message: String },
}
```

Then update the `otto_protocol` import (line 16) from:

```rust
use otto_protocol::{Command, Event, SessionId, WorkspaceRequest};
```

to:

```rust
use otto_protocol::{Command, Event, ServerMessage, SessionId, WorkspaceRequest};
```

(All existing `ServerMessage::…` references in `serve.rs` stay unchanged.)

- [ ] **Step 6: Verify the whole workspace still builds and tests green**

Run: `cargo test -p otto-engine --test serve`
Expected: PASS — the four existing serve tests (`streams_a_turn_then_reconnects_with_replay`, `rejects_missing_token`, `rejects_wrong_token`, `streams_a_turn_over_wss`) still pass with the relocated enum.

Run: `cargo build --workspace`
Expected: builds clean.

- [ ] **Step 7: Commit**

```bash
git add crates/protocol/src/lib.rs crates/engine/src/serve.rs
git commit -m "feat(protocol): move ServerMessage WS framing enum into protocol"
```

---

## Task 2: Accept the bearer token via `?token=` query param

A browser's native `WebSocket` constructor can't set an `Authorization` header. Extend `/ws` auth to accept the token from the header **or** a `?token=` query param (header takes precedence). `POST /workspace` is unchanged (header-only).

**Files:**
- Modify: `crates/engine/src/serve.rs:30-34` (`ConnectParams`), `:43-49` (auth), `:97-107` (`ws_handler`)
- Test: `crates/engine/tests/serve.rs`

- [ ] **Step 1: Write the failing integration tests**

In `crates/engine/tests/serve.rs`, after `rejects_wrong_token` (line 155), add:

```rust
#[tokio::test]
async fn authorizes_via_query_token() {
    let (port, _dir) = start_server().await;
    // No Authorization header — token rides in the query string (browser path).
    let url = format!("ws://127.0.0.1:{port}/ws?token={TOKEN}");
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("query-token connection must be accepted");
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
}

#[tokio::test]
async fn rejects_wrong_query_token() {
    let (port, _dir) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws?token=wrong-token");
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(result.is_err(), "a wrong query token must be rejected");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine --test serve query_token`
Expected: FAIL — `authorizes_via_query_token` fails (the upgrade is rejected because today only the header is checked).

- [ ] **Step 3: Add the `token` field to `ConnectParams`**

In `crates/engine/src/serve.rs`, change `ConnectParams` (lines 30-34) from:

```rust
#[derive(Deserialize, Default)]
struct ConnectParams {
    session: Option<String>,
    last_seq: Option<u64>,
}
```

to:

```rust
#[derive(Deserialize, Default)]
struct ConnectParams {
    session: Option<String>,
    last_seq: Option<u64>,
    /// Bearer token carried in the query string. A browser `WebSocket` can't set an
    /// `Authorization` header, so the `/ws` upgrade accepts the token here as well.
    /// Security: tokens in URLs can leak into server logs and browser history — acceptable
    /// for the loopback/dev posture of sub-project A; the header path stays the recommended
    /// one for non-browser clients. A later sub-project may move this to a WS subprotocol
    /// carrier or route through Tauri's Rust-side WS client (which can set headers).
    token: Option<String>,
}
```

- [ ] **Step 4: Add a query-aware auth helper and use it in `ws_handler`**

In `crates/engine/src/serve.rs`, after the existing `authorized` fn (ends line 49), add:

```rust
/// True if the `/ws` upgrade is authorized: a matching `Authorization: Bearer` header
/// (preferred) or a matching `?token=` query param (the browser path).
fn authorized_ws(headers: &HeaderMap, token: &str, params: &ConnectParams) -> bool {
    authorized(headers, token) || params.token.as_deref() == Some(token)
}
```

Then change the check at the top of `ws_handler` (line 103) from:

```rust
    if !authorized(&headers, &state.token) {
```

to:

```rust
    if !authorized_ws(&headers, &state.token, &params) {
```

(`workspace_handler` keeps calling the header-only `authorized` — unchanged.)

- [ ] **Step 5: Run the new tests to verify they pass**

Run: `cargo test -p otto-engine --test serve query_token`
Expected: PASS — both new tests green.

- [ ] **Step 6: Verify no regression in existing auth tests**

Run: `cargo test -p otto-engine --test serve`
Expected: PASS — all serve tests (existing header tests + new query tests) green.

- [ ] **Step 7: Commit**

```bash
git add crates/engine/src/serve.rs crates/engine/tests/serve.rs
git commit -m "feat(serve): accept /ws bearer token via ?token= query param"
```

---

## Task 3: Scaffold the standalone `ui/` crate

Create the crate skeleton: manifest, wasm getrandom backend config, trunk config, entry HTML/CSS, a minimal mountable `App`, and the pure `url.rs` module with host-runnable unit tests. Exclude `ui` from the workspace.

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `ui/Cargo.toml`, `ui/.cargo/config.toml`, `ui/Trunk.toml`, `ui/index.html`, `ui/style.css`, `ui/src/main.rs`, `ui/src/url.rs`

- [ ] **Step 1: Exclude `ui` from the workspace**

In the root `Cargo.toml`, add an `exclude` key to the `[workspace]` table (after the `members = [...]` array):

```toml
exclude = ["ui"]
```

So `cargo build --workspace` never compiles `ui`, and running cargo from inside `ui/` treats it as its own standalone workspace.

- [ ] **Step 2: Create `ui/Cargo.toml`**

```toml
[package]
name = "otto-ui"
version = "0.1.0"
edition = "2021"

[dependencies]
leptos = { version = "0.8", features = ["csr"] }
otto-protocol = { path = "../crates/protocol" }
wasm-bindgen = "0.2"
web-sys = { version = "0.3", features = [
    "WebSocket",
    "MessageEvent",
    "CloseEvent",
    "ErrorEvent",
    "Node",
    "Element",
    "HtmlElement",
    "HtmlDivElement",
] }
serde_json = "1"
urlencoding = "2.1"
uuid = "1"
console_error_panic_hook = "0.1"
# uuid pulls getrandom (v4); on wasm32-unknown-unknown getrandom needs an explicit
# browser backend even though the UI never generates randomness — enable it here and
# via .cargo/config.toml. See troubleshooting in Step 8 if the version differs.
getrandom = { version = "0.4", features = ["wasm_js"] }
```

- [ ] **Step 3: Create `ui/.cargo/config.toml`**

```toml
# getrandom >= 0.3 requires selecting the wasm browser backend via this cfg in addition
# to the `wasm_js` feature. Applies only to the wasm target; host `cargo test` is unaffected.
[target.wasm32-unknown-unknown]
rustflags = ['--cfg', 'getrandom_backend="wasm_js"']
```

- [ ] **Step 4: Create `ui/Trunk.toml`**

```toml
[serve]
address = "127.0.0.1"
port = 8080
open = false
```

- [ ] **Step 5: Create `ui/index.html`**

```html
<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>otto</title>
    <link data-trunk rel="css" href="style.css" />
  </head>
  <body></body>
</html>
```

- [ ] **Step 6: Create `ui/style.css` (placeholder; fleshed out in Task 8)**

```css
:root { color-scheme: dark; }
body { margin: 0; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
```

- [ ] **Step 7: Create `ui/src/url.rs` with the failing tests first**

```rust
//! Pure, browser-free helpers. Unit-tested with plain `cargo test` on the host
//! (no wasm, no DOM) — this is the determinism seam for the UI's logic.

/// Build the `/ws` connection URL. `session`/`last_seq` are appended only when reconnecting.
pub fn build_ws_url(
    base: &str,
    token: &str,
    session: Option<&str>,
    last_seq: Option<u64>,
) -> String {
    let base = base.trim_end_matches('/');
    let mut url = format!("{base}/ws?token={}", urlencoding::encode(token));
    if let Some(s) = session {
        url.push_str(&format!("&session={}", urlencoding::encode(s)));
    }
    if let Some(seq) = last_seq {
        url.push_str(&format!("&last_seq={seq}"));
    }
    url
}

/// True if an incoming event seq is newer than what we've applied
/// (never re-apply an event with seq ≤ the current high-water mark).
pub fn should_apply(current: Option<u64>, incoming: u64) -> bool {
    match current {
        Some(c) => incoming > c,
        None => true,
    }
}

/// Advance the high-water seq mark; never moves backwards.
pub fn advance_last_seq(current: Option<u64>, incoming: u64) -> Option<u64> {
    match current {
        Some(c) if incoming <= c => Some(c),
        _ => Some(incoming),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_without_reconnect_has_only_token() {
        let u = build_ws_url("ws://127.0.0.1:8787", "tok", None, None);
        assert_eq!(u, "ws://127.0.0.1:8787/ws?token=tok");
    }

    #[test]
    fn url_trims_trailing_slash_and_encodes_token() {
        let u = build_ws_url("ws://host/", "a b&c", None, None);
        assert_eq!(u, "ws://host/ws?token=a%20b%26c");
    }

    #[test]
    fn url_with_reconnect_appends_session_and_seq() {
        let u = build_ws_url("ws://h", "t", Some("sess-1"), Some(12));
        assert_eq!(u, "ws://h/ws?token=t&session=sess-1&last_seq=12");
    }

    #[test]
    fn should_apply_only_for_strictly_newer() {
        assert!(should_apply(None, 0));
        assert!(should_apply(Some(5), 6));
        assert!(!should_apply(Some(5), 5));
        assert!(!should_apply(Some(5), 4));
    }

    #[test]
    fn advance_never_goes_backwards() {
        assert_eq!(advance_last_seq(None, 0), Some(0));
        assert_eq!(advance_last_seq(Some(3), 7), Some(7));
        assert_eq!(advance_last_seq(Some(7), 3), Some(7));
    }
}
```

- [ ] **Step 8: Create `ui/src/main.rs` (minimal mountable app)**

```rust
mod url;

use leptos::prelude::*;

#[component]
fn App() -> impl IntoView {
    view! { <div class="app">"otto"</div> }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
```

- [ ] **Step 9: Run the pure-logic tests on the host**

Run: `cd ui && cargo test`
Expected: PASS — the five `url` tests pass. (Host build compiles the whole crate; leptos `csr` and web-sys compile for the host even though their DOM calls are never invoked here.)

- [ ] **Step 10: Verify the wasm target compiles**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: compiles clean.

Troubleshooting: if it fails on `getrandom` ("the wasm … target is not supported"), the compiler prints the exact backend instruction. Match `ui/Cargo.toml`'s `getrandom` version to the one actually resolved (`cd ui && cargo tree -i getrandom`), and ensure the feature in Step 2 and the cfg in Step 3 match what the error names (`wasm_js` / `getrandom_backend="wasm_js"` for 0.3/0.4).

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml ui/
git commit -m "feat(ui): scaffold standalone Leptos CSR crate with pure url helpers"
```

---

## Task 4: WebSocket client wrapper (`ws.rs`)

Wrap `web_sys::WebSocket`: open a socket with message/close/error callbacks that feed parsed `ServerMessage`s back to the caller, and send a `protocol::Command` as a JSON text frame.

**Files:**
- Create: `ui/src/ws.rs`
- Modify: `ui/src/main.rs` (declare `mod ws;`)

- [ ] **Step 1: Create `ui/src/ws.rs`**

```rust
//! Thin wrapper over `web_sys::WebSocket`. Browser-only; verified by compiling for wasm
//! and by manual browser testing (the pure routing/seq logic lives in `url.rs`).

use otto_protocol::{Command, ServerMessage};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{CloseEvent, ErrorEvent, MessageEvent, WebSocket};

/// Open a WebSocket to `url`, wiring callbacks:
/// - `on_msg`: each text frame parsed as `ServerMessage` (or `Err(detail)` on a parse failure).
/// - `on_close`: the socket closed.
/// - `on_error`: a socket error (e.g. a rejected handshake).
///
/// The event closures are `forget()`-leaked for the socket's lifetime. Each `open_ws` leaks
/// three small closures; acceptable for sub-project A's connect/reconnect cadence.
pub fn open_ws(
    url: &str,
    on_msg: impl Fn(Result<ServerMessage, String>) + 'static,
    on_close: impl Fn() + 'static,
    on_error: impl Fn() + 'static,
) -> Result<WebSocket, String> {
    let ws = WebSocket::new(url).map_err(|e| format!("{e:?}"))?;

    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Some(txt) = e.data().as_string() {
            let parsed = serde_json::from_str::<ServerMessage>(&txt).map_err(|err| err.to_string());
            on_msg(parsed);
        }
    });
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    let onclose = Closure::<dyn FnMut(CloseEvent)>::new(move |_e: CloseEvent| on_close());
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    onclose.forget();

    let onerror = Closure::<dyn FnMut(ErrorEvent)>::new(move |_e: ErrorEvent| on_error());
    ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    Ok(ws)
}

/// Serialize a `Command` to JSON and send it as a text frame.
pub fn send_command(ws: &WebSocket, cmd: &Command) -> Result<(), String> {
    let json = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
    ws.send_with_str(&json).map_err(|e| format!("{e:?}"))
}
```

- [ ] **Step 2: Declare the module in `main.rs`**

In `ui/src/main.rs`, add `mod ws;` below `mod url;`:

```rust
mod url;
mod ws;
```

(`ws` is unused for now — that's expected; it's wired in Task 7. If the unused-warning is noisy, it disappears once Task 7 references it.)

- [ ] **Step 3: Verify the wasm target compiles**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: compiles clean (warnings about unused `ws`/`url` items are acceptable at this stage).

- [ ] **Step 4: Commit**

```bash
git add ui/src/ws.rs ui/src/main.rs
git commit -m "feat(ui): add web_sys WebSocket client wrapper"
```

---

## Task 5: View-model helpers (`view_model.rs`)

Pure, host-testable rendering helpers: the connection state enum, a uniform log-row type, the per-`EventKind` formatter, and small label helpers. No leptos/web-sys here so it stays plain-`cargo test`-able.

**Files:**
- Create: `ui/src/view_model.rs`
- Modify: `ui/src/main.rs` (declare `mod view_model;`)

- [ ] **Step 1: Create `ui/src/view_model.rs` with tests first**

```rust
//! Pure view helpers — formatting and connection state. Browser-free, host-tested.

use otto_protocol::EventKind;

/// The single connection-state signal that drives the whole UI.
#[derive(Clone, PartialEq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected { session: String },
}

/// A single rendered row in the event log. `class` is a CSS class; `text` is the line.
#[derive(Clone, PartialEq)]
pub struct LogRow {
    pub class: &'static str,
    pub text: String,
}

fn row(class: &'static str, text: String) -> LogRow {
    LogRow { class, text }
}

/// Human label for the status line.
pub fn status_label(c: &ConnState) -> &'static str {
    match c {
        ConnState::Disconnected => "disconnected",
        ConnState::Connecting => "connecting…",
        ConnState::Connected { .. } => "connected",
    }
}

/// Shorten a session id (uuid string) for the status line: first 4 chars + ellipsis.
pub fn short_session(id: &str) -> String {
    let head: String = id.chars().take(4).collect();
    if id.chars().count() > 4 {
        format!("{head}…")
    } else {
        head
    }
}

/// Format one engine event into a log row.
pub fn describe_event(kind: &EventKind) -> LogRow {
    match kind {
        EventKind::AgentStarted { role } => row("row-agent", format!("▸ {role:?} started")),
        EventKind::AgentFinished { role } => row("row-agent", format!("▸ {role:?} finished")),
        EventKind::FileEdit { path, bytes_written } => row(
            "row-edit",
            format!("✎ FileEdit {} (+{} bytes)", path.display(), bytes_written),
        ),
        EventKind::VerifyResult { ok, detail } => row(
            "row-verify",
            format!(
                "{} Verify {}",
                if *ok { "✓" } else { "✗" },
                if detail.is_empty() { "ok".to_string() } else { detail.clone() },
            ),
        ),
        EventKind::Log { message } => row("row-log", format!("· {message}")),
        EventKind::TurnComplete { ok } => row(
            "row-turn",
            format!("● TurnComplete {}", if *ok { "ok" } else { "failed" }),
        ),
    }
}

/// A server-sent `Error` frame as a row.
pub fn error_row(message: &str) -> LogRow {
    row("row-error", format!("error: {message}"))
}

/// A client-side problem (parse failure, refused connection) as a row.
pub fn client_error_row(message: &str) -> LogRow {
    row("row-error", format!("client: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_protocol::Role;
    use std::path::PathBuf;

    #[test]
    fn short_session_truncates_long_ids() {
        assert_eq!(short_session("3f9a1b2c-dead"), "3f9a…");
        assert_eq!(short_session("ab"), "ab");
    }

    #[test]
    fn status_labels() {
        assert_eq!(status_label(&ConnState::Disconnected), "disconnected");
        assert_eq!(
            status_label(&ConnState::Connected { session: "x".into() }),
            "connected"
        );
    }

    #[test]
    fn describe_file_edit() {
        let r = describe_event(&EventKind::FileEdit {
            path: PathBuf::from("src/main.rs"),
            bytes_written: 42,
        });
        assert_eq!(r.class, "row-edit");
        assert_eq!(r.text, "✎ FileEdit src/main.rs (+42 bytes)");
    }

    #[test]
    fn describe_turn_complete_and_verify() {
        assert_eq!(
            describe_event(&EventKind::TurnComplete { ok: true }).text,
            "● TurnComplete ok"
        );
        assert_eq!(
            describe_event(&EventKind::VerifyResult { ok: false, detail: "boom".into() }).text,
            "✗ Verify boom"
        );
    }

    #[test]
    fn describe_agent_uses_role_name() {
        let r = describe_event(&EventKind::AgentStarted { role: Role::Planner });
        assert_eq!(r.text, "▸ Planner started");
    }
}
```

- [ ] **Step 2: Declare the module in `main.rs`**

In `ui/src/main.rs`:

```rust
mod url;
mod view_model;
mod ws;
```

- [ ] **Step 3: Run the host tests**

Run: `cd ui && cargo test`
Expected: PASS — `url` tests plus the five new `view_model` tests.

- [ ] **Step 4: Commit**

```bash
git add ui/src/view_model.rs ui/src/main.rs
git commit -m "feat(ui): add pure view-model helpers (conn state, event formatting)"
```

---

## Task 6: Components (`status_line`, `event_log`, `prompt_bar`, `connection_form`)

Four dumb view components driven by signals/callbacks passed as props. They establish the component pattern the rest of the UI (B–F) inherits.

**Files:**
- Create: `ui/src/components/mod.rs`, `ui/src/components/status_line.rs`, `ui/src/components/event_log.rs`, `ui/src/components/prompt_bar.rs`, `ui/src/components/connection_form.rs`
- Modify: `ui/src/main.rs` (declare `mod components;`)

- [ ] **Step 1: Create `ui/src/components/status_line.rs`**

```rust
use leptos::prelude::*;

use crate::view_model::{ConnState, short_session, status_label};

/// Connection state + short session id + last seq. The seam sub-project B extends into the
/// capabilities status strip.
#[component]
pub fn StatusLine(conn: RwSignal<ConnState>, last_seq: RwSignal<Option<u64>>) -> impl IntoView {
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
        </div>
    }
}
```

- [ ] **Step 2: Create `ui/src/components/event_log.rs`**

```rust
use leptos::html::Div;
use leptos::prelude::*;

use crate::view_model::LogRow;

/// Scrolling list of received rows, newest at the bottom, auto-scrolled on append.
#[component]
pub fn EventLog(rows: RwSignal<Vec<LogRow>>) -> impl IntoView {
    let container: NodeRef<Div> = NodeRef::new();

    // After each change to `rows`, pin the scroll position to the bottom.
    Effect::new(move |_| {
        rows.track();
        if let Some(el) = container.get() {
            el.set_scroll_top(el.scroll_height());
        }
    });

    view! {
        <div class="log" node_ref=container>
            {move || {
                rows.get()
                    .into_iter()
                    .map(|r| view! { <div class=format!("row {}", r.class)>{r.text}</div> })
                    .collect_view()
            }}
        </div>
    }
}
```

- [ ] **Step 3: Create `ui/src/components/prompt_bar.rs`**

```rust
use leptos::prelude::*;

use crate::view_model::ConnState;

/// Prompt input with Send/Abort. Enabled only while `Connected`. Send clears the input.
#[component]
pub fn PromptBar(
    conn: RwSignal<ConnState>,
    on_send: Callback<String>,
    on_abort: Callback<()>,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let connected = move || matches!(conn.get(), ConnState::Connected { .. });

    let send = move |_| {
        let t = text.get();
        if !t.trim().is_empty() {
            on_send.run(t);
            text.set(String::new());
        }
    };

    view! {
        <div class="prompt">
            <input
                class="prompt-input"
                type="text"
                placeholder="prompt…"
                prop:value=move || text.get()
                on:input=move |e| text.set(event_target_value(&e))
                disabled=move || !connected()
            />
            <button on:click=send disabled=move || !connected()>"Send"</button>
            <button
                on:click=move |_| on_abort.run(())
                disabled=move || !connected()
            >"Abort"</button>
        </div>
    }
}
```

- [ ] **Step 4: Create `ui/src/components/connection_form.rs`**

```rust
use leptos::prelude::*;

use crate::view_model::ConnState;

/// Engine URL + token inputs; Connect when disconnected, Disconnect otherwise.
/// Inputs are disabled while not disconnected so the URL/token can't change mid-session.
#[component]
pub fn ConnectionForm(
    url: RwSignal<String>,
    token: RwSignal<String>,
    conn: RwSignal<ConnState>,
    on_connect: Callback<()>,
    on_disconnect: Callback<()>,
) -> impl IntoView {
    let disconnected = move || matches!(conn.get(), ConnState::Disconnected);

    view! {
        <div class="conn-form">
            <input
                class="url-input"
                type="text"
                placeholder="ws://127.0.0.1:8787"
                prop:value=move || url.get()
                on:input=move |e| url.set(event_target_value(&e))
                disabled=move || !disconnected()
            />
            <input
                class="token-input"
                type="password"
                placeholder="token"
                prop:value=move || token.get()
                on:input=move |e| token.set(event_target_value(&e))
                disabled=move || !disconnected()
            />
            {move || {
                if disconnected() {
                    view! { <button on:click=move |_| on_connect.run(())>"Connect"</button> }
                        .into_any()
                } else {
                    view! { <button on:click=move |_| on_disconnect.run(())>"Disconnect"</button> }
                        .into_any()
                }
            }}
        </div>
    }
}
```

- [ ] **Step 5: Create `ui/src/components/mod.rs`**

```rust
mod connection_form;
mod event_log;
mod prompt_bar;
mod status_line;

pub use connection_form::ConnectionForm;
pub use event_log::EventLog;
pub use prompt_bar::PromptBar;
pub use status_line::StatusLine;
```

- [ ] **Step 6: Declare the module in `main.rs`**

In `ui/src/main.rs`:

```rust
mod components;
mod url;
mod view_model;
mod ws;
```

- [ ] **Step 7: Verify the wasm target compiles**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: compiles clean (the components are unused until Task 7 — unused warnings acceptable).

- [ ] **Step 8: Commit**

```bash
git add ui/src/components/ ui/src/main.rs
git commit -m "feat(ui): add status-line, event-log, prompt-bar, connection-form components"
```

---

## Task 7: Wire it together in `app.rs`

The `App` root owns all signals, the connect/disconnect/send/abort logic, and the inbound-message handler; it composes the four components. Replaces the placeholder `App` in `main.rs`.

**Files:**
- Create: `ui/src/app.rs`
- Modify: `ui/src/main.rs` (declare `mod app;`, mount `app::App`, drop the placeholder)

- [ ] **Step 1: Create `ui/src/app.rs`**

```rust
use leptos::prelude::*;
use otto_protocol::{Command, ServerMessage, SessionId};
use uuid::Uuid;
use web_sys::WebSocket;

use crate::components::{ConnectionForm, EventLog, PromptBar, StatusLine};
use crate::url::{advance_last_seq, build_ws_url, should_apply};
use crate::view_model::{ConnState, LogRow, client_error_row, describe_event, error_row};
use crate::ws::{open_ws, send_command};

#[component]
pub fn App() -> impl IntoView {
    // Form inputs.
    let url = RwSignal::new("ws://127.0.0.1:8787".to_string());
    let token = RwSignal::new(String::new());

    // Connection + stream state.
    let conn = RwSignal::new(ConnState::Disconnected);
    let rows = RwSignal::new(Vec::<LogRow>::new());
    let last_seq = RwSignal::new(None::<u64>); // retained across disconnects for replay
    let session = RwSignal::new(None::<String>); // retained across disconnects for reconnect
    let socket = RwSignal::new(None::<WebSocket>);

    // Connect (also used for reconnect: session/last_seq are appended when present).
    let connect = move || {
        let base = url.get();
        let tok = token.get();
        if base.trim().is_empty() || tok.trim().is_empty() {
            rows.update(|v| v.push(client_error_row("URL and token are required")));
            return;
        }
        let target = build_ws_url(&base, &tok, session.get().as_deref(), last_seq.get());
        conn.set(ConnState::Connecting);

        let on_msg = move |incoming: Result<ServerMessage, String>| match incoming {
            Ok(ServerMessage::Ready { session: s }) => {
                let id = s.0.to_string();
                session.set(Some(id.clone()));
                conn.set(ConnState::Connected { session: id });
            }
            Ok(ServerMessage::Event { event }) => {
                if should_apply(last_seq.get_untracked(), event.seq) {
                    last_seq.set(advance_last_seq(last_seq.get_untracked(), event.seq));
                    rows.update(|v| v.push(describe_event(&event.kind)));
                }
            }
            Ok(ServerMessage::Error { message }) => {
                rows.update(|v| v.push(error_row(&message)));
            }
            Err(detail) => {
                rows.update(|v| v.push(client_error_row(&detail)));
            }
        };
        let on_close = move || conn.set(ConnState::Disconnected);
        let on_error = move || {
            rows.update(|v| v.push(client_error_row("connection rejected — check URL/token")));
            conn.set(ConnState::Disconnected);
        };

        match open_ws(&target, on_msg, on_close, on_error) {
            Ok(ws) => socket.set(Some(ws)),
            Err(e) => {
                rows.update(|v| v.push(client_error_row(&e)));
                conn.set(ConnState::Disconnected);
            }
        }
    };

    let disconnect = move || {
        if let Some(ws) = socket.get() {
            let _ = ws.close();
        }
        socket.set(None);
        conn.set(ConnState::Disconnected);
    };

    // The server ignores the `session` field of an inbound Command, but the type requires it;
    // fill it from the retained session id.
    let send_prompt = move |text: String| {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        let Ok(uuid) = Uuid::parse_str(&sid) else {
            return;
        };
        let cmd = Command::SendPrompt { session: SessionId(uuid), text };
        if let Err(e) = send_command(&ws, &cmd) {
            rows.update(|v| v.push(client_error_row(&e)));
        }
    };

    let abort = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(&ws, &Command::Abort { session: SessionId(uuid) });
        }
    };

    view! {
        <div class="app">
            <StatusLine conn=conn last_seq=last_seq />
            <EventLog rows=rows />
            <PromptBar
                conn=conn
                on_send=Callback::new(send_prompt)
                on_abort=Callback::new(move |()| abort())
            />
            <ConnectionForm
                url=url
                token=token
                conn=conn
                on_connect=Callback::new(move |()| connect())
                on_disconnect=Callback::new(move |()| disconnect())
            />
        </div>
    }
}
```

- [ ] **Step 2: Update `ui/src/main.rs` to mount the real `App`**

Replace the entire contents of `ui/src/main.rs` with:

```rust
mod app;
mod components;
mod url;
mod view_model;
mod ws;

use app::App;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
```

- [ ] **Step 3: Verify host tests still pass and wasm still compiles**

Run: `cd ui && cargo test`
Expected: PASS — `url` + `view_model` tests still green (the new web-facing code isn't exercised by host tests but must compile).

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: compiles clean, with no unused-item warnings now that everything is wired.

- [ ] **Step 4: Commit**

```bash
git add ui/src/app.rs ui/src/main.rs
git commit -m "feat(ui): wire app shell — connect, stream, abort, reconnect"
```

---

## Task 8: Styling + manual acceptance

Flesh out the terminal-like single-column layout and run the end-to-end manual acceptance against a local `otto serve`.

**Files:**
- Modify: `ui/style.css`

- [ ] **Step 1: Replace `ui/style.css` with the full layout**

```css
:root {
  color-scheme: dark;
  --bg: #0b0d10;
  --fg: #cdd3da;
  --dim: #6b7480;
  --accent: #7aa2f7;
  --error: #f7768e;
}

* { box-sizing: border-box; }

body {
  margin: 0;
  background: var(--bg);
  color: var(--fg);
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 13px;
}

.app {
  display: flex;
  flex-direction: column;
  height: 100vh;
}

.status {
  padding: 6px 10px;
  border-bottom: 1px solid #1c2128;
  color: var(--dim);
  flex: 0 0 auto;
}

.log {
  flex: 1 1 auto;
  overflow-y: auto;
  padding: 8px 10px;
}

.row { white-space: pre-wrap; line-height: 1.5; }
.row-agent { color: var(--accent); }
.row-edit { color: #9ece6a; }
.row-verify { color: #e0af68; }
.row-log { color: var(--fg); }
.row-turn { color: var(--accent); font-weight: bold; }
.row-error { color: var(--error); }

.prompt, .conn-form {
  display: flex;
  gap: 6px;
  padding: 8px 10px;
  border-top: 1px solid #1c2128;
  flex: 0 0 auto;
}

.prompt-input, .url-input, .token-input {
  flex: 1 1 auto;
  background: #11151a;
  color: var(--fg);
  border: 1px solid #1c2128;
  padding: 6px 8px;
  font: inherit;
}
.url-input { flex: 2 1 auto; }
.token-input { flex: 1 1 auto; }

button {
  background: #1c2128;
  color: var(--fg);
  border: 1px solid #2a313c;
  padding: 6px 12px;
  font: inherit;
  cursor: pointer;
}
button:disabled { opacity: 0.4; cursor: not-allowed; }
button:hover:not(:disabled) { border-color: var(--accent); }
```

- [ ] **Step 2: Confirm `trunk` is installed (prerequisite for manual run)**

Run: `trunk --version`
Expected: prints a version. If not installed: `cargo install trunk` (and ensure the wasm target: `rustup target add wasm32-unknown-unknown`).

- [ ] **Step 3: Start a local engine**

In a separate terminal, from the repo root. The token is supplied via the `OTTO_TOKEN`
env var (mandatory; the server refuses to start without it); the port is `--port` (default
7878) and binds `127.0.0.1:<port>`:

```bash
OTTO_TOKEN=devtoken cargo run -p otto-engine -- serve --port 8787
```

It prints `otto serve listening on ws://127.0.0.1:8787/ws`. Use that URL and the token
`devtoken` in the UI. (Optionally add `--root <path>` to point at a project workspace.)

- [ ] **Step 4: Serve the UI**

```bash
cd ui && trunk serve
```

Open the printed URL (default `http://127.0.0.1:8080`) in a browser.

- [ ] **Step 5: Manual acceptance — walk the definition of done**

Confirm each, matching the spec's acceptance list:
1. Enter the engine URL (`ws://127.0.0.1:8787`) + token → click **Connect** → status line shows `connected` + a short session id.
2. Type a prompt → **Send** → event rows stream in (agent started/finished, file edit, verify) and end with `● TurnComplete`.
3. **Abort** during/after a turn works (socket closes → status returns to `disconnected`, session retained).
4. Click **Connect** again (same URL/token) → prior events replay via `last_seq`, **no duplicates**, then live events resume.
5. Try a wrong token → an error row appears; the form stays usable.

If any step fails, debug with the browser devtools console (the panic hook surfaces Rust panics) before proceeding.

- [ ] **Step 6: Commit**

```bash
git add ui/style.css
git commit -m "feat(ui): terminal-like styling for the app shell"
```

---

## Done criteria

- `cargo test --workspace` and `cargo build --workspace` green (engine side unaffected; determinism preserved).
- `cd ui && cargo test` green (pure `url` + `view_model` logic).
- `cd ui && cargo build --target wasm32-unknown-unknown` compiles.
- Manual acceptance (Task 8, Step 5) passes end-to-end against a local `otto serve`.
- Two additive protocol/serve changes landed; all pre-existing serve tests still pass.
