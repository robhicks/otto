# UI Sub-project C — Workspace Tree + Editor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a file tree and a code-editor pane to the otto browser UI: browse the served workspace (`List`), open a file (`Read`) into a `kode-leptos` editor, edit it in a local buffer (no persistence — that's Sub-project D).

**Architecture:** One additive engine change — a `tower-http` CORS layer on `serve::app` so the browser can make its preflighted cross-origin `POST /workspace` calls. Everything else is in the standalone `ui/` crate: a `gloo-net` fetch client for the existing `/workspace` RPC, pure host-tested helpers (url→http base, flat-paths→tree, extension→language, bytes→text/binary), two new Leptos components (file tree, editor pane), and app wiring. The pure logic is unit-tested on the host; the browser glue (fetch client, components) is verified by the wasm build + manual smoke test, matching the existing UI convention (see `ui/src/ws.rs`).

**Tech Stack:** Rust 2024 (engine) / Rust 2021 (ui, wasm32), axum 0.8, tower-http (cors), Leptos 0.8 CSR, `kode-leptos` 0.5.4, `gloo-net` 0.6, `trunk`.

**Spec:** [`docs/superpowers/specs/2026-06-18-ui-workspace-tree-editor-design.md`](../specs/2026-06-18-ui-workspace-tree-editor-design.md)

## File structure

**Engine (workspace crate `otto-engine`):**
- Modify: `crates/engine/Cargo.toml` — add `tower-http` dep + `tower` dev-dep.
- Modify: `crates/engine/src/serve.rs:71-87` (`app`) — add the CORS layer.
- Create: `crates/engine/tests/cors.rs` — preflight test via `tower::ServiceExt::oneshot`.

**UI (standalone `ui/` crate — NOT a workspace member):**
- Modify: `ui/Cargo.toml` — add `kode-leptos`, `gloo-net`.
- Modify: `ui/src/main.rs` — register `mod tree; mod workspace;`.
- Modify: `ui/src/url.rs` — add `ws_to_http_base`.
- Create: `ui/src/tree.rs` — `TreeNode`, `build_tree`, `language_for_path`, `FileBody`, `decode_or_binary` (all pure, host-tested).
- Create: `ui/src/workspace.rs` — `gloo-net` fetch client (`list_files`, `read_file`).
- Create: `ui/src/components/file_tree.rs` — `FileTree` component.
- Create: `ui/src/components/editor_pane.rs` — `EditorPane` component wrapping `kode-leptos`.
- Modify: `ui/src/components/mod.rs` — register + re-export the two components.
- Modify: `ui/src/app.rs` — signals, list-on-connect, open-file, two-pane layout, Refresh.
- Modify: `ui/style.css` — tree/editor styling.

**Docs:**
- Modify: `docs/superpowers/specs/2026-06-17-ui-roadmap.md` and `CLAUDE.md` — record C built.

---

## Task 1: Engine CORS layer

**Files:**
- Modify: `crates/engine/Cargo.toml`
- Modify: `crates/engine/src/serve.rs`
- Create: `crates/engine/tests/cors.rs`

- [ ] **Step 1: Add the deps**

In `crates/engine/Cargo.toml`, under `[dependencies]` add:

```toml
tower-http = { version = "0.6", features = ["cors"] }
```

Under `[dev-dependencies]` add:

```toml
tower = { version = "0.5", features = ["util"] }
```

- [ ] **Step 2: Write the failing preflight test**

Create `crates/engine/tests/cors.rs`:

```rust
//! The /workspace endpoint must advertise CORS so the browser UI (served from a different
//! origin by trunk) can make its preflighted cross-origin POST. Tested in-process via
//! tower's oneshot — no port binding, no network.

use std::sync::Arc;

use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, serve_app,
};
use otto_engine_core::traits::Workspace;
use otto_providers::LocalProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use tower::ServiceExt; // for `oneshot`

const TOKEN: &str = "test-token";

/// Build the serve router (unbound) over a temp workspace.
async fn build_app() -> (axum::Router, tempfile::TempDir) {
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
    );
    (app, dir)
}

#[tokio::test]
async fn workspace_preflight_advertises_cors() {
    let (app, _dir) = build_app().await;
    let req = axum::http::Request::builder()
        .method(axum::http::Method::OPTIONS)
        .uri("/workspace")
        .header(axum::http::header::ORIGIN, "http://127.0.0.1:8080")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "authorization,content-type")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(
        resp.status().is_success(),
        "preflight should be answered 2xx, got {}",
        resp.status()
    );
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*"),
    );
    let methods = resp
        .headers()
        .get("access-control-allow-methods")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(methods.contains("POST"), "allow-methods was {methods:?}");
}
```

