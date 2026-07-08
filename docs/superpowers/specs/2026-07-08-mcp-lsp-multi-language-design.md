# mcp-lsp multi-language dispatch

Date: 2026-07-08
Status: approved design — supersedes the "Rust-only v1 / multi-language deferred" line in
[2026-07-07-mcp-lsp-design.md](2026-07-07-mcp-lsp-design.md).

## What this adds

`mcp-lsp` today bridges the four `lsp.*` tools (`diagnostics`/`definition`/`references`/`hover`)
to a single `rust-analyzer` child. This slice generalizes it to route each file to the right
language server by extension, adding **TypeScript/JavaScript** (`typescript-language-server`),
**Python** (`pyright-langserver`), and **Go** (`gopls`) alongside the existing Rust support.

No new tools and no protocol change — the same four `lsp.*` tools now work across five
languages. The v1 design pre-committed this shape: *"extending that table and spawning one
additional `LspClient` per language (routed by file extension), not restructuring the
framing/request/diagnostics-cache plumbing."* This is that extension.

## Why it's the right next slice

The engine builds an `IndexedRetriever` and wires `mcp-lsp` for `run`/`serve`, so the
ContextFinder and Coder can already reach `lsp.*` — but only on Rust files. Most workspaces
otto will touch are not Rust. Making the LSP bridge multi-language is the highest-capability
additive step left in the tooling tier: it directly sharpens navigation and diagnostics on the
non-Rust codebases that are the majority, and `lsp_client.rs`'s framing/request/diagnostics
plumbing is already language-agnostic, so the change is contained.

## Fixed decisions (from brainstorming)

- **Language set:** Rust (existing) + TS/JS + Python + Go — the full set the v1 design named,
  mirroring `retrieval`'s tree-sitter language set (Rust/JS/TS/Python/Go).
- **Python server:** `pyright-langserver --stdio` (Microsoft Pyright), env-overridable.
- **Spawn strategy:** **lazy per-language** — a server is spawned+initialized only on the first
  file of its language, then cached. A Rust repo never boots `gopls`/`pyright`.

## Architecture

### 1. Language table & dispatch model

The load-bearing distinction: **languageId and server-process are separate keys.** The four
TS/JS extensions all drive one `typescript-language-server` process but send different
`didOpen` languageIds. So the client registry is keyed by *server*, and a static extension
table maps each extension → (server, languageId).

Two small static structs in `main.rs`:

```rust
struct ServerSpec {
    key: &'static str,          // registry key; ".ts" and ".tsx" share "typescript-language-server"
    default_bin: &'static str,  // e.g. "typescript-language-server"
    args: &'static [&'static str], // e.g. &["--stdio"]  (rust-analyzer: &[])
    env_override: &'static str, // e.g. "OTTO_TS_LANGUAGE_SERVER_BIN"
}

/// ext (lowercased, no dot) → (server, LSP languageId). None ⇒ unsupported.
fn config_for_extension(ext: &str) -> Option<(&'static ServerSpec, &'static str)>;

/// env override (if set) else default_bin.
fn resolved_bin(spec: &ServerSpec) -> String;
```

The table:

| Ext | languageId | Server key | Default command | Env override |
|---|---|---|---|---|
| `rs` | `rust` | `rust-analyzer` | `rust-analyzer` | `OTTO_RUST_ANALYZER_BIN` |
| `ts` | `typescript` | `typescript-language-server` | `typescript-language-server --stdio` | `OTTO_TS_LANGUAGE_SERVER_BIN` |
| `tsx` | `typescriptreact` | `typescript-language-server` | *(same)* | *(same)* |
| `js`, `mjs`, `cjs` | `javascript` | `typescript-language-server` | *(same)* | *(same)* |
| `jsx` | `javascriptreact` | `typescript-language-server` | *(same)* | *(same)* |
| `py`, `pyi` | `python` | `pyright-langserver` | `pyright-langserver --stdio` | `OTTO_PYRIGHT_BIN` |
| `go` | `go` | `gopls` | `gopls` | `OTTO_GOPLS_BIN` |

