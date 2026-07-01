# Sub-project G: Tauri Desktop Wrapper Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wrap the existing browser UI (`ui/`) in a native Tauri 2 desktop app that auto-launches a local `otto serve` as a sidecar process and auto-connects — no manual URL/token setup.

**Architecture:** A new, workspace-excluded crate `desktop/src-tauri/` (scaffolded by `cargo tauri init`, matching the tool's own `src-tauri/` convention) spawns the compiled `otto` binary as a Tauri sidecar over `tauri-plugin-shell`, using a native folder picker (`tauri-plugin-dialog`) for the workspace root. It navigates its webview — which loads the existing, unmodified `ui/dist` build — to a URL carrying bootstrap query params (`?ws=...&token=...&autoconnect=1`) that a new pure helper in `ui/src/url.rs` reads to skip the manual connect form. No `otto-*` crate is linked into `desktop/`; no protocol or engine change is needed.

**Tech Stack:** Rust (edition 2024 for the two `ui/`/`otto` files touched; `desktop/src-tauri` uses whatever edition `cargo tauri init` scaffolds, currently 2021 — do not "fix" this, it's a separate crate with its own toolchain). Tauri 2.11, `tauri-plugin-shell` 2.3, `tauri-plugin-dialog` 2.7, `uuid` 1.x. These are the exact versions resolved during planning (via `cargo add`/`cargo fetch` against a scratch project) — pin to these or newer within the 2.x line; if `cargo tauri init` on your machine resolves different exact patch versions, that's fine, the APIs used here have been stable since Tauri 2.0 GA.

---

## Context an implementer needs

- **Design spec:** `docs/superpowers/specs/2026-07-01-ui-tauri-desktop-wrapper-design.md`. Read it first — this plan implements it exactly, with one corrected detail: the design says "no nested `src-tauri/` — `desktop/` *is* the Tauri crate," but `cargo tauri init` (verified against the actually-installed `tauri-cli 2.10.1`) always scaffolds into `<directory>/src-tauri/` — there is no flag to flatten this. So the real crate lives at `desktop/src-tauri/`, with `desktop/` as its parent. This is a path-depth correction only; every behavior/architecture point in the spec (workspace-excluded, zero `otto-*` deps, `frontendDist` → `ui/dist`) is unchanged.
- **`ui/`** (`crates` sibling, NOT a cargo workspace member — see root `Cargo.toml`'s `exclude`) is a Leptos CSR app. Relevant existing files:
  - `ui/src/url.rs` — pure, DOM-free helpers, unit-tested with plain `cargo test`. You add `parse_launch_params` here.
  - `ui/src/app.rs` — the `App` component. `connect`/`disconnect` closures are defined ~line 50-169; two `Effect::new` blocks already exist (~line 330, ~338) that read signals and act on mount/change — you add a third, following the exact same pattern.
  - `ui/src/app.rs:22-23`: `url`/`token` are `RwSignal<String>`, defaulting to `"ws://127.0.0.1:8787"` / `""`.
- **`otto serve`**'s existing readiness log line (`crates/engine/src/main.rs:676`): `eprintln!("otto serve listening on {scheme}://{addr}/ws")`. You do not touch this file — you only match against its output.
- **Fixed port 8787** — the sidecar always serves on `127.0.0.1:8787` (design decision 4; explicitly no dynamic port, no engine changes).
- **Tauri APIs used below were verified against the actual installed `tauri-cli 2.10.1`** by scaffolding a throwaway project and reading the real dependency source (not from memory): `tauri 2.11.5`, `tauri-plugin-shell 2.3.5`, `tauri-plugin-dialog 2.7.1`. Exact signatures:
  - `app.shell().sidecar("otto")? .args([...]).env(K, V).spawn()? -> (Receiver<CommandEvent>, CommandChild)` (`tauri_plugin_shell::ShellExt`/`process::CommandEvent`/`CommandChild`).
  - `app.dialog().file().blocking_pick_folder() -> Option<tauri_plugin_dialog::FilePath>`; `FilePath::into_path() -> Result<PathBuf, _>` (`tauri_plugin_dialog::DialogExt`).
  - `app.manage(state)` / `handle.state::<T>()`; `handle.get_webview_window("main") -> Option<WebviewWindow>`; `window.navigate(url: url::Url) -> Result<()>`.
  - `Builder::build(context) -> Result<App>`, then `App::run(|app_handle, event: RunEvent| {...})` (NOT `Builder::run`, which doesn't take a callback).
  - `RunEvent::ExitRequested { code, api }`.
  - `AppHandle::exit(code: i32)`.

---

## Task 1: Scaffold `desktop/` and wire it to `ui/dist`

**Files:**
- Create: `desktop/src-tauri/` (via `cargo tauri init`) — `Cargo.toml`, `tauri.conf.json`, `build.rs`, `src/main.rs`, `src/lib.rs`, `capabilities/default.json`, `icons/*` (auto-generated default icons — fine to keep as placeholders for this dev-mode-only slice).
- Modify: `/home/robhicks/dev/otto-next/Cargo.toml:20` (`exclude` list).

- [ ] **Step 1: Confirm `tauri-cli` is available**

Run: `cargo tauri --version`
Expected: prints a version like `tauri-cli 2.x.x`. If this errors with "no such subcommand," run `cargo install tauri-cli --version "^2.0.0" --locked` first.

- [ ] **Step 2: Scaffold the project**

From the repo root:

```bash
mkdir -p desktop
cargo tauri init \
  --ci \
  --directory desktop \
  --app-name otto-desktop \
  --window-title otto \
  --frontend-dist ../../ui/dist
```

Expected: creates `desktop/src-tauri/` with `Cargo.toml`, `tauri.conf.json`, `build.rs`, `src/main.rs`, `src/lib.rs`, `capabilities/default.json`, and an `icons/` directory with default placeholder icons. `desktop/tauri.conf.json` does not exist — `tauri.conf.json` lives inside `src-tauri/`.

- [ ] **Step 3: Remove the unused dev/build commands from the generated config**

`cargo tauri init` fills `beforeDevCommand`/`beforeBuildCommand` with npm placeholders (`"npm run dev"`/`"npm run build"`) since it assumes a JS frontend. `ui/` is built separately with `trunk` (not driven by Tauri), so remove both keys entirely.

Open `desktop/src-tauri/tauri.conf.json` and delete the `beforeDevCommand` and `beforeBuildCommand` lines from the `build` object, leaving:

```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "otto-desktop",
  "version": "0.1.0",
  "identifier": "com.tauri.dev",
  "build": {
    "frontendDist": "../../ui/dist"
  },
  "app": {
    "windows": [
      {
        "title": "otto",
        "width": 800,
        "height": 600,
        "resizable": true,
        "fullscreen": false
      }
    ],
    "security": {
      "csp": null
    }
  },
  "bundle": {
    "active": true,
    "targets": "all",
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.icns",
      "icons/icon.ico"
    ]
  }
}
```

(Every other field is left exactly as scaffolded — the `productName`/`identifier`/window size are cosmetic and not part of this slice's scope.)

- [ ] **Step 4: Exclude `desktop` from the cargo workspace**

In `/home/robhicks/dev/otto-next/Cargo.toml`, change:

```toml
exclude = ["ui"]
```

to:

```toml
exclude = ["ui", "desktop"]
```

- [ ] **Step 5: Verify the main workspace is untouched**

Run: `cargo build --workspace` from the repo root.
Expected: builds exactly as before — `desktop/src-tauri` is not a workspace member and is not built by this command. No new errors, no new crates listed in the build output.

- [ ] **Step 6: Verify the scaffold itself compiles**

Run: `cargo build --manifest-path desktop/src-tauri/Cargo.toml`
Expected: compiles successfully (this builds the default Tauri scaffold, which doesn't yet reference `ui/dist` content — that's fine, `frontendDist` is only resolved at `tauri dev`/`tauri build` time, not `cargo build` time).

- [ ] **Step 7: Commit**

```bash
git add desktop Cargo.toml
git commit -m "feat(desktop): scaffold Tauri project, point frontendDist at ui/dist"
```

---

## Task 2: `ui/src/url.rs` — `parse_launch_params`

**Files:**
- Modify: `ui/src/url.rs`
- Test: `ui/src/url.rs` (existing `#[cfg(test)] mod tests` block, starts at line 57)

- [ ] **Step 1: Write the failing tests**

Add this to the `mod tests` block in `ui/src/url.rs` (after the existing `url_tolerates_base_already_ending_in_ws` test, still inside the closing `}` of `mod tests`):

```rust
    #[test]
    fn launch_params_requires_ws_token_and_autoconnect() {
        assert_eq!(
            parse_launch_params("ws=ws%3A%2F%2F127.0.0.1%3A8787&token=abc-123&autoconnect=1"),
            Some(LaunchParams {
                ws: "ws://127.0.0.1:8787".to_string(),
                token: "abc-123".to_string(),
            })
        );
    }

    #[test]
    fn launch_params_tolerates_leading_question_mark() {
        assert_eq!(
            parse_launch_params("?ws=ws://h&token=t&autoconnect=1"),
            Some(LaunchParams {
                ws: "ws://h".to_string(),
                token: "t".to_string(),
            })
        );
    }

    #[test]
    fn launch_params_none_without_autoconnect() {
        assert_eq!(parse_launch_params("ws=ws://h&token=t"), None);
        assert_eq!(parse_launch_params("ws=ws://h&token=t&autoconnect=0"), None);
    }

    #[test]
    fn launch_params_none_when_ws_or_token_missing_or_empty() {
        assert_eq!(parse_launch_params("token=t&autoconnect=1"), None);
        assert_eq!(parse_launch_params("ws=ws://h&autoconnect=1"), None);
        assert_eq!(parse_launch_params("ws=&token=t&autoconnect=1"), None);
        assert_eq!(parse_launch_params(""), None);
    }

    #[test]
    fn launch_params_ignores_unknown_keys_and_malformed_pairs() {
        assert_eq!(
            parse_launch_params("ws=ws://h&token=t&autoconnect=1&extra=ignored&malformed"),
            Some(LaunchParams {
                ws: "ws://h".to_string(),
                token: "t".to_string(),
            })
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui && cargo test url::tests::launch_params -- --nocapture`
Expected: FAIL to compile — `parse_launch_params` and `LaunchParams` don't exist yet.

- [ ] **Step 3: Implement `parse_launch_params`**

Add this above the existing `#[cfg(test)]` line in `ui/src/url.rs` (i.e. after `ws_to_http_base`, before the test module):

```rust
/// The Tauri desktop wrapper's auto-connect bootstrap (sub-project G): the local sidecar's
/// WS base URL and a freshly-generated bearer token, carried as query params on the webview's
/// initial navigation (`desktop/src-tauri/src/launch.rs`'s `build_launch_url` is the writer
/// side of this contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchParams {
    pub ws: String,
    pub token: String,
}

/// Parse `ws`/`token`/`autoconnect` from a query string (with or without a leading `?`, as
/// returned by `web_sys`'s `Location::search()`). Returns `Some` only when `autoconnect=1` and
/// both `ws` and `token` are present and non-empty — anything else (a plain browser visit with
/// no query string, a manually-typed URL, a malformed/partial query) yields `None`, leaving the
/// existing manual connect form as the fallback. Unknown keys and malformed `key=value` pairs
/// are silently ignored, not an error.
pub fn parse_launch_params(query: &str) -> Option<LaunchParams> {
    let query = query.strip_prefix('?').unwrap_or(query);
    let mut ws = None;
    let mut token = None;
    let mut autoconnect = false;
    for pair in query.split('&') {
        let Some((key, raw_value)) = pair.split_once('=') else {
            continue;
        };
        let Ok(value) = urlencoding::decode(raw_value) else {
            continue;
        };
        match key {
            "ws" => ws = Some(value.into_owned()),
            "token" => token = Some(value.into_owned()),
            "autoconnect" => autoconnect = value == "1",
            _ => {}
        }
    }
    if !autoconnect {
        return None;
    }
    let ws = ws.filter(|s| !s.is_empty())?;
    let token = token.filter(|s| !s.is_empty())?;
    Some(LaunchParams { ws, token })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd ui && cargo test url:: -- --nocapture`
Expected: PASS — all `url::tests::*` tests green, including the 5 new ones and all pre-existing ones (unaffected).

- [ ] **Step 5: Commit**

```bash
git add ui/src/url.rs
git commit -m "feat(ui): parse_launch_params for the desktop auto-connect bootstrap"
```

---

## Task 3: `ui/src/app.rs` — auto-connect on launch params

**Files:**
- Modify: `ui/src/app.rs`

- [ ] **Step 1: Add the auto-connect Effect**

In `ui/src/app.rs`, insert this new `Effect::new` block immediately before the existing `// Auto-load the tree when the connection reaches Connected.` comment (currently ~line 329, right after the `load_files` `Callback::new` block closes and before the first existing `Effect::new`):

```rust
    // Auto-connect when launched with `?ws=...&token=...&autoconnect=1` (the Tauri desktop
    // wrapper's bootstrap — sub-project G). Runs once on mount: it reads no reactive signal,
    // so this Effect never re-fires. A plain browser visit has no query string, so
    // `parse_launch_params` returns `None` and the manual form behaves exactly as before.
    Effect::new(move |_| {
        let Some(search) = web_sys::window().and_then(|w| w.location().search().ok()) else {
            return;
        };
        if let Some(params) = crate::url::parse_launch_params(&search) {
            url.set(params.ws);
            token.set(params.token);
            connect();
        }
    });

```

- [ ] **Step 2: Verify the wasm build compiles**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: compiles cleanly. (`connect` is already used in two other closures in this file after being defined with `move`, confirming it's safely reusable here too — Leptos signals captured by `move` are `Copy`, so a closure over only `Copy` captures is itself `Copy`.)

- [ ] **Step 3: Add a "connect to a different engine" fallback affordance**

So a desktop-app user can still reach the manual form (e.g. to point at a promoted/VPS-hosted `otto serve`), add a small always-visible link above the `ConnectionForm` in the `view!` block. Find:

```rust
            <ConnectionForm
                url=url
                token=token
                conn=conn
                on_connect=Callback::new(move |_| connect())
                on_disconnect=Callback::new(move |_| disconnect())
            />
```

Replace with:

```rust
            <button
                class="link-button"
                on:click=move |_| {
                    disconnect();
                    url.set("ws://127.0.0.1:8787".to_string());
                    token.set(String::new());
                }
                disabled=move || matches!(conn.get(), ConnState::Connecting)
            >"Connect to a different engine"</button>
            <ConnectionForm
                url=url
                token=token
                conn=conn
                on_connect=Callback::new(move |_| connect())
                on_disconnect=Callback::new(move |_| disconnect())
            />
```

This resets the form to the plain (non-autoconnected) defaults and disconnects, so the existing `ConnectionForm` is immediately usable for a manual connection — no new component, no new state.

- [ ] **Step 4: Verify the wasm build still compiles**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: compiles cleanly.

- [ ] **Step 5: Manual verification in a browser (no launch params — regression check)**

```bash
cd ui && trunk build
python3 -m http.server 8080 --directory dist
```

Open `http://127.0.0.1:8080` in a browser. Expected: the manual connect form shows exactly as before (URL pre-filled `ws://127.0.0.1:8787`, token empty) — no auto-connect attempt, since there's no query string. Confirms Task 3 doesn't regress plain browser access.

- [ ] **Step 6: Commit**

```bash
git add ui/src/app.rs
git commit -m "feat(ui): auto-connect on desktop launch params, add manual-connect fallback"
```

---

## Task 4: `desktop/src-tauri` — add the shell, dialog, and uuid dependencies

**Files:**
- Modify: `desktop/src-tauri/Cargo.toml`
- Modify: `desktop/src-tauri/src/lib.rs` (plugin registration only, no logic yet)
- Modify: `desktop/src-tauri/capabilities/default.json`

- [ ] **Step 1: Add the dependencies**

Run:

```bash
cargo add tauri-plugin-shell tauri-plugin-dialog --manifest-path desktop/src-tauri/Cargo.toml
cargo add uuid --features v4 --manifest-path desktop/src-tauri/Cargo.toml
```

Expected: `desktop/src-tauri/Cargo.toml`'s `[dependencies]` gains `tauri-plugin-shell`, `tauri-plugin-dialog`, and `uuid` (with the `v4` feature) entries.

- [ ] **Step 2: Register the plugins**

In `desktop/src-tauri/src/lib.rs`, find:

```rust
  tauri::Builder::default()
    .setup(|app| {
```

Replace with:

```rust
  tauri::Builder::default()
    .plugin(tauri_plugin_shell::init())
    .plugin(tauri_plugin_dialog::init())
    .setup(|app| {
```

- [ ] **Step 3: Grant the capabilities these plugins need**

Open `desktop/src-tauri/capabilities/default.json`. Find:

```json
  "permissions": [
    "core:default"
  ]
```

Replace with:

```json
  "permissions": [
    "core:default",
    "shell:allow-execute",
    "dialog:allow-open"
  ]
```

(`shell:allow-execute` covers sidecar spawning; the exact permission identifier set for sidecars has shifted across Tauri 2.x point releases — if `cargo build` in Step 4 reports a missing/unrecognized permission at runtime, check the installed `tauri-plugin-shell`'s own docs/`permissions/` directory in its source, e.g. via `cargo doc -p tauri-plugin-shell --open`, and adjust this list to match. This is the one spot in this plan where the exact string is worth double-checking against your resolved version.)

- [ ] **Step 4: Verify it builds**

Run: `cargo build --manifest-path desktop/src-tauri/Cargo.toml`
Expected: compiles cleanly.

- [ ] **Step 5: Commit**

```bash
git add desktop/src-tauri/Cargo.toml desktop/src-tauri/Cargo.lock desktop/src-tauri/src/lib.rs desktop/src-tauri/capabilities/default.json
git commit -m "feat(desktop): add shell/dialog/uuid dependencies and capabilities"
```

---

## Task 5: `desktop/src-tauri/src/launch.rs` — pure helpers

**Files:**
- Create: `desktop/src-tauri/src/launch.rs`
- Modify: `desktop/src-tauri/src/lib.rs` (add `mod launch;`)

- [ ] **Step 1: Write the failing tests**

Create `desktop/src-tauri/src/launch.rs`:

```rust
//! Pure, Tauri-free helpers for the sidecar bootstrap — unit-tested with plain `cargo test`,
//! no window/event loop/process needed. Keeps the logic that's actually worth testing out of
//! `lib.rs`'s Tauri-API-heavy glue code.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_line_matches_otto_serves_own_readiness_message() {
        assert!(is_ready_line(
            "otto serve listening on ws://127.0.0.1:8787/ws"
        ));
        assert!(is_ready_line(
            "otto serve listening on wss://127.0.0.1:8787/ws"
        ));
    }

    #[test]
    fn ready_line_rejects_unrelated_output() {
        assert!(!is_ready_line(""));
        assert!(!is_ready_line("warning: something else"));
        assert!(!is_ready_line("otto run finished"));
    }

    #[test]
    fn launch_url_carries_ws_token_and_autoconnect() {
        assert_eq!(
            build_launch_url("ws://127.0.0.1:8787", "abc-123"),
            "index.html?ws=ws://127.0.0.1:8787&token=abc-123&autoconnect=1"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --manifest-path desktop/src-tauri/Cargo.toml launch:: -- --nocapture`
Expected: FAIL to compile — `is_ready_line` and `build_launch_url` don't exist yet.

- [ ] **Step 3: Implement the two functions**

Add above the `#[cfg(test)]` line in `desktop/src-tauri/src/launch.rs`:

```rust
/// True when `line` (one line of the sidecar's stderr) is otto serve's own readiness message
/// (`crates/engine/src/main.rs`: `eprintln!("otto serve listening on {scheme}://{addr}/ws")`).
/// The port is fixed at 8787 by this slice's design (no `--port 0` support yet), so this is a
/// pure "has it started" signal — the address itself isn't parsed out of the line.
pub fn is_ready_line(line: &str) -> bool {
    line.contains("otto serve listening on")
}

/// Build the URL the desktop webview navigates to once the sidecar is ready: the existing
/// `ui/` app's index page with the auto-connect bootstrap query params. `ui/src/url.rs`'s
/// `parse_launch_params` is the reader side of this exact contract — the query key names
/// (`ws`, `token`, `autoconnect`) must match.
///
/// `ws_base` and `token` are never percent-encoded here: `ws_base` is always the fixed
/// `ws://127.0.0.1:8787` (no dynamic port this slice) and `token` is a `Uuid::new_v4()`
/// string (hex digits and hyphens only) — neither can contain a character that needs
/// percent-encoding, so encoding would be dead code.
pub fn build_launch_url(ws_base: &str, token: &str) -> String {
    format!("index.html?ws={ws_base}&token={token}&autoconnect=1")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --manifest-path desktop/src-tauri/Cargo.toml launch:: -- --nocapture`
Expected: PASS — all 3 tests green.

- [ ] **Step 5: Wire the module into `lib.rs`**

At the top of `desktop/src-tauri/src/lib.rs`, add:

```rust
mod launch;
```

- [ ] **Step 6: Verify the whole crate still builds**

Run: `cargo build --manifest-path desktop/src-tauri/Cargo.toml`
Expected: compiles cleanly (clippy would flag `launch` as unused right now if it exported nothing consumed yet — but both functions will be `pub` and are about to be used in Task 6/7, so no dead-code warning should appear; if one does, it's expected to disappear after Task 7 and is not worth chasing here).

- [ ] **Step 7: Commit**

```bash
git add desktop/src-tauri/src/launch.rs desktop/src-tauri/src/lib.rs
git commit -m "feat(desktop): pure readiness-line and launch-URL helpers"
```

---

## Task 6: Folder picker + token + sidecar spawn

**Files:**
- Modify: `desktop/src-tauri/src/lib.rs`

- [ ] **Step 1: Replace the `run()` function**

`desktop/src-tauri/src/lib.rs` currently looks like (after Tasks 4-5):

```rust
mod launch;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_shell::init())
    .plugin(tauri_plugin_dialog::init())
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
```

Replace the whole file with:

```rust
mod launch;

use std::sync::Mutex;

use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_shell::{process::CommandChild, ShellExt};

/// Holds the spawned sidecar's handle so it can be killed on app exit (Task 8). `None` until
/// the sidecar spawns, and stays `None` if the user cancels the folder picker (nothing to kill).
struct SidecarHandle(Mutex<Option<CommandChild>>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_shell::init())
    .plugin(tauri_plugin_dialog::init())
    .manage(SidecarHandle(Mutex::new(None)))
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }

      let Some(folder) = app.dialog().file().blocking_pick_folder() else {
        // No workspace chosen on launch — nothing to serve. Exit cleanly rather than show
        // an empty, disconnected window.
        app.handle().exit(0);
        return Ok(());
      };
      let root = folder
        .into_path()
        .map_err(|e| format!("chosen folder is not a filesystem path: {e}"))?;

      let token = uuid::Uuid::new_v4().to_string();
      let (_rx, child) = app
        .shell()
        .sidecar("otto")
        .map_err(|e| e.to_string())?
        .args([
          "serve",
          "--root",
          &root.to_string_lossy(),
          "--port",
          "8787",
        ])
        .env("OTTO_TOKEN", &token)
        .spawn()
        .map_err(|e| e.to_string())?;
      app
        .state::<SidecarHandle>()
        .0
        .lock()
        .unwrap()
        .replace(child);

      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
```

(`_rx` — the sidecar's output stream — is unused until Task 7, which watches it for the readiness line. `cargo build` will warn on the unused binding name being prefixed with `_`, which suppresses that warning intentionally for this intermediate step.)

- [ ] **Step 2: Verify it builds**

Run: `cargo build --manifest-path desktop/src-tauri/Cargo.toml`
Expected: compiles cleanly, no warnings (the `_rx` prefix suppresses the unused-variable warning; `token` and `child` are both used).

- [ ] **Step 3: Commit**

```bash
git add desktop/src-tauri/src/lib.rs
git commit -m "feat(desktop): folder picker, token generation, sidecar spawn"
```

---

## Task 7: Readiness watching + timeout + webview navigation

**Files:**
- Modify: `desktop/src-tauri/src/lib.rs`

- [ ] **Step 1: Add the readiness-watching task and error dialog helper**

In `desktop/src-tauri/src/lib.rs`, first update the imports at the top:

```rust
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_shell::{
  process::{CommandChild, CommandEvent},
  ShellExt,
};
```

Then replace the line:

```rust
      app
        .state::<SidecarHandle>()
        .0
        .lock()
        .unwrap()
        .replace(child);

      Ok(())
    })
```

with:

```rust
      app
        .state::<SidecarHandle>()
        .0
        .lock()
        .unwrap()
        .replace(child);

      watch_for_readiness(app.handle().clone(), rx, token);

      Ok(())
    })
```

and rename the sidecar spawn's `let (_rx, child) = ...` back to `let (rx, child) = ...` (it's consumed now, no longer unused):

```rust
      let (rx, child) = app
        .shell()
        .sidecar("otto")
```

Finally, add these two functions at the bottom of the file (after `run()`):

```rust
/// Watches the sidecar's output for otto serve's readiness line (with a 5s timeout), then
/// navigates the main window to the bootstrap URL — or shows an error dialog on failure/timeout.
fn watch_for_readiness(
  app: AppHandle,
  mut rx: tokio::sync::mpsc::Receiver<CommandEvent>,
  token: String,
) {
  tauri::async_runtime::spawn(async move {
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
      while let Some(event) = rx.recv().await {
        match event {
          CommandEvent::Stderr(line) => {
            let text = String::from_utf8_lossy(&line);
            if launch::is_ready_line(&text) {
              return Ok(());
            }
          }
          CommandEvent::Terminated(payload) => {
            return Err(format!(
              "otto serve exited before starting (code {:?})",
              payload.code
            ));
          }
          _ => {}
        }
      }
      Err("otto serve's output stream closed unexpectedly".to_string())
    })
    .await;

    match outcome {
      Ok(Ok(())) => {
        let target = launch::build_launch_url("ws://127.0.0.1:8787", &token);
        if let Some(window) = app.get_webview_window("main") {
          match target.parse() {
            Ok(url) => {
              let _ = window.navigate(url);
            }
            Err(e) => show_startup_error(&app, &format!("invalid launch URL: {e}")),
          }
        }
      }
      Ok(Err(message)) => show_startup_error(&app, &message),
      Err(_) => show_startup_error(&app, "otto serve did not start within 5 seconds"),
    }
  });
}