- [ ] **Step 3: Run it — expect failure**

Run: `cargo test -p otto-engine --test cors`
Expected: FAIL — without the CORS layer, `OPTIONS /workspace` returns 405 and carries no `access-control-allow-origin` header.

- [ ] **Step 4: Add the CORS layer**

In `crates/engine/src/serve.rs`, extend the imports near the top (the file already has `use axum::http::{HeaderMap, StatusCode};`):

```rust
use axum::http::{HeaderMap, Method, StatusCode, header};
use tower_http::cors::{Any, CorsLayer};
```

Then in `pub fn app(...)` change the router builder so the layer is applied (loopback/dev posture — see comment):

```rust
    // CORS for the browser UI: it is served from a different origin (trunk on :8080) and its
    // POST /workspace carries an `Authorization` header, so the browser sends a preflight.
    // `allow_origin(Any)` matches the loopback/dev posture already accepted for the `?token=`
    // query param on /ws; auth rides the Authorization header (not cookies), so wildcard origin
    // without credentials mode is correct and exposes nothing extra.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);
    AxumRouter::new()
        .route("/ws", get(ws_handler))
        .route("/workspace", post(workspace_handler))
        .layer(cors)
        .with_state(state)
```

- [ ] **Step 5: Run the test — expect pass**

Run: `cargo test -p otto-engine --test cors`
Expected: PASS.

- [ ] **Step 6: Confirm nothing else broke**

Run: `cargo test -p otto-engine`
Expected: PASS (existing `serve`, `remote_workspace`, `promote` tests stay green).

- [ ] **Step 7: Commit**

```bash
git add crates/engine/Cargo.toml crates/engine/Cargo.lock crates/engine/src/serve.rs crates/engine/tests/cors.rs
git commit -m "feat(engine): CORS layer on /workspace for the browser UI"
```

---

## Task 2: Add UI deps + confirm the kode-leptos 0.5.4 API

This front-loads the riskiest integration (a young crate) before any component depends on it.

**Files:**
- Modify: `ui/Cargo.toml`

- [ ] **Step 1: Add the dependencies**

In `ui/Cargo.toml` under `[dependencies]` add:

```toml
kode-leptos = "=0.5.4"
gloo-net = { version = "0.6", features = ["http"] }
```

- [ ] **Step 2: Confirm the wasm build still resolves**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: PASS (the crates compile to wasm; no code uses them yet).

- [ ] **Step 3: Record the exact 0.5.4 editor API**

The published `kode-leptos` README tracks an older `0.1`/alpha line where `Language` is an enum (`Language::Sql`); in `0.5.4` `Language` is a **struct**. Discover the real surface so Task 8 isn't guessing:

Run: `cd ui && cargo doc -p kode-leptos --no-deps`
Then open `ui/target/doc/kode_leptos/index.html` (or inspect the source under `~/.cargo/registry/src/*/kode-leptos-0.5.4/src/`).

Write down, in the Task 8 working notes / commit body, the exact:
- `CodeEditor` prop names + types (expected: `language: Signal<Language>`, `content: Signal<String>`, `theme: Signal<Theme>`, `on_change: Option<Arc<dyn Fn(String) + Send + Sync>>`, `placeholder: Signal<String>`).
- How to construct a `Language` (constructor / `from_name` / per-language fns) and a `Theme` (e.g. a `Theme::*()` constructor / `Default`).

- [ ] **Step 4: Commit**

```bash
git add ui/Cargo.toml ui/Cargo.lock
git commit -m "build(ui): add kode-leptos and gloo-net deps"
```

---

## Task 3: `ws_to_http_base` (pure)

**Files:**
- Modify: `ui/src/url.rs`

- [ ] **Step 1: Write the failing test**

Append to the `mod tests` block in `ui/src/url.rs`:

