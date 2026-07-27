# Dioxus UI Migration Phase 2 — Build & Serve Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `otto serve` host the Dioxus web bundle, make `dx bundle` produce an installable desktop app with the `otto` sidecar staged inside it, and give both a checked-in build script — so Phase 3 can sign off parity against real artifacts instead of dev servers.

**Architecture:** One additive axum layer (`with_ui_dir`) applied after router construction, so no existing constructor or call site changes. Desktop packaging moves from Tauri's `externalBin` to Dioxus 0.7's `[bundle] external_bin`, with the sidecar path resolved at runtime from a layout that is **measured from a built `.deb`, not inferred from docs**. Three scripts encode the build commands; the Fly image builds the web bundle in its own stage and serves it.

**Tech Stack:** Rust (edition 2024, toolchain pinned 1.85), axum + tower-http `fs`, Dioxus 0.7 / `dx` 0.7.9, Docker (Fly deploy image), bash scripts.

**Spec:** `docs/superpowers/specs/2026-07-27-ui-dioxus-build-serve-design.md` — read it first. This plan implements it exactly.

## Global Constraints

- **Never widen the permission surface.** `ServeDir` does not consult the sensitive-path floor. `--ui-dir` has **no default and no env fallback**; when absent, the route is not installed at all. It must never point at a workspace root.
- **The static route is unauthenticated by design.** `/ws`, `/workspace`, `/promote`, `/export` keep their bearer checks unchanged. Do not add auth to the static route — it would break first load.
- **`ui/` and `desktop/` are untouched.** They keep building and shipping until Phase 4. Do not delete, edit, or repoint them.
- **No protocol, agent, or orchestrator change.** The only workspace-crate edits in this plan are `crates/engine/src/serve.rs`, `crates/engine/src/lib.rs`, `crates/engine/src/main.rs`, and one feature in `crates/engine/Cargo.toml`.
- **Both UI crates stay workspace-excluded.** `cargo build --workspace` and `cargo test --workspace` must never require `dx`.
- **`[application] name` stays `otto-ui-dioxus`.** `ui-dioxus/scripts/measure-web-bundle.sh` hardcodes it. The rename to `otto-desktop` belongs to Phase 4.
- **`dx` version is pinned to 0.7.9** wherever it is installed (the version this migration was verified against).
- **No Claude/AI self-attribution** in any commit message, comment, or doc.
- Run `cargo fmt --all` before every Rust commit; rustfmt is pinned in `rust-toolchain.toml`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/engine/src/serve.rs` | **Modify.** Add `with_ui_dir` — the static-fallback layer + its two security invariants as doc comments. |
| `crates/engine/src/lib.rs:46-48` | **Modify.** Re-export it as `serve_with_ui_dir`. |
| `crates/engine/src/main.rs` | **Modify.** `parse_ui_dir` helper + unit tests; apply the layer in `cmd_serve`; update both usage strings. |
| `crates/engine/Cargo.toml:39` | **Modify.** `tower-http` gains the `fs` feature. |
| `crates/engine/tests/ui_dir.rs` | **Create.** HTTP-level tests for the route, including the route-absent regression guard. |
| `ui-dioxus/Dioxus.toml` | **Modify.** Add the `[bundle]` block. |
| `ui-dioxus/icons/` | **Create.** Copied from `desktop/src-tauri/icons/` before Phase 4 deletes the original. |
| `ui-dioxus/.gitignore` | **Modify.** Ignore `/binaries`. |
| `ui-dioxus/scripts/stage-sidecar.sh` | **Create.** Builds release `otto`, stages `binaries/otto-<triple>`. |
| `ui-dioxus/scripts/build-web.sh` | **Create.** Owns the release web build **and all four trust guards**; prints the bundle dir. |
| `ui-dioxus/scripts/measure-web-bundle.sh` | **Modify.** Becomes a thin wrapper over `build-web.sh` + the size table. |
| `ui-dioxus/scripts/build-desktop.sh` | **Create.** `stage-sidecar.sh`, then `dx bundle`. |
| `ui-dioxus/src/desktop_boot.rs:80` | **Modify.** Three-step sidecar binary resolution + unit tests. |
| `deploy/fly/Dockerfile` | **Modify.** Web-bundle build stage; copy bundle; `OTTO_UI_DIR`; `--ui-dir` in `CMD`. |
| `deploy/fly/README.md` | **Modify.** Browser URL + token requirement. |
| `docs/superpowers/plans/2026-07-22-ui-dioxus-migration.md` | **Modify.** Rewrite the Phase 2 bullets against the three premise corrections; mark Phase 2 complete. |

## Task Order & Rationale

1–2 are the engine (independent of everything else). 3 **measures** the bundle layout; 4 implements resolution **against that measurement** — this order is mandatory, not stylistic. 5 is the scripts. 6 (Fly) depends on `build-web.sh` from 5. 7 is docs.

---

### Task 1: `with_ui_dir` static route on the serve router

**Files:**
- Modify: `crates/engine/Cargo.toml:39`
- Modify: `crates/engine/src/serve.rs` (imports near `:30`, new fn after `app_with_base` which ends `:134`)
- Modify: `crates/engine/src/lib.rs:46-48`
- Test: `crates/engine/tests/ui_dir.rs` (create)

**Interfaces:**
- Consumes: `serve::app` (`serve.rs:100`), unchanged.
- Produces: `pub fn with_ui_dir(app: AxumRouter, dir: PathBuf) -> AxumRouter`, re-exported from the crate root as `serve_with_ui_dir`. Task 2 calls the re-exported name.

- [ ] **Step 1: Add the `fs` feature to tower-http**

In `crates/engine/Cargo.toml:39`, change:

```toml
tower-http = { version = "0.6", features = ["cors"] }
```

to:

```toml
tower-http = { version = "0.6", features = ["cors", "fs"] }
```

- [ ] **Step 2: Write the failing test**

Create `crates/engine/tests/ui_dir.rs`. This is modeled on the existing `crates/engine/tests/cors.rs` — same in-process `tower::ServiceExt::oneshot` approach, no port binding, no network.

```rust
//! `otto serve --ui-dir <path>` serves a pre-built web UI bundle as the router's fallback.
//!
//! The route is deliberately unauthenticated (a browser must fetch index.html and the wasm before
//! it has a token) and deliberately absent when `--ui-dir` is not passed. Both properties are
//! asserted here — the second is the regression guard that keeps this feature inert by default.

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, serve_app, serve_with_ui_dir,
};
use otto_engine_core::traits::Workspace;
use otto_providers::LocalProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use tower::ServiceExt; // for `oneshot`

const TOKEN: &str = "test-token";