`OTTO_RUST_ANALYZER_BIN` keeps its existing name/behavior. `lsp_client.rs` (framing,
request/response dispatch, generation-tracked diagnostics cache) is **untouched** — it is
already generic over the stream and carries no language assumptions.

### 2. `LspServer`: single client → lazy client registry

`LspServer.lsp: Arc<LspClient>` is replaced by a lazily-populated registry keyed by server key:

```rust
enum ServerState {
    Ready(Arc<LspClient>, tokio::process::Child), // Child retained so kill_on_drop doesn't fire
    Failed(String),                               // cached spawn/init error; no re-spawn hammering
}

pub struct LspServer {
    clients: Arc<Mutex<HashMap<&'static str, ServerState>>>,
    workspace: Arc<LocalWorkspace>,
    root: PathBuf,
    open_docs: Arc<Mutex<HashMap<String, i32>>>, // URI → version, unchanged (URIs globally unique)
}
```

- `get_or_spawn(spec) -> anyhow::Result<Arc<LspClient>>`: lock the registry; `Ready` → return the
  clone; `Failed(msg)` → return the cached error; absent → `spawn_process(bin, args)` +
  `client.initialize(&root)`, insert `Ready` and return the `Arc`; on any spawn/init failure,
  insert `Failed(msg)` and return the error. The lock is held across spawn+initialize so two
  concurrent first-calls for the same language don't double-spawn.
- `open_if_needed(path) -> anyhow::Result<(Arc<LspClient>, String /*uri*/, u64 /*generation*/)>`:
  resolve the extension via `config_for_extension` (unsupported → `"no language server configured
  for .<ext>"` error), `get_or_spawn` the server, then send `didOpen`/`didChange` on **that
  client** using the config's languageId, and return the client so each `do_*` method uses it in
  place of the former `self.lsp`.
- Each `do_diagnostics`/`do_definition`/`do_references`/`do_hover` changes only its first line:
  it binds the client returned by `open_if_needed` and issues its request against that client
  rather than `self.lsp`.

`spawn_process` gains an `args: &[&str]` parameter (`rust-analyzer` passes `&[]`); it builds the
`Command` with those args before wiring the `LspClient` to the child's stdio. `kill_on_drop` and
the null stderr are unchanged.

### 3. Startup availability gate + one behavior change

Today `main()` eagerly spawns `rust-analyzer` and errors if it is absent, so `connect_lsp` fails
and the engine registers **no** `lsp.*` tools — the codebase's additive-absence pattern
(grep/git/bash/lsp are all registered only when their backend exists). Lazy spawning must
preserve that at the toolset level.

**Gate:** at startup, probe PATH for each distinct server's `resolved_bin`. If **none** resolve
to an executable, exit nonzero → `connect_lsp` fails → lsp tools absent. If **≥1** resolves,
serve and spawn nothing yet (servers come up lazily per language). `connect_lsp` in
`crates/engine/src/mcp.rs` is **unchanged**: a `mcp-lsp` process that exits before the MCP
handshake surfaces to `connect` exactly like a binary that failed to spawn.