```rust
    #[test]
    fn ws_to_http_base_maps_schemes_and_strips_ws_suffix() {
        assert_eq!(ws_to_http_base("ws://127.0.0.1:8787"), "http://127.0.0.1:8787");
        assert_eq!(ws_to_http_base("wss://host:9000"), "https://host:9000");
        assert_eq!(ws_to_http_base("ws://h/ws"), "http://h");
        assert_eq!(ws_to_http_base("ws://h/ws/"), "http://h");
        // A non-ws base is passed through untouched (only trailing slash/`/ws` trimmed).
        assert_eq!(ws_to_http_base("http://h:1/"), "http://h:1");
    }
```

- [ ] **Step 2: Run it — expect failure**

Run: `cd ui && cargo test ws_to_http_base`
Expected: FAIL — `ws_to_http_base` not found.

- [ ] **Step 3: Implement**

Add to `ui/src/url.rs` (above the `#[cfg(test)]` block):

```rust
/// Derive the HTTP origin for the `/workspace` RPC from the `ws`/`wss` connection URL.
/// `ws://…`→`http://…`, `wss://…`→`https://…`; a trailing slash and a `/ws` suffix are
/// trimmed (the UI form may hold the endpoint URL `otto serve` prints).
pub fn ws_to_http_base(ws_url: &str) -> String {
    let trimmed = ws_url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/ws").unwrap_or(trimmed);
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_string()
    }
}
```

- [ ] **Step 4: Run it — expect pass**

Run: `cd ui && cargo test ws_to_http_base`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ui/src/url.rs
git commit -m "feat(ui): ws_to_http_base url helper"
```

---

## Task 4: `tree.rs` — `TreeNode` + `build_tree` (pure)

**Files:**
- Create: `ui/src/tree.rs`
- Modify: `ui/src/main.rs`

- [ ] **Step 1: Register the module**

In `ui/src/main.rs`, add to the module list (keep alphabetical with the others):

```rust
mod tree;
mod workspace;
```

(`workspace` is created in Task 6; declaring it now means Task 6 needs no main.rs edit. If the build runs before Task 6, temporarily omit the `mod workspace;` line and add it in Task 6.)

- [ ] **Step 2: Write the failing test**

Create `ui/src/tree.rs`:

```rust
//! Pure, browser-free workspace-view helpers: build a nested tree from the flat path list the
//! `/workspace` `List` RPC returns, pick an editor language from a path, and decode file bytes
//! into editable text / a binary marker. Unit-tested on the host.

use std::path::{Path, PathBuf};

/// One node in the rendered file tree. Files have an empty `children`; directories may have
/// any number. `path` is the full workspace-relative path (used as the `Read` key for files
/// and as a stable list key for both).
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub children: Vec<TreeNode>,
}

/// Build a sorted nested tree from a flat list of file paths. Directories sort before files;
/// within a kind, lexicographically by segment.
pub fn build_tree(paths: &[PathBuf]) -> Vec<TreeNode> {
    let mut roots: Vec<TreeNode> = Vec::new();
    for path in paths {
        let comps: Vec<String> = path
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        insert_components(&mut roots, &comps, PathBuf::new());
    }
    sort_nodes(&mut roots);
    roots
}

fn insert_components(level: &mut Vec<TreeNode>, comps: &[String], prefix: PathBuf) {
    let Some((head, rest)) = comps.split_first() else {
        return;
    };
    let here = prefix.join(head);
    let is_dir = !rest.is_empty();
    let idx = match level.iter().position(|n| n.name == *head) {
        Some(i) => i,
        None => {
            level.push(TreeNode {
                name: head.clone(),
                path: here.clone(),
                is_dir,
                children: Vec::new(),
            });
            level.len() - 1
        }
    };
    if is_dir {
        level[idx].is_dir = true;
        insert_components(&mut level[idx].children, rest, here);
    }
}

fn sort_nodes(nodes: &mut [TreeNode]) {
    nodes.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    for n in nodes.iter_mut() {
        sort_nodes(&mut n.children);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn build_tree_nests_and_sorts_dirs_before_files() {
        let tree = build_tree(&[p("src/main.rs"), p("README.md"), p("src/app.rs")]);
        // Top level: `src` (dir) before `README.md` (file).
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "src");
        assert!(tree[0].is_dir);
        assert_eq!(tree[1].name, "README.md");
        assert!(!tree[1].is_dir);
        // src children sorted: app.rs before main.rs, both files with full paths.
        let kids: Vec<&str> = tree[0].children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(kids, vec!["app.rs", "main.rs"]);
        assert_eq!(tree[0].children[0].path, p("src/app.rs"));
    }

    #[test]
    fn build_tree_merges_shared_dirs() {
        let tree = build_tree(&[p("a/b/x.rs"), p("a/b/y.rs"), p("a/c.rs")]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "a");
        // `a` has dir `b` then file `c.rs`.
        assert_eq!(tree[0].children[0].name, "b");
        assert!(tree[0].children[0].is_dir);
        assert_eq!(tree[0].children[0].children.len(), 2);
        assert_eq!(tree[0].children[1].name, "c.rs");
    }
}
```

