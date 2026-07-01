# Sub-project G — Tauri desktop wrapper (design)

**Status:** Approved (2026-07-01). New sub-project appended to the roadmap
([2026-06-17-ui-roadmap.md](2026-06-17-ui-roadmap.md), row **G**) — the original
decomposition scoped "the full otto UI" to A–F (all shipped) and named the Tauri
wrapper only as a future item ("added in a later sub-project, reusing the same WASM
bundle," per sub-project A's fixed decisions). This spec scopes that item.

## Goal

Wrap the existing browser UI in a native desktop app: double-click an icon, pick a
workspace folder, and get a working otto session with **no manual `otto serve` /
URL / token setup**. This is the originally-envisioned **EMBEDDED (v1 default)** mode
from `ARCHITECTURE.md` — everything shipped so far (A–F) is the **SERVED (v2)** mode,
a pure WebSocket client against an already-running, manually-started `otto serve`.

## Key insight

This composes cleanly with everything already built, without touching engine code.
Tauri's **sidecar** mechanism spawns the compiled `otto` binary as an opaque external
subprocess and talks to it over the exact same bearer-authed WebSocket/HTTP protocol
the browser client already speaks — no `engine-core` linking, no protocol changes.
It's the same "talk to it as a black box over a wire protocol" pattern the MCP tool
crates already use (`crates/mcp-fs`, `crates/mcp-bash`, etc.), applied to `otto serve`
itself instead of a tool server. `ui/`'s architectural invariant — "depends only on
`protocol`, must never link `engine-core`" — is preserved unchanged: `ui/` doesn't
even know it's running inside Tauri except via a URL query string.

## Fixed decisions (from brainstorming)

1. **Sidecar mode, not a pure shell.** Tauri auto-launches a local `otto serve` on
   app start (own token, fixed port) rather than just wrapping the existing
   manual-connect client in a native window. This is the meaningfully different,
   originally-envisioned desktop experience — "it just works," matching the
   EMBEDDED (v1 default) mode `ARCHITECTURE.md` describes.
2. **macOS + Linux only this slice.** Matches where the OS sandbox
   (`bwrap`/`sandbox-exec`) already works. Windows has no sandbox backend yet
   (`crates/tools/src/sandbox.rs`), so `bash` would be silently absent there
   regardless of this feature; Windows is deferred rather than shipped degraded
   without comment.
3. **Dev-mode packaging only.** `cargo tauri dev` / a debug build runs the app —
   matches how sub-project A shipped browser-first via `trunk serve` before any
   production build concerns. Signed installers (dmg/AppImage), auto-update, and CI
   packaging are a later, separate infra thread (alongside the still-unbuilt
   `otto serve` deployment pipeline).
4. **Fixed port 8787, no engine changes.** The sidecar always binds
   `127.0.0.1:8787` — the same default the UI's connect form already hardcodes
   (`ui/src/app.rs`). This was a real fork: `otto serve --port 0` (OS-assigned port)
   doesn't actually work today — `crates/engine/src/main.rs`'s startup log line and
   the public WS base URL are both built from the *requested* port before the
   listener binds, not the socket's real address (`listener.local_addr()`), so
   `--port 0` would misreport port `0`. Fixing that is a small, legitimate engine
   change, but it's explicitly deferred: this slice stays 100% additive at the
   Tauri/UI layer, and the desktop app treats `otto` as a pure black-box subprocess.
   Consequence: no multi-window/multi-workspace support this slice (see Out of scope).

## New crate: `desktop/`

- Sibling to `ui/`, **excluded from the cargo workspace** exactly like `ui/` is (add
  to the root `Cargo.toml`'s `exclude` list) — so `cargo build --workspace` and the
  offline determinism suite stay untouched.
- A Tauri 2 Rust crate (no nested `src-tauri/` — `desktop/` *is* the Tauri crate).
  `tauri.conf.json`'s `build.frontendDist` points at `../ui/dist` (the existing
  `trunk build` output, unmodified); `bundle.externalBin` references the compiled
  `otto` binary as a sidecar (Tauri's target-triple-suffixed binary convention, e.g.
  `binaries/otto-x86_64-apple-darwin` / `binaries/otto-aarch64-apple-darwin` /
  `binaries/otto-x86_64-unknown-linux-gnu`).
- Dependencies: `tauri`, `tauri-plugin-shell` (spawn + monitor the sidecar),
  `tauri-plugin-dialog` (native folder picker), `uuid` (token generation). **Zero**
  `otto-*` crate dependencies — `desktop/` never imports `engine-core`, `protocol`,
  or any otto crate; it only shells out to the `otto` binary and manipulates the
  webview's URL.

## Launch flow

1. On app start, a native "Open Folder" dialog (`tauri-plugin-dialog`) prompts for a
   workspace root. Re-prompted every launch — no persisted "recent workspaces" list
   in v1 (see Out of scope).
2. Generate a random bearer token (`uuid::Uuid::new_v4()`). Spawn the sidecar:
   `otto serve --root <chosen> --port 8787`, with `OTTO_TOKEN=<token>` passed as an
   **environment variable**, never a CLI argument — so it never appears in a process
   listing (`ps`/Activity Monitor), matching the existing `OTTO_TOKEN=<token> ...`
   invocation convention documented in the root `CLAUDE.md` Commands section.
3. Watch the sidecar's stderr (via `tauri-plugin-shell`'s event stream) for its
   `otto serve listening on ws://127.0.0.1:8787/ws` line as the readiness signal,
   with a timeout (e.g. 5s). Two failure paths, both surfaced as a native error
   dialog (never a blank/hung window):
   - The process exits before printing the readiness line (e.g. port 8787 already
     in use by another instance or an unrelated process) → show the sidecar's
     stderr output in the dialog.
   - The timeout elapses with no readiness line and the process is still running
     (unexpected hang) → kill it and show a generic startup-timeout error.