The gate uses **PATH executable resolution**, not exec-based `--version` probing: the four
servers have inconsistent version invocations (`gopls version` is a subcommand; the others take
`--version`), so a generic exec-probe is fragile. A ~10-line hand-rolled PATH search (matching
the codebase's hand-rolled ethos — no `which` crate) resolves a bare binary name against `PATH`
entries, or checks the file directly when the (override) value contains a path separator.

**Conscious behavior change:** a host whose *only* configured server is a broken rustup
`rust-analyzer` shim (present on PATH, but exits nonzero because the component isn't installed)
previously advertised **no** lsp tools (the v1 integration test's exit-status probe caught this).
Now it passes the PATH gate, `mcp-lsp` starts, and the first `.rs` call returns a clear
`Failed(...)` spawn error instead. Visible per-call error > silent tool-absence — recorded here
as a deliberate trade, not an accident.

### 4. Engine side

No change to `crates/engine/src/mcp.rs::connect_lsp` or to `build_tools_preferring_mcp`'s
additive registration. The whole generalization lives inside the `mcp-lsp` crate. On, e.g., a
Go-only host: `mcp-lsp` starts (gopls resolves), the lsp tools register, and a stray `.rs`
diagnostics call lazily attempts `rust-analyzer`, returning a clean error if absent — never
poisoning the Go path.

## Data flow (unchanged shape, now routed)

`lsp.diagnostics {path: "app/main.py"}`
→ `do_diagnostics` → `open_if_needed("app/main.py")`
→ `config_for_extension("py")` = (`pyright-langserver`, `"python"`)
→ `get_or_spawn` → (first `.py` file) spawn `pyright-langserver --stdio`, `initialize(root)`, cache `Ready`
→ `didOpen` with languageId `"python"` on the pyright client
→ `wait_for_diagnostics(uri, generation, timeout)` (the existing quiescence-debounced cache)
→ structured `{diagnostics, timed_out}`.

Positions stay 1-based in/out; out-of-root locations are still skipped, not errors.

## Testing

- **Unit — table:** `config_for_extension` maps each extension to the right (server key,
  languageId) and returns `None` for an unknown extension; `.ts`/`.tsx`/`.js`/`.jsx` all resolve
  to the one `typescript-language-server` key but distinct languageIds.
- **Unit — dispatch:** a test-only seam pre-populates the `clients` registry with a
  duplex-backed `LspClient` under a chosen server key (bypassing exec), then asserts that a `.py`
  `open_if_needed` sends `didOpen` with languageId `"python"` and a `.go` open sends `"go"`. The
  existing Rust duplex tests reseed under the `rust-analyzer` key and are otherwise unchanged.
- **Unit — env override:** `resolved_bin` honors the per-server env var when set, else the
  default binary.
- **Integration (self-skipping):** per-language full round-trips against a real server —
  `gopls`, `pyright-langserver`, `typescript-language-server` — each printing a skip message and
  returning when its binary is not on PATH, mirroring the existing `rust-analyzer` integration
  test. CI realistically exercises only the Rust one; the others document intent and run where the
  toolchain exists.

The offline-determinism suite is untouched: `mcp-lsp` is spawned only by the binary, and no unit
test executes a real language server.

## Non-goals (v2)

- **Per-sub-project rootUri.** All servers use the single workspace root as `rootUri`, as
  `rust-analyzer` does today. Monorepos with independent sub-projects per language are out of
  scope.
- **Server restart/reconnect on crash.** A crashed server's cached client yields per-call
  timeouts/errors; no supervision loop (matches v1's no-restart posture).
- **Incremental `didChange`.** Full-text document sync is retained.
- **Auto-installing missing language servers.** The gate detects presence; it never fetches.
- **Languages beyond these five**, and config-file-driven registration of arbitrary servers.

## Files touched

- `crates/mcp-lsp/src/main.rs` — `ServerSpec` + `config_for_extension` + `resolved_bin` table;
  `LspServer` client registry (`ServerState`, `get_or_spawn`); `open_if_needed` returns the
  client; `do_*` use it; PATH-resolution startup gate in `main()`; new unit tests + per-language
  integration tests.
- `crates/mcp-lsp/src/lsp_client.rs` — `spawn_process` gains `args: &[&str]` (only signature +
  the one internal `Command` build change; the `spawn_process_with_bogus_binary_errors` test call
  updates to `&[]`).
- `CLAUDE.md` — the `mcp-lsp` crate row and the intro paragraph lose "additive and Rust-only in
  v1, multi-language dispatch deferred"; now multi-language (Rust/TS/JS/Python/Go), lazy
  per-language spawn, PATH-gated presence, per-server env overrides.
- `docs/ARCHITECTURE.md` — the mcp-lsp line drops "deferred to v2"/"Rust-only v1".
- `docs/superpowers/specs/2026-07-07-mcp-lsp-design.md` — a status note pointing here (its
  "Future generalization" is now built).

## New dependencies

None. No new crates; `spawn_process` args and the PATH search are hand-rolled.