- [ ] **Step 3: Run it — expect pass**

Run: `cd ui && cargo test tree::`
Expected: PASS (the module ships with its implementation and tests together).

> TDD note: this module is created implementation-and-tests-together because the type and function are needed by the test to compile. Treat Step 2's test as the spec; if you prefer strict red-green, write the test first against an empty `build_tree` stub, watch it fail, then fill in.

- [ ] **Step 4: Commit**

```bash
git add ui/src/main.rs ui/src/tree.rs
git commit -m "feat(ui): TreeNode + build_tree (flat paths -> nested tree)"
```

---

## Task 5: `tree.rs` — `language_for_path` + `decode_or_binary` (pure)

**Files:**
- Modify: `ui/src/tree.rs`

- [ ] **Step 1: Write the failing test**

Add to `tree.rs`'s `mod tests`:

```rust
    #[test]
    fn language_for_path_maps_known_extensions() {
        assert_eq!(language_for_path(Path::new("src/main.rs")), "rust");
        assert_eq!(language_for_path(Path::new("Cargo.toml")), "toml");
        assert_eq!(language_for_path(Path::new("a/b.json")), "json");
        assert_eq!(language_for_path(Path::new("notes.md")), "markdown");
        assert_eq!(language_for_path(Path::new("LICENSE")), "text");
    }

    #[test]
    fn decode_or_binary_classifies_bytes() {
        assert_eq!(decode_or_binary(b"hello"), FileBody::Text("hello".into()));
        // Invalid UTF-8 → Binary.
        assert_eq!(decode_or_binary(&[0xff, 0xfe, 0x00]), FileBody::Binary);
        // Over the cap → TooLarge (regardless of validity).
        let big = vec![b'a'; MAX_EDITABLE_BYTES + 1];
        assert_eq!(decode_or_binary(&big), FileBody::TooLarge);
    }
```

- [ ] **Step 2: Run it — expect failure**

Run: `cd ui && cargo test tree::`
Expected: FAIL — `language_for_path`, `decode_or_binary`, `FileBody`, `MAX_EDITABLE_BYTES` undefined.

- [ ] **Step 3: Implement**

Add to `ui/src/tree.rs` (above the `#[cfg(test)]` block):

```rust
/// The largest file we mount in the editor; bigger files show a notice instead.
pub const MAX_EDITABLE_BYTES: usize = 512 * 1024;

/// A file's body as the UI treats it.
#[derive(Clone, Debug, PartialEq)]
pub enum FileBody {
    /// Valid UTF-8 text, ready to edit.
    Text(String),
    /// Not valid UTF-8 — not editable here.
    Binary,
    /// Over `MAX_EDITABLE_BYTES` — not mounted.
    TooLarge,
}

/// Editor language id for a path, by extension. Returns a stable lowercase id (mapped to a
/// `kode_leptos::Language` in the editor component); unknown extensions get `"text"`.
pub fn language_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("toml") => "toml",
        Some("json") => "json",
        Some("md") | Some("markdown") => "markdown",
        Some("js") | Some("mjs") | Some("cjs") => "javascript",
        Some("ts") => "typescript",
        Some("py") => "python",
        Some("html") | Some("htm") => "html",
        Some("css") => "css",
        Some("sh") | Some("bash") => "bash",
        Some("sql") => "sql",
        Some("yaml") | Some("yml") => "yaml",
        _ => "text",
    }
}

/// Classify raw file bytes for the editor: size cap first, then UTF-8 validity.
pub fn decode_or_binary(bytes: &[u8]) -> FileBody {
    if bytes.len() > MAX_EDITABLE_BYTES {
        return FileBody::TooLarge;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => FileBody::Text(s.to_string()),
        Err(_) => FileBody::Binary,
    }
}
```