fn show_startup_error(app: &AppHandle, message: &str) {
  app
    .dialog()
    .message(message)
    .title("otto failed to start")
    .kind(MessageDialogKind::Error)
    .blocking_show();
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --manifest-path desktop/src-tauri/Cargo.toml`
Expected: compiles cleanly. If `tokio::sync::mpsc::Receiver` doesn't resolve, check whether `tauri_plugin_shell::process::CommandEvent`'s `spawn()` return type re-exports its own `Receiver` alias (it does, as `tauri::async_runtime::Receiver` — if the compiler complains about a type mismatch, change the parameter type in `watch_for_readiness` from `tokio::sync::mpsc::Receiver<CommandEvent>` to `tauri::async_runtime::Receiver<CommandEvent>`, which is the alias the plugin's `spawn()` actually returns).

- [ ] **Step 3: Commit**

```bash
git add desktop/src-tauri/src/lib.rs
git commit -m "feat(desktop): watch sidecar readiness, navigate webview, error dialog on failure"
```

---

## Task 8: Kill the sidecar on app exit

**Files:**
- Modify: `desktop/src-tauri/src/lib.rs`

- [ ] **Step 1: Switch from `Builder::run` to `build` + `App::run`**

`Builder::run(context)` doesn't take an event callback — `RunEvent::ExitRequested` handling requires building the `App` first, then calling `App::run` with a closure. In `desktop/src-tauri/src/lib.rs`, find the end of `run()`:

```rust
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
```

Replace with:

```rust
    .build(tauri::generate_context!())
    .expect("error while building tauri application")
    .run(|app_handle, event| {
      if let tauri::RunEvent::ExitRequested { .. } = event {
        if let Some(child) = app_handle
          .state::<SidecarHandle>()
          .0
          .lock()
          .unwrap()
          .take()
        {
          let _ = child.kill();
        }
      }
    });
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --manifest-path desktop/src-tauri/Cargo.toml`
Expected: compiles cleanly.

- [ ] **Step 3: Commit**

```bash
git add desktop/src-tauri/src/lib.rs
git commit -m "feat(desktop): kill the sidecar process on app exit"
```

---

## Task 9: Packaging glue script

**Files:**
- Create: `desktop/build-sidecar.sh`

- [ ] **Step 1: Write the script**

Tauri's sidecar resolution expects the binary at `desktop/src-tauri/binaries/otto-<target-triple>` (see `externalBin`'s convention — referenced implicitly by `app.shell().sidecar("otto")`; Tauri appends `-<target-triple>` and, on Windows, `.exe`, then looks in `src-tauri/binaries/`). This script builds `otto` in release mode and copies it into place for the current host.

Create `desktop/build-sidecar.sh`:

```bash
#!/usr/bin/env bash
# Builds the otto binary and stages it as this platform's Tauri sidecar
# (desktop/src-tauri/binaries/otto-<target-triple>), per Tauri's externalBin convention.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
bin_dir="$repo_root/desktop/src-tauri/binaries"

echo "Building otto (release)..."
cargo build --release -p otto-engine --manifest-path "$repo_root/Cargo.toml"

mkdir -p "$bin_dir"
dest="$bin_dir/otto-$target_triple"
if [[ "$target_triple" == *windows* ]]; then
  dest="$dest.exe"
fi
cp "$repo_root/target/release/otto" "$dest"
echo "Staged sidecar: $dest"
```

- [ ] **Step 2: Make it executable**

Run: `chmod +x desktop/build-sidecar.sh`

- [ ] **Step 3: Add the `externalBin` config now that the binary will exist**

In `desktop/src-tauri/tauri.conf.json`, add `externalBin` to the `bundle` object:

```json
  "bundle": {
    "active": true,
    "targets": "all",
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.icns",
      "icons/icon.ico"
    ],
    "externalBin": ["binaries/otto"]
  }
