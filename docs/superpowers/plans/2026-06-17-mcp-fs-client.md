# MCP Client Adapter + mcp-fs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `mcp-fs` (an rmcp stdio server exposing path-contained `fs.read`/`fs.write`/`fs.list`) and an engine-side MCP client adapter that spawns it and registers its tools as `Tool`s behind the existing permission gate; wire the binary to prefer `mcp-fs` with an in-process fallback.

**Architecture:** `crates/mcp-fs` wraps a `LocalWorkspace` (rooted at `argv[1]`) and serves three rmcp tools with the EXACT existing JSON shapes. `crates/engine/src/mcp.rs` spawns the server via rmcp's `TokioChildProcess`, lists its tools, and wraps each as an `McpTool` (forwards `call` to the rmcp client). `build_tool_registry` is unchanged (sync, in-process — the fallback); `cmd_run`/`cmd_serve` add one async step that registers the MCP fs tools (overwriting the in-process ones) or logs and falls back. The gate runs before dispatch regardless.

**Tech Stack:** Rust (edition 2024), `rmcp` (client+server, transport-child-process, transport-io, macros), `otto-workspace`, `escargot` (dev, builds the bin in tests), tokio/serde/anyhow.

**Spec:** `docs/superpowers/specs/2026-06-17-mcp-fs-client-design.md`.

---

## rmcp API latitude (read first)

Tasks 1–4 touch `rmcp`, whose API evolves and is NOT pinnable from this plan verbatim. For every rmcp-specific call (server `#[tool]`/`#[tool_router]`/`ServerHandler`/`serve(stdio())`, client `TokioChildProcess`/`().serve(...)`/`list_tools`/`call_tool`, `CallToolRequestParam`/`CallToolResult`/structured-content accessors, `Parameters<T>`):

- **Pin a recent rmcp version** with `cargo add` and resolve features so it builds.
- **Consult current rmcp docs** via the context7 tool (`resolve-library-id` → `query-docs` on `/websites/rs_rmcp`) for the exact API; the code blocks below are STRUCTURAL guides, not guaranteed-exact signatures.
- **Keep behavior + the test assertions fixed**: tool names `fs.read`/`fs.write`/`fs.list` (or `fs_read`/etc. remapped by the client adapter to the dotted gate names if the macro rejects dots), and result shapes `{content}`/`{bytes_written}`/`{paths}`.
- If rmcp's API diverges so far that the design can't fit, STOP and report rather than thrashing.

Non-rmcp parts of every task are exact — follow them verbatim.

---

### Task 1: Scaffold `crates/mcp-fs` and get rmcp building

**Files:**
- Create: `crates/mcp-fs/Cargo.toml`
- Create: `crates/mcp-fs/src/main.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Add the workspace member**

In the root `Cargo.toml` `members` array, add `"crates/mcp-fs",`.

- [ ] **Step 2: Create the crate manifest**

Create `crates/mcp-fs/Cargo.toml`. Resolve the exact `rmcp` version/features with `cargo add rmcp --features server,transport-io,macros` (adjust feature names to what the resolved version exposes — e.g. `transport-io` may be the stdio transport feature; confirm via docs). A starting point:

```toml
[package]
name = "otto-mcp-fs"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "mcp-fs"
path = "src/main.rs"

[dependencies]
otto-workspace = { path = "../workspace" }
otto-engine-core = { path = "../engine-core" }   # for the Edit type used by apply_edit
rmcp = { version = "0.x", features = ["server", "transport-io", "macros"] }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "io-std"] }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 3: Minimal `main.rs` that builds against rmcp**

Create `crates/mcp-fs/src/main.rs` with a minimal server that compiles (no tools yet) — this validates the rmcp dependency + features resolve before writing handlers. Structural guide (adapt to the resolved rmcp API):

```rust
//! `mcp-fs <root>` — an MCP stdio server exposing path-contained fs.read/fs.write/fs.list over a
//! `LocalWorkspace` rooted at <root>. The engine spawns this and registers its tools behind the gate.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-fs <root>"))?;
    let _ = root; // tools land in Task 2
    // Serve over stdio. The exact rmcp serve call is filled in in Task 2.
    Ok(())
}
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p otto-mcp-fs`
Expected: PASS (rmcp + features resolve; the binary builds). Record the exact `rmcp` version you pinned.

- [ ] **Step 5: Commit**

