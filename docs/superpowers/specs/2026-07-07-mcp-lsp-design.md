# `mcp-lsp` design

Status: shipped. Closed the last "deferred to v2" line in the MCP
tool-server tier (`docs/ARCHITECTURE.md`'s crate table).

> **Update (2026-07-08):** the "Future generalization" section below is now built — multi-language
> dispatch (Rust/TS/JS/Python/Go) shipped per
> [2026-07-08-mcp-lsp-multi-language-design.md](2026-07-08-mcp-lsp-multi-language-design.md).

## Motivation

The MCP tool-server tier (`mcp-fs`, `mcp-grep`, `mcp-git`, `mcp-bash`) is complete; `mcp-lsp`
is the one crate in that tier that was named but never built. Today the Verifier learns
about errors only by running the project's full test/build command and the ContextFinder
only navigates by lexical search (grep-style prefilter + the `retrieval` index). Neither
gets structured, per-file compiler/language-server output, and neither can jump to a
symbol's real definition or usages. `mcp-lsp` bridges otto to a real language server
(rust-analyzer for v1) so agents get:

- **Diagnostics** — structured, per-file errors/warnings, faster and cheaper than a full
  test run, feeding the Verifier/Repair loop.
- **Navigation** — go-to-definition, find-references, hover, feeding ContextFinder/Coder
  with real symbol information instead of grep-based guessing.

## Goals

- Ship four read-only tools: `lsp.diagnostics`, `lsp.definition`, `lsp.references`,
  `lsp.hover`.
- v1 bridges to **Rust only**, via `rust-analyzer`. otto's own codebase is Rust, so this is
  immediately dogfoodable; the design generalizes to other languages later without a
  rewrite (see "Future generalization" below).
- Follow the existing `mcp-*` crate pattern exactly: standalone rmcp stdio binary, spawned
  by the engine, never linked.
- Preserve the project's offline-determinism invariant: `cargo test --workspace` must not
  require `rust-analyzer` (or any language server) to be installed.
- Purely additive: an environment without `rust-analyzer` on `PATH` gets an otto session
  with no `lsp.*` tools registered — `otto run`/`otto serve` is otherwise unaffected, same
  as a missing `mcp-grep`/`mcp-git` binary today.

## Non-goals (v1)

- Multi-language support (TypeScript/Python/Go language servers) — deferred, see below.
- Write-capable LSP features: rename, code actions, formatting.
- Auto-restart of a crashed `rust-analyzer` process — a dead process makes subsequent
  `lsp.*` calls return a clear tool error; restarting it is a later improvement if it turns
  out to matter in practice.
- Incremental (diff-based) document sync — v1 always sends full-text `didChange`, which is
  simpler and correct; incremental sync is a pure performance optimization with no
  behavioral difference, deferred until profiling shows it matters.

## Architecture

A new standalone binary crate, `crates/mcp-lsp` (package `otto-mcp-lsp`, binary `mcp-lsp`),
matching `mcp-fs`/`mcp-grep`/`mcp-git`/`mcp-bash`'s shape:

- Takes `mcp-lsp <root>` as its argument (the workspace root), exactly like the other
  servers.
- An rmcp `#[tool_router]` server (`LspServer`) exposing the four tools, each a thin shim
  over an `rmcp`-independent `do_*` method (unit-testable directly, same convention as
  `FsServer::do_read`/`do_write`/`do_list`).
- Internally hand-rolls a minimal **LSP client** — no LSP client crate exists in the
  workspace today (confirmed: no prior art, no `Cargo.lock` entries for `lsp-types`/
  `tower-lsp`). `mcp-lsp` is the one place in the codebase that speaks LSP-over-stdio as a
  *client* to a real language server, the mirror image of the MCP-over-stdio *server* role
  every other `mcp-*` crate plays toward the engine.

```
otto engine  <--MCP/stdio-->  mcp-lsp  <--LSP/stdio-->  rust-analyzer
             (agent-facing)            (bridge)         (language-facing)
```

Tool names are dotted, consistent with `fs.*`/`git.*`: `lsp.diagnostics`, `lsp.definition`,
`lsp.references`, `lsp.hover`. All four are read-only. No gate changes are required:
`DefaultPermissionGate` already Allows any tool but `bash` by default, and because every
`lsp.*` tool takes a `path` arg, the existing sensitive-path floor (which inspects
`path`/`paths`/`glob` keys) auto-denies queries against `.env`/`.git`/etc. for free — the
same free-ride every other path-bearing tool gets today.

## Components

### `LspClient`

Owns the `rust-analyzer` child process's stdin/stdout (generic over
`AsyncRead + AsyncWrite` so tests can substitute an in-memory duplex pipe instead of a real
subprocess) plus:

- A monotonic request-id counter and a `Mutex<HashMap<i64, oneshot::Sender<Value>>>`
  mapping in-flight request ids to their awaiting caller.
- A background reader task that demuxes incoming `Content-Length`-framed JSON-RPC messages:
  a message with an `id` matching a pending request resolves that request's oneshot; a
  `textDocument/publishDiagnostics` notification updates a
  `Arc<Mutex<HashMap<Url, (version: i32, Vec<Diagnostic>)>>>` cache, keyed and versioned so
  a diagnostics response from *before* the latest edit is never mistaken for current.
- `initialize(root)` / `initialized()` — called once at startup.
- `request(method, params, timeout)` — send a request, await its oneshot with a timeout;
  returns a tool-level error on timeout rather than hanging the calling turn.
- `notify(method, params)` — fire-and-forget (used for `didOpen`/`didChange`).

### `LspServer`

The rmcp-facing struct (mirrors `FsServer`):

- `lsp_client: LspClient`
- `workspace: Arc<LocalWorkspace>` (reused from `otto-workspace`, same as `mcp-fs` — gives
  path-containment safety for free instead of hand-rolling it).
- `open_docs: Mutex<HashMap<PathBuf, i32>>` — tracks each open document's last-synced
  version, so a query after a Coder edit resends fresh content via `didChange` rather than
  querying stale server-side state.
- `open_if_needed(path)`: reads current disk content through `LocalWorkspace::read`,
  compares against `open_docs`; sends `didOpen` (first time) or a full-text `didChange`
  (content changed) before every tool call that touches that file.

### Tool schemas

All four tools take 1-based `line`/`character` (matching how a human or an agent reading
compiler output/grep results naturally refers to file positions — most compilers report
1-based line numbers); `mcp-lsp` converts to LSP's 0-based `Position` internally. This
avoids the single most common off-by-one confusion an agent calling these tools would hit.

- `lsp.diagnostics { path }` → `{ diagnostics: [{ line, character, severity, message,
  code }], timed_out: bool }`
- `lsp.definition { path, line, character }` → `{ locations: [{ path, line, character }] }`
  (empty if none found)
- `lsp.references { path, line, character }` → `{ locations: [{ path, line, character }] }`
- `lsp.hover { path, line, character }` → `{ contents: string | null }`

## Data flow

1. **Startup**: mcp-lsp spawns `rust-analyzer`, sends `initialize` (`rootUri` = the crate
   root) then `initialized`. If the binary isn't found or the handshake errors, mcp-lsp
   exits with a non-zero status — which surfaces through the engine's existing
   `connect_lsp(...).await` as an `Err`, logged and skipped exactly like a missing
   `mcp-grep`/`mcp-git` binary today. No separate "is rust-analyzer installed" pre-check is
   needed in the engine; the failure already flows through the standard connect-error path.
2. **Navigation calls** (`definition`/`references`/`hover`): `open_if_needed`, then a plain
   request/response over `LspClient::request` with a ~10s timeout.