```

- [ ] **Step 4: Run it and verify the binary lands**

Run: `./desktop/build-sidecar.sh`
Expected: prints "Staged sidecar: .../desktop/src-tauri/binaries/otto-x86_64-unknown-linux-gnu" (or your host's actual target triple) and the file exists.

Run: `ls desktop/src-tauri/binaries/`
Expected: one `otto-<target-triple>` file present.

- [ ] **Step 5: Ignore the staged binary and the crate's own build output**

Add to `desktop/src-tauri/.gitignore` (create if `cargo tauri init` didn't already, or append to the existing one):

```
binaries/
```

- [ ] **Step 6: Commit**

```bash
git add desktop/build-sidecar.sh desktop/src-tauri/tauri.conf.json desktop/src-tauri/.gitignore
git commit -m "feat(desktop): sidecar build/staging script, externalBin config"
```

---

## Task 10: Manual smoke test + final verification

**Files:** none (verification only).

- [ ] **Step 1: Build the frontend**

Run: `cd ui && trunk build`
Expected: produces `ui/dist/index.html` and assets.

- [ ] **Step 2: Stage the sidecar**

Run: `./desktop/build-sidecar.sh` (from the repo root)
Expected: as in Task 9 Step 4.

- [ ] **Step 3: Run the desktop app**

Run: `cargo tauri dev --config desktop/src-tauri/tauri.conf.json` (or `cd desktop/src-tauri && cargo tauri dev`)
Expected walkthrough:
1. A native "Open Folder" dialog appears. Pick any directory (e.g. the repo root itself, or a scratch dir with a couple of files).
2. The dialog closes; within ~1-2 seconds a window opens showing the otto UI, already connected (no login form) — the status strip shows `Connected`.
3. Type a prompt and send it; confirm the turn runs and events stream in (offline/deterministic is fine — no `ANTHROPIC_API_KEY` needed for this smoke test).
4. Use the file tree to confirm it lists the chosen folder's contents.
5. Click "Connect to a different engine" — confirm it disconnects and shows the manual form with the default `ws://127.0.0.1:8787` URL and an empty token (Task 3's fallback).

