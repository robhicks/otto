# otto Design — MCP Client Adapter + mcp-fs

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-17

## Goal

Stand up the MCP tool-server pipeline as a vertical slice: an `mcp-fs` standalone server (rmcp,
stdio) exposing path-contained `fs.read`/`fs.write`/`fs.list`, and an engine-side MCP client
adapter that spawns it, lists its tools, and registers each as a `Tool` behind the existing
permission gate. The binary prefers `mcp-fs` and falls back to the in-process fs tools. First
sub-project of the MCP axis; proves the whole client↔server pipeline end to end.

## Context

- The `Tool` trait is already MCP-shaped: `fn name(&self) -> &str` + `async fn call(Value) -> Result<Value>`.
- `ToolRegistry::call` runs the `DefaultPermissionGate` (sensitive-path floor) BEFORE dispatch,
  and `register` inserts by name (so re-registering a name overwrites).
- The existing in-process fs tools (`crates/tools/src/fs.rs`) define the shapes callers depend
  on: `fs.read {path} -> {content}`, `fs.write {path, contents} -> {bytes_written}`,
  `fs.list {glob?} -> {paths}`. The **Coder agent calls `fs.read` as a tool** (`coder.rs`),
  so these shapes must be preserved exactly.
- `build_tool_registry(workspace, root)` is sync and has ~17 call sites (main + every engine
  test + service/remote). It must NOT change signature.
- rmcp (the official Rust MCP SDK) provides `TokioChildProcess` (client child-process
  transport), `().serve(transport)` → `RunningService<RoleClient>` with `list_tools`/`call_tool`,
  and `#[tool]`/`#[tool_router]` + `stdio()` for servers. Crate features: client/server,
  `transport-child-process`, `transport-io`, `macros`.

## Decisions (locked during brainstorming)

1. **MCP-preferred with in-process fallback.** `build_tool_registry` stays sync + in-process
   (unchanged — it is the fallback and what the test suite uses). Only the binary
   (`cmd_run`/`cmd_serve`) adds one async step: try `connect_fs(mcp-fs)`, register its `fs.*`
   tools (overwriting the in-process ones by name) and hold the connection; on any failure, log
   and keep the in-process tools. The engine is never crippled if `mcp-fs` is absent.
2. **Gate stays in front.** The permission gate runs in `ToolRegistry::call` before dispatch, so
   the MCP swap does not weaken the sensitive-path floor; `mcp-fs` also path-contains (defense in
   depth). No auth is pushed into the server.
