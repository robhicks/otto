# otto UI — Sub-project C: Workspace tree + editor

**Date:** 2026-06-18
**Status:** Approved design — ready for implementation planning.
**Roadmap:** [`2026-06-17-ui-roadmap.md`](2026-06-17-ui-roadmap.md) (Sub-project C). Builds on
A (app shell + live session) and B (capabilities + status strip), both shipped.

## Goal

Add a **file tree** and an **editor pane** to the existing UI shell. The user browses the
served workspace's files (`List`), opens one (`Read`) into a code editor, and edits it in a
**local buffer**. Editing does **not** persist to disk in this slice — gated persistence is
Sub-project D (diff approval). The shell remains a minimalist, diff-first surface, not a
project-wide IDE.

## Decisions (locked during brainstorming)

1. **Transport: add CORS to the engine.** The browser UI (served by `trunk`, a different
   origin) reaching `POST /workspace` with an `Authorization: Bearer` header triggers a CORS
   preflight. The engine currently sends no CORS headers, so the call is blocked. We add a
   `tower-http` CORS layer to `serve::app`. This is the **only** engine change and is **not** a
   protocol change. (The WebSocket already works cross-origin because WS is not subject to
   CORS.)
2. **Editor: `kode-leptos`** (`=0.5.4`, pinned). A native **Leptos 0.8 CSR** code-editor
   component — pure Rust/WASM, **no npm/JS bundler**, MIT-licensed, with tree-sitter syntax
   highlighting. This eliminates the hand-rolled wasm-bindgen JS glue seam
   (`mountEditor`/`getDoc`/`setDoc`/`onChange`) the roadmap originally anticipated: we bind
   Leptos signals instead. *Risk accepted:* the crate is young (0.5.x, single org, repo
   self-describes as alpha) — hence the exact-version pin.
3. **Save behavior: local only (no save).** Edits live in a local buffer; nothing is written
   to disk. Persistence flows through Sub-project D's gated `fs.write` `Ask` path, keeping the
   permission gate the sole write path. We do **not** wire the editor to the `/workspace`
   `ApplyEdit` RPC in this slice.

## Why these hold up

- `workspace_rpc` is **session-free** — it operates on the engine's single workspace and needs
  only auth. So the tree works the moment the client is connected; no session id required.
- `WorkspaceResponse::Read` returns raw `Vec<u8>`, so the UI decides UTF-8-vs-binary itself.
- The server already enforces the **sensitive-path floor** (`.env*`, `.ssh/`, `.git/`, …) and
  path containment on `/workspace`. The UI is a thin client: it surfaces denials, it does not
  re-implement them.

## Architecture

### Engine (one additive change)

A CORS layer on the axum router returned by `serve::app`:

```
CorsLayer::new()
    .allow_origin(Any)
    .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
    .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
```

- Dependency: `tower-http = { version = "0.6", features = ["cors"] }` (compatible with the
  existing axum 0.8 / tower stack).
- **Security posture:** `allow_origin(Any)` matches the existing loopback/dev posture already
  accepted for the `?token=` query param in Sub-project A. Because auth rides the
  `Authorization` header (not cookies), wildcard origin without credentials mode is correct and
  no credentials are exposed. Documented inline at the layer.
- No change to `/ws`, to the protocol crate, or to `workspace_rpc` itself.

### UI (standalone `ui/` crate, Rust → WASM)