- [ ] **Step 4: Run it — expect pass**

Run: `cd ui && cargo test tree::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ui/src/tree.rs
git commit -m "feat(ui): language_for_path + decode_or_binary helpers"
```

---

## Task 6: `workspace.rs` — fetch client

Browser-only (`gloo-net` uses `fetch`, available on wasm32). Verified by the wasm build, like `ws.rs`.

**Files:**
- Create: `ui/src/workspace.rs`
- (Module already declared in Task 4's `main.rs` edit.)

- [ ] **Step 1: Implement the client**

Create `ui/src/workspace.rs`:

```rust
//! Browser-only `fetch` client for the engine's bearer-authed `POST /workspace` RPC.
//! `gloo-net` targets wasm32 (it wraps `fetch`), so this module is verified by the wasm
//! build and manual testing — the pure routing/decoding logic lives in `url.rs`/`tree.rs`.

use std::path::PathBuf;

use gloo_net::http::Request;
use otto_protocol::{WorkspaceRequest, WorkspaceResponse};

/// Send one `WorkspaceRequest` to `{http_base}/workspace` with the bearer token.
/// Maps transport failures, non-2xx, and `WorkspaceResponse::Error` to `Err(String)`.
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

/// List every file in the served workspace.
pub async fn list_files(http_base: &str, token: &str) -> Result<Vec<PathBuf>, String> {
    match rpc(
        http_base,
        token,
        &WorkspaceRequest::List {
            glob: "**/*".to_string(),
        },
    )
    .await?
    {
        WorkspaceResponse::List { paths } => Ok(paths),
        other => Err(format!("unexpected response to List: {other:?}")),
    }
}

/// Read one file's bytes.
pub async fn read_file(http_base: &str, token: &str, path: PathBuf) -> Result<Vec<u8>, String> {
    match rpc(http_base, token, &WorkspaceRequest::Read { path }).await? {
        WorkspaceResponse::Read { bytes } => Ok(bytes),
        other => Err(format!("unexpected response to Read: {other:?}")),
    }
}
```

- [ ] **Step 2: Confirm the wasm build**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add ui/src/main.rs ui/src/workspace.rs
git commit -m "feat(ui): gloo-net fetch client for the /workspace RPC"
```

---

## Task 7: `FileTree` component

**Files:**
- Create: `ui/src/components/file_tree.rs`
- Modify: `ui/src/components/mod.rs`

- [ ] **Step 1: Implement the component**

Create `ui/src/components/file_tree.rs`:

```rust
use std::path::PathBuf;

use leptos::prelude::*;

use crate::tree::TreeNode;

/// The workspace file tree. `nodes` is the current tree; clicking a file invokes `on_open`
/// with its full path. Directories toggle expand/collapse locally.
#[component]
pub fn FileTree(nodes: Signal<Vec<TreeNode>>, on_open: Callback<PathBuf>) -> impl IntoView {
    view! {
        <div class="file-tree">
            {move || {
                let items = nodes.get();
                if items.is_empty() {
                    view! { <div class="tree-empty">"no files"</div> }.into_any()
                } else {
                    items
                        .into_iter()
                        .map(|n| view! { <TreeNodeView node=n on_open=on_open /> }.into_any())
                        .collect_view()
                        .into_any()
                }
            }}
        </div>
    }
}

#[component]
fn TreeNodeView(node: TreeNode, on_open: Callback<PathBuf>) -> impl IntoView {
    if node.is_dir {
        let expanded = RwSignal::new(false);
        let name = node.name.clone();
        let children = node.children.clone();
        view! {
            <div class="tree-dir">
                <div
                    class="tree-row tree-dir-row"
                    on:click=move |_| expanded.update(|e| *e = !*e)
                >
                    {move || if expanded.get() { "▾ " } else { "▸ " }}
                    {name.clone()}
                </div>
                <Show when=move || expanded.get() fallback=|| ()>
                    <div class="tree-children">
                        {children
                            .clone()
                            .into_iter()
                            .map(|c| view! { <TreeNodeView node=c on_open=on_open /> }.into_any())
                            .collect_view()}
                    </div>
                </Show>
            </div>
        }
        .into_any()
    } else {
        let path = node.path.clone();
        let name = node.name.clone();
        view! {
            <div
                class="tree-row tree-file-row"
                on:click=move |_| on_open.run(path.clone())
            >
                {"  "}
                {name}
            </div>
        }
        .into_any()
    }
}
```

- [ ] **Step 2: Register + re-export**

In `ui/src/components/mod.rs` add `mod file_tree;` (alphabetical) and `pub use file_tree::FileTree;`.

- [ ] **Step 3: Confirm the wasm build**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: PASS. (If the recursive component trips a move/borrow error, clone the captured value before the closure — the wasm compiler is the gate here, per the UI convention.)

- [ ] **Step 4: Commit**

```bash
git add ui/src/components/file_tree.rs ui/src/components/mod.rs
git commit -m "feat(ui): FileTree component"
```

---

## Task 8: `EditorPane` component (kode-leptos)

Uses the `kode-leptos` `0.5.4` API recorded in Task 2, Step 3.

**Files:**
- Create: `ui/src/components/editor_pane.rs`
- Modify: `ui/src/components/mod.rs`

- [ ] **Step 1: Implement the component**

Create `ui/src/components/editor_pane.rs`. The skeleton below uses the documented prop shape; **substitute the exact `Language`/`Theme` constructors recorded in Task 2** in the `kode_language` helper and the `theme=` prop. Since this slice has no save and no diff, `on_change` only flips the dirty flag — the live text is not read back (Sub-project D will need it).

```rust
use std::path::PathBuf;
use std::sync::Arc;

use kode_leptos::{CodeEditor, Language, Theme};
use leptos::prelude::*;

use crate::tree::{FileBody, language_for_path};

/// The editor pane. `open` is the currently-open file (path + classified body); `seed` is the
/// initial document text set once at open time (it drives the editor's `content` and is NOT
/// updated on keystroke, so typing never resets the doc); `dirty` is flipped true on the first
/// edit and shown as a "●" marker. No persistence in this slice.
#[component]
pub fn EditorPane(
    open: Signal<Option<(PathBuf, FileBody)>>,
    seed: Signal<String>,
    dirty: RwSignal<bool>,
) -> impl IntoView {
    view! {
        <div class="editor-pane">
            {move || match open.get() {
                None => view! { <div class="editor-empty">"No file open"</div> }.into_any(),
                Some((_, FileBody::Binary)) => {
                    view! { <div class="editor-notice">"binary file — not editable"</div> }
                        .into_any()
                }
                Some((_, FileBody::TooLarge)) => {
                    view! { <div class="editor-notice">"file too large to edit"</div> }.into_any()
                }
                Some((path, FileBody::Text(_))) => {
                    let lang = kode_language(language_for_path(&path));
                    let header = path.display().to_string();
                    view! {
                        <div class="editor-host">
                            <div class="editor-header">
                                {header}
                                {move || if dirty.get() { " ●" } else { "" }}
                            </div>
                            <CodeEditor
                                language=Signal::stored(lang)
                                content=seed
                                theme=Signal::stored(Theme::default())
                                on_change=Some(Arc::new(move |_text: String| dirty.set(true)))
                            />
                        </div>
                    }
                    .into_any()
                }
            }}
        </div>
    }
}

/// Map the stable language id from `language_for_path` to a `kode_leptos::Language`.
/// NOTE: fill in using the exact 0.5.4 constructor recorded in Task 2 (e.g. a `Language::from_*`
/// / per-language fn). The arm names below are the ids `language_for_path` produces.
fn kode_language(id: &str) -> Language {
    match id {
        // "rust" => Language::rust(),
        // "json" => Language::json(),
        // … fill remaining ids; fall through to a plain-text language for "text"/unknown.
        _ => Language::default(),
    }
}
```

> If `0.5.4`'s prop types differ from the skeleton (e.g. `content` wants an owned `Signal<String>` vs the `seed` signal, `theme` has no `Default`, or `on_change` is a bare `Arc` not `Option`), adapt to the recorded API — the prop *names* (`language`/`content`/`theme`/`on_change`) are stable; the constructors are what to confirm.

- [ ] **Step 2: Register + re-export**

In `ui/src/components/mod.rs` add `mod editor_pane;` (alphabetical) and `pub use editor_pane::EditorPane;`.

- [ ] **Step 3: Confirm the wasm build**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add ui/src/components/editor_pane.rs ui/src/components/mod.rs
git commit -m "feat(ui): EditorPane wrapping kode-leptos CodeEditor"
```

---

## Task 9: Wire tree + editor into `app.rs` (+ CSS)

**Files:**
- Modify: `ui/src/app.rs`
- Modify: `ui/style.css`

- [ ] **Step 1: Add imports**

In `ui/src/app.rs`, extend the `use crate::...` lines:

```rust
use crate::components::{ConnectionForm, EditorPane, EventLog, FileTree, PromptBar, StatusLine};
use crate::tree::{FileBody, TreeNode, build_tree, decode_or_binary};
use crate::url::{advance_last_seq, build_ws_url, should_apply, ws_to_http_base};
use crate::workspace::{list_files, read_file};
use std::path::PathBuf;
```

- [ ] **Step 2: Add the workspace signals**

Inside `App`, after the existing `capabilities` signal, add:

```rust
    // Workspace tree + editor state.
    let tree = RwSignal::new(Vec::<TreeNode>::new());
    let open_file = RwSignal::new(None::<(PathBuf, FileBody)>);
    let editor_seed = RwSignal::new(String::new()); // file text set once at open time
    let editor_dirty = RwSignal::new(false);
```

- [ ] **Step 3: Add the load-files + open-file actions**

Still inside `App`, before the `view!`, add (uses `leptos::task::spawn_local`):

```rust
    // Fetch the file list over the /workspace RPC and rebuild the tree. No-op without
    // url+token (the form gates Connect on both, so by Connected they are present).
    let load_files = move || {
        let http_base = ws_to_http_base(&url.get());
        let tok = token.get();
        if http_base.is_empty() || tok.is_empty() {
            return;
        }
        leptos::task::spawn_local(async move {
            match list_files(&http_base, &tok).await {
                Ok(paths) => tree.set(build_tree(&paths)),
                Err(e) => rows.update(|v| v.push(client_error_row(&e))),
            }
        });
    };

    // Read a file and mount it in the editor (or show a binary/oversize notice).
    let open_path = move |path: PathBuf| {
        let http_base = ws_to_http_base(&url.get());
        let tok = token.get();
        leptos::task::spawn_local(async move {
            match read_file(&http_base, &tok, path.clone()).await {
                Ok(bytes) => {
                    let body = decode_or_binary(&bytes);
                    if let FileBody::Text(ref s) = body {
                        editor_seed.set(s.clone());
                        editor_dirty.set(false);
                    }
                    open_file.set(Some((path, body)));
                }
                Err(e) => rows.update(|v| v.push(client_error_row(&e))),
            }
        });
    };

    // Auto-load the tree when the connection reaches Connected.
    Effect::new(move |_| {
        if matches!(conn.get(), ConnState::Connected { .. }) {
            load_files();
        }
    });
```

> Note: `client_error_row` and `ConnState` are already imported in `app.rs`. `Effect` and `spawn_local` come from `leptos::prelude::*` / `leptos::task` (Leptos 0.8). If `load_files`/`open_path` hit `FnMut` move issues when used in both a closure and a callback, wrap them with `Callback::new` once and `.run(...)` them (as the existing `send_prompt`/`abort` are wired).

- [ ] **Step 4: Add the workspace pane to the view**

In the `view!`, insert a workspace row between `StatusLine` and `EventLog`:

```rust
            <div class="workspace">
                <div class="workspace-side">
                    <button
                        class="refresh-btn"
                        on:click=move |_| load_files()
                        disabled=move || !matches!(conn.get(), ConnState::Connected { .. })
                    >
                        "Refresh files"
                    </button>
                    <FileTree
                        nodes=tree.into()
                        on_open=Callback::new(open_path)
                    />
                </div>
                <EditorPane
                    open=open_file.into()
                    seed=editor_seed.into()
                    dirty=editor_dirty
                />
            </div>
```

- [ ] **Step 5: Add CSS**

Append to `ui/style.css`:

```css
.workspace {
  display: flex;
  flex: 1 1 auto;
  min-height: 0;
  border-top: 1px solid #1c2128;
}
.workspace-side {
  flex: 0 0 240px;
  display: flex;
  flex-direction: column;
  border-right: 1px solid #1c2128;
  overflow: auto;
}
.refresh-btn { margin: 6px; }
.file-tree { padding: 4px 0; font: inherit; }
.tree-row { padding: 1px 8px; white-space: nowrap; cursor: pointer; }
.tree-row:hover { background: #11151a; }
.tree-dir-row { color: var(--accent); }
.tree-file-row { color: var(--fg); }
.tree-children { padding-left: 12px; }
.tree-empty, .editor-empty, .editor-notice { padding: 8px 10px; color: var(--dim); }
.editor-pane { flex: 1 1 auto; display: flex; min-width: 0; }
.editor-host { flex: 1 1 auto; display: flex; flex-direction: column; min-width: 0; }
.editor-header {
  padding: 4px 10px;
  border-bottom: 1px solid #1c2128;
  color: var(--dim);
  font: inherit;
}
```

- [ ] **Step 6: Confirm host tests + wasm build**

Run: `cd ui && cargo test`
Expected: PASS (pure helpers unaffected).

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: PASS.

- [ ] **Step 7: Manual browser smoke test**

In one terminal: `OTTO_TOKEN=dev cargo run -p otto-engine -- serve --port 8787 --root .`
In another: `cd ui && trunk serve`
Open `http://127.0.0.1:8080`, connect to `ws://127.0.0.1:8787` with token `dev`. Verify: the tree populates on connect; clicking a `.rs`/`.md` file opens it with syntax highlighting; typing shows the "●" modified marker; a `.git/`-style sensitive path (if clicked) surfaces a denial row rather than content; **Refresh files** reloads the tree. Confirm no console errors (CORS works).

- [ ] **Step 8: Commit**

```bash
git add ui/src/app.rs ui/style.css
git commit -m "feat(ui): wire workspace tree + editor into the app shell"
```

---

## Task 10: Docs — record Sub-project C built

**Files:**
- Modify: `docs/superpowers/specs/2026-06-17-ui-roadmap.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update the roadmap**

In `docs/superpowers/specs/2026-06-17-ui-roadmap.md`:
- Update the `**Status:**` line to note C shipped with its design + plan links.
- In the sub-projects table, change the **C** row's `#` cell from `**C**` to `**C** ✅` and append to its "Sub-project" cell: `*(shipped — [design](2026-06-18-ui-workspace-tree-editor-design.md) · [plan](../plans/2026-06-18-ui-workspace-tree-editor.md))*`. Change its "Protocol / engine changes" cell to record the actual change: `**Done:** added a tower-http CORS layer to /workspace so the browser can call it cross-origin (no protocol change). Editor is kode-leptos (native Leptos CSR); no JS-glue seam needed.`

- [ ] **Step 2: Update CLAUDE.md**

In `CLAUDE.md`, extend the UI paragraph (the "Sub-project B then shipped" sentence) with a "Sub-project C then shipped" sentence: the UI now lists the served workspace via the bearer-authed `POST /workspace` RPC (unblocked by a new CORS layer on the engine — the one engine change, not a protocol change), renders a collapsible file tree, and opens files into a `kode-leptos` editor with a local (unsaved) buffer; persistence stays deferred to Sub-project D. Note `kode-leptos`/`gloo-net` are UI-only deps and the `ui/` crate still depends only on `protocol`.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-06-17-ui-roadmap.md CLAUDE.md
git commit -m "docs: record sub-project C (workspace tree + editor) built"
```

---

## Done criteria

- `cargo test -p otto-engine` green (CORS test + existing suite).
- `cargo build --workspace` and the offline determinism suite untouched (the `ui/` crate stays excluded from the workspace).
- `cd ui && cargo test` green (pure helpers); `cd ui && cargo build --target wasm32-unknown-unknown` green.
- Manual smoke test passes: tree loads on connect, files open with highlighting, edits flag dirty, Refresh works, sensitive-path denials surface as rows, no CORS console errors.
- Roadmap + CLAUDE.md updated.