```bash
git add crates/mcp-fs/Cargo.toml crates/mcp-fs/src/main.rs Cargo.toml Cargo.lock
git commit -m "build(mcp-fs): scaffold crate + rmcp dependency"
```

---

### Task 2: Implement the three fs tools in `mcp-fs`

**Files:**
- Modify: `crates/mcp-fs/src/main.rs` (or split a `server` module)

- [ ] **Step 1: Write the failing unit tests**

Add a `#[cfg(test)] mod tests` to `crates/mcp-fs/src/main.rs` that tests the tool LOGIC directly (call the handler methods on the server struct against a tempdir, without the MCP transport — so the test is independent of rmcp's wire layer). Use whatever the handler methods are named; the assertions on behavior are fixed:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let server = FsServer::new(dir.path().to_path_buf());
        let w = server.do_write("a.txt".into(), "hi".into()).await.unwrap();
        assert_eq!(w, 2); // bytes_written
        let c = server.do_read("a.txt".into()).await.unwrap();
        assert_eq!(c, "hi"); // content
    }

    #[tokio::test]
    async fn list_returns_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let server = FsServer::new(dir.path().to_path_buf());
        server.do_write("a.txt".into(), "x".into()).await.unwrap();
        let paths = server.do_list(Some("**".into())).await.unwrap();
        assert!(paths.contains(&"a.txt".to_string()));
    }

    #[tokio::test]
    async fn read_rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let server = FsServer::new(dir.path().to_path_buf());
        assert!(server.do_read("../escape.txt".into()).await.is_err());
    }
}
```

This test calls plain async inherent methods `do_read`/`do_write`/`do_list` on `FsServer` — define those as the tool implementations, and have the rmcp `#[tool]` handlers be thin wrappers over them. This keeps the fs behavior testable without rmcp's transport, and isolates rmcp to the wrapper layer.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-mcp-fs`
Expected: FAIL to compile — `FsServer` / `do_*` not defined.

- [ ] **Step 3: Implement `FsServer` + the plain methods + the rmcp tool wrappers**

In `crates/mcp-fs/src/main.rs`:

The plain, rmcp-independent core (exact — follow verbatim):

```rust
use std::path::PathBuf;
use std::sync::Arc;

use otto_engine_core::types::Edit;
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_workspace::LocalWorkspace;

#[derive(Clone)]
pub struct FsServer {
    ws: Arc<LocalWorkspace>,
    // rmcp may also require a `tool_router: ToolRouter<Self>` field; add it per the macro docs.
}

impl FsServer {
    pub fn new(root: PathBuf) -> Self {
        Self { ws: Arc::new(LocalWorkspace::new(root)) }
    }

    pub async fn do_read(&self, path: String) -> anyhow::Result<String> {
        let bytes = self.ws.read(std::path::Path::new(&path)).await?;
        Ok(String::from_utf8(bytes)?)
    }

    pub async fn do_write(&self, path: String, contents: String) -> anyhow::Result<u64> {
        self.ws
            .apply_edit(&Edit { path: PathBuf::from(path), new_contents: contents })
            .await
    }

    pub async fn do_list(&self, glob: Option<String>) -> anyhow::Result<Vec<String>> {
        let paths = self.ws.list(glob.as_deref().unwrap_or("*")).await?;
        Ok(paths.into_iter().map(|p| p.to_string_lossy().into_owned()).collect())
    }
}
```

Then the rmcp layer (STRUCTURAL — adapt to the resolved rmcp API; consult docs):
- Define arg structs `ReadArgs { path: String }`, `WriteArgs { path: String, contents: String }`,
  `ListArgs { glob: Option<String> }` deriving `serde::Deserialize` + whatever rmcp's schema
  derive requires (e.g. `schemars::JsonSchema` if rmcp needs it — add the dep if so).
- `#[tool_router] impl FsServer { #[tool(name = "fs.read", ...)] async fn read(&self, params: Parameters<ReadArgs>) -> Result<CallToolResult, ...> { let content = self.do_read(params.0.path).await.map_err(...)?; Ok(CallToolResult::structured(serde_json::json!({ "content": content }))) } ... }`
  — `fs.write` returns structured `{ "bytes_written": n }`, `fs.list` returns `{ "paths": [...] }`.
  Use STRUCTURED content carrying exactly those JSON objects. If the macro rejects the dotted
  name `fs.read`, use `fs_read`/`fs_write`/`fs_list` (the client adapter remaps in Task 3).