3. **Diagnostics**: diagnostics are **server-pushed**, not request/response. After
   `open_if_needed` triggers a `didOpen`/`didChange`, `lsp.diagnostics` polls the versioned
   cache for that URI (checking the cached version against the version just sent) until
   either a matching-or-newer entry lands or a longer timeout elapses (default 15s,
   overridable via `OTTO_MCP_LSP_DIAG_TIMEOUT_MS` — rust-analyzer's first-open index can be
   slow). On timeout, whatever's cached is returned with `timed_out: true`, so the
   Verifier/Coder never mistakes "rust-analyzer hasn't responded yet" for "compiles clean."

   Implementation note (discovered against the real rust-analyzer): the server publishes an
   *empty* pre-analysis diagnostics set shortly after `didOpen`, then the real set once analysis
   completes — so returning on the first fresh publish reads broken code as clean. The cache
   therefore debounces: a fresh entry is returned only once the publish stream has been quiet for
   `DIAGNOSTICS_QUIESCENCE` (2s, sized from observed 0.5-0.6s empty-to-real gaps), or at the
   deadline (a fresh-but-unquiesced entry still returns `timed_out: false` — the debounce delays,
   it never converts fresh into timeout).

## Engine wiring

- `crates/engine/src/mcp.rs`: add `connect_lsp(bin, root)`, mirroring
  `connect_grep`/`connect_git` exactly (no in-process fallback — there isn't one, same
  category as grep/git).
- `crates/engine/src/main.rs`: add `mcp_lsp_bin()` (env `OTTO_MCP_LSP_BIN`, default
  `"mcp-lsp"`) and a `mcp_connect_lsp` call inside `build_tools_preferring_mcp`, additive
  like grep/git — attempt, log-and-skip on `Err`. Called from both `cmd_run` and
  `build_serve_tools`/`cmd_serve`, same as every other MCP connection, so `otto run` and
  `otto serve` gain `lsp.*` identically.

## Error handling / lifecycle

One `rust-analyzer` child process is spawned once and kept warm for the whole mcp-lsp
session (restarting it is expensive — it has to re-index). No auto-restart on crash in v1
(YAGNI): if the process dies, subsequent `lsp.*` calls surface a normal tool-call error.
Concurrent tool calls are safe — the request-id map and diagnostics cache are both behind a
mutex, and each request gets its own oneshot regardless of call ordering. The binary
canonicalizes its argv root at startup so the rootUri sent to rust-analyzer always matches
the document URIs derived from the workspace root.

## Testing

- **Unit tests** (offline, deterministic, no `rust-analyzer` needed): drive `LspClient`
  over `tokio::io::duplex()` against an in-test fake "server" task scripted to send canned
  responses/notifications. Covers `Content-Length` framing, request/response matching,
  diagnostics caching + version-guarding, and timeout behavior. These run in the default
  `cargo test --workspace` and keep the project's offline-determinism invariant intact.
- **Integration test** (real `rust-analyzer`, gated): a small fixture Rust crate exercised
  end-to-end through all four tools. Self-skips with a printed message when `rust-analyzer`
  is not found on `PATH`, matching the existing `os_sandbox_available()`-gated test pattern
  used for hooks/bash elsewhere in `crates/engine`.

## Future generalization (not built now, but designed for)

Language dispatch is a one-entry table (`".rs" → "rust-analyzer"`) rather than hardcoded
throughout `LspServer`/`LspClient`. Adding TypeScript/Python/Go later means extending that
table and spawning one additional `LspClient` per language (routed by file extension), not
restructuring the framing/request/diagnostics-cache plumbing — that part is already
language-agnostic. This mirrors how `retrieval`'s tree-sitter symbol chunking already
supports Rust/JS/TS/Python/Go: `mcp-lsp` starting Rust-only is a scoping decision for v1,
not an architectural ceiling.

## New dependencies

- `lsp-types` (new, `mcp-lsp`-only) — provides `Diagnostic`/`Location`/`Hover`/`Position`
  wire types so the JSON-RPC payloads are built from well-tested structs instead of
  hand-rolled ones. Not added to `[workspace.dependencies]`, matching how `rmcp`/`schemars`
  are repeated per-crate rather than hoisted today.
- `rmcp` (`server`, `transport-io`, `macros` features), `schemars`, `serde`, `serde_json`,
  `anyhow`, `tokio` — same as every other `mcp-*` crate.