4. Once ready, navigate the main window's webview to
   `index.html?ws=ws://127.0.0.1:8787&token=<token>&autoconnect=1`.
5. On app quit or the main window closing (`tauri`'s `RunEvent::ExitRequested` /
   `on_window_event(CloseRequested)`), kill the sidecar child process. No orphaned
   `otto serve` processes should survive the app.

## UI changes (`ui/`, still depends only on `protocol`)

- A new pure helper in `ui/src/url.rs` (host-testable with plain `cargo test`, no
  DOM) parses `ws`/`token`/`autoconnect` from a query string:
  `pub fn parse_launch_params(query: &str) -> Option<LaunchParams>` returning
  `{ ws: String, token: String }` when both `ws` and `token` are present and
  `autoconnect=1`, else `None`. Pure string/query parsing — no new dependency
  (the crate already depends on `urlencoding`).
- `app.rs`'s startup path calls `web_sys::window().location().search()` (an
  existing-category browser API call — `ui/` already does DOM/`web_sys` work) and,
  if `parse_launch_params` returns `Some`, pre-fills the URL/token signals and
  immediately triggers the existing connect flow — skipping the manual login form
  entirely. If `None` (plain browser access, or Tauri's manual "Connect to remote
  engine" path — see below), behavior is **exactly** today's: the manual form shows.
- No change to `ui/`'s `Cargo.toml`. No Tauri-specific dependency, no `#[cfg]`
  target-gating — the same `ui/dist` build artifact serves both the browser and the
  Tauri webview unmodified, which is exactly what makes this additive rather than a
  fork.
- The existing manual URL/token connect form remains reachable (e.g. a "Connect to
  a different engine" link/button when the pre-filled autoconnect form is showing),
  so a desktop-app user can still point at a promoted/VPS-hosted `otto serve` —
  sub-project F's promote/demote flow is completely untouched by this slice.

## Testing

- **`desktop/` (Rust, host-native, not wasm):** unit tests for the readiness-line
  parser (given a line of stderr text, extract "ready" vs "not yet"), the
  timeout/failure-dialog decision logic, and the launch-URL construction — all pure
  functions, independent of an actual spawned process or a real window.
- **`ui/src/url.rs`:** `parse_launch_params` unit tests — present/absent each of
  `ws`/`token`/`autoconnect`, malformed query strings, URL-encoded token values.
  Runs under plain `cargo test` (the existing host-side determinism seam for `ui/`
  logic — no wasm, no DOM), consistent with `build_ws_url`'s existing tests.
- **Manual/smoke (no automated E2E this slice):** `cargo tauri dev` on macOS and
  Linux — folder picker → sidecar spawns → webview auto-connects → a prompt runs
  end-to-end → closing the window kills the `otto` process (verified via `ps`).
  Automated cross-process E2E (spawn Tauri, drive its webview, assert on the
  spawned child) is out of scope for this slice; the existing `crates/engine/tests/`
  loopback/serve integration tests already cover the engine side of this contract.
- **Determinism:** unchanged. No engine code changes at all in this slice, so the
  offline/deterministic test suite is untouched by construction.

## Build sequence

1. Scaffold `desktop/` (Tauri 2 project, `tauri.conf.json` pointing at `../ui/dist`),
   add it to the root `Cargo.toml`'s workspace `exclude` list.
2. `ui/src/url.rs`: `parse_launch_params` + tests.
3. `ui/src/app.rs`: read `location.search` on startup, auto-fill + auto-connect when
   launch params are present, add the "connect to a different engine" fallback link.
4. `desktop/`: sidecar spawn (`tauri-plugin-shell`), stderr readiness parsing +
   timeout/failure dialog, folder picker (`tauri-plugin-dialog`), token generation,
   webview navigation with launch params, kill-on-exit wiring.
5. Packaging glue: a small script/Makefile target that builds `otto` (release),
   copies it into `desktop/binaries/otto-<target-triple>` per Tauri's sidecar
   naming convention, builds `ui/` via `trunk build`, then runs `cargo tauri dev`
   (or `build` for a local debug bundle — still not a signed/distributable
   installer this slice).
6. Tests: the two pure-function test suites above; manual smoke pass on macOS + Linux.

## Out of scope (boundaries)

- Windows support (no OS sandbox backend yet — deferred with the wrapper, not
  shipped silently degraded).
- Signed installers, auto-update, CI packaging/release pipeline (separate infra
  thread, alongside the still-unbuilt `otto serve` deployment story).
- OS-assigned port / multi-window / multi-workspace-per-app-instance (needs the
  `otto serve --port 0` engine fix named above; deferred).
- Persisted "recent workspaces" (re-pick the folder every launch this slice).
- Automated Tauri-level E2E testing (manual smoke only this slice).
- Any change to `otto serve`, `protocol`, or `engine-core` — this slice is
  Tauri-layer + a two-function `ui/` addition only.