- [ ] **Step 4: Verify process cleanup on quit**

While the app is running, in a separate terminal: `ps aux | grep "[o]tto serve"` — confirm one process is running with `--port 8787`.

Close the app window. Then re-run: `ps aux | grep "[o]tto serve"`
Expected: no output — the sidecar process is gone (Task 8's `ExitRequested` handler killed it).

- [ ] **Step 5: Verify the startup-failure path**

With the app closed (previous sidecar killed), start something else on port 8787 first: `python3 -m http.server 8787 &`. Then run `cargo tauri dev` again from `desktop/src-tauri`, pick any folder.
Expected: after picking the folder, an error dialog appears (title "otto failed to start") reporting that `otto serve` exited before starting — because the port bind failed. Close the dialog; the app may still show a blank/disconnected window, which is acceptable for this dev-mode-only slice (no retry UX).

Kill the python server: `kill %1` (or find and kill the `http.server` process).

- [ ] **Step 6: Full-repo regression check**

Run: `cargo build --workspace && cargo test --workspace && cargo fmt --all -- --check && cargo clippy --workspace --all-targets`
Expected: all clean/green — this entire slice added zero files/changes inside the cargo workspace's member crates (only `ui/` and the new workspace-excluded `desktop/`), so the offline determinism suite and every existing test is untouched.

Run: `cd ui && cargo test`
Expected: all `ui/` host-side tests pass, including every test added in Task 2.

Run: `cargo test --manifest-path desktop/src-tauri/Cargo.toml`
Expected: all `desktop/src-tauri` tests pass, including every test added in Task 5.

- [ ] **Step 7: No commit this task** (verification only — nothing changed).

---

## Spec coverage check

- Sidecar mode (auto-launch local `otto serve`, no manual setup) → Tasks 1, 6, 7.
- Fixed port 8787, no engine changes → Task 6 (hardcoded `--port 8787`; `crates/engine/` is never touched by this plan).
- Folder picker, re-prompted every launch (no persistence) → Task 6 Step 1.
- Token via env var, never a CLI arg → Task 6 Step 1 (`.env("OTTO_TOKEN", &token)`).
- Readiness detection + timeout + failure dialog → Task 7.
- Webview navigation with bootstrap query params → Task 7 (`watch_for_readiness`) + Task 5 (`build_launch_url`).
- `ui/` auto-connect + manual-form fallback, zero new `ui/` dependencies → Tasks 2, 3.
- Kill sidecar on quit → Task 8.
- `desktop/` workspace-excluded, zero `otto-*` crate dependencies → Task 1 (exclude), and no task ever adds an `otto-*` dependency to `desktop/src-tauri/Cargo.toml`.
- Packaging (dev-mode only, no installers) → Task 9 provides just enough to run `cargo tauri dev`; no signing/bundling step is included, matching the spec's explicit boundary.
- Manual smoke test (macOS + Linux) → Task 10 (commands are OS-agnostic; run once per platform).
- No regression to `cargo build --workspace` / determinism suite → Task 10 Step 6.