| File | Role |
|---|---|
| `ui/src/workspace.rs` *(new)* | `gloo-net` fetch client for `POST {http_base}/workspace`. Sends `WorkspaceRequest::{List,Read}` with the bearer token; parses `WorkspaceResponse`. First async I/O in the UI — driven by `spawn_local`, results pushed into signals. |
| `ui/src/url.rs` *(extend)* | Pure `ws_to_http_base(&str) -> String`: `ws://…`→`http://…`, `wss://…`→`https://…`. Host-tested. |
| `ui/src/tree.rs` *(new)* | Pure helpers: `build_tree(&[PathBuf]) -> Vec<TreeNode>` (flat list → sorted, nested dirs/files); `language_for_path(&Path) -> &str` (extension → `kode-leptos` language id); `decode_or_binary(&[u8]) -> FileBody` (valid UTF-8 → text; else a binary marker). All host-tested. |
| `ui/src/components/file_tree.rs` *(new)* | Renders the tree; directories collapsible (local signal); file click → callback. |
| `ui/src/components/editor_pane.rs` *(new)* | Wraps `kode-leptos` `CodeEditor`. `content`/`language`/`theme` signals + `on_change` → local buffer + a "● modified" indicator. Empty state when no file is open; binary/oversize files show a notice instead of mounting. |
| `ui/src/app.rs` *(extend)* | New signals (`files`, `open_file`, dirty flag); a left-tree / right-editor layout beside the existing log + prompt + connection form. Tree auto-loads on transition to `Connected`, plus a manual **Refresh** button. |

## Data flow

1. **Connect** (existing) → on transition to `Connected`, call `list("**/*")` →
   `WorkspaceResponse::List { paths }` → `build_tree` → tree renders. (Also re-runnable via
   **Refresh**.)
2. **Open** — file click → `read(path)` → `decode_or_binary(bytes)`:
   - text → `open_file` signal set (path + content); editor mounts with the content and
     `language_for_path(path)`.
   - binary / oversize → editor shows a notice, no mount.
3. **Edit** — `on_change(new_text)` → local buffer signal + modified flag. **No disk write.**

## Error handling

- Fetch / HTTP-non-2xx / CORS failures and `WorkspaceResponse::Error` (e.g. the server's
  sensitive-path floor denying `.env`/`.ssh`, or path-escape attempts) → surfaced as an error
  row / tree-status line, reusing the existing `client_error_row` style. The UI **displays**
  denials; it does not decide them.
- Non-UTF-8 bytes → "binary file — not editable" (no mojibake).
- Very large files → a soft size cap with a notice rather than mounting the editor.

## Testing

- **Host-side** (`cd ui && cargo test`, no browser): `ws_to_http_base`, `build_tree`,
  `language_for_path`, `decode_or_binary`. Matches the existing pure-helper test pattern in
  `url.rs` / `view_model.rs`.
- **Engine** (`cargo test -p otto-engine`): an `OPTIONS /workspace` preflight returns the
  expected `Access-Control-Allow-*` headers; a cross-origin-shaped `POST` still succeeds; the
  existing `/workspace` and `/ws` tests stay green.
- **wasm build check**: `cd ui && cargo build --target wasm32-unknown-unknown` (compiles
  `kode-leptos` + the fetch client). Manual browser smoke test for tree + editor, as in
  Sub-project A.

## Out of scope (YAGNI / deferred)

- Save / persist and diff rendering → **Sub-project D**.
- File create / delete / rename.
- Multiple open files / tabs — **one file at a time** (diff-first, per the design spec's
  non-goals).
- Live external-change watching — manual **Refresh** instead.

## Dependencies added

- **Engine:** `tower-http = { version = "0.6", features = ["cors"] }`.
- **UI:** `kode-leptos = "=0.5.4"` (pinned); `gloo-net = { version = "0.6", features = ["http"] }`.
  - *Cost:* `kode-leptos` pulls tree-sitter grammars (the `arborium` crates), so `ui/Cargo.lock`
    and the wasm bundle grow meaningfully — the price of real syntax highlighting. Audit
    `kode-leptos` default features during implementation and enable only what the editor needs.

## Invariants preserved

- `ui/` stays a standalone crate, **excluded from the workspace**, path-depending only on
  `protocol`. `kode-leptos`/`gloo-net` are UI-only and never enter the engine build, so
  `cargo build --workspace` and the offline determinism suite are untouched.
- The permission gate remains the **sole disk-write path**; this slice adds no write path.
- The protocol crate is unchanged (read/list reuse the existing `/workspace` RPC).