3. **rmcp is the SDK** (client + servers) — spec-compliant, external-interop-capable. API
   specifics are implementation latitude (the crate's API evolves).
4. **Exact shapes preserved**: `mcp-fs` returns `{content}`/`{bytes_written}`/`{paths}` so the
   Coder and any other caller are unaffected.

## Architecture

### `crates/mcp-fs` (new binary)

An rmcp stdio server, invoked as `mcp-fs <root>`. Holds a `LocalWorkspace` rooted at `<root>`
(reusing its path-containment + list walk). Exposes three tools via `#[tool]`/`#[tool_router]`:

- `fs.read { path: String } -> { content: String }` — `workspace.read(path)` → utf8 string.
- `fs.write { path: String, contents: String } -> { bytes_written: u64 }` — `workspace.apply_edit`.
- `fs.list { glob: Option<String> } -> { paths: [String] }` — `workspace.list(glob ?? "*")`.

Results are returned as MCP **structured content** carrying the exact JSON object above. Tool
names are the dotted `fs.read`/`fs.write`/`fs.list` to match the gate + existing callers; if the
rmcp macro rejects dotted names, name them `fs_read`/`fs_write`/`fs_list` and have the client
adapter remap to the dotted gate names (see below). `main` builds the server and runs
`serve(stdio())`. Deps: `rmcp` (server/transport-io/macros), `otto-workspace`, `tokio`, `serde`,
`serde_json`, `anyhow`.

### Engine MCP client adapter (`crates/engine/src/mcp.rs`)

```rust
/// A live connection to an MCP stdio server. Holds the running rmcp client service; kept alive
/// as long as its McpTools are registered.
pub struct McpConnection { /* Arc<RunningService<RoleClient>> */ }

/// Spawn an MCP server (`command`), initialize, list its tools, and wrap each as a `Tool`.
pub async fn connect(command: tokio::process::Command) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)>;

/// Convenience for the fs server: builds the `mcp-fs <root>` command from `bin`/`root` and calls `connect`.
pub async fn connect_fs(bin: &str, root: &Path) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)>;
```

- `connect`: `TokioChildProcess::new(command)` → `().serve(transport).await?` → an
  `Arc<RunningService>`. `service.list_tools(None).await?` → for each, build an
  `McpTool { client: Arc<RunningService>, name: String, gate_name: String }`. `gate_name` is the
  name the tool registers under (the dotted `fs.read` etc.); if the server used `fs_read` it is
  remapped here so the gate and callers see `fs.read`.
- `McpTool`:
  - `name()` → `gate_name`.
  - `call(args)` → `client.call_tool(CallToolRequestParam { name: <server name>, arguments: args-as-object })` →
    convert `CallToolResult` to a `Value`: prefer `structured_content`; else concatenate/parse
    text content as JSON; an `is_error` result becomes an `anyhow::Error`.
- The `Arc<RunningService>` in each `McpTool` keeps the child process + service task alive while
  the tools are registered. `McpConnection` is returned so the caller can also hold it explicitly
  (and so a future `teardown` can cancel it); dropping all references shuts the child down.

Deps added to `engine`: `rmcp` (client/transport-child-process). Dev: `escargot` (build the
`mcp-fs` bin in the integration test).

### Binary wiring (`crates/engine/src/main.rs`)

`build_tool_registry` is unchanged. In `cmd_run` and `cmd_serve`, after building the registry
and before constructing the `EngineService`, add:

```rust
let mut tools = build_tool_registry(tools_workspace, root.clone());
let _mcp = match otto_engine::mcp_connect_fs(&mcp_fs_bin(), &root).await {
    Ok((conn, mcp_tools)) => {
        for t in mcp_tools { tools.register(t); }   // overwrites in-process fs.read/write/list
        Some(conn)                                   // hold the connection for the process lifetime
    }
    Err(e) => { eprintln!("mcp-fs unavailable ({e}); using in-process fs tools"); None }
};
let tools = Arc::new(tools);
```

`mcp_fs_bin()` = `std::env::var("OTTO_MCP_FS_BIN").unwrap_or_else(|_| "mcp-fs".to_string())`.
`_mcp` (the `McpConnection`) is bound for the duration of the run/serve so the child stays alive
(also kept alive transitively by the registered `McpTool` Arcs). The connect helper is re-exported
from the engine lib as `mcp_connect_fs`.

### Workspace + crate graph

- New member `crates/mcp-fs`. It depends on `otto-workspace` (for `LocalWorkspace`) — fine
  (workspace is a lib). The engine does NOT link `mcp-fs` (it spawns the binary), preserving the
  architecture's "engine talks to MCP servers over stdio, never by linking."

## Error handling & determinism

- `connect_fs` failure (binary not found, spawn/initialize error) → the binary logs and keeps the
  in-process fs tools (fallback). Never a hard failure of the engine.
- The gate runs before dispatch regardless of MCP, so a denied call never reaches `mcp-fs`.
- `mcp-fs` path-contains every op (via `LocalWorkspace`), so even a direct/malicious MCP call
  cannot escape the root.
- Determinism: the integration test spawns the `mcp-fs` child over stdio on the local machine
  (no network); the default `build_tool_registry` path (and thus the existing suite) is unchanged
  and offline.

## Testing

- **`mcp-fs` unit tests:** each tool handler against a `LocalWorkspace` over a tempdir —
  `fs.write` then `fs.read` round-trips with the right shapes; `fs.list` returns relative paths;
  a `../` path is rejected (containment).
- **Engine MCP integration test** (`escargot` builds `mcp-fs`): build a `ToolRegistry` (real
  `DefaultPermissionGate`), `mcp_connect_fs(bin, root)`, register the returned tools, then through
  `ToolRegistry::call`:
  - `fs.write {path:"a.txt", contents:"hi"}` → `{bytes_written: 2}`; the file exists on disk under root.
  - `fs.read {path:"a.txt"}` → `{content:"hi"}` (exact shape — the Coder depends on this).
  - `fs.list {glob:"**"}` → `{paths:[...]}` containing `a.txt`.
  - `fs.write {path:".env", ...}` → **denied by the gate before reaching `mcp-fs`** (error), and
    no `.env` is written.
- **Fallback (light):** `mcp_connect_fs` with a bogus binary path returns `Err` (so the binary's
  fallback branch is exercisable); a unit test asserts the error path.

**Implementation latitude (rmcp/escargot only):** the exact rmcp API — `serve`/`list_tools`/
`call_tool` shapes, `CallToolRequestParam`/`CallToolResult`/structured-content accessors,
`#[tool]`/`#[tool_router]`/`ServerHandler` wiring, the `stdio()`/`TokioChildProcess` constructors,
and dotted-vs-underscore tool names — are pinned to the resolved rmcp version and adjusted to its
real surface; keep the behavior and assertions fixed. Same for `escargot`'s build API.

## Out of scope (named, not silently dropped)

- **`mcp-grep`/`mcp-git`/`mcp-bash`/`mcp-lsp`** — subsequent sub-projects (bash migrates the
  sandboxed `BashTool`; lsp is v2).
- **External / Claude-Code MCP-server discovery** — the rmcp client is interop-capable, but the
  `extensions` crate that discovers and registers third-party MCP servers is a separate axis.
- **Dropping the in-process fs fallback** — kept until `mcp-fs` ships as a guaranteed packaged
  sidecar.
- **Making `build_tool_registry` async / MCP everywhere** — deliberately avoided (17 call sites);
  MCP is wired into the binary + the integration test only.
