# Dioxus UI-axis Evaluation Spike — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a complete parallel browser+desktop Dioxus client that reaches parity with the shipped Leptos `ui/` (slices A–F), instrumented per-slice, and produce a Dioxus-vs-Leptos verdict.

**Architecture:** One workspace-excluded crate `ui-dioxus/` depending only on `otto-protocol`, targeting web (WASM, `dioxus-web`) and native desktop (`dioxus-desktop`) from a single feature-gated source tree. The framework-agnostic pure-logic modules (`url`/`tree`/`view_model`) port verbatim from `ui/` and become the shared seam; `cfg(feature)` appears **only** at the transport/platform edges (the WebSocket and the `/workspace` HTTP RPC). The Leptos `ui/src/*.rs` files are the executable spec — port behavior, not code.

**Tech Stack:** Rust (edition 2021), Dioxus (web + desktop), `otto-protocol` (path dep), `web-sys`/`gloo-net` (web transport), `tokio-tungstenite`/`reqwest` (desktop transport), `rfd` (desktop folder picker), `serde_json`, `urlencoding`, `uuid`.

## Global Constraints

- **New crate `ui-dioxus/`, workspace-excluded.** Add `"ui-dioxus"` to the root `Cargo.toml` `exclude` list (currently `exclude = ["ui", "desktop"]` at `Cargo.toml:22`). `cargo build --workspace` and the offline determinism suite must stay byte-for-byte untouched.
- **Depends only on `otto-protocol`** among `otto-*` crates (path dep: `otto-protocol = { path = "../crates/protocol" }`). Never link `engine-core` or any impl crate. Non-`otto` deps (Dioxus, transport crates) are fine.
- **One crate, two targets via cargo features `web` and `desktop`.** They are mutually exclusive at build time. `cfg(feature = "web")` / `cfg(feature = "desktop")` appears only in the transport and platform-glue modules. Every gated edge is counted (the unification tax).
- **No protocol change, no engine change.** The Leptos client is proof both are unnecessary. A needed `protocol` addition is **finding #1** to record in the report, not a change to make.
- **Dioxus API drift is expected and is itself a measured DX result.** Pin the latest stable Dioxus in `Cargo.toml` at execution time; confirm exact reactivity API (`use_signal`, `use_coroutine`, `use_future`, `rsx!`, `dioxus::launch`) against current Dioxus docs via the `context7` MCP before writing component code. Where the code below diverges from the pinned version's API, adjust it — and **log the friction**, because that friction is a scored DX data point.
- **Instrument every slice.** As each slice reaches working parity, append a row to the report file `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md`: LOC split into **view/reactivity LOC** vs **pure-logic LOC**, wall-clock to parity, and the count of `cfg(feature)` edge-gates the slice added. This is a required step in each slice's task, not an afterthought.
- **Verdict form = narrative + priority gate.** Multi-target unification and parity effort *decide* the verdict; DX/reactivity, build/toolchain (incl. WASM bundle size), ecosystem/editor, and runtime perf are reported as narrative evidence, not numerically scored.
- **Testing mirrors `ui/` exactly.** Pure modules get host `cargo test`; transport/reactive parts are verified by build checks (`cargo build --features web --target wasm32-unknown-unknown`, `cargo build --features desktop`) plus manual drive against a live `otto serve`. Do **not** build a component-test harness the incumbent lacks (it would make the parity comparison unfair) — but if Dioxus makes component testing meaningfully easier, note that as a DX narrative win.
- **Design source of truth:** `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-design.md`.

---

## File Structure

Created under `ui-dioxus/`:

| File | Responsibility |
|---|---|
| `Cargo.toml` | Crate manifest; `web`/`desktop` feature sets, target-specific deps. |
| `.cargo/config.toml` | `getrandom` wasm backend for the web build (mirrors `ui/.cargo/config.toml`). |
| `Dioxus.toml` | Dioxus CLI config (web asset dir, app name). |
| `index.html` | Web entry HTML (mirrors `ui/index.html`), links `style.css`. |
| `style.css` | Ported verbatim from `ui/style.css` (framework-agnostic). |
| `src/main.rs` | Entry: `dioxus::launch(App)`; per-target mount glue. |
| `src/app.rs` | The root `App` component + reactivity spine (ports `ui/src/app.rs`). |
| `src/net/url.rs` | **Pure, ported verbatim** from `ui/src/url.rs`. |
| `src/net/tree.rs` | **Pure, ported verbatim** from `ui/src/tree.rs`. |
| `src/net/view_model.rs` | **Pure, ported verbatim** from `ui/src/view_model.rs`. |
| `src/net/mod.rs` | Re-exports the three pure modules. |
| `src/transport/mod.rs` | Target-agnostic transport seam: `Sink` trait, `SocketEvent`, `connect()`, `list_files()`, `read_file()` facades. |
| `src/transport/web.rs` | `cfg(feature = "web")` impls (web-sys WebSocket + gloo-net fetch). |
| `src/transport/desktop.rs` | `cfg(feature = "desktop")` impls (tokio-tungstenite + reqwest). |
| `src/components/*.rs` | Dioxus components: `event_log`, `status_line`, `prompt_bar`, `approval_panel`, `file_tree`, `editor_pane`, `connection_form`. |
| `src/editor/mod.rs` | Slice-C controlled-buffer editor + highlight seam. |
| `src/editor/highlight_native.rs` | `cfg(feature = "desktop")` tree-sitter highlighting. |
| `src/editor/highlight_web.rs` | `cfg(feature = "web")` web-tree-sitter highlighting (timeboxed). |
| `src/desktop_boot.rs` | `cfg(feature = "desktop")` folder-picker → sidecar-launch → auto-connect. |

Created under `docs/superpowers/specs/`:

| File | Responsibility |
|---|---|
| `2026-07-11-ui-dioxus-spike-report.md` | The deliverable: per-slice effort table + narrative + verdict. |

---

# Phase P0 — Unification spine (slice A on web + desktop)

## Task 1: Crate scaffold + report skeleton (both targets build an empty shell)

**Files:**
- Modify: `Cargo.toml:22` (root — add `"ui-dioxus"` to `exclude`)
- Create: `ui-dioxus/Cargo.toml`
- Create: `ui-dioxus/.cargo/config.toml`
- Create: `ui-dioxus/Dioxus.toml`
- Create: `ui-dioxus/index.html`
- Create: `ui-dioxus/src/main.rs`
- Create: `ui-dioxus/src/app.rs`
- Create: `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md`

**Interfaces:**
- Produces: crate `ui-dioxus` with features `web`/`desktop`; a root `App` component (`pub fn App() -> Element`); a report file with the effort table header. Later tasks add modules to `main.rs` and fill `App`.

- [ ] **Step 1: Add the crate to the workspace exclude list**

Modify `Cargo.toml:22`:

```toml
exclude = ["ui", "desktop", "ui-dioxus"]
```

- [ ] **Step 2: Write the crate manifest**

Create `ui-dioxus/Cargo.toml`. Confirm the current Dioxus version via `context7` (resolve library id `dioxus`) before pinning; the versions below are the expected shape, adjust to what's current.

```toml
[workspace]

[package]
name = "otto-ui-dioxus"
version = "0.1.0"
edition = "2021"

[features]
default = []
web = ["dioxus/web", "dep:web-sys", "dep:gloo-net", "dep:wasm-bindgen", "dep:getrandom"]
desktop = ["dioxus/desktop", "dep:tokio", "dep:tokio-tungstenite", "dep:reqwest", "dep:rfd", "dep:futures-util"]

[dependencies]
dioxus = "0.6"
otto-protocol = { path = "../crates/protocol" }
serde_json = "1"
urlencoding = "2.1"
uuid = { version = "1", features = ["v4", "js"] }

# web transport (only compiled with --features web)
wasm-bindgen = { version = "0.2", optional = true }
gloo-net = { version = "0.6", features = ["http", "websocket"], optional = true }
getrandom = { version = "0.4", features = ["wasm_js"], optional = true }
web-sys = { version = "0.3", features = ["Window", "Location", "History"], optional = true }

# desktop transport (only compiled with --features desktop)
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync"], optional = true }
tokio-tungstenite = { version = "0.24", optional = true }
reqwest = { version = "0.12", features = ["json"], optional = true }
rfd = { version = "0.15", optional = true }
futures-util = { version = "0.3", optional = true }
```

- [ ] **Step 3: Write the wasm getrandom config**