/// Build the serve router (unbound), optionally with a `--ui-dir` bundle directory layered on.
async fn build_app(ui_dir: Option<PathBuf>) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(LocalProvider::new())));
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
    let app = serve_app(
        service,
        TOKEN.to_string(),
        otto_protocol::CapabilitiesManifest {
            engine_remote: false,
            local_llm: false,
            remote_llm: false,
            sandbox: false,
        },
        None,
        false,
    );
    let app = match ui_dir {
        Some(d) => serve_with_ui_dir(app, d),
        None => app,
    };
    (app, dir)
}

/// A throwaway "bundle": an index and one hashed asset, the shape `dx build` emits.
fn write_bundle() -> tempfile::TempDir {
    let bundle = tempfile::tempdir().unwrap();
    std::fs::write(bundle.path().join("index.html"), b"<html>otto ui</html>").unwrap();
    std::fs::create_dir_all(bundle.path().join("assets")).unwrap();
    std::fs::write(bundle.path().join("assets/app-abc123.wasm"), b"\0asm-fake").unwrap();
    bundle
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn serves_index_at_root_without_a_token() {
    let bundle = write_bundle();
    let (app, _dir) = build_app(Some(bundle.path().to_path_buf())).await;
    let req = axum::http::Request::builder()
        .uri("/")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(
        resp.status().is_success(),
        "GET / must succeed with no Authorization header — a browser has no token on first \
         load, got {}",
        resp.status()
    );
    assert!(body_string(resp).await.contains("otto ui"));
}

#[tokio::test]
async fn serves_a_hashed_asset_without_a_token() {
    let bundle = write_bundle();
    let (app, _dir) = build_app(Some(bundle.path().to_path_buf())).await;
    let req = axum::http::Request::builder()
        .uri("/assets/app-abc123.wasm")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(resp.status().is_success(), "got {}", resp.status());
}

#[tokio::test]
async fn unknown_path_falls_back_to_index() {
    let bundle = write_bundle();
    let (app, _dir) = build_app(Some(bundle.path().to_path_buf())).await;
    let req = axum::http::Request::builder()
        .uri("/no/such/path")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(resp.status().is_success(), "got {}", resp.status());
    assert!(body_string(resp).await.contains("otto ui"));
}

/// The regression guard: with no `--ui-dir`, the feature must be completely inert.
#[tokio::test]
async fn without_ui_dir_root_is_not_served() {
    let (app, _dir) = build_app(None).await;
    let req = axum::http::Request::builder()
        .uri("/")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::NOT_FOUND,
        "with no --ui-dir there must be no static route at all"
    );
}

/// The existing API routes must behave identically with the layer installed. `/ws` without an
/// upgrade is the cheapest probe that proves the fallback did not swallow a real route.
#[tokio::test]
async fn existing_routes_are_unaffected_by_the_fallback() {
    let bundle = write_bundle();
    let (app, _dir) = build_app(Some(bundle.path().to_path_buf())).await;
    let req = axum::http::Request::builder()
        .uri("/ws")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_ne!(
        resp.status(),
        axum::http::StatusCode::OK,
        "/ws must still be handled by the ws route (rejecting a non-upgrade request), not \
         served as a static file"
    );
    assert!(
        !body_string(resp).await.contains("otto ui"),
        "/ws must never fall through to index.html"
    );
}