- `impl ServerHandler for FsServer` with a `get_info`/capabilities advertising tools (per docs).

Update `main` to serve it:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-fs <root>"))?;
    let server = FsServer::new(root);
    // Adapt to the resolved rmcp API, e.g.:
    let service = rmcp::ServiceExt::serve(server, rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-mcp-fs`
Expected: PASS (3 tests — the rmcp-independent core logic).

- [ ] **Step 5: Commit**

```bash
git add crates/mcp-fs/src/main.rs
git commit -m "feat(mcp-fs): fs.read/fs.write/fs.list over a contained LocalWorkspace"
```

---

### Task 3: Engine MCP client adapter

**Files:**
- Modify: `crates/engine/Cargo.toml`
- Create: `crates/engine/src/mcp.rs`
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Add the rmcp client dependency**

In `crates/engine/Cargo.toml` `[dependencies]`, add (resolve features per docs):

```toml
rmcp = { version = "0.x", features = ["client", "transport-child-process"] }
```

Under `[dev-dependencies]`, add (builds the `mcp-fs` bin in the integration test):

```toml
escargot = "0.5"
```

- [ ] **Step 2: Write the failing error-path test**

Create `crates/engine/src/mcp.rs` with the adapter and a unit test for the spawn-failure path (no real server needed):

```rust
//! MCP client adapter: spawn an MCP stdio server, list its tools, and wrap each as a `Tool` so it
//! registers in the `ToolRegistry` behind the permission gate. The engine talks to MCP servers
//! over stdio (never by linking). rmcp API specifics are pinned to the resolved version.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use serde_json::Value;

// (rmcp imports filled in per the resolved API)

/// A live MCP client connection (holds the running rmcp service; kept alive while its tools are).
pub struct McpConnection {
    // Arc<rmcp RunningService<RoleClient>> or equivalent
    #[allow(dead_code)]
    service: Arc<McpClientService>,
}

// Alias for whatever rmcp's running-client type is (set in Step 3).
type McpClientService = (); // placeholder replaced in Step 3

/// Spawn `command` as an MCP server, initialize, list its tools, and return the connection plus a
/// `Tool` for each advertised tool.
pub async fn connect(
    command: tokio::process::Command,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    // Implemented in Step 3.
    let _ = command;
    anyhow::bail!("not yet implemented")
}

/// Convenience: build the `mcp-fs <root>` command and connect.
pub async fn connect_fs(
    bin: &str,
    root: &Path,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let mut command = tokio::process::Command::new(bin);
    command.arg(root);
    connect(command).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_fs_with_bogus_binary_errors() {
        let err = connect_fs("definitely-not-a-real-binary-xyz", Path::new(".")).await;
        assert!(err.is_err());
    }
}
```

- [ ] **Step 3: Implement the adapter**

Replace the placeholders in `crates/engine/src/mcp.rs` with the real rmcp-backed implementation (STRUCTURAL — adapt to the resolved API; consult docs):

- `McpClientService` = rmcp's running client service type (e.g. `rmcp::service::RunningService<rmcp::RoleClient, ()>`).
- `connect`:
  - `let transport = rmcp::transport::TokioChildProcess::new(command)?;`
  - `let service = rmcp::ServiceExt::serve((), transport).await?;` → wrap in `Arc`.
  - `let tools = service.list_tools(None).await?;` → for each tool `t`, build
    `McpTool { service: Arc::clone(&service), server_name: t.name.to_string(), gate_name: dotted(t.name) }`
    where `dotted` maps `fs_read`→`fs.read` etc. (identity if already dotted).
  - Return `(McpConnection { service }, mcp_tools)`.
- `McpTool` implements `Tool`:
  - `name(&self) -> &str` → `&self.gate_name`.
  - `call(&self, args: Value)`:
    - build `CallToolRequestParam { name: self.server_name.clone().into(), arguments: args.as_object().cloned() }`,
    - `let result = self.service.call_tool(param).await?;`
    - if `result.is_error == Some(true)` → `anyhow::bail!` with the text content,
    - else return the result as a `Value`: prefer `result.structured_content` if `Some`; otherwise
      concatenate the text content items and `serde_json::from_str` them into a `Value`.

A `dotted(name: &str) -> String` helper: `name.replace('_', ".")` only for the known `fs_*` names,
or simpler — map exactly `{"fs_read"->"fs.read", "fs_write"->"fs.write", "fs_list"->"fs.list"}` and
otherwise return the name unchanged. (If `mcp-fs` used dotted names directly, this is a no-op.)

- [ ] **Step 4: Register the module + re-export**

In `crates/engine/src/lib.rs`, add:

```rust
mod mcp;

pub use mcp::{McpConnection, connect_fs as mcp_connect_fs};
```

- [ ] **Step 5: Run the error-path test**

Run: `cargo test -p otto-engine mcp::tests::connect_fs_with_bogus_binary_errors`
Expected: PASS (spawning a nonexistent binary errors).

- [ ] **Step 6: Commit**

```bash
git add crates/engine/Cargo.toml crates/engine/src/mcp.rs crates/engine/src/lib.rs Cargo.lock
git commit -m "feat(engine): MCP client adapter (connect_fs + McpTool)"
```

---

### Task 4: Engine ↔ mcp-fs integration test (escargot)

**Files:**
- Create: `crates/engine/tests/mcp_fs.rs`

- [ ] **Step 1: Write the integration test**

Create `crates/engine/tests/mcp_fs.rs`. It builds the `mcp-fs` binary with `escargot`, connects via the MCP client, registers the tools in a real `ToolRegistry`, and exercises them through `ToolRegistry::call` (so the gate runs). (escargot's exact build API: adapt to the resolved version.)

```rust
//! End-to-end: build mcp-fs, spawn it via the MCP client, register its tools in a gated
//! ToolRegistry, and confirm fs.read/write/list round-trip with the exact shapes the Coder depends
//! on, and that the gate denies a sensitive path BEFORE it reaches the server. Loopback (stdio).

use std::sync::Arc;

use otto_engine::mcp_connect_fs;
use otto_engine_core::tool::{DenyAsk, ToolRegistry};
use otto_tools::DefaultPermissionGate;
use serde_json::json;

#[tokio::test]
async fn mcp_fs_tools_round_trip_and_stay_gated() {
    // Build the mcp-fs binary once; get its path.
    let bin = escargot::CargoBuild::new()
        .package("otto-mcp-fs")
        .bin("mcp-fs")
        .run()
        .expect("build mcp-fs")
        .path()
        .to_path_buf();

    let dir = tempfile::tempdir().unwrap();

    // Connect and register the MCP fs tools in a gated registry.
    let (_conn, tools) = mcp_connect_fs(bin.to_str().unwrap(), dir.path())
        .await
        .expect("connect to mcp-fs");
    let mut registry = ToolRegistry::new(
        Arc::new(DefaultPermissionGate::new()),
        Arc::new(DenyAsk),
    );
    for t in tools {
        registry.register(t);
    }

    // write -> {bytes_written}
    let w = registry
        .call("fs.write", json!({ "path": "a.txt", "contents": "hi" }))
        .await
        .unwrap();
    assert_eq!(w, json!({ "bytes_written": 2 }));

    // read -> {content} (exact shape the Coder relies on)
    let r = registry.call("fs.read", json!({ "path": "a.txt" })).await.unwrap();
    assert_eq!(r, json!({ "content": "hi" }));

    // list -> {paths}
    let l = registry.call("fs.list", json!({ "glob": "**" })).await.unwrap();
    assert!(l["paths"].as_array().unwrap().iter().any(|p| p == "a.txt"));

    // The gate denies a sensitive write BEFORE it reaches mcp-fs.
    assert!(
        registry.call("fs.write", json!({ "path": ".env", "contents": "SECRET=x" })).await.is_err(),
        "the gate must deny a sensitive path over the MCP-backed tool"
    );
    // Nothing was written.
    assert!(!dir.path().join(".env").exists());
}
```

Note: `DefaultPermissionGate` is exported from `otto-tools` (confirm the path; it is used by `build_tool_registry`). `otto-tools` is a dependency of `otto-engine`, so it is usable from the engine's integration test. If `DefaultPermissionGate`/`DenyAsk` import paths differ, adjust to the real ones (this is not rmcp latitude — use the actual exports).

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p otto-engine --test mcp_fs`
Expected: PASS — the MCP round-trip produces the exact shapes and the gate denies `.env`.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/tests/mcp_fs.rs
git commit -m "test(engine): mcp-fs round-trip through the gated registry"
```

---

### Task 5: Wire the binary to prefer mcp-fs (with fallback)

**Files:**
- Modify: `crates/engine/src/main.rs`

- [ ] **Step 1: Add the MCP-preferred tool setup to `cmd_run` and `cmd_serve`**

In `crates/engine/src/main.rs`, add a small helper and use it in both commands. The helper builds the in-process registry, then tries to register the mcp-fs tools over it (overwriting `fs.*`), falling back on error. It returns the registry plus the connection to keep alive.

Add the import to the `use otto_engine::{...}` line: `mcp_connect_fs`, `McpConnection`.

Add the helper:

```rust
fn mcp_fs_bin() -> String {
    std::env::var("OTTO_MCP_FS_BIN").unwrap_or_else(|_| "mcp-fs".to_string())
}

/// Build the tool registry, preferring mcp-fs for fs tools and falling back to in-process.
/// Returns the registry and the (optional) MCP connection to keep alive for the process lifetime.
async fn build_tools_preferring_mcp(
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
) -> (ToolRegistry, Option<McpConnection>) {
    let mut registry = build_tool_registry(tools_workspace, root.clone());
    match mcp_connect_fs(&mcp_fs_bin(), &root).await {
        Ok((conn, mcp_tools)) => {
            for t in mcp_tools {
                registry.register(t); // overwrites in-process fs.read/write/list by name
            }
            (registry, Some(conn))
        }
        Err(e) => {
            eprintln!("mcp-fs unavailable ({e}); using in-process fs tools");
            (registry, None)
        }
    }
}
```

`ToolRegistry` and `McpConnection` must be in scope — add `use otto_engine_core::tool::ToolRegistry;` if not already imported in `main.rs`, and `McpConnection` via the `otto_engine::{...}` import.

In `cmd_run`, replace the existing:

```rust
    let tools = Arc::new(build_tool_registry(tools_workspace, root.clone()));
```

with:

```rust
    let (tools, _mcp) = build_tools_preferring_mcp(tools_workspace, root.clone()).await;
    let tools = Arc::new(tools);
    // _mcp is held until end of function so the mcp-fs child stays alive.
```

Do the SAME replacement in `cmd_serve`. In `cmd_serve`, `_mcp` must stay alive across
`serve_run(...).await` — bind it with a name like `_mcp` at function scope (not dropped early);
since `serve_run` is awaited in the same function, the binding lives long enough. Confirm `_mcp`
is not dropped before `serve_run` by keeping it bound in the function body until after the serve
call (rename to `let _mcp_guard = ...;` if clearer).

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p otto-engine`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): prefer mcp-fs for fs tools, fall back to in-process"
```

---

### Task 6: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, and test the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: fmt clean (or trivial changes you then include), clippy clean, all tests green — the `mcp-fs` unit tests, the `mcp::` error-path test, the `mcp_fs` integration test, and every existing crate unchanged (`build_tool_registry` and its 17 call sites untouched).

- [ ] **Step 2: If `cargo fmt` changed anything, commit it**

```bash
git add -A
git commit -m "style: cargo fmt after mcp-fs"
```

(If fmt made no changes, skip. Do NOT `git add` any stray build artifact — only source files fmt touched.)

---

## Done criteria

- `crates/mcp-fs` is a workspace member: an rmcp stdio server exposing `fs.read`/`fs.write`/`fs.list` over a path-contained `LocalWorkspace`, returning `{content}`/`{bytes_written}`/`{paths}`; its core logic is unit-tested.
- `crates/engine/src/mcp.rs`: `connect`/`connect_fs` spawn an MCP server and wrap its tools as `Tool`s (`McpTool`), held alive by the connection/registry; the bogus-binary path errors.
- Engine integration test: through a gated `ToolRegistry`, the MCP-backed `fs.*` tools round-trip with the exact shapes, and a sensitive-path write is gate-denied before reaching `mcp-fs`.
- The binary prefers `mcp-fs` (`OTTO_MCP_FS_BIN` / `mcp-fs` on PATH) and falls back to in-process; `build_tool_registry` and the existing suite are unchanged.
- `cargo test --workspace` green; clippy/fmt clean.

**Next in the MCP axis:** `mcp-grep` (new search capability), then `mcp-git`, then migrating the sandboxed `BashTool` to `mcp-bash`. (`mcp-lsp` is v2.)