Create `ui-dioxus/.cargo/config.toml` (mirrors `ui/.cargo/config.toml` — `uuid`'s `getrandom` needs an explicit wasm backend):

```toml
[target.wasm32-unknown-unknown]
rustflags = ['--cfg', 'getrandom_backend="wasm_js"']
```

- [ ] **Step 4: Write Dioxus.toml, index.html, and the entry point**

Create `ui-dioxus/Dioxus.toml`:

```toml
[application]
name = "otto-ui-dioxus"

[web.app]
title = "otto (Dioxus)"

[web.resource]
style = ["/style.css"]
```

Create `ui-dioxus/index.html`:

```html
<!DOCTYPE html>
<html>
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>otto (Dioxus)</title>
  </head>
  <body>
    <div id="main"></div>
  </body>
</html>
```

Create `ui-dioxus/src/main.rs`:

```rust
mod app;

use app::App;

fn main() {
    dioxus::launch(App);
}
```

Create `ui-dioxus/src/app.rs` (empty shell that compiles on both targets):

```rust
use dioxus::prelude::*;

#[component]
pub fn App() -> Element {
    rsx! {
        div { class: "app",
            h1 { "otto — Dioxus client" }
        }
    }
}
```

- [ ] **Step 5: Copy the stylesheet verbatim**

```bash
cp ui/style.css ui-dioxus/style.css
```

- [ ] **Step 6: Create the report skeleton with the effort table header**

Create `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md`:

```markdown
# Dioxus UI-axis Spike — Comparison Report

**Date started:** 2026-07-11
**Design:** `2026-07-11-ui-dioxus-spike-design.md`
**Status:** 🚧 In progress — rows appended as slices land.

## Per-slice effort log

| Slice | View/reactivity LOC | Pure-logic LOC | Wall-clock | `cfg` edge-gates | Notes |
|---|---|---|---|---|---|
| _(rows appended per slice)_ | | | | | |

**Leptos baseline (for comparison):** `ui/` totals — measure with
`tokei ui/src` or `wc -l ui/src/**/*.rs` and record here once, split the same way.

## Narrative evidence (not scored)

### DX / reactivity
_(notes)_

### Build / toolchain (incl. WASM bundle size)
_(notes)_

### Ecosystem / editor
_(notes)_

### Runtime perf
_(notes)_

## Priority gate ① — Multi-target unification
_(% shared tree, edge-gate total, and the yes/no: does the one crate replace `ui/` + `desktop/` + Tauri?)_

## Priority gate ② — Parity effort
_(total view/reactivity LOC + wall-clock vs the Leptos baseline)_

## Verdict
_(keep-Leptos / adopt-Dioxus / inconclusive, + the evidence that drove it)_
```

- [ ] **Step 7: Verify both targets build the empty shell**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
cd ui-dioxus && cargo build --no-default-features --features desktop
```
Expected: both compile (empty `App` renders on each target). If Dioxus feature names differ from the pinned version, fix per its docs and note the friction.

- [ ] **Step 8: Verify the workspace is untouched**

Run: `cargo build --workspace` (from repo root)
Expected: PASS, and `ui-dioxus` is not built as part of the workspace (it is excluded).

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): scaffold workspace-excluded dual-target crate + report skeleton"
```

---

## Task 2: Port the pure-logic seam (`url`, `tree`, `view_model`) with tests

The three modules in `ui/src/{url,tree,view_model}.rs` are already browser-free and host-tested; `view_model.rs` imports only `otto_protocol`. They port **verbatim** (the shared-seam, pure-logic-LOC baseline). Copy them exactly — do not rewrite — so their existing tests carry over unchanged.

**Files:**
- Create: `ui-dioxus/src/net/mod.rs`
- Create: `ui-dioxus/src/net/url.rs` (copy of `ui/src/url.rs`)
- Create: `ui-dioxus/src/net/tree.rs` (copy of `ui/src/tree.rs`)
- Create: `ui-dioxus/src/net/view_model.rs` (copy of `ui/src/view_model.rs`)
- Modify: `ui-dioxus/src/main.rs` (add `mod net;`)

**Interfaces:**
- Produces (from `net::url`): `build_ws_url(base, token, session: Option<&str>, last_seq: Option<u64>) -> String`, `should_apply(current: Option<u64>, incoming: u64) -> bool`, `advance_last_seq(current: Option<u64>, incoming: u64) -> Option<u64>`, `ws_to_http_base(&str) -> String`, `struct LaunchParams { ws: String, token: String }`, `parse_launch_params(&str) -> Option<LaunchParams>`.
- Produces (from `net::tree`): `struct TreeNode { name, path, is_dir, children }`, `build_tree(&[PathBuf]) -> Vec<TreeNode>`, `enum FileBody { Text(String), Binary, TooLarge }`, `language_for_path(&Path) -> &'static str`, `decode_or_binary(&[u8]) -> FileBody`, `const MAX_EDITABLE_BYTES`.
- Produces (from `net::view_model`): `enum ConnState { Disconnected, Connecting, Connected { session } }`, `struct LogRow { class, text }`, `enum DiffKind`, `struct DiffLine`, `diff_lines(old: Option<&str>, new: &str) -> Vec<DiffLine>`, `struct CapSegment`, `capability_segments(&CapabilitiesManifest) -> Vec<CapSegment>`, `can_promote(...)`, `can_demote(...)`, `format_meter(u64,u64) -> String`, `cost_estimate(u64,u64,bool) -> Option<f64>`, `status_label(&ConnState)`, `short_session(&str)`, `describe_event(&EventKind) -> LogRow`, `error_row(&str)`, `client_error_row(&str)`.

- [ ] **Step 1: Copy the three pure modules verbatim**

```bash
mkdir -p ui-dioxus/src/net
cp ui/src/url.rs        ui-dioxus/src/net/url.rs
cp ui/src/tree.rs       ui-dioxus/src/net/tree.rs
cp ui/src/view_model.rs ui-dioxus/src/net/view_model.rs
```

- [ ] **Step 2: Write the module re-export**

Create `ui-dioxus/src/net/mod.rs`:

```rust
//! Pure, browser-free, framework-agnostic logic ported verbatim from the Leptos `ui/`.
//! This is the shared seam: zero Dioxus/Leptos dependency, host-tested with plain `cargo test`.
pub mod tree;
pub mod url;
pub mod view_model;
```

- [ ] **Step 3: Register the module**

Modify `ui-dioxus/src/main.rs` — add `mod net;` above `mod app;`:

```rust
mod app;
mod net;

use app::App;

fn main() {
    dioxus::launch(App);
}
```

- [ ] **Step 4: Run the ported tests on the host and verify they pass**

Run: `cd ui-dioxus && cargo test --no-default-features net::`
Expected: PASS — every test copied from `ui/`'s three modules (e.g. `url_with_reconnect_appends_session_and_seq`, `build_tree_nests_and_sorts_dirs_before_files`, `diff_middle_change_keeps_context_head_and_tail`, `can_promote_only_when_connected_local_and_idle`) runs green with no changes.

> Note: `cargo test` with no target builds for the host; these modules have no Dioxus/transport deps, so they compile without a feature selected. If the crate's default build pulls a Dioxus item that needs a feature, gate `mod app;` behind `#[cfg(any(feature = "web", feature = "desktop"))]` so `cargo test net::` stays feature-free.

- [ ] **Step 5: Commit**

```bash
git add ui-dioxus/src/net ui-dioxus/src/main.rs
git commit -m "feat(ui-dioxus): port pure-logic seam (url/tree/view_model) verbatim with tests"
```

---

## Task 3: Transport seam (socket + workspace RPC, web + desktop impls)

Define one target-agnostic transport facade the reactivity spine drives. `cfg(feature)` lives **only** here. The socket delivers inbound frames as `SocketEvent`s over an `UnboundedReceiver`; outbound `Command`s go through a boxed `Sink`. The `/workspace` RPC helpers (`list_files`/`read_file`) are async facades used later by slice C.

**Files:**
- Create: `ui-dioxus/src/transport/mod.rs`
- Create: `ui-dioxus/src/transport/web.rs`
- Create: `ui-dioxus/src/transport/desktop.rs`
- Modify: `ui-dioxus/src/main.rs` (add `mod transport;`)

**Interfaces:**
- Consumes: `otto_protocol::{Command, ServerMessage, WorkspaceRequest, WorkspaceResponse}`.
- Produces: `enum SocketEvent { Message(Result<ServerMessage, String>), Closed, Errored }`; `trait Sink { fn send(&self, cmd: &Command) -> Result<(), String>; }` (**no `Send` bound** — the web `WebSocket` is `!Send`, so the trait cannot require it; the spine stores the sink as `Rc<dyn Sink>`); `fn connect(ws_url: &str) -> Result<(Box<dyn Sink>, futures_channel::mpsc::UnboundedReceiver<SocketEvent>), String>`; `async fn list_files(http_base: &str, token: &str) -> Result<Vec<PathBuf>, String>`; `async fn read_file(http_base: &str, token: &str, path: PathBuf) -> Result<Vec<u8>, String>`. The two impl modules provide `connect_impl`/`list_files_impl`/`read_file_impl`; `mod.rs` dispatches by feature.

- [ ] **Step 1: Write the facade + seam types**

Create `ui-dioxus/src/transport/mod.rs`:

```rust
//! The one place `cfg(feature)` splits web vs desktop. The reactivity spine sees only these
//! target-agnostic types; each target supplies the impl.
use std::path::PathBuf;

use otto_protocol::{Command, ServerMessage};

/// An inbound socket event, delivered over the receiver `connect` returns.
pub enum SocketEvent {
    Message(Result<ServerMessage, String>),
    Closed,
    Errored,
}

/// The outbound half of a live socket: serialize + send a `Command`. Boxed so the spine holds a
/// trait object, never a concrete socket type.
pub trait Sink {
    fn send(&self, cmd: &Command) -> Result<(), String>;
}

#[cfg(feature = "web")]
mod web;
#[cfg(feature = "desktop")]
mod desktop;

/// Open a socket to `ws_url`. Returns the outbound sink and a stream of inbound events.
pub fn connect(
    ws_url: &str,
) -> Result<(Box<dyn Sink>, futures_channel::mpsc::UnboundedReceiver<SocketEvent>), String> {
    #[cfg(feature = "web")]
    {
        web::connect_impl(ws_url)
    }
    #[cfg(feature = "desktop")]
    {
        desktop::connect_impl(ws_url)
    }
}

/// List every file in the served workspace (`POST /workspace` `List`).
pub async fn list_files(http_base: &str, token: &str) -> Result<Vec<PathBuf>, String> {
    #[cfg(feature = "web")]
    {
        web::list_files_impl(http_base, token).await
    }
    #[cfg(feature = "desktop")]
    {
        desktop::list_files_impl(http_base, token).await
    }
}

/// Read one file's bytes (`POST /workspace` `Read`).
pub async fn read_file(http_base: &str, token: &str, path: PathBuf) -> Result<Vec<u8>, String> {
    #[cfg(feature = "web")]
    {
        web::read_file_impl(http_base, token, path).await
    }
    #[cfg(feature = "desktop")]
    {
        desktop::read_file_impl(http_base, token, path).await
    }
}
```

Add `futures-channel = "0.3"` to `[dependencies]` in `ui-dioxus/Cargo.toml` (non-optional — used by both targets):

```toml
futures-channel = "0.3"
```

- [ ] **Step 2: Write the web transport impl**

Create `ui-dioxus/src/transport/web.rs` (ports `ui/src/ws.rs` + `ui/src/workspace.rs`; the web_sys closures forward into the channel):

```rust
use std::path::PathBuf;

use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use gloo_net::http::Request;
use otto_protocol::{Command, ServerMessage, WorkspaceRequest, WorkspaceResponse};
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use web_sys::{CloseEvent, ErrorEvent, MessageEvent, WebSocket};

use super::{Sink, SocketEvent};

struct WebSink(WebSocket);
impl Sink for WebSink {
    fn send(&self, cmd: &Command) -> Result<(), String> {
        let json = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
        self.0.send_with_str(&json).map_err(|e| format!("{e:?}"))
    }
}

pub fn connect_impl(
    ws_url: &str,
) -> Result<(Box<dyn Sink>, UnboundedReceiver<SocketEvent>), String> {
    let (tx, rx) = unbounded::<SocketEvent>();
    let ws = WebSocket::new(ws_url).map_err(|e| format!("{e:?}"))?;

    let tx_msg: UnboundedSender<SocketEvent> = tx.clone();
    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Some(txt) = e.data().as_string() {
            let parsed = serde_json::from_str::<ServerMessage>(&txt).map_err(|err| err.to_string());
            let _ = tx_msg.unbounded_send(SocketEvent::Message(parsed));
        }
    });
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    let tx_close = tx.clone();
    let onclose = Closure::<dyn FnMut(CloseEvent)>::new(move |_e: CloseEvent| {
        let _ = tx_close.unbounded_send(SocketEvent::Closed);
    });
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    onclose.forget();

    let tx_err = tx.clone();
    let onerror = Closure::<dyn FnMut(ErrorEvent)>::new(move |_e: ErrorEvent| {
        let _ = tx_err.unbounded_send(SocketEvent::Errored);
    });
    ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    Ok((Box::new(WebSink(ws)), rx))
}

async fn rpc(
    http_base: &str,
    token: &str,
    req: &WorkspaceRequest,
) -> Result<WorkspaceResponse, String> {
    let url = format!("{}/workspace", http_base.trim_end_matches('/'));
    let resp = Request::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .json(req)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("workspace rpc failed: HTTP {}", resp.status()));
    }
    let parsed: WorkspaceResponse = resp.json().await.map_err(|e| e.to_string())?;
    if let WorkspaceResponse::Error { message } = &parsed {
        return Err(message.clone());
    }
    Ok(parsed)
}

pub async fn list_files_impl(http_base: &str, token: &str) -> Result<Vec<PathBuf>, String> {
    match rpc(http_base, token, &WorkspaceRequest::List { glob: "**/*".into() }).await? {
        WorkspaceResponse::List { paths } => Ok(paths),
        other => Err(format!("unexpected response to List: {other:?}")),
    }
}

pub async fn read_file_impl(
    http_base: &str,
    token: &str,
    path: PathBuf,
) -> Result<Vec<u8>, String> {
    match rpc(http_base, token, &WorkspaceRequest::Read { path }).await? {
        WorkspaceResponse::Read { bytes } => Ok(bytes),
        other => Err(format!("unexpected response to Read: {other:?}")),
    }
}
```

- [ ] **Step 3: Write the desktop transport impl**

Create `ui-dioxus/src/transport/desktop.rs`. A tokio task owns the tungstenite read/write loop; inbound frames forward into the channel, outbound `Command`s arrive over an mpsc the `Sink` writes to. `reqwest` handles the workspace RPC.

```rust
use std::path::PathBuf;

use futures_channel::mpsc::{unbounded, UnboundedReceiver};
use futures_util::{SinkExt, StreamExt};
use otto_protocol::{Command, ServerMessage, WorkspaceRequest, WorkspaceResponse};
use tokio_tungstenite::tungstenite::Message;

use super::{Sink, SocketEvent};

struct DesktopSink(tokio::sync::mpsc::UnboundedSender<String>);
impl Sink for DesktopSink {
    fn send(&self, cmd: &Command) -> Result<(), String> {
        let json = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
        self.0.send(json).map_err(|e| e.to_string())
    }
}

pub fn connect_impl(
    ws_url: &str,
) -> Result<(Box<dyn Sink>, UnboundedReceiver<SocketEvent>), String> {
    let (inbound_tx, inbound_rx) = unbounded::<SocketEvent>();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let url = ws_url.to_string();

    // The desktop build runs on a tokio runtime (dioxus-desktop provides one); spawn the socket
    // loop onto it. All errors surface as SocketEvent::Errored/Closed, matching the web path.
    tokio::spawn(async move {
        let (stream, _resp) = match tokio_tungstenite::connect_async(&url).await {
            Ok(ok) => ok,
            Err(_) => {
                let _ = inbound_tx.unbounded_send(SocketEvent::Errored);
                return;
            }
        };
        let (mut write, mut read) = stream.split();
        let inbound_reader = inbound_tx.clone();
        // Reader task: forward each text frame as a parsed ServerMessage.
        let reader = tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(txt)) => {
                        let parsed = serde_json::from_str::<ServerMessage>(&txt)
                            .map_err(|e| e.to_string());
                        let _ = inbound_reader.unbounded_send(SocketEvent::Message(parsed));
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        let _ = inbound_reader.unbounded_send(SocketEvent::Closed);
                        break;
                    }
                    _ => {}
                }
            }
        });
        // Writer loop: drain outbound commands until the sink is dropped.
        while let Some(json) = out_rx.recv().await {
            if write.send(Message::Text(json)).await.is_err() {
                let _ = inbound_tx.unbounded_send(SocketEvent::Errored);
                break;
            }
        }
        reader.abort();
    });

    Ok((Box::new(DesktopSink(out_tx)), inbound_rx))
}

async fn rpc(
    http_base: &str,
    token: &str,
    req: &WorkspaceRequest,
) -> Result<WorkspaceResponse, String> {
    let url = format!("{}/workspace", http_base.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(req)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("workspace rpc failed: HTTP {}", resp.status()));
    }
    let parsed: WorkspaceResponse = resp.json().await.map_err(|e| e.to_string())?;
    if let WorkspaceResponse::Error { message } = &parsed {
        return Err(message.clone());
    }
    Ok(parsed)
}

pub async fn list_files_impl(http_base: &str, token: &str) -> Result<Vec<PathBuf>, String> {
    match rpc(http_base, token, &WorkspaceRequest::List { glob: "**/*".into() }).await? {
        WorkspaceResponse::List { paths } => Ok(paths),
        other => Err(format!("unexpected response to List: {other:?}")),
    }
}

pub async fn read_file_impl(
    http_base: &str,
    token: &str,
    path: PathBuf,
) -> Result<Vec<u8>, String> {
    match rpc(http_base, token, &WorkspaceRequest::Read { path }).await? {
        WorkspaceResponse::Read { bytes } => Ok(bytes),
        other => Err(format!("unexpected response to Read: {other:?}")),
    }
}
```

- [ ] **Step 4: Register the module**

Modify `ui-dioxus/src/main.rs` — add `mod transport;`:

```rust
mod app;
mod net;
mod transport;

use app::App;

fn main() {
    dioxus::launch(App);
}
```

- [ ] **Step 5: Verify both targets compile the transport seam**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
cd ui-dioxus && cargo build --no-default-features --features desktop
```
Expected: both compile. Fix any transport-crate API drift per the pinned versions' docs; record friction as a build/toolchain narrative note.

- [ ] **Step 6: Commit**

```bash
git add ui-dioxus/src/transport ui-dioxus/src/main.rs ui-dioxus/Cargo.toml
git commit -m "feat(ui-dioxus): dual-target transport seam (web-sys/gloo + tungstenite/reqwest)"
```

---

## Task 4: Slice A — app shell + live session (both targets)

Port `ui/src/app.rs`'s reactivity to Dioxus signals + a `use_future` that drains the socket receiver. Deliver slice A: connect, `SendPrompt`, live event render, `Abort`, and `last_seq` reconnect. Web is the primary drive target; desktop must launch the shell and connect to a manually-run `otto serve`. **This task carries the headline reactivity comparison.**

**Files:**
- Modify: `ui-dioxus/src/app.rs`
- Create: `ui-dioxus/src/components/mod.rs`
- Create: `ui-dioxus/src/components/event_log.rs`
- Create: `ui-dioxus/src/components/prompt_bar.rs`
- Create: `ui-dioxus/src/components/connection_form.rs`
- Modify: `ui-dioxus/src/main.rs` (add `mod components;`)

**Interfaces:**
- Consumes: `net::url::{build_ws_url, should_apply, advance_last_seq}`, `net::view_model::{ConnState, LogRow, describe_event, client_error_row, error_row}`, `transport::{connect, Sink, SocketEvent}`, `otto_protocol::{Command, ServerMessage, EventKind, SessionId}`.
- Produces: a working `App` with reactive `conn`/`rows`/`last_seq`/`session`/`sink` signals; child components `EventLog`, `PromptBar`, `ConnectionForm`.

- [ ] **Step 1: Write the leaf components**

Create `ui-dioxus/src/components/event_log.rs`:

```rust
use dioxus::prelude::*;
use crate::net::view_model::LogRow;

#[component]
pub fn EventLog(rows: Signal<Vec<LogRow>>) -> Element {
    rsx! {
        div { class: "event-log",
            for r in rows.read().iter() {
                div { class: "{r.class}", "{r.text}" }
            }
        }
    }
}
```

Create `ui-dioxus/src/components/prompt_bar.rs`:

```rust
use dioxus::prelude::*;
use crate::net::view_model::ConnState;

#[component]
pub fn PromptBar(
    conn: Signal<ConnState>,
    on_send: EventHandler<String>,
    on_abort: EventHandler<()>,
) -> Element {
    let mut text = use_signal(String::new);
    let connected = matches!(*conn.read(), ConnState::Connected { .. });
    rsx! {
        div { class: "prompt-bar",
            input {
                value: "{text}",
                disabled: !connected,
                oninput: move |e| text.set(e.value()),
            }
            button {
                disabled: !connected,
                onclick: move |_| {
                    let t = text.read().clone();
                    if !t.trim().is_empty() { on_send.call(t); text.set(String::new()); }
                },
                "Send"
            }
            button {
                disabled: !connected,
                onclick: move |_| on_abort.call(()),
                "Abort"
            }
        }
    }
}
```

Create `ui-dioxus/src/components/connection_form.rs`:

```rust
use dioxus::prelude::*;
use crate::net::view_model::ConnState;

#[component]
pub fn ConnectionForm(
    url: Signal<String>,
    token: Signal<String>,
    conn: Signal<ConnState>,
    on_connect: EventHandler<()>,
    on_disconnect: EventHandler<()>,
) -> Element {
    let connected = matches!(*conn.read(), ConnState::Connected { .. });
    rsx! {
        div { class: "connection-form",
            input {
                value: "{url}",
                placeholder: "ws://127.0.0.1:8787",
                oninput: move |e| url.set(e.value()),
            }
            input {
                value: "{token}",
                r#type: "password",
                placeholder: "token",
                oninput: move |e| token.set(e.value()),
            }
            if connected {
                button { onclick: move |_| on_disconnect.call(()), "Disconnect" }
            } else {
                button { onclick: move |_| on_connect.call(()), "Connect" }
            }
        }
    }
}
```

Create `ui-dioxus/src/components/mod.rs`:

```rust
mod connection_form;
mod event_log;
mod prompt_bar;

pub use connection_form::ConnectionForm;
pub use event_log::EventLog;
pub use prompt_bar::PromptBar;
```

- [ ] **Step 2: Write the reactivity spine**

Rewrite `ui-dioxus/src/app.rs`. The socket receiver is stored in a signal and drained by a `use_future`; `connect()` swaps it. This is the direct analogue of `ui/src/app.rs`'s `on_msg`/`connect`/`send_prompt`/`abort`.

```rust
use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedReceiver;
// Slice A references only these; later slices (D/E/F) add `EventKind` when they match on it.
use otto_protocol::{Command, ServerMessage, SessionId};
use uuid::Uuid;

use crate::components::{ConnectionForm, EventLog, PromptBar};
use crate::net::url::{advance_last_seq, build_ws_url, should_apply};
use crate::net::view_model::{
    client_error_row, describe_event, error_row, ConnState, LogRow,
};
use crate::transport::{connect, Sink, SocketEvent};

#[component]
pub fn App() -> Element {
    let mut url = use_signal(|| "ws://127.0.0.1:8787".to_string());
    let mut token = use_signal(String::new);
    let mut conn = use_signal(|| ConnState::Disconnected);
    let mut rows = use_signal(Vec::<LogRow>::new);
    let mut last_seq = use_signal(|| None::<u64>);
    let mut session = use_signal(|| None::<String>);
    // The live outbound sink; None when disconnected.
    let mut sink = use_signal(|| None::<std::rc::Rc<dyn Sink>>);
    // The inbound receiver for the current socket, handed to the drain future on connect.
    let mut incoming = use_signal(|| None::<UnboundedReceiver<SocketEvent>>);

    // Drain loop: whenever a new receiver is installed, pull events until the socket closes.
    use_future(move || async move {
        loop {
            // Take the receiver out (if any) and drain it fully, then wait for the next connect.
            let rx = incoming.write().take();
            if let Some(mut rx) = rx {
                use futures_util::StreamExt;
                while let Some(ev) = rx.next().await {
                    match ev {
                        SocketEvent::Message(Ok(ServerMessage::Ready { session: s, .. })) => {
                            let id = s.0.to_string();
                            session.set(Some(id.clone()));
                            conn.set(ConnState::Connected { session: id });
                        }
                        SocketEvent::Message(Ok(ServerMessage::Event { event })) => {
                            if should_apply(*last_seq.read(), event.seq) {
                                last_seq.set(advance_last_seq(*last_seq.read(), event.seq));
                                rows.write().push(describe_event(&event.kind));
                            }
                        }
                        SocketEvent::Message(Ok(ServerMessage::Error { message })) => {
                            rows.write().push(error_row(&message));
                        }
                        SocketEvent::Message(Ok(_)) => {}
                        SocketEvent::Message(Err(detail)) => {
                            rows.write().push(client_error_row(&detail));
                        }
                        SocketEvent::Closed | SocketEvent::Errored => {
                            conn.set(ConnState::Disconnected);
                            sink.set(None);
                            break;
                        }
                    }
                }
            }
            // Yield so we don't busy-spin when idle. `gloo_timers`/tokio sleep both work; a
            // zero-delay yield is enough since we only re-enter after `incoming` is repopulated.
            futures_util::future::poll_fn(|_| std::task::Poll::Ready(())).await;
            gloo_or_tokio_yield().await;
        }
    });

    let mut do_connect = move || {
        let base = url.read().clone();
        let tok = token.read().clone();
        if base.trim().is_empty() || tok.trim().is_empty() {
            rows.write().push(client_error_row("URL and token are required"));
            return;
        }
        let target = build_ws_url(&base, &tok, session.read().as_deref(), *last_seq.read());
        conn.set(ConnState::Connecting);
        match connect(&target) {
            Ok((s, rx)) => {
                sink.set(Some(std::rc::Rc::from(s)));
                incoming.set(Some(rx));
            }
            Err(e) => {
                rows.write().push(client_error_row(&e));
                conn.set(ConnState::Disconnected);
            }
        }
    };

    let send = move |cmd: Command| {
        if let Some(s) = sink.read().as_ref() {
            if let Err(e) = s.send(&cmd) {
                rows.write().push(client_error_row(&e));
            }
        }
    };

    let send_prompt = move |text: String| {
        if let Some(sid) = session.read().clone() {
            if let Ok(uuid) = Uuid::parse_str(&sid) {
                send(Command::SendPrompt { session: SessionId(uuid), text });
            }
        }
    };
    let abort = move |_| {
        if let Some(sid) = session.read().clone() {
            if let Ok(uuid) = Uuid::parse_str(&sid) {
                send(Command::Abort { session: SessionId(uuid) });
            }
        }
    };

    rsx! {
        div { class: "app",
            EventLog { rows }
            PromptBar {
                conn,
                on_send: move |t| send_prompt(t),
                on_abort: abort,
            }
            ConnectionForm {
                url, token, conn,
                on_connect: move |_| do_connect(),
                on_disconnect: move |_| {
                    sink.set(None);
                    conn.set(ConnState::Disconnected);
                },
            }
        }
    }
}

/// Cross-target cooperative yield. Replace with the pinned Dioxus/runtime idiom during
/// implementation (e.g. `gloo_timers::future::TimeoutFuture::new(0)` on web, `tokio::task::yield_now`
/// on desktop) — this is a known cfg edge; count it.
async fn gloo_or_tokio_yield() {
    #[cfg(feature = "web")]
    gloo_timers::future::TimeoutFuture::new(0).await;
    #[cfg(feature = "desktop")]
    tokio::task::yield_now().await;
}
```

Dependency changes in `ui-dioxus/Cargo.toml`: add `gloo-timers = { version = "0.3", features = ["futures"], optional = true }` and list it under the `web` feature; and **promote `futures-util` to a non-optional dependency** — `futures-util = "0.3"` at the top level, removed from the `desktop` feature list (the drain loop uses it on both targets, so it can no longer be desktop-gated).

> **Reactivity note (record in the report):** this drain-loop-over-a-signal-held-receiver pattern is the Dioxus analogue of Leptos's `forget()`-leaked closures writing signals directly. Whether it is cleaner or more awkward than the Leptos version is the core DX/reactivity finding — write it down while it's fresh.

> **`Sink`-storage friction to verify here:** the spine holds the sink as `Rc<dyn Sink>` because the web `WebSocket` is `!Send`. Dioxus-desktop runs a multithreaded runtime and *may* require signal-held values to be `Send`. If the desktop build rejects the `!Send` `Rc<dyn Sink>` signal, the fix is a target-split storage type (`Rc` on web, `Arc<dyn Sink + Send + Sync>` on desktop with a `Send + Sync` `DesktopSink`) — and that split is itself a `cfg`-edge unification finding to count and record, not a silent workaround.

- [ ] **Step 3: Register components**

Modify `ui-dioxus/src/main.rs` — add `mod components;`.

- [ ] **Step 4: Build both targets**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
cd ui-dioxus && cargo build --no-default-features --features desktop
```
Expected: both compile. Resolve Dioxus API drift (signal read/write, `EventHandler`, `use_future`) against the pinned version's docs; log friction.

- [ ] **Step 5: Manually drive slice A on web**

Run a serve, then the web app:
```bash
OTTO_TOKEN=devtoken cargo run -p otto-engine -- serve --port 8787 --root . &
cd ui-dioxus && dx serve --features web
```
In the browser: connect with `ws://127.0.0.1:8787` + `devtoken`, send a prompt, watch the live event log fill, click Abort, kill/restart serve and reconnect (verify `last_seq` replay produces no duplicate rows). Expected: matches `ui/`'s slice-A behavior. Record the `dx serve` vs `trunk` toolchain experience.

- [ ] **Step 6: Manually drive the shell on desktop**

Run: `cd ui-dioxus && dx serve --features desktop` (or `cargo run --features desktop`), connect to the same running serve.
Expected: the native window opens, connects, and streams events. (Auto-connect/folder-picker comes in Task 13; here connect manually.)

- [ ] **Step 7: Instrument slice A**

Append a row to `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md`'s effort table: measure view/reactivity LOC (`app.rs` + the three components), pure-logic LOC (0 new — reused from Task 2), wall-clock for this task, and `cfg` edge-gate count so far (the `gloo_or_tokio_yield` split + the transport module). Add DX/reactivity + toolchain narrative notes.

- [ ] **Step 8: Commit**

```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice A — app shell + live session on web+desktop, instrumented"
```

---

# Phase P1 — view slices, web-first (B, D, E, F)

Each P1 task ports one slice's view logic (pure helpers already exist in `net::view_model` from Task 2), delivers web parity, confirms the desktop build, and appends an effort row. No new transport is needed — every P1 command rides the existing socket.

## Task 5: Slice B — capabilities status strip

**Files:**
- Create: `ui-dioxus/src/components/status_line.rs`
- Modify: `ui-dioxus/src/components/mod.rs`, `ui-dioxus/src/app.rs`

**Interfaces:**
- Consumes: `net::view_model::{ConnState, capability_segments, status_label, short_session}`, `otto_protocol::CapabilitiesManifest`.
- Produces: `StatusLine` component; a `capabilities: Signal<Option<CapabilitiesManifest>>` set on `Ready` and cleared on every disconnect path.

- [ ] **Step 1: Write the StatusLine component**

Create `ui-dioxus/src/components/status_line.rs`:

```rust
use dioxus::prelude::*;
use otto_protocol::CapabilitiesManifest;
use crate::net::view_model::{capability_segments, short_session, status_label, ConnState};

#[component]
pub fn StatusLine(
    conn: Signal<ConnState>,
    last_seq: Signal<Option<u64>>,
    capabilities: Signal<Option<CapabilitiesManifest>>,
) -> Element {
    let c = conn.read();
    let seq = last_seq.read().map(|s| s.to_string()).unwrap_or_else(|| "—".into());
    rsx! {
        div { class: "status-line",
            span { class: "status-conn", "{status_label(&c)}" }
            if let ConnState::Connected { session } = &*c {
                span { class: "status-session", "{short_session(session)}" }
                span { class: "status-seq", "seq {seq}" }
                // Only render the capability strip when connected AND a manifest is present.
                if let Some(m) = capabilities.read().as_ref() {
                    for seg in capability_segments(m) {
                        span {
                            class: if seg.degraded { "cap cap-degraded" } else { "cap" },
                            "{seg.label}: {seg.value}"
                        }
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 2: Wire capabilities into the spine**

In `app.rs`: add `let mut capabilities = use_signal(|| None::<CapabilitiesManifest>);`. Set it in the `Ready` arm (`capabilities.set(Some(caps))` — bind `capabilities: caps` in the pattern), and clear it (`capabilities.set(None)`) in the `Closed`/`Errored` arm, the disconnect handler, and at the top of `do_connect`. Render `StatusLine { conn, last_seq, capabilities }` as the first child of the `app` div. Export `StatusLine` from `components/mod.rs`.

- [ ] **Step 3: Build both targets**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
cd ui-dioxus && cargo build --no-default-features --features desktop
```
Expected: both PASS.

- [ ] **Step 4: Manually verify the degradation states**

Drive web against a default (offline) serve → LLM segment shows "offline (deterministic)" degraded; against `ANTHROPIC_API_KEY=… otto serve` → "remote", not degraded. Confirm sandbox on/off renders. Expected: matches `ui/`'s `status_line`.

- [ ] **Step 5: Instrument slice B + commit**

Append the slice-B effort row. Then:
```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice B — capabilities status strip"
```

---

## Task 6: Slice D — diff approval

`diff_lines` is already ported (Task 2). This task renders it and wires the `ApproveDiff` command. (Ordered before E/F to match the Leptos build order.)

**Files:**
- Create: `ui-dioxus/src/components/approval_panel.rs`
- Modify: `ui-dioxus/src/components/mod.rs`, `ui-dioxus/src/app.rs`

**Interfaces:**
- Consumes: `net::view_model::{diff_lines, DiffKind}`, `otto_protocol::{Command, EventKind, SessionId}`, `uuid::Uuid`.
- Produces: `type PendingApproval = (Uuid, PathBuf, Option<String>, String)`; `ApprovalPanel` component; a `pending_approval` signal set on `ApprovalRequest` events and cleared on `TurnComplete`/`Error`/decision.

- [ ] **Step 1: Write the ApprovalPanel component**

Create `ui-dioxus/src/components/approval_panel.rs`:

```rust
use std::path::PathBuf;
use dioxus::prelude::*;
use uuid::Uuid;
use crate::net::view_model::{diff_lines, DiffKind};

pub type PendingApproval = (Uuid, PathBuf, Option<String>, String);

#[component]
pub fn ApprovalPanel(
    pending: Signal<Option<PendingApproval>>,
    on_decide: EventHandler<(Uuid, bool)>,
) -> Element {
    let Some((id, path, old, new)) = pending.read().clone() else {
        return rsx! {};
    };
    let lines = diff_lines(old.as_deref(), &new);
    rsx! {
        div { class: "approval-panel",
            div { class: "approval-head", "approval needed: {path.display()}" }
            pre { class: "approval-diff",
                for l in lines {
                    div {
                        class: match l.kind {
                            DiffKind::Add => "diff-add",
                            DiffKind::Del => "diff-del",
                            DiffKind::Context => "diff-context",
                        },
                        "{l.text}"
                    }
                }
            }
            div { class: "approval-actions",
                button { onclick: move |_| on_decide.call((id, true)), "Approve" }
                button { onclick: move |_| on_decide.call((id, false)), "Reject" }
            }
        }
    }
}
```

- [ ] **Step 2: Wire pending-approval state**

In `app.rs`: add `let mut pending_approval = use_signal(|| None::<crate::components::PendingApproval>);`. In the `Event` arm, when `event.kind` is `EventKind::ApprovalRequest { id, path, old, new }`, `pending_approval.set(Some((*id, path.clone(), old.clone(), new.clone())))`; on `TurnComplete` and `Error` set it to `None`. Add a `decide` closure that sends `Command::ApproveDiff { session, id, approved }` and clears the panel **only on send success** (mirror `ui/src/app.rs:263-282`). Render `ApprovalPanel { pending: pending_approval, on_decide: move |(id, ok)| decide(id, ok) }`. Clear `pending_approval` on every disconnect path. Export `ApprovalPanel`/`PendingApproval` from `components/mod.rs`.

- [ ] **Step 3: Build both targets**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
cd ui-dioxus && cargo build --no-default-features --features desktop
```
Expected: both PASS.

- [ ] **Step 4: Manually verify approval**

Drive web against `otto serve --approve-edits` with a prompt that triggers a Coder edit → the diff panel renders (adds green, dels red), Approve applies, Reject skips. Confirm a failed send keeps the panel up. Expected: matches `ui/`'s `approval_panel`.

- [ ] **Step 5: Instrument slice D + commit**

Append the slice-D effort row (note `diff_lines` LOC counts as reused pure-logic, not new). Then:
```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice D — diff approval panel"
```

---

## Task 7: Slice E — token/cost meter + pause/resume

**Files:**
- Modify: `ui-dioxus/src/components/status_line.rs` (or a small meter view), `ui-dioxus/src/components/prompt_bar.rs`, `ui-dioxus/src/app.rs`

**Interfaces:**
- Consumes: `net::view_model::{format_meter, cost_estimate}`, `otto_protocol::{Command, EventKind, SessionId}`.
- Produces: a `meter: Signal<Option<(u64,u64)>>` signal and a `paused: Signal<bool>`; `PromptBar` gains Pause/Resume; the meter renders in the status area.

- [ ] **Step 1: Render the meter**

In `status_line.rs`, add a `meter: Signal<Option<(u64,u64)>>` prop and, when connected and `Some((i,o))`, render `format_meter(i,o)` plus, when a remote model is configured (`capabilities … remote_llm`), `cost_estimate(i,o,true)` formatted as `${:.4}`:

```rust
if let Some((i, o)) = *meter.read() {
    span { class: "meter", "{format_meter(i, o)}" }
    if let Some(m) = capabilities.read().as_ref() {
        if let Some(cost) = crate::net::view_model::cost_estimate(i, o, m.remote_llm) {
            span { class: "meter-cost", "${cost:.4}" }
        }
    }
}
```

(Add the `meter` prop to `StatusLine`'s signature and pass it from `app.rs`.)

- [ ] **Step 2: Add Pause/Resume to PromptBar**

Add `paused: Signal<bool>`, `on_pause: EventHandler<()>`, `on_resume: EventHandler<()>` props. Render a single button that reads "Pause" when `!paused` and "Resume" when `paused`, calling the matching handler, disabled unless connected.

- [ ] **Step 3: Wire meter + pause state**

In `app.rs`: add `let mut meter = use_signal(|| None::<(u64,u64)>);` and `let mut paused = use_signal(|| false);`. In the `Event` arm, on `EventKind::TokenCostMeter { input_tokens, output_tokens }` set `meter.set(Some((*input_tokens, *output_tokens)))`. Reset `meter`/`paused` at the start of `send_prompt` and on disconnect. Add `pause`/`resume` closures sending `Command::Pause`/`Command::Resume` and setting `paused`. Pass the new props to `StatusLine` and `PromptBar`.

- [ ] **Step 4: Build both targets**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
cd ui-dioxus && cargo build --no-default-features --features desktop
```
Expected: both PASS.

- [ ] **Step 5: Manually verify**

Against a metered serve (`ANTHROPIC_API_KEY=…`), run a turn → meter counts climb, cost estimate shows; against offline serve → tokens only, no cost. Pause/Resume toggles. Expected: matches `ui/`.

- [ ] **Step 6: Instrument slice E + commit**

Append the slice-E effort row. Then:
```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice E — token/cost meter + pause/resume"
```

---

## Task 8: Slice F — promote/demote + handover reconnect

**Files:**
- Modify: `ui-dioxus/src/app.rs`

**Interfaces:**
- Consumes: `net::view_model::{can_promote, can_demote}`, `otto_protocol::{Command, ServerMessage, SessionId}`.
- Produces: Promote/Demote buttons gated by `can_promote`/`can_demote`; a `reconnect_to: Signal<Option<String>>` that a `use_effect` drains to reconnect to the handed-back endpoint (reusing token+session+last_seq).

- [ ] **Step 1: Add a turn_running signal**

In `app.rs`: add `let mut turn_running = use_signal(|| false);`. Set it `true` on `AgentStarted`, `false` on `TurnComplete`/`Error`/disconnect (mirror `ui/src/app.rs`).

- [ ] **Step 2: Handle Promoted/Demoted frames**

In `app.rs`, declare `let mut reconnect_to = use_signal(|| None::<String>);` alongside the other signals. Then, in the `Message(Ok(...))` match, add arms for `ServerMessage::Promoted { endpoint, .. }` and `ServerMessage::Demoted { endpoint, .. }` that set `reconnect_to.set(Some(endpoint))`.

- [ ] **Step 3: Add the handover reconnect effect**

```rust
use_effect(move || {
    if let Some(endpoint) = reconnect_to.write().take() {
        url.set(endpoint);
        do_connect();
    }
});
```
(`do_connect` closes nothing here because the old socket's drain loop breaks on `Closed`; reuse of session+last_seq happens because `build_ws_url` reads the retained signals. Confirm the old receiver is replaced — `incoming.set(Some(rx))` swaps it, and the drain loop `take()`s the new one on its next lap.)

- [ ] **Step 4: Render the handover buttons**

```rust
div { class: "handover",
    button {
        disabled: !can_promote(&conn.read(), &capabilities.read(), *turn_running.read()),
        onclick: move |_| { /* send Command::PromoteToRemote */ },
        "Promote to remote"
    }
    button {
        disabled: !can_demote(&conn.read(), &capabilities.read(), *turn_running.read()),
        onclick: move |_| { /* send Command::DemoteToLocal */ },
        "Demote to local"
    }
}
```
Fill the `onclick` bodies with the `send(Command::PromoteToRemote { session })` / `DemoteToLocal` pattern used by the other command closures.

- [ ] **Step 5: Build both targets**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
cd ui-dioxus && cargo build --no-default-features --features desktop
```
Expected: both PASS.

- [ ] **Step 6: Manually verify handover**

Against `otto serve --accept-promotions` (a receiver on another port also running), Promote → the status strip flips engine local→remote after the auto-reconnect; run a turn on the remote; Demote → flips back. Confirm buttons disable mid-turn. Expected: matches `ui/`'s slice F. **Record whether the reconnect-under-Dioxus-lifecycle works cleanly — this is a called-out risk.**

- [ ] **Step 7: Instrument slice F + commit**

Append the slice-F effort row. Then:
```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice F — promote/demote + handover reconnect"
```

---

# Phase P2 — the editor (slice C)

## Task 9: Slice C part 1 — workspace tree + file open (unhighlighted)

The transport RPC facades (`list_files`/`read_file`) already exist (Task 3). This task renders the tree and mounts a file into a plain (unhighlighted) controlled buffer.

**Files:**
- Create: `ui-dioxus/src/components/file_tree.rs`
- Create: `ui-dioxus/src/editor/mod.rs`
- Modify: `ui-dioxus/src/components/mod.rs`, `ui-dioxus/src/app.rs`, `ui-dioxus/src/main.rs` (add `mod editor;`)

**Interfaces:**
- Consumes: `net::tree::{TreeNode, build_tree, decode_or_binary, FileBody, language_for_path}`, `transport::{list_files, read_file}`, `net::url::ws_to_http_base`.
- Produces: `FileTree` (recursive, collapsible) with an `on_open: EventHandler<PathBuf>`; `Editor` component taking `open: Signal<Option<(PathBuf, FileBody)>>` and a `seed` buffer; `tree: Signal<Vec<TreeNode>>` + `open_file` signals; a `load_files` action + auto-load effect on Connected.

- [ ] **Step 1: Write the recursive FileTree**

Create `ui-dioxus/src/components/file_tree.rs`:

```rust
use std::path::PathBuf;
use dioxus::prelude::*;
use crate::net::tree::TreeNode;

#[component]
pub fn FileTree(nodes: Vec<TreeNode>, on_open: EventHandler<PathBuf>) -> Element {
    rsx! {
        ul { class: "file-tree",
            for node in nodes {
                FileTreeNode { node: node.clone(), on_open: on_open.clone() }
            }
        }
    }
}

#[component]
fn FileTreeNode(node: TreeNode, on_open: EventHandler<PathBuf>) -> Element {
    let mut expanded = use_signal(|| true);
    if node.is_dir {
        rsx! {
            li {
                span {
                    class: "tree-dir",
                    onclick: move |_| expanded.toggle(),
                    if *expanded.read() { "▾ " } else { "▸ " }
                    "{node.name}"
                }
                if *expanded.read() {
                    ul {
                        for child in node.children.clone() {
                            FileTreeNode { node: child, on_open: on_open.clone() }
                        }
                    }
                }
            }
        }
    } else {
        let path = node.path.clone();
        rsx! {
            li {
                span {
                    class: "tree-file",
                    onclick: move |_| on_open.call(path.clone()),
                    "{node.name}"
                }
            }
        }
    }
}
```

- [ ] **Step 2: Write the controlled-buffer editor (no highlighting yet)**

Create `ui-dioxus/src/editor/mod.rs`:

```rust
use std::path::PathBuf;
use dioxus::prelude::*;
use crate::net::tree::FileBody;

#[component]
pub fn Editor(open: Signal<Option<(PathBuf, FileBody)>>, seed: Signal<String>) -> Element {
    let Some((path, body)) = open.read().clone() else {
        return rsx! { div { class: "editor-empty", "No file open" } };
    };
    match body {
        FileBody::Binary => rsx! { div { class: "editor-notice", "binary file — not editable" } },
        FileBody::TooLarge => rsx! { div { class: "editor-notice", "file too large to edit" } },
        FileBody::Text(_) => {
            let mut buf = use_signal(|| seed.read().clone());
            // Re-seed when a different file opens.
            use_effect(move || buf.set(seed.read().clone()));
            rsx! {
                div { class: "editor",
                    div { class: "editor-path", "{path.display()}" }
                    textarea {
                        class: "editor-area",
                        value: "{buf}",
                        oninput: move |e| buf.set(e.value()),
                    }
                }
            }
        }
    }
}
```

> This uses a `textarea` as the controlled buffer for part 1. Part 2 replaces it with a styled-span render; the diff-first non-goal means no VSCode-scale features.

- [ ] **Step 3: Wire tree + open into the spine**

In `app.rs`: add `let mut tree = use_signal(Vec::<TreeNode>::new);`, `let mut open_file = use_signal(|| None::<(PathBuf, FileBody)>);`, `let mut editor_seed = use_signal(String::new);`. Add a `load_files` action that spawns (`spawn`) `list_files(ws_to_http_base(&url), &token)` and sets `tree.set(build_tree(&paths))`. Add an `open_path` action that reads a file via `read_file`, computes `decode_or_binary(&bytes)`, seeds the editor on `Text`, and sets `open_file`. Add a `use_effect` that calls `load_files` when `conn` becomes `Connected`. Render a "Refresh files" button + `FileTree { nodes: tree.read().clone(), on_open: move |p| open_path(p) }` + `Editor { open: open_file, seed: editor_seed }`. Register `mod editor;`; export `FileTree`.

- [ ] **Step 4: Build both targets**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
cd ui-dioxus && cargo build --no-default-features --features desktop
```
Expected: both PASS — **this is the first exercise of the desktop workspace RPC (`reqwest`), the desktop analogue of `gloo-net`.**

- [ ] **Step 5: Manually verify tree + open on both targets**

Web: connect → tree auto-loads, dirs collapse/expand, click a text file → it mounts and is editable, a binary/oversize file shows the notice. **Desktop: same, exercising `reqwest` against the running serve** (CORS is irrelevant for the native client — note that as a unification data point).

- [ ] **Step 6: Instrument slice C-part-1 + commit**

Append the row. Then:
```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice C.1 — workspace tree + unhighlighted editor (web+desktop)"
```

---

## Task 10: Slice C part 2 — Dioxus-native styled-span editor

Replace the `textarea` with a controlled buffer rendered as styled spans (the substrate highlighting attaches to). Keep it a controlled buffer with a local, unsaved edit state (persistence stays deferred, per the design).

**Files:**
- Modify: `ui-dioxus/src/editor/mod.rs`
- Create: `ui-dioxus/src/editor/tokens.rs` (line/segment model the highlighter fills)

**Interfaces:**
- Produces: `struct Span { class: &'static str, text: String }`; `fn plain_spans(text: &str) -> Vec<Vec<Span>>` (one `Vec<Span>` per line; part 1 = a single `class:"tok-plain"` span per line); the editor renders `Vec<Vec<Span>>`. The highlighter tasks (11/12) swap `plain_spans` for a real tokenizer behind the same signature.

- [ ] **Step 1: Write the span model**

Create `ui-dioxus/src/editor/tokens.rs`:

```rust
/// One styled run within a rendered editor line. `class` maps to a CSS color rule.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub class: &'static str,
    pub text: String,
}

/// The no-highlight baseline: one plain span per line. The highlight backends replace this with a
/// tokenized version behind the same `(text, lang) -> Vec<Vec<Span>>` shape.
pub fn plain_spans(text: &str) -> Vec<Vec<Span>> {
    text.lines()
        .map(|line| vec![Span { class: "tok-plain", text: line.to_string() }])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_spans_one_run_per_line() {
        let s = plain_spans("a\nbb");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], vec![Span { class: "tok-plain", text: "a".into() }]);
        assert_eq!(s[1][0].text, "bb");
    }

    #[test]
    fn plain_spans_empty_is_empty() {
        assert!(plain_spans("").is_empty());
    }
}
```

- [ ] **Step 2: Run the token model tests**

Run: `cd ui-dioxus && cargo test --no-default-features editor::tokens::`
Expected: PASS.

- [ ] **Step 3: Render styled spans in the editor**

In `editor/mod.rs`, in the `Text` arm, render the buffer as a two-layer control: a transparent `textarea` capturing input over a `pre` of styled spans (a standard controlled-highlight pattern), or — simpler for the spike — an editable `pre` with `contenteditable` is discouraged; keep the `textarea` for input and render `plain_spans(&buf.read())` into an aligned `pre` beneath it:

```rust
let spans = crate::editor::tokens::plain_spans(&buf.read());
rsx! {
    div { class: "editor",
        div { class: "editor-path", "{path.display()}" }
        div { class: "editor-stack",
            pre { class: "editor-highlight",
                for line in spans {
                    div { class: "hl-line",
                        for sp in line { span { class: "{sp.class}", "{sp.text}" } }
                    }
                }
            }
            textarea {
                class: "editor-area editor-overlay",
                value: "{buf}",
                oninput: move |e| buf.set(e.value()),
            }
        }
    }
}
```

Add CSS to `style.css` aligning `.editor-overlay` (transparent text, caret visible) over `.editor-highlight` (same font metrics). Add `.tok-plain` and (for later) `.tok-keyword`/`.tok-string`/`.tok-comment`/`.tok-type`/`.tok-number` color rules.

- [ ] **Step 4: Build both targets + manually verify**

Run the two builds; drive web + desktop: open a file, edit it, confirm the overlay stays aligned and the buffer is editable. Expected: same behavior as part 1, now over the span substrate.

- [ ] **Step 5: Instrument + commit**

Append the row. Then:
```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice C.2 — native styled-span editor substrate"
```

---

## Task 11: Slice C part 3 — tree-sitter highlighting (desktop-native)

Desktop compiles the native `tree-sitter` crate + grammar crates. Reuse the exact language set `retrieval` vendors (Rust/JS/TS/Python/Go — confirm the crate versions used in `crates/retrieval/Cargo.toml` and match them). Produce real `Vec<Vec<Span>>` from a parse.

**Files:**
- Create: `ui-dioxus/src/editor/highlight_native.rs`
- Modify: `ui-dioxus/src/editor/mod.rs` (call the highlighter behind `cfg(feature = "desktop")`), `ui-dioxus/Cargo.toml` (tree-sitter deps under the `desktop` feature)

**Interfaces:**
- Consumes: `net::tree::language_for_path` (the `&'static str` lang id), `editor::tokens::Span`.
- Produces: `fn highlight(text: &str, lang: &str) -> Vec<Vec<Span>>` — same shape as `plain_spans`, but token-classified for supported langs; falls back to `plain_spans` for `"text"`/unsupported.

- [ ] **Step 1: Add the desktop tree-sitter deps**

Confirm the versions in `crates/retrieval/Cargo.toml`, then add under the `desktop` feature in `ui-dioxus/Cargo.toml` (example shape):

```toml
tree-sitter = { version = "0.24", optional = true }
tree-sitter-rust = { version = "0.23", optional = true }
tree-sitter-javascript = { version = "0.23", optional = true }
tree-sitter-typescript = { version = "0.23", optional = true }
tree-sitter-python = { version = "0.23", optional = true }
tree-sitter-go = { version = "0.23", optional = true }
```
Add each to the `desktop = [...]` feature list.

- [ ] **Step 2: Write the native highlighter**

Create `ui-dioxus/src/editor/highlight_native.rs`. Use the `tree-sitter-highlight` crate with the `HIGHLIGHTS_QUERY` each grammar crate ships, walk its events into a per-byte class map, then coalesce into per-line `Span`s. `language()` returns the `(Language, query)` pair from the start (no placeholder query). Map capture names to the CSS classes from Task 10.

Add `tree-sitter-highlight = { version = "0.24", optional = true }` under the `desktop` feature too (alongside the grammar crates from Step 1).

```rust
use crate::editor::tokens::{plain_spans, segment_lines, Span};

/// Map a `language_for_path` id to its loaded grammar + highlights query; None ⇒ no highlighting.
fn language(lang: &str) -> Option<(tree_sitter::Language, &'static str)> {
    Some(match lang {
        "rust" => (tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY),
        "javascript" => (
            tree_sitter_javascript::LANGUAGE.into(),
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        ),
        "typescript" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ),
        "python" => (tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY),
        "go" => (tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY),
        _ => return None,
    })
}

/// Highlight-capture names we style; the index into this array is the `Highlight.0` the events
/// carry. `class_for` maps each back to a CSS class from Task 10.
const CAPTURES: [&str; 5] = ["keyword", "string", "comment", "type", "number"];

fn class_for(idx: usize) -> &'static str {
    match CAPTURES.get(idx).copied().unwrap_or("") {
        "keyword" => "tok-keyword",
        "string" => "tok-string",
        "comment" => "tok-comment",
        "type" => "tok-type",
        "number" => "tok-number",
        _ => "tok-plain",
    }
}

/// Highlight `text` for `lang`. Falls back to `plain_spans` for unsupported langs or any parse
/// failure (highlighting is best-effort; the editor must never break on it).
pub fn highlight(text: &str, lang: &str) -> Vec<Vec<Span>> {
    match language(lang).and_then(|(language, query)| highlight_inner(text, language, query)) {
        Some(spans) => spans,
        None => plain_spans(text),
    }
}

fn highlight_inner(
    text: &str,
    language: tree_sitter::Language,
    query: &str,
) -> Option<Vec<Vec<Span>>> {
    use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};
    let mut cfg = HighlightConfiguration::new(language, "editor", query, "", "").ok()?;
    cfg.configure(&CAPTURES);
    let mut hl = Highlighter::new();
    let mut cur = "tok-plain";
    let mut class_per_byte = vec!["tok-plain"; text.len()];
    let events = hl.highlight(&cfg, text.as_bytes(), None, |_| None).ok()?;
    for ev in events {
        match ev.ok()? {
            HighlightEvent::HighlightStart(h) => cur = class_for(h.0),
            HighlightEvent::HighlightEnd => cur = "tok-plain",
            HighlightEvent::Source { start, end } => {
                for b in start..end.min(class_per_byte.len()) {
                    class_per_byte[b] = cur;
                }
            }
        }
    }
    Some(segment_lines(text, &class_per_byte))
}
```

> **Version note:** the exact grammar-crate query constant names differ across versions (`HIGHLIGHTS_QUERY` vs `HIGHLIGHT_QUERY`, and TypeScript exposes both a TS and TSX language). Confirm each against the pinned crate's docs via `context7` and adjust `language()`; these are trivial name fixes, not design changes.

- [ ] **Step 3: Move `segment_lines` into the shared token module and unit-test it**

`segment_lines` is pure and target-independent, and Task 12 needs it too — so it lives in `editor/tokens.rs`, not in `highlight_native.rs`. Add it there:

```rust
/// Split `text` into per-line `Span`s, coalescing runs of equal class. `class_per_byte` is a
/// class-per-source-byte map; the `+1` stride skips the `\n` that `lines()` strips.
pub fn segment_lines(text: &str, class_per_byte: &[&'static str]) -> Vec<Vec<Span>> {
    let mut out = Vec::new();
    let mut byte = 0usize;
    for line in text.lines() {
        let mut spans: Vec<Span> = Vec::new();
        let line_bytes = line.len();
        let mut i = 0usize;
        while i < line_bytes {
            let class = class_per_byte.get(byte + i).copied().unwrap_or("tok-plain");
            let mut j = i;
            while j < line_bytes
                && class_per_byte.get(byte + j).copied().unwrap_or("tok-plain") == class
            {
                j += 1;
            }
            spans.push(Span { class, text: line[i..j].to_string() });
            i = j;
        }
        out.push(spans);
        byte += line_bytes + 1;
    }
    out
}
```

Add a host test to `editor/tokens.rs`'s existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn segment_lines_coalesces_equal_classes_per_line() {
    // "ab\ncd": bytes 0,1 = keyword; byte 2 = '\n' (skipped); bytes 3,4 = string.
    let per_byte = ["tok-keyword", "tok-keyword", "tok-plain", "tok-string", "tok-string"];
    let out = segment_lines("ab\ncd", &per_byte);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], vec![Span { class: "tok-keyword", text: "ab".into() }]);
    assert_eq!(out[1], vec![Span { class: "tok-string", text: "cd".into() }]);
}
```

Run: `cd ui-dioxus && cargo test --no-default-features editor::tokens::`
Expected: PASS (this test is feature-free — `segment_lines` has no tree-sitter dependency).

- [ ] **Step 4: Call the highlighter from the editor (desktop only)**

In `editor/mod.rs`, replace the `plain_spans` call with a cfg-selected highlighter:

```rust
#[cfg(feature = "desktop")]
let spans = crate::editor::highlight_native::highlight(&buf.read(), lang);
#[cfg(feature = "web")]
let spans = crate::editor::tokens::plain_spans(&buf.read()); // web highlighting: Task 12
```
where `lang = language_for_path(&path)`. Register `mod highlight_native;` under `#[cfg(feature = "desktop")]` in `editor/mod.rs`.