/// `ServeDir` must not escape the bundle directory. This matters more here than in a normal
/// static server: the sensitive-path floor that guards every other file access in otto does not
/// apply to this route.
///
/// The bundle is nested one level down inside the tempdir so the "outside" file has somewhere
/// real to live — putting it in the shared system temp dir under a fixed name would collide
/// between concurrent test runs.
#[tokio::test]
async fn path_traversal_does_not_escape_the_bundle_dir() {
    let outer = tempfile::tempdir().unwrap();
    let bundle_dir = outer.path().join("public");
    std::fs::create_dir_all(&bundle_dir).unwrap();
    std::fs::write(bundle_dir.join("index.html"), b"<html>otto ui</html>").unwrap();
    std::fs::write(outer.path().join("outside-secret.txt"), b"TOP SECRET").unwrap();

    let (app, _dir) = build_app(Some(bundle_dir)).await;
    let req = axum::http::Request::builder()
        .uri("/../outside-secret.txt")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(
        !body_string(resp).await.contains("TOP SECRET"),
        "traversal must not read outside the bundle dir"
    );
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p otto-engine --test ui_dir`
Expected: FAIL to compile — `unresolved import otto_engine::serve_with_ui_dir`.

- [ ] **Step 4: Add the `with_ui_dir` layer**

In `crates/engine/src/serve.rs`, add to the imports (next to the existing `use tower_http::cors::{Any, CorsLayer};` at `:30`):

```rust
use tower_http::services::{ServeDir, ServeFile};
```

Then add this function immediately after `app_with_base` (which ends at `:134`):

```rust
/// Serve a pre-built web UI bundle from `dir` as this router's fallback.
///
/// Applied *after* construction rather than threaded through `app`/`app_with_base`. Those two
/// already differ only by one optional argument; a `ui_dir` parameter would make four
/// constructors and churn all six call sites. This layer changes none of them.
///
/// ## Two security invariants, both deliberate
///
/// 1. **This route is unauthenticated on purpose.** A browser must fetch `index.html` and the
///    wasm *before* it has a token to present, so requiring a bearer here would break first
///    load. It is safe because the bundle is public build output: every path that carries
///    session data or workspace contents — `/ws`, `/workspace`, `/promote`, `/export` — keeps
///    its own bearer check, unchanged. Do not "fix" this by adding auth.
///
/// 2. **`dir` must never be a workspace root.** `ServeDir` does *not* consult the permission
///    gate's sensitive-path floor, so pointing this at a workspace would serve `.env`, `.ssh/`,
///    and `.git/` over plain HTTP — bypassing the single most important invariant in the
///    codebase. It is operator-supplied via `--ui-dir`, has no default and no env fallback, and
///    when the flag is absent this layer is never applied and the route does not exist.
pub fn with_ui_dir(app: AxumRouter, dir: PathBuf) -> AxumRouter {
    let index = dir.join("index.html");
    app.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
}
```

- [ ] **Step 5: Re-export it**

In `crates/engine/src/lib.rs`, the `pub use serve::{...}` block at `:46-48` — add `with_ui_dir as serve_with_ui_dir` to the list, keeping the existing entries and alphabetical-ish ordering:

```rust
pub use serve::{
    app as serve_app, app_with_base as serve_app_with_base, resolve_tls_paths, run as serve_run,
    with_ui_dir as serve_with_ui_dir,
};
```

(If the existing block has more entries than shown, keep every one of them and only add the new line.)

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p otto-engine --test ui_dir`
Expected: PASS — 6 tests.

- [ ] **Step 7: Verify nothing else regressed**

Run: `cargo test --workspace`
Expected: PASS. One pre-existing failure is known and unrelated — `mcp-lsp`'s rust-analyzer round-trip fails on `main` already. Everything else must pass.

- [ ] **Step 8: Format and commit**

```bash
cargo fmt --all
git add crates/engine/Cargo.toml crates/engine/src/serve.rs crates/engine/src/lib.rs crates/engine/tests/ui_dir.rs
git commit -m "engine: serve a web UI bundle from a --ui-dir static fallback"
```

---

### Task 2: Wire `--ui-dir` into the `otto serve` CLI

**Files:**
- Modify: `crates/engine/src/main.rs:1` and `:31` (usage strings), `:563` (`cmd_serve`), `:704` (app construction), and the `#[cfg(test)] mod tests` block at `:728`
- Test: same file's test module (this crate tests CLI parsing inline, next to the code — see `parse_agent_flag_extracts_name` at `:730`)

**Interfaces:**
- Consumes: `otto_engine::serve_with_ui_dir` from Task 1.
- Produces: `fn parse_ui_dir(args: &[String]) -> Option<PathBuf>` — private to `main.rs`, no later task depends on it.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `crates/engine/src/main.rs` (alongside the existing `parse_agent_flag_*` tests):

```rust
#[test]
fn parse_ui_dir_extracts_path() {
    let args = vec![
        "--port".to_string(),
        "9000".to_string(),
        "--ui-dir".to_string(),
        "/srv/otto-ui".to_string(),
    ];
    assert_eq!(parse_ui_dir(&args), Some(PathBuf::from("/srv/otto-ui")));
}

/// The security-relevant default: absent flag means the static route is never installed.
/// There is deliberately no default path and no env fallback — see `serve::with_ui_dir`.
#[test]
fn parse_ui_dir_absent_is_none() {
    let args = vec!["--port".to_string(), "9000".to_string()];
    assert_eq!(parse_ui_dir(&args), None);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine --bin otto parse_ui_dir`
Expected: FAIL to compile — `cannot find function parse_ui_dir`.

- [ ] **Step 3: Add the parser**

In `crates/engine/src/main.rs`, next to `parse_agent_flag` (`:59`), add:

```rust
/// Parse `--ui-dir <path>` from serve args. `None` means the static UI route is not installed.
///
/// SECURITY: deliberately no default and no env fallback. `ServeDir` does not consult the
/// sensitive-path floor, so a defaulted or inferred value pointing at a workspace root would
/// serve `.env`/`.ssh/` over plain HTTP. See `serve::with_ui_dir`. Deployments that configure
/// this through the environment pass it as a flag from their launcher — as
/// `deploy/fly/Dockerfile`'s `CMD` already does for `OTTO_PORT` and `OTTO_ROOT`.
fn parse_ui_dir(args: &[String]) -> Option<PathBuf> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--ui-dir" {
            match it.next() {
                Some(p) => return Some(PathBuf::from(p)),
                None => {
                    eprintln!("error: --ui-dir requires a path");
                    std::process::exit(2);
                }
            }
        }
    }
    None
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine --bin otto parse_ui_dir`
Expected: PASS — 2 tests.

- [ ] **Step 5: Apply the layer in `cmd_serve`**

In `cmd_serve` (`:563`), right after the existing `let (root, positional) = parse_root(&args);`, add:

```rust
    let ui_dir = parse_ui_dir(&positional);
```

Then at `:704`, where `serve_app_with_base` builds the app, wrap the result:

```rust
    let app = serve_app_with_base(
        service,
        token,
        capabilities,
        promote,
        accept_promotions,
        public_ws_base,
    );
    // Static UI route: installed only when the operator passed --ui-dir. Absent by default —
    // see `serve::with_ui_dir` for why this must never be defaulted or inferred.
    let app = match ui_dir {
        Some(dir) => {
            eprintln!("otto serve serving web UI from {}", dir.display());
            otto_engine::serve_with_ui_dir(app, dir)
        }
        None => app,
    };
```

The existing arg-parsing `match` loop needs no new arm — its `_ => {}` already ignores tokens it does not recognize, and `parse_ui_dir` reads the same slice independently.

- [ ] **Step 6: Update both usage strings**

`crates/engine/src/main.rs:2` (the module doc line) — add `[--ui-dir <path>]` to the `otto serve` synopsis:

```rust
//! `otto serve [--root <path>] [--port <p>] [--ui-dir <path>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm | --promote-fly] [--accept-promotions]` — serve over WebSocket (needs OTTO_TOKEN).
```

`:31` (the runtime usage message) — same insertion in the `otto serve` line of that string.

- [ ] **Step 7: Verify the flag end-to-end by hand**

```bash
cargo build -p otto-engine
mkdir -p /tmp/fake-ui && echo '<html>hello otto</html>' > /tmp/fake-ui/index.html
OTTO_TOKEN=t ./target/debug/otto serve --port 7999 --ui-dir /tmp/fake-ui &
sleep 2
curl -s http://127.0.0.1:7999/ | grep -q "hello otto" && echo "UI-DIR OK" || echo "UI-DIR FAILED"
kill %1
```

Expected: `UI-DIR OK`.

Then confirm it stays absent by default:

```bash
OTTO_TOKEN=t ./target/debug/otto serve --port 7998 &
sleep 2
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7998/
kill %1
```

Expected: `404`.

- [ ] **Step 8: Format and commit**

```bash
cargo fmt --all
git add crates/engine/src/main.rs
git commit -m "engine: add otto serve --ui-dir flag"
```

---

### Task 3: Desktop bundle config, staged sidecar, and **measure** the bundle layout

This task's deliverable is a built `.deb` **and a recorded fact**: where `dx` actually places `external_bin` inside it. Task 4 depends on that fact. Do not skip the measurement and do not substitute a guess — three confident process-behavior claims in this project's history turned out wrong when finally probed.

**Files:**
- Create: `ui-dioxus/icons/` (copied from `desktop/src-tauri/icons/`)
- Create: `ui-dioxus/scripts/stage-sidecar.sh`
- Modify: `ui-dioxus/Dioxus.toml`
- Modify: `ui-dioxus/.gitignore`

**Interfaces:**
- Produces: `ui-dioxus/binaries/otto-<host-triple>` (a build artifact, gitignored); a `[bundle]` block in `Dioxus.toml`; and a recorded bundle layout that Task 4 consumes.

- [ ] **Step 1: Copy the icons out of `desktop/` before Phase 4 deletes them**

```bash
cd /home/robhicks/dev/otto-next
mkdir -p ui-dioxus/icons
cp desktop/src-tauri/icons/32x32.png \
   desktop/src-tauri/icons/128x128.png \
   desktop/src-tauri/icons/128x128@2x.png \
   desktop/src-tauri/icons/icon.icns \
   desktop/src-tauri/icons/icon.ico \
   ui-dioxus/icons/
ls ui-dioxus/icons/
```

Expected: five files listed. Do **not** delete the originals — `desktop/` stays shipped until Phase 4.

- [ ] **Step 2: Ignore the staged sidecar directory**

`ui-dioxus/.gitignore` currently contains only `/target`. Add a second line:

```gitignore
/target
# Staged `otto` sidecar binaries for `dx bundle` ([bundle] external_bin in Dioxus.toml).
# Build artifacts produced by scripts/stage-sidecar.sh — never committed.
/binaries
```

- [ ] **Step 3: Write the sidecar staging script**

Create `ui-dioxus/scripts/stage-sidecar.sh`:

```bash
#!/usr/bin/env bash
# Builds the otto binary and stages it as this platform's Dioxus bundle sidecar
# (ui-dioxus/binaries/otto-<target-triple>), per the `[bundle] external_bin` entry in
# Dioxus.toml.
#
# dx uses the same target-triple-suffix convention Tauri's `externalBin` did, which is why this
# is a near-copy of desktop/build-sidecar.sh — the script it replaces when Phase 4 retires the
# Tauri wrapper. Keep the two in sync until then.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
bin_dir="$repo_root/ui-dioxus/binaries"

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

Then:

```bash
chmod +x ui-dioxus/scripts/stage-sidecar.sh
```

- [ ] **Step 4: Run it and verify the binary is staged**

```bash
cd /home/robhicks/dev/otto-next && ./ui-dioxus/scripts/stage-sidecar.sh
ls -la ui-dioxus/binaries/
```

Expected: one `otto-x86_64-unknown-linux-gnu` (or your host triple), executable, tens of MB.

- [ ] **Step 5: Add the `[bundle]` block**

Append to `ui-dioxus/Dioxus.toml`:

```toml
# Desktop packaging (Phase 2 of the Dioxus migration). Ported field-for-field from the Tauri
# wrapper's tauri.conf.json, which this replaces in Phase 4.
#
# `identifier` deliberately matches the Tauri app's, so installing this package upgrades the
# Tauri one rather than sitting alongside it as a confusing second install.
#
# `external_bin` is dx's equivalent of Tauri's `externalBin`: it uses the same
# target-triple-suffix convention, so `binaries/otto` here resolves to the
# `binaries/otto-<triple>` that scripts/stage-sidecar.sh produces. Run that script before
# `dx bundle`, or the bundle ships without a sidecar and a fresh install cannot start a server.
#
# `[application] name` stays `otto-ui-dioxus` — scripts/measure-web-bundle.sh hardcodes it in
# the asset path, and that script is the only guard against re-shipping unoptimized wasm.
# Renaming to `otto-desktop` belongs to Phase 4, with the one-line script update.
[bundle]
identifier = "dev.otto.desktop"
publisher = "otto"
icon = [
    "icons/32x32.png",
    "icons/128x128.png",
    "icons/128x128@2x.png",
    "icons/icon.icns",
    "icons/icon.ico",
]
external_bin = ["binaries/otto"]
```

- [ ] **Step 6: Build a `.deb`**

```bash
cd /home/robhicks/dev/otto-next/ui-dioxus
dx bundle --release --platform desktop --features desktop --package-types deb
```

Expected: completes and reports an output path. If it fails on a missing system dependency (webkit2gtk and friends), install what it names and re-run — that is a host-setup issue, not a design problem.

Find the artifact:

```bash
find target/dx -name '*.deb' -newermt '-10 minutes'
```

- [ ] **Step 7: MEASURE the sidecar's location inside the package**

```bash
deb=$(find target/dx -name '*.deb' | head -1)
echo "inspecting: $deb"
dpkg-deb -c "$deb" | grep -Ei 'otto|bin/' | sed -n '1,40p'
```

Record, in the task's commit message and in a comment you will write in Task 4:

1. the **absolute installed path of the app executable**, and
2. the **absolute installed path of the staged `otto` binary**, and
3. **whether the triple suffix survives** (i.e. is it installed as `otto` or as `otto-x86_64-unknown-linux-gnu`?).

Task 4 writes its resolution logic against these three observed facts. If the sidecar is **not** a sibling of the executable, say so plainly in the commit message — Task 4's code and the spec's §2 both change to match, and that is a normal outcome of measuring, not a failure.

- [ ] **Step 8: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add ui-dioxus/icons ui-dioxus/.gitignore ui-dioxus/scripts/stage-sidecar.sh ui-dioxus/Dioxus.toml
git commit -m "ui-dioxus: add dx bundle config and sidecar staging

Ports the Tauri wrapper's packaging to Dioxus 0.7's [bundle] block: same
identifier (so it upgrades rather than duplicates), icons copied out of
desktop/src-tauri before Phase 4 deletes them, and external_bin pointing at
the otto binary staged by the new scripts/stage-sidecar.sh.

Measured .deb layout:
  app executable: <path from Step 7>
  staged sidecar: <path from Step 7>
  triple suffix stripped at bundle time: <yes/no from Step 7>"
```

---

### Task 4: Resolve the sidecar binary inside an installed bundle

**Files:**
- Modify: `ui-dioxus/src/desktop_boot.rs:80` (the `let bin = ...` line) and its `#[cfg(test)] mod tests` block at `:266`

**Interfaces:**
- Consumes: the measured layout from Task 3, Step 7.
- Produces: `fn resolve_otto_bin_in(env_override: Option<String>, exe_dir: Option<&Path>) -> String` (pure, unit-tested) and `fn resolve_otto_bin() -> String` (the impure wrapper `boot()` calls).

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `ui-dioxus/src/desktop_boot.rs`:

```rust
#[test]
fn resolve_otto_bin_prefers_the_env_override() {
    let dir = tempfile::tempdir().unwrap();
    let sibling = dir.path().join("otto");
    std::fs::write(&sibling, b"#!/bin/sh\n").unwrap();

    let got = resolve_otto_bin_in(Some("/custom/otto".to_string()), Some(dir.path()));

    assert_eq!(
        got, "/custom/otto",
        "an explicit OTTO_BIN must win over a bundled sibling"
    );
}

#[test]
fn resolve_otto_bin_finds_the_bundled_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let sibling = dir.path().join("otto");
    std::fs::write(&sibling, b"#!/bin/sh\n").unwrap();

    let got = resolve_otto_bin_in(None, Some(dir.path()));

    assert_eq!(
        got,
        sibling.to_string_lossy(),
        "an installed bundle stages `otto` beside the app executable"
    );
}

/// The pre-bundle dev behavior, which must survive: no override, nothing staged beside the
/// executable, so fall through to whatever `otto` is on PATH.
#[test]
fn resolve_otto_bin_falls_back_to_path() {
    let dir = tempfile::tempdir().unwrap();

    let got = resolve_otto_bin_in(None, Some(dir.path()));

    assert_eq!(got, "otto");
}

/// An empty OTTO_BIN is a misconfiguration, not an instruction to spawn "".
#[test]
fn resolve_otto_bin_ignores_an_empty_override() {
    let dir = tempfile::tempdir().unwrap();

    let got = resolve_otto_bin_in(Some(String::new()), Some(dir.path()));

    assert_eq!(got, "otto");
}
```

`tempfile` is needed as a dev-dependency of `ui-dioxus`. Check `ui-dioxus/Cargo.toml`'s `[dev-dependencies]` — if `tempfile` is absent, add `tempfile = "3"` to the plain `[dev-dependencies]` table (the one **above** the `[target.'cfg(target_arch = "wasm32")'.dev-dependencies]` header — everything after that header belongs to the wasm table, as its comment warns).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui-dioxus && cargo test --features desktop resolve_otto_bin`
Expected: FAIL to compile — `cannot find function resolve_otto_bin_in`.

- [ ] **Step 3: Implement the resolver**

In `ui-dioxus/src/desktop_boot.rs`, add above `boot()`:

```rust
/// Resolve the `otto` binary the sidecar is spawned from.
///
/// Order, and why each step exists:
///
/// 1. **`OTTO_BIN`** — an explicit operator override always wins (dev runs against a
///    freshly-built binary, unusual installs). An empty value is treated as unset: it is a
///    misconfiguration, not a request to spawn `""`.
/// 2. **A sibling of the running executable** — this is where `dx bundle`'s `[bundle]
///    external_bin` places the staged `otto` inside an installed package. VERIFIED by inspecting
///    a built `.deb` (see the Phase 2 plan, Task 3 Step 7); not inferred from documentation,
///    which only describes macOS `.app` placement.
/// 3. **Bare `otto` on `PATH`** — preserves the pre-bundle dev behavior this file shipped with.
///
/// Split into a pure inner function so the order is unit-testable without an installed bundle.
fn resolve_otto_bin_in(env_override: Option<String>, exe_dir: Option<&Path>) -> String {
    if let Some(bin) = env_override.filter(|b| !b.is_empty()) {
        return bin;
    }
    if let Some(dir) = exe_dir {
        let candidate = dir.join("otto");
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "otto".to_string()
}

/// `resolve_otto_bin_in` wired to the real environment. The only caller is `boot()`.
fn resolve_otto_bin() -> String {
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(Path::parent);
    resolve_otto_bin_in(std::env::var("OTTO_BIN").ok(), exe_dir)
}
```

**If Task 3 Step 7 measured a different layout** — the sidecar not beside the executable, or the triple suffix surviving — change `dir.join("otto")` to match what was observed, update the doc comment to describe the real layout, and note the correction in the commit message. The measurement is the authority here, not this plan.

- [ ] **Step 4: Use it in `boot()`**

In `boot()`, replace line `:80`:

```rust
    let bin = std::env::var("OTTO_BIN").unwrap_or_else(|_| "otto".into());
```

with:

```rust
    let bin = resolve_otto_bin();
```

Update the comment block just above it (`:69-70`) — the sentence "`otto` must be on PATH (or point OTTO_BIN at it); mirrors desktop/'s sidecar contract" is now out of date. Replace that sentence with:

```rust
    // The sidecar binary is resolved by `resolve_otto_bin`: OTTO_BIN, else a binary staged
    // beside this executable by `dx bundle`, else `otto` on PATH. See that function for why.
```

Leave the rest of the comment (the `kill_on_drop` / `PR_SET_PDEATHSIG` explanation) exactly as it is.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd ui-dioxus && cargo test --features desktop resolve_otto_bin`
Expected: PASS — 4 tests.

- [ ] **Step 6: Run the whole desktop test suite**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS, including the existing `serve_command_*` and `pdeathsig_tests`.

- [ ] **Step 7: Verify the installed package actually starts**

```bash
cd /home/robhicks/dev/otto-next/ui-dioxus
./scripts/stage-sidecar.sh
dx bundle --release --platform desktop --features desktop --package-types deb
sudo dpkg -i "$(find target/dx -name '*.deb' | head -1)"
```

Then launch the installed app **with `otto` removed from `PATH`**, which is the whole point — a fresh install must not depend on a developer's environment:

```bash
env -u OTTO_BIN PATH=/usr/bin:/bin otto-ui-dioxus
```

(Substitute the actual installed executable name from Task 3 Step 7.)

Expected: the folder picker appears; after choosing a folder the app connects and the status strip shows a live engine. If it reports a spawn failure, the resolution path is wrong — go back to Step 3 with the real layout.

- [ ] **Step 8: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add ui-dioxus/src/desktop_boot.rs ui-dioxus/Cargo.toml
git commit -m "ui-dioxus: resolve the otto sidecar from an installed bundle

A dx-bundled app has no otto on PATH, so boot() now resolves OTTO_BIN, then a
binary staged beside the running executable, then PATH. The bundled path is
written against the layout measured from a built .deb, not from docs.

Verified by installing the .deb and launching with otto absent from PATH."
```

---

### Task 5: Build scripts

**Files:**
- Create: `ui-dioxus/scripts/build-web.sh`
- Modify: `ui-dioxus/scripts/measure-web-bundle.sh`
- Create: `ui-dioxus/scripts/build-desktop.sh`

**Interfaces:**
- Consumes: `stage-sidecar.sh` (Task 3).
- Produces: `build-web.sh`, which Task 6's Dockerfile invokes. It prints the bundle directory as its final stdout line and exits non-zero if the bundle cannot be trusted.

- [ ] **Step 1: Create `build-web.sh` by moving the build and all four guards out of `measure-web-bundle.sh`**

Create `ui-dioxus/scripts/build-web.sh` with the content below. Everything from `MAX_WASM_BYTES` through guard 4 is **moved verbatim** from `measure-web-bundle.sh` — do not paraphrase or "improve" those guards; they encode findings that cost a real debugging cycle.

```bash
#!/usr/bin/env bash
#
# Build the Dioxus web release bundle, refusing to produce one that cannot be trusted.
#
#   cd ui-dioxus && ./scripts/build-web.sh
#
# Prints the bundle directory as its final line — feed it to `otto serve --ui-dir`:
#
#   otto serve --ui-dir "$(cd ui-dioxus && ./scripts/build-web.sh | tail -1)"
#
# This owns the four guards that used to live in measure-web-bundle.sh, which is now a thin
# wrapper over this script. They live in exactly one place on purpose: `dx` reports success
# (exit 0) even when its `wasm-opt` step crashes, in which case the bundle ships UNOPTIMIZED
# wasm — 2.16 MB instead of 795 KB. That is precisely what happened before the
# [profile.wasm-release] fix in Cargo.toml (see the comment there). Two copies of these guards
# would drift, and their not drifting is the only thing standing between this project and
# silently re-shipping that bundle.
#
#   1. wipes target/dx first, because dx never prunes stale hashed assets from a previous build
#      and leaving them behind makes "which wasm is the current one?" ambiguous;
#   2. fails if dx logged a wasm-opt failure;
#   3. fails if the emitted wasm still carries DWARF (`.debug_*` sections) — the signature of the
#      Cargo.toml `strip` having been dropped;
#   4. fails if the wasm exceeds MAX_WASM_BYTES. Guards 2 and 3 both fail open: 2 is a string
#      match on dx's log wording, and 3 cannot fire at all once `strip` removes the DWARF up
#      front, so a wasm-opt that is skipped or silently dropped by a future dx would produce a
#      clean-but-unoptimized wasm that sails past both. Guard 4 is value-based and cannot.
#
set -euo pipefail

# Ceiling, not a target. Optimized wasm measured 795,188 B on 2026-07-24; unoptimized was
# 2,164,985 B. Anything above this is far likelier to be a broken wasm-opt than real growth.
# Raise it deliberately (with a new measurement) when the app legitimately grows.
MAX_WASM_BYTES=${MAX_WASM_BYTES:-1200000}

cd "$(dirname "$0")/.."

command -v dx >/dev/null 2>&1 || {
    echo "error: 'dx' (Dioxus CLI) not found on PATH — install with: cargo install dioxus-cli" >&2
    exit 1
}

# Everything below assumes the build lands in ./target; CARGO_TARGET_DIR would relocate it and
# make `rm -rf target/dx` a no-op, so the run would fail later with a confusing "asset dir not
# found" instead of naming the cause.
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    echo "error: CARGO_TARGET_DIR is set ($CARGO_TARGET_DIR); this script expects ./target." >&2
    echo "       Unset it and re-run." >&2
    exit 1
fi

# GNU `mktemp -t` requires the trailing X's; BSD/macOS treats the whole string as a filename
# prefix and appends its own suffix. This form works on both.
log=$(mktemp -t build-web.XXXXXX)
trap 'rm -f "$log"' EXIT

# 1. Stale assets from earlier builds would make the artifact set ambiguous.
rm -rf target/dx

echo "==> dx build --release --platform web --features web" >&2
# `--platform web` is required in addition to `--features web`: the feature only enables the
# crate's own cargo feature, it does not tell dx which platform to build for.
dx build --release --platform web --features web 2>&1 | tee "$log" >&2

# 2. dx exits 0 even when wasm-opt aborts, so its log is the only signal.
if grep -qi "wasm-opt failed" "$log"; then
    echo >&2
    echo "error: wasm-opt failed — the bundle is UNOPTIMIZED and must not be shipped." >&2
    echo "       See the [profile.wasm-release] comment in Cargo.toml." >&2
    exit 1
fi

public="target/dx/otto-ui-dioxus/release/web/public"
assets="$public/assets"
[ -d "$assets" ] || { echo "error: expected asset dir not found: $assets" >&2; exit 1; }

# Deliberately not `mapfile` — that is a bash 4 builtin and macOS still ships bash 3.2.
# `$(( ))` normalizes BSD `wc`'s leading whitespace.
wasm=$(find "$assets" -maxdepth 1 -name '*.wasm' -type f)
wasm_count=$(( $(printf '%s' "$wasm" | grep -c . || true) ))
if [ "$wasm_count" -ne 1 ]; then
    echo "error: expected exactly one .wasm in $assets, found $wasm_count:" >&2
    printf '  %s\n' $wasm >&2
    exit 1
fi

# 3. DWARF in the shipped wasm means the Cargo.toml `strip` is gone — wasm-opt will have aborted.
if grep -aq '\.debug_info' "$wasm"; then
    echo >&2
    echo "error: $wasm still contains DWARF (.debug_info) — the [profile.wasm-release] strip" >&2
    echo "       setting in Cargo.toml is missing or ineffective." >&2
    exit 1
fi

# 4. The one guard that cannot fail open. `$(( ))` strips BSD `wc`'s leading whitespace.
wasm_bytes=$(( $(wc -c <"$wasm") ))
if [ "$wasm_bytes" -gt "$MAX_WASM_BYTES" ]; then
    echo >&2
    echo "error: wasm is $wasm_bytes B, over the $MAX_WASM_BYTES B ceiling — wasm-opt most likely" >&2
    echo "       did not run. If this is genuine growth, raise MAX_WASM_BYTES in this script." >&2
    exit 1
fi

# Progress goes to stderr (above) so that stdout carries exactly one thing: the bundle dir.
echo "$public"
```

Note the deliberate `>&2` redirections: every progress line goes to stderr so stdout is exactly the bundle path, which makes `$(build-web.sh | tail -1)` and the Dockerfile's use of it reliable.

```bash
chmod +x ui-dioxus/scripts/build-web.sh
```

- [ ] **Step 2: Verify `build-web.sh` builds and prints a usable path**

```bash
cd /home/robhicks/dev/otto-next/ui-dioxus
out=$(./scripts/build-web.sh)
echo "bundle dir: $out"
ls "$out/index.html" && echo "BUILD-WEB OK"
```

Expected: `BUILD-WEB OK`, and `$out` is `target/dx/otto-ui-dioxus/release/web/public`.

- [ ] **Step 3: Rewrite `measure-web-bundle.sh` as a wrapper**

Replace the whole of `ui-dioxus/scripts/measure-web-bundle.sh` with:

```bash
#!/usr/bin/env bash
#
# Report the size of the Dioxus web release bundle.
#
#   cd ui-dioxus && ./scripts/measure-web-bundle.sh
#
# This is the sanctioned way to produce a bundle-size figure for this crate. The build itself —
# and the four guards that make a reported figure trustworthy — live in build-web.sh, which this
# script calls. They are not duplicated here on purpose: two copies would drift, and a drifted
# guard means silently quoting a size taken from an unoptimized bundle.
#
# So the contract is: if build-web.sh exits non-zero, no number is printed at all.
#
set -euo pipefail

cd "$(dirname "$0")/.."

# stdout of build-web.sh is exactly the bundle dir; its progress output already went to stderr.
public=$(./scripts/build-web.sh)
assets="$public/assets"

# Every emitted asset, not just wasm/js/css — a future item (e.g. web syntax highlighting via JS
# interop) can add an `assets/snippets/` tree, and a TOTAL that quietly skipped it would be an
# understatement waiting to be quoted.
echo
echo "==> bundle: $assets"
printf '%-46s %12s %12s\n' "FILE" "RAW" "GZIP(-9)"
total_raw=0
total_gz=0
while IFS= read -r f; do
    raw=$(( $(wc -c <"$f") ))
    gz=$(( $(gzip -9 -c "$f" | wc -c) ))
    total_raw=$((total_raw + raw))
    total_gz=$((total_gz + gz))
    printf '%-46s %12s %12s\n' "$(basename "$f")" "$raw" "$gz"
done <<EOF
$(find "$assets" -type f | sort)
EOF
printf '%-46s %12s %12s\n' "TOTAL" "$total_raw" "$total_gz"
echo
echo "(dir: $assets — excludes the generated index.html, which lives one level up in public/)"
```

- [ ] **Step 4: Verify the wrapper still reports the same figures**

```bash
cd /home/robhicks/dev/otto-next/ui-dioxus && ./scripts/measure-web-bundle.sh
```

Expected: the same size table as before the refactor, with a wasm figure near 795 KB (well under the 1,200,000 B ceiling). If the wasm is ~2.16 MB, guard 4 fired and the build is broken — stop and fix before continuing.

- [ ] **Step 5: Create `build-desktop.sh`**

Create `ui-dioxus/scripts/build-desktop.sh`:

```bash
#!/usr/bin/env bash
#
# Build the installable desktop packages (.deb + .rpm) with the otto sidecar staged inside.
#
#   cd ui-dioxus && ./scripts/build-desktop.sh
#
# Staging must happen before `dx bundle`: `[bundle] external_bin` in Dioxus.toml reads
# binaries/otto-<triple> at bundle time, and dx does not build it. Skipping the staging step
# produces a package that installs cleanly and then cannot start a server — which is why the two
# steps live in one script rather than being left to a reader's memory.
#
set -euo pipefail

cd "$(dirname "$0")/.."

command -v dx >/dev/null 2>&1 || {
    echo "error: 'dx' (Dioxus CLI) not found on PATH — install with: cargo install dioxus-cli" >&2
    exit 1
}

./scripts/stage-sidecar.sh

echo "==> dx bundle --release --platform desktop --features desktop"
dx bundle --release --platform desktop --features desktop \
    --package-types deb --package-types rpm

echo
echo "==> packages:"
find target/dx -name '*.deb' -o -name '*.rpm'
```

```bash
chmod +x ui-dioxus/scripts/build-desktop.sh
```

- [ ] **Step 6: Verify it produces both packages**

```bash
cd /home/robhicks/dev/otto-next/ui-dioxus && ./scripts/build-desktop.sh
```

Expected: a `.deb` and a `.rpm` listed. If `.rpm` generation fails for a missing host tool, record that in the commit message and drop `--package-types rpm` to a documented follow-up rather than silently shipping a script that always fails.

- [ ] **Step 7: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add ui-dioxus/scripts/
git commit -m "ui-dioxus: add build-web and build-desktop scripts

build-web.sh takes over the release build and the four bundle-trust guards;
measure-web-bundle.sh becomes a thin wrapper over it so the guards exist in
one place and cannot drift. build-desktop.sh pairs sidecar staging with
dx bundle, since a bundle built without staging installs but cannot serve."
```

---

### Task 6: Fly image builds and serves its own UI

**Files:**
- Modify: `deploy/fly/Dockerfile`
- Modify: `deploy/fly/README.md`

**Interfaces:**
- Consumes: `ui-dioxus/scripts/build-web.sh` (Task 5); `otto serve --ui-dir` (Task 2).

- [ ] **Step 1: Add a web-bundle build stage**

In `deploy/fly/Dockerfile`, after the existing `RUN cargo build --release -p otto-engine ...` line in the `build` stage, add a **separate stage** so a UI toolchain problem can never break the engine build:

```dockerfile
# The web UI bundle. Its own stage: the Dioxus CLI is a heavy, separately-versioned toolchain,
# and keeping it out of the engine stage means a dx problem cannot break the engine build (and
# the engine layer stays cached when only the UI changes).
#
# dx is pinned to the version this migration was verified against. build-web.sh runs the same
# four bundle-trust guards it runs locally, so this image cannot ship the unoptimized 2.16 MB
# wasm even if a future dx regresses wasm-opt.
FROM rust:1.85-bookworm AS webbuild
WORKDIR /src
COPY . .
RUN rustup target add wasm32-unknown-unknown \
    && cargo install dioxus-cli --version 0.7.9 --locked
RUN cd ui-dioxus && ./scripts/build-web.sh
```

- [ ] **Step 2: Verify the toolchain assumption, and apply the fallback if it fails**

Build just that stage:

```bash
cd /home/robhicks/dev/otto-next
docker build -f deploy/fly/Dockerfile --target webbuild -t otto-webbuild-probe .
```

Expected: succeeds.

**If `cargo install dioxus-cli --version 0.7.9 --locked` fails because dx requires a newer rustc than the repo's pinned 1.85**, that is a real possibility and not a plan error. Apply this fallback: change the stage's base image and bypass the repo's toolchain file for the `dx` install only:

```dockerfile
FROM rust:bookworm AS webbuild
WORKDIR /src
COPY . .
# dioxus-cli needs a newer rustc than the repo's pinned 1.85; RUSTUP_TOOLCHAIN overrides
# rust-toolchain.toml for the CLI install only. The bundle itself is still built by the pinned
# toolchain, because build-web.sh runs cargo from inside ui-dioxus where the file applies.
RUN rustup target add wasm32-unknown-unknown \
    && RUSTUP_TOOLCHAIN=stable cargo install dioxus-cli --version 0.7.9 --locked
RUN cd ui-dioxus && ./scripts/build-web.sh
```

Re-run the probe build. Record in the commit message which of the two forms was needed.

- [ ] **Step 3: Copy the bundle into the runtime image and pass the flag**

In the runtime stage of `deploy/fly/Dockerfile`, after the existing `COPY --from=build ... mcp-git` line, add:

```dockerfile
# The web UI bundle, served by `otto serve --ui-dir` (see CMD). Built in the `webbuild` stage.
# SECURITY: this directory must contain only build output. The static route is unauthenticated
# by design (a browser fetches index.html and the wasm before it has a token), and ServeDir does
# not consult otto's sensitive-path floor — so OTTO_UI_DIR must never be pointed at /workspace.
COPY --from=webbuild /src/ui-dioxus/target/dx/otto-ui-dioxus/release/web/public /usr/local/share/otto/ui
```

Change the `ENV` line from:

```dockerfile
ENV OTTO_PORT=8787 OTTO_ROOT=/workspace OTTO_HOST=0.0.0.0
```

to:

```dockerfile
ENV OTTO_PORT=8787 OTTO_ROOT=/workspace OTTO_HOST=0.0.0.0 OTTO_UI_DIR=/usr/local/share/otto/ui
```

And change the `CMD` from:

```dockerfile
CMD ["sh", "-c", "otto serve --accept-promotions --port \"$OTTO_PORT\" --root \"$OTTO_ROOT\""]
```

to:

```dockerfile
CMD ["sh", "-c", "otto serve --accept-promotions --port \"$OTTO_PORT\" --root \"$OTTO_ROOT\" --ui-dir \"$OTTO_UI_DIR\""]
```

`otto serve` reads no `OTTO_UI_DIR` itself — the `ENV`-to-flag pass-through here mirrors exactly how `OTTO_PORT` and `OTTO_ROOT` already work in this `CMD`.

- [ ] **Step 4: Build the whole image and verify it serves the UI**

```bash
cd /home/robhicks/dev/otto-next
docker build -f deploy/fly/Dockerfile -t otto-fly-probe .
docker run --rm -d -p 8787:8787 -e OTTO_TOKEN=probe --name otto-probe otto-fly-probe
sleep 5
curl -s http://127.0.0.1:8787/ | head -c 200
echo
curl -s -o /dev/null -w 'index status: %{http_code}\n' http://127.0.0.1:8787/
docker rm -f otto-probe
```

Expected: HTML from the Dioxus bundle and `index status: 200`.

- [ ] **Step 5: Confirm the UI directory does not expose the workspace**

```bash
docker run --rm -d -p 8787:8787 -e OTTO_TOKEN=probe --name otto-probe otto-fly-probe
sleep 5
docker exec otto-probe sh -c 'echo SECRET > /workspace/.env'
curl -s -o /dev/null -w 'workspace .env status: %{http_code}\n' http://127.0.0.1:8787/.env
docker rm -f otto-probe
```

Expected: a status that is **not** 200 with the secret body — the static route serves `/usr/local/share/otto/ui`, which has no `.env`. (A 200 here returning the index-fallback HTML is correct and fine; a 200 returning `SECRET` is a critical failure — stop and fix.)

- [ ] **Step 6: Update the Fly README**

In `deploy/fly/README.md`, add a short section:

```markdown
## Browser UI

The image bundles the Dioxus web UI and serves it from the same port as the API
(`otto serve --ui-dir`, wired through `OTTO_UI_DIR` in the Dockerfile). Once a machine is
running, open its app URL in a browser — `https://<app>.fly.dev/` — and connect using the
per-session token the provisioner injected as `OTTO_TOKEN`.

The static route is intentionally unauthenticated: a browser has to load `index.html` and the
wasm before it has a token to present. Only build output is served this way. Every path that
touches session data or workspace contents (`/ws`, `/workspace`, `/promote`, `/export`) still
requires the bearer token, so an unauthenticated visitor can load the UI and do nothing with it.
```

- [ ] **Step 7: Commit**

```bash
git add deploy/fly/Dockerfile deploy/fly/README.md
git commit -m "deploy/fly: build and serve the web UI from the image

Adds a webbuild stage that installs the pinned dx and runs build-web.sh (so
the image inherits the bundle-trust guards), copies the bundle to
/usr/local/share/otto/ui, and passes it to otto serve --ui-dir via an ENV the
CMD forwards as a flag — the same pattern OTTO_PORT and OTTO_ROOT already use.

A promoted session is now reachable in a browser at the app URL."
```

- [ ] **Step 8: Deploy and verify against a real promoted session**

This is the one item gated on external infrastructure, and it is the reason it lands last.

```bash
cd /home/robhicks/dev/otto-next/deploy/fly
fly deploy
```

Then run a `--promote-fly` handover as recorded in the existing Fly round-trip notes, and open the resulting app URL in a browser. Confirm the UI loads, connects with the session token, and streams events.

Record the result. If the deploy or round-trip cannot be run in this environment, **say so explicitly** rather than marking this step done — the rest of Task 6 is verified locally by Steps 4–5, and this step is what makes the Fly claim real.

---

### Task 7: Update the migration plan

**Files:**
- Modify: `docs/superpowers/plans/2026-07-22-ui-dioxus-migration.md:146-158` (the Phase 2 section)

- [ ] **Step 1: Rewrite the Phase 2 section**

Replace the three unchecked bullets under `## Phase 2 — Build & serve story (NEEDS ITS OWN DESIGN PASS)` with a completed section. Change the heading to:

```markdown
## Phase 2 — Build & serve story

**Status: COMPLETE.** Design pass done: `docs/superpowers/specs/2026-07-27-ui-dioxus-build-serve-design.md`.
Implemented per `docs/superpowers/plans/2026-07-27-ui-dioxus-build-serve.md`.

Three premises in this section's original bullets were wrong, and are corrected here so they are
not re-derived later:

- [x] **`otto serve` now serves the web bundle** via an additive `--ui-dir` static fallback.
      The original bullet said "today it serves `ui/dist`" — it did not serve static files at
      all; its routes were `/ws`, `/workspace`, `/promote`, `/export`, and `tower-http` carried
      only the `cors` feature. The engine change is one post-construction layer
      (`serve::with_ui_dir`), so no existing constructor or call site changed. No default and no
      env fallback: `ServeDir` does not consult the sensitive-path floor.
- [x] **Desktop packaging moved to `dx bundle`** with `[bundle] external_bin` staging the `otto`
      sidecar, reaching parity with the Tauri `.deb`/`.rpm`. The real gap was never build-tool
      parity — it was that a `dx`-built app had no sidecar and required `otto` on `PATH`.
      `desktop_boot` now resolves OTTO_BIN → bundled sibling → PATH, written against a layout
      measured from a built `.deb`. Icons were copied to `ui-dioxus/icons/` so Phase 4's deletion
      of `desktop/` cannot break the bundle.
- [x] **Build scripts, not CI.** The original bullet assumed `ui/` wasm-build and `desktop/`
      Tauri-build CI jobs existed to replace; the repo has no CI at all. Phase 2 ships
      `build-web.sh` (build + the four bundle-trust guards), `stage-sidecar.sh`, and
      `build-desktop.sh`; `measure-web-bundle.sh` became a thin wrapper so the guards exist in
      one place. Standing up CI is its own project.
- [x] **The Fly image serves its own UI** — added beyond the original three bullets. A `webbuild`
      stage builds the bundle in-image and `CMD` passes `--ui-dir`, so a promoted session is
      browser-reachable at its app URL.
```

- [ ] **Step 2: Note what Phase 3 and Phase 4 inherit**

Under `## Phase 3 — Parity sign-off`, add one line at the top of the bullet list:

```markdown
- Phase 2 means this can now run against real artifacts: `otto serve --ui-dir` for web and an
  installed `.deb` for desktop, rather than dev servers.
```

Under `## Phase 4 — Retire the incumbent`, add:

```markdown
- Rename `[application] name` from `otto-ui-dioxus` to `otto-desktop` in `ui-dioxus/Dioxus.toml`,
  and update the hardcoded asset path in `ui-dioxus/scripts/build-web.sh` to match. Deferred from
  Phase 2 to avoid touching the wasm-opt guard script mid-migration.
- `desktop/src-tauri/icons/` can be deleted — Phase 2 already copied them to `ui-dioxus/icons/`.
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/plans/2026-07-22-ui-dioxus-migration.md
git commit -m "docs: mark Phase 2 complete and correct its premises"
```

---

## Final Verification

- [ ] `cargo test --workspace` passes (except the known pre-existing `mcp-lsp` rust-analyzer failure).
- [ ] `cargo clippy --workspace --all-targets` is clean for the changed crates.
- [ ] `cd ui-dioxus && cargo test --features desktop` passes.
- [ ] `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web` compiles.
- [ ] `git status crates/` shows only the four intended engine files were ever touched.
- [ ] `otto serve` with no `--ui-dir` still returns 404 at `/` — the feature is inert by default.
- [ ] `ui/` and `desktop/` are unmodified: `git diff --stat main -- ui/ desktop/` is empty.