- [ ] **Step 5: Build + manually verify desktop highlighting**

Run: `cd ui-dioxus && cargo build --no-default-features --features desktop` then drive it: open a `.rs`, `.py`, `.ts`, `.js`, `.go` file → keywords/strings/comments are colored; an unsupported ext (e.g. `.toml`) renders plain. Confirm editing stays responsive (note typing latency for the runtime-perf narrative).

- [ ] **Step 6: Instrument + commit**

Append the row (note this slice adds a `cfg` edge — the highlighter split). Then:
```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice C.3 — desktop-native tree-sitter highlighting"
```

---

## Task 12: Slice C part 4 — tree-sitter highlighting on web (timeboxed)

The C-based `tree-sitter` crate does not drop onto `wasm32-unknown-unknown` cleanly. Spike `web-tree-sitter` (the official wasm build) via Dioxus JS interop, grammars as `.wasm`. **Timebox: 1 working day.** If it blows the timebox, web degrades to unhighlighted editing (part 2's `plain_spans`) while desktop keeps highlighting — and that asymmetry is a recorded result, not a hidden one.

**Files:**
- Create: `ui-dioxus/src/editor/highlight_web.rs`
- Modify: `ui-dioxus/src/editor/mod.rs`, `ui-dioxus/index.html` (load `web-tree-sitter` + grammar wasm as static assets)

**Interfaces:**
- Produces (on success): `fn highlight(text: &str, lang: &str) -> Vec<Vec<Span>>` matching `highlight_native`'s signature, backed by `web-tree-sitter` through `wasm-bindgen`/JS interop. On timebox failure: this module is not created and web keeps `plain_spans`.

- [ ] **Step 1: Start the timebox and record the start**

Add a line to the report's ecosystem/editor narrative: "web tree-sitter spike started <slice N>; timebox 1 day." (No `Date.now()` — record the wall-clock the same way as other slices.)

- [ ] **Step 2: Vendor the wasm assets and load them**

Fetch `tree-sitter.wasm` (from `web-tree-sitter`) and the per-language grammar `.wasm` files (`tree-sitter-rust.wasm`, etc.), place under `ui-dioxus/assets/`, and reference them from `index.html`. Confirm `dx serve` serves the `assets/` dir (adjust `Dioxus.toml` `[web.resource]` if needed).

- [ ] **Step 3: Write the JS-interop highlighter**

Create `ui-dioxus/src/editor/highlight_web.rs` using `wasm-bindgen` to call `web-tree-sitter`'s `Parser`/`Language.load`/`Query` API, producing the same per-byte-class → `segment_lines` pipeline as the native path (reuse `segment_lines` — extract it into `editor/tokens.rs` so both backends share it). Because grammar loading is async, expose an init that resolves the `Language` promises once and caches them; `highlight` runs synchronously against the cached parser.

> If `web-tree-sitter`'s async init does not fit cleanly into the synchronous `highlight(text, lang)` signature within the timebox, that mismatch is the finding — stop and take the fallback (Step 5).

- [ ] **Step 4 (success path): Wire web highlighting + verify**

Swap the `#[cfg(feature = "web")]` line in `editor/mod.rs` from `plain_spans` to `highlight_web::highlight`. Build web, drive it: `.rs`/`.py`/`.ts` files highlight in the browser. Confirm bundle-size impact (record the WASM bundle size delta — a build/toolchain data point).

- [ ] **Step 5 (fallback path): Record the asymmetry**

If the timebox expires: leave `editor/mod.rs`'s web branch on `plain_spans`, do **not** create `highlight_web.rs`, and write in the report's ecosystem/editor narrative + the unification gate: "web tree-sitter exceeded the 1-day timebox; web ships unhighlighted, desktop highlights — a real native/wasm divergence and a headline unification finding." This is a valid completion of the task.

- [ ] **Step 6: Instrument + commit**

The web highlighter (success path) reuses the shared `segment_lines` already in `editor/tokens.rs` (moved there in Task 11) — no extraction needed here. Append the slice-C-part-4 effort row (note which path was taken: real web highlighting, or the recorded unhighlighted fallback). Then:
```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): slice C.4 — web tree-sitter highlighting (or recorded fallback)"
```

---

# Phase P3 — desktop packaging + report

## Task 13: Desktop auto-connect UX (folder-picker → sidecar → auto-connect)

Reproduce the Tauri `desktop/` UX **inside the one Dioxus crate** — no separate wrapper, no `ui/dist` sidecar handoff. On desktop launch: pick a workspace folder (`rfd`), spawn `otto serve` as a child on the fixed port 8787 with a generated token, then auto-connect. **Whether this genuinely replaces `desktop/` + Tauri is the single most important reported result.**

**Files:**
- Create: `ui-dioxus/src/desktop_boot.rs`
- Modify: `ui-dioxus/src/app.rs` (desktop-only auto-connect on mount), `ui-dioxus/src/main.rs` (add `#[cfg(feature="desktop")] mod desktop_boot;`)

**Interfaces:**
- Consumes: `net::url::LaunchParams` (reuse the same `{ws, token}` contract the web/Tauri path uses), `transport::connect`.
- Produces: `#[cfg(feature="desktop")] fn boot() -> Option<(std::process::Child, LaunchParams)>` — picks a folder, spawns the sidecar, returns the child handle (kept alive for the app's lifetime) + the `ws`/`token` to connect with. `None` if the user cancels the picker.

- [ ] **Step 1: Write the desktop boot module**

Create `ui-dioxus/src/desktop_boot.rs`:

```rust
//! Desktop bootstrap: fold the Tauri `desktop/` wrapper's job (pick workspace → launch a local
//! `otto serve` sidecar → auto-connect) into the one Dioxus crate. Fixed port 8787, generated token.
use std::process::{Child, Command};

use crate::net::url::LaunchParams;

/// Pick a workspace folder, spawn `otto serve` there, and return the child + connect params.
/// Returns None if the user cancels the folder picker.
pub fn boot() -> Option<(Child, LaunchParams)> {
    let root = rfd::FileDialog::new().set_title("Choose a workspace folder").pick_folder()?;
    let token = uuid::Uuid::new_v4().to_string();
    // Spawn the sidecar. `otto` must be on PATH (or point OTTO_BIN at it); mirrors desktop/'s
    // sidecar contract. Fixed port 8787 to match the LaunchParams the web path also uses.
    let child = Command::new(std::env::var("OTTO_BIN").unwrap_or_else(|_| "otto".into()))
        .arg("serve")
        .arg("--port").arg("8787")
        .arg("--root").arg(&root)
        .env("OTTO_TOKEN", &token)
        .spawn()
        .ok()?;
    Some((child, LaunchParams { ws: "ws://127.0.0.1:8787".into(), token }))
}
```

- [ ] **Step 2: Auto-connect on desktop mount**

In `app.rs`, add a desktop-only mount effect that runs `desktop_boot::boot()` once, stores the `Child` in a signal (so it lives for the window's lifetime and is killed on drop), sets `url`/`token` from the returned `LaunchParams`, waits briefly for the sidecar to bind, then calls `do_connect()`:

```rust
#[cfg(feature = "desktop")]
{
    let mut sidecar = use_signal(|| None::<std::process::Child>);
    use_future(move || async move {
        if let Some((child, params)) = crate::desktop_boot::boot() {
            sidecar.set(Some(child));
            url.set(params.ws);
            token.set(params.token);
            // Give the sidecar a moment to bind the port before connecting.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            do_connect();
        }
    });
}
```
(Ensure `do_connect` and the signals are declared before this block. Keep the manual `ConnectionForm` as a fallback if `boot()` returns `None`.)

- [ ] **Step 3: Build desktop + verify the web build is unaffected**

Run:
```bash
cd ui-dioxus && cargo build --no-default-features --features desktop
cd ui-dioxus && cargo build --no-default-features --features web --target wasm32-unknown-unknown
```
Expected: both PASS (the boot module is desktop-gated; web is untouched).

- [ ] **Step 4: Manually verify the full desktop flow**

Ensure `otto` is on PATH (`cargo install --path crates/engine` or set `OTTO_BIN`). Run `cd ui-dioxus && dx serve --features desktop`: the folder picker appears → choose a repo → the window auto-connects to the auto-launched sidecar with no manual URL/token entry → send a prompt, browse the tree, open a file. Confirm the sidecar process is killed when the window closes. Expected: reproduces `desktop/`'s slice-G UX.

- [ ] **Step 5: Record the headline unification result**

In the report's unification gate: state plainly whether the single Dioxus crate replaces `ui/` + `desktop/` + Tauri, with the evidence (does the sidecar+picker+auto-connect work end-to-end natively? what broke or was awkward?). Append the effort row.

- [ ] **Step 6: Commit**

```bash
git add ui-dioxus docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "feat(ui-dioxus): desktop auto-connect (folder-picker + sidecar), no Tauri wrapper"
```

---

## Task 14: Write the verdict

Fill the report's narrative + both priority gates + the verdict from the accumulated instrumentation, and flip its status.

**Files:**
- Modify: `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md`

- [ ] **Step 1: Record the Leptos baseline**

Run `wc -l ui/src/**/*.rs desktop/src-tauri/src/*.rs` (and/or `tokei ui/src desktop/src-tauri/src`) and fill the "Leptos baseline" line, split the same view/pure-logic way used for the Dioxus rows, so parity effort compares like-for-like.

- [ ] **Step 2: Complete the narrative sections**

Fill DX/reactivity (the drain-loop-vs-leaked-closures comparison, signal ergonomics, Dioxus API-drift friction), build/toolchain (`dx` vs `trunk`, **WASM bundle size** hard number vs the Leptos bundle, desktop build time/artifact size), ecosystem/editor (tree-sitter reality on both targets — which path C.4 took), and runtime perf (event-stream render, editor typing latency) from the notes accumulated per slice.

- [ ] **Step 3: Write both priority gates**

- **Unification gate:** % shared component tree (shared LOC ÷ total), the total `cfg`-edge-gate count summed across slices, and the yes/no on replacing `ui/` + `desktop/` + Tauri (from Task 13).
- **Parity-effort gate:** total Dioxus view/reactivity LOC + summed wall-clock vs the Leptos baseline; state explicitly where the view/pure-logic line was drawn.

- [ ] **Step 4: Write the verdict**

State keep-Leptos / adopt-Dioxus / inconclusive, driven by the two gates, with the narrative as supporting evidence. Include any protocol/engine "finding #1" surfaced (there should be none — record that too). Flip the report status from "🚧 In progress" to "✅ Complete".

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "docs(ui-dioxus): spike comparison report + Dioxus-vs-Leptos verdict"
```

---

## Notes for the implementer

- **The pure-logic seam is load-bearing and must not be rewritten.** Tasks 2/6/7/8 reuse `net::{url,tree,view_model}` verbatim; any temptation to "clean it up" defeats the parity-effort measurement (it would inflate the Dioxus LOC with work Leptos already paid for).
- **Every `cfg(feature)` you add is data.** Before adding one, check whether the split is genuinely a platform edge (transport, tree-sitter backend, sidecar) or an accident of API shape — and record it either way.
- **Highlighting is best-effort and must never break the editor.** Both backends fall back to `plain_spans` on any failure.
- **Dioxus version drift:** the component/reactivity code targets the 0.6-era API. Confirm the pinned version's exact API via `context7` (library `dioxus`) at execution and adjust; the adjustments are DX findings, not plan errors.
