# mcp-lsp multi-language dispatch

Date: 2026-07-08
Status: approved design (revised after three-agent review — see "Review resolutions") —
supersedes the "Rust-only v1 / multi-language deferred" line in
[2026-07-07-mcp-lsp-design.md](2026-07-07-mcp-lsp-design.md).

## What this adds

`mcp-lsp` today bridges the four `lsp.*` tools (`diagnostics`/`definition`/`references`/`hover`)
to a single `rust-analyzer` child. This slice generalizes it to route each file to the right
language server by extension, adding **TypeScript/JavaScript** (`typescript-language-server`),
**Python** (`pyright-langserver`), and **Go** (`gopls`) alongside the existing Rust support.

No new tools and no protocol change — the same four `lsp.*` tools now work across five
languages. The v1 design pre-committed this shape: *"extending that table and spawning one
additional `LspClient` per language (routed by file extension), not restructuring the
framing/request/diagnostics-cache plumbing."* This is that extension, plus the hardening the
review surfaced.

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
    key: &'static str,             // registry key; ".ts" and ".tsx" share "typescript-language-server"
    default_bin: &'static str,     // e.g. "typescript-language-server"
    args: &'static [&'static str], // e.g. &["--stdio"]  (rust-analyzer: &[])
    env_override: &'static str,    // e.g. "OTTO_TYPESCRIPT_LANGUAGE_SERVER_BIN"
    first_open_diag_timeout: Duration, // cold-start budget for the FIRST diagnostics call (see §Timeouts)
}

/// ext (already lowercased, no dot) → (server, LSP languageId). None ⇒ unsupported.
fn config_for_extension(ext: &str) -> Option<(&'static ServerSpec, &'static str)>;

/// env override (if set) else default_bin. Callers lowercase the extension first.
fn resolved_bin(spec: &ServerSpec) -> String;
```

The table:

| Ext | languageId | Server key | Default command | Env override |
|---|---|---|---|---|
| `rs` | `rust` | `rust-analyzer` | `rust-analyzer` | `OTTO_RUST_ANALYZER_BIN` |
| `ts` | `typescript` | `typescript-language-server` | `typescript-language-server --stdio` | `OTTO_TYPESCRIPT_LANGUAGE_SERVER_BIN` |
| `tsx` | `typescriptreact` | `typescript-language-server` | *(same)* | *(same)* |
| `js`, `mjs`, `cjs` | `javascript` | `typescript-language-server` | *(same)* | *(same)* |
| `jsx` | `javascriptreact` | `typescript-language-server` | *(same)* | *(same)* |
| `py`, `pyi` | `python` | `pyright-langserver` | `pyright-langserver --stdio` | `OTTO_PYRIGHT_LANGSERVER_BIN` |
| `go` | `go` | `gopls` | `gopls` | `OTTO_GOPLS_BIN` |

`OTTO_RUST_ANALYZER_BIN` keeps its existing name/behavior. The new env vars use the
exact-binary-name form to match its precedent. **Env overrides are a bare executable path, not a
command line** — a value with embedded args (`"node server.js"`) is treated as one binary name
and fails to spawn (same latent limit as today's `OTTO_RUST_ANALYZER_BIN`; documented, not
fixed). The languageIds (`typescript`/`typescriptreact`/`javascript`/`javascriptreact`/
`python`/`go`) and invocations (`--stdio` for TS/pyright, bare `gopls` for stdio) are the exact
strings these servers expect.

### 2. `LspServer`: single client → lazy per-key registry

`LspServer.lsp: Arc<LspClient>` is replaced by a lazily-populated registry keyed by server key.
**Per-key locking** is mandatory (see Review resolutions R1): a single map-wide `Mutex` held
across a cold `initialize` (up to 30s) would freeze *every* `lsp.*` call — including a warm
rust-analyzer lookup — behind one slow first-spawn, regressing the shipped "concurrent tool
calls are safe" invariant.

```rust
enum ServerState {
    Ready { client: Arc<LspClient>, _child: Option<tokio::process::Child> },
    // _child is Option so a test seam can insert a duplex-backed client with no OS process
    // (a duplex pipe has nothing for kill_on_drop to protect). Production spawns store Some.
}

pub struct LspServer {
    // Outer lock is held only briefly, to get-or-insert the per-key slot — never across spawn/init.
    slots: Arc<Mutex<HashMap<&'static str, Arc<tokio::sync::Mutex<Option<ServerState>>>>>>,
    absent: Arc<Mutex<HashSet<&'static str>>>, // servers whose bin is definitely not on PATH (permanent)
    workspace: Arc<LocalWorkspace>,
    root: PathBuf,
    open_docs: Arc<Mutex<HashMap<String, i32>>>, // URI → version, unchanged (URIs globally unique)
}
```

- **`get_or_spawn(spec) -> anyhow::Result<Arc<LspClient>>`:**
  1. If `spec.key ∈ absent`, return a stable "no `<bin>` on PATH" error immediately (permanent
     negative cache — a definitely-absent binary is never retried; avoids spawn hammering).
  2. Briefly lock `slots` to get-or-insert an `Arc<tokio::sync::Mutex<Option<ServerState>>>` for
     this key, then **release the outer lock**.
  3. Lock only that key's slot. If `Some(Ready{client, ..})`, return `client.clone()`. If `None`,
     resolve the binary on PATH; if unresolvable, record the key in `absent` and return the
     not-found error. Otherwise `spawn_process(bin, args)` + `client.initialize(&root)`; on
     success store `Ready` and return the client; **on spawn/init failure leave the slot `None`
     and return the error** (transient failures — EAGAIN/ENOMEM, a mid-session toolchain install
     — stay retry-eligible; only definitely-absent binaries are cached permanently).

  Different languages take different per-key locks, so a cold `gopls` init never blocks a warm
  `.rs` call. Same-key concurrent first-calls serialize on the one slot lock — no double-spawn.

- **Crashed-server eviction (Review resolution R4):** when a `do_*` request against a cached
  `Ready` client fails with a transport/connection error (closed pipe / dropped client — the
  server process died and its reader loop hit EOF), the caller **evicts** the slot back to `None`
  so the next call re-spawns, instead of every subsequent call hanging the full request timeout
  forever. A plain LSP-level error response or a timeout-with-live-client does **not** evict.

- **`open_if_needed(path) -> anyhow::Result<(Arc<LspClient>, String /*uri*/, u64 /*generation*/)>`:**
  lowercase the extension (`Path::extension` preserves case), resolve it via
  `config_for_extension` (unsupported → `"no language server configured for .<ext>"` error),
  `get_or_spawn` the server, then send `didOpen`/`didChange` on **that** client using the
  config's languageId, and return the client so each `do_*` method uses it in place of the
  former `self.lsp`.

- Each `do_diagnostics`/`do_definition`/`do_references`/`do_hover` binds the client returned by
  `open_if_needed` and issues its request against that client. `do_diagnostics` uses the
  per-server first-open budget on a language's first diagnostics call (§Timeouts).

**Constructor:** `LspServer::new(root: PathBuf)` (registry starts empty). A `#[cfg(test)]`
seeding method — `fn seed_ready_for_test(&self, key: &'static str, client: LspClient)` — inserts
a `Ready{ client, _child: None }` slot so duplex-backed unit tests exercise dispatch without
spawning a process.

`spawn_process` gains an `args: &[&str]` parameter (`rust-analyzer` passes `&[]`); it builds the
`Command` with those args before wiring the `LspClient` to the child's stdio. `kill_on_drop` and
the null stderr are unchanged.

### 3. `lsp_client.rs`: defend the default-capabilities invariant

The three new servers return diagnostics/hover/definition with **no `initializationOptions`**
for one reason: `initialize` advertises `capabilities: ClientCapabilities::default()`, which
suppresses the server→client requests (`workspace/configuration`,
`client/registerCapability`, `window/workDoneProgress/create`) that the reader loop does not
answer. This is load-bearing and was undocumented (Review resolution R2).

Two changes make it safe rather than accidental:

1. **Documented invariant.** `initialize`'s doc comment states: the client answers no
   server→client requests, so `ClientCapabilities` must stay minimal — advertising a richer
   capability (e.g. pull diagnostics, or config via `workspace/configuration`) requires teaching
   the reader loop to reply first, or the server stalls.
2. **Defensive reply.** The reader loop already routes (a) responses and (b)
   `publishDiagnostics`. Add a fall-through: a message that carries **both** an `id` and a
   `method` (a server→client *request*) we don't handle gets a `MethodNotFound` (-32601) JSON-RPC
   error response written back, instead of being silently dropped. This keeps a
   capability-mismatched server from blocking on an un-answered request, and is a ~10-line guard
   in the existing loop. (Notifications — `method` but no `id` — other than `publishDiagnostics`
   are still ignored, which is correct.)

Everything else in `lsp_client.rs` (framing, request/response dispatch, the generation-tracked
diagnostics cache, the quiescence debounce) is unchanged and remains language-agnostic.

### 4. Timeouts

`DEFAULT_DIAGNOSTICS_TIMEOUT` (15s) was set for warm rust-analyzer; the Rust integration test
already uses 60s because *first-open* indexing is slow. pyright (venv/typeshed) and gopls
(module load, possibly `go mod download`) commonly exceed 15s on the first `.py`/`.go` file — so
a first diagnostics call would return `{diagnostics: [], timed_out: true}`, which an agent that
ignores `timed_out` misreads as "clean" (Review resolution R3).

Fix: **per-server first-open budget.** `ServerSpec.first_open_diag_timeout` (rust-analyzer 60s,
pyright 60s, gopls 60s, typescript-language-server 30s). `do_diagnostics` uses that budget the
first time it opens a file for a given server key (tracked by a "has this key served diagnostics
before" flag), and the existing `diagnostics_timeout()` (env-overridable via
`OTTO_MCP_LSP_DIAG_TIMEOUT_MS`) for steady-state calls. `timed_out: true` is already in the
structured result; its meaning ("results may be incomplete — a slow first index, not
necessarily clean") is documented on the tool.

### 5. Startup availability gate + one behavior change

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
`--version`), so a generic exec-probe is fragile. A hand-rolled PATH search (no `which` crate)
resolves a bare binary name against `PATH` entries **and checks the executable bit**; when the
value contains a path separator (an override), it checks that file directly, also for the
executable bit — so a present-but-non-executable file fails the gate rather than starting the
server only to fail per-call.

**Conscious behavior change:** a host whose *only* configured server is a broken rustup
`rust-analyzer` shim (present + executable on PATH, but exits nonzero because the component isn't
installed) previously advertised **no** lsp tools (the v1 integration test's exit-status probe
caught this). Now it passes the PATH gate, `mcp-lsp` starts, and the first `.rs` call returns a
clear spawn error via the retry-eligible path instead. This generalizes: any server whose binary
resolves but is non-functional (a `typescript-language-server`/`pyright-langserver` present but a
broken/absent `node`; `gopls` present but no `go` toolchain) passes the gate and fails per-call
with a clear error. Visible per-call error > silent tool-absence — recorded here as a deliberate
trade. The one unstated consequence: PATH-presence can no longer distinguish "installed and
working" from "installed but broken," so were LSP ever surfaced in the `CapabilitiesManifest`, it
would read available-but-nonfunctional; not surfaced today.

### 6. Engine side

No change to `crates/engine/src/mcp.rs::connect_lsp` or to `build_tools_preferring_mcp`'s
additive registration; run and serve reach LSP identically through the shared
`build_composed_tools` → `build_tools_preferring_mcp` path (verified — no LSP-specific
branching). The whole generalization lives inside the `mcp-lsp` crate.

## Data flow (unchanged shape, now routed)

`lsp.diagnostics {path: "app/main.py"}`
→ `do_diagnostics` → `open_if_needed("app/main.py")`
→ lowercase ext `"py"` → `config_for_extension` = (`pyright-langserver`, `"python"`)
→ `get_or_spawn` → (first `.py` file) per-key slot lock → spawn `pyright-langserver --stdio`,
   `initialize(root)`, cache `Ready`
→ `didOpen` with languageId `"python"` on the pyright client
→ `wait_for_diagnostics(uri, generation, first_open_budget)` (the existing quiescence-debounced cache)
→ structured `{diagnostics, timed_out}`.

Positions stay 1-based in/out; out-of-root locations are still skipped, not errors.

## Testing

- **Unit — table:** `config_for_extension` maps each extension to the right (server key,
  languageId) and returns `None` for an unknown/empty extension; `.ts`/`.tsx`/`.js`/`.jsx` all
  resolve to the one `typescript-language-server` key but distinct languageIds; case-folding
  (`.PY`/`.TSX`) resolves.
- **Unit — dispatch:** `seed_ready_for_test` pre-populates the registry with a duplex-backed
  `LspClient` under a chosen server key (no exec), then asserts a `.py` `open_if_needed` sends
  `didOpen` with languageId `"python"` and a `.go` open sends `"go"`. The existing Rust duplex
  tests reseed under the `rust-analyzer` key.
- **Unit — env override:** `resolved_bin` honors the per-server env var when set, else the
  default binary.
- **Unit — PATH resolver:** deterministic tests with a fabricated `PATH` env var + tempdir
  fixtures: bare name resolves to an executable in `PATH`; a non-executable file does not
  resolve; a path-separator override resolves iff the file is executable; absent → not resolved.
- **Unit — per-key liveness:** an `open_if_needed` against a seeded-then-closed duplex client
  surfaces a transport error and evicts the slot (next `get_or_spawn` re-enters the spawn path).
- **Unit — reader-loop defensive reply:** a server→client request (`id` + unknown `method`) over
  the duplex gets a `-32601` error response written back; a `publishDiagnostics` notification is
  still cached; an unknown notification is ignored.
- **Integration (self-skipping):** per-language full round-trips against a real server —
  `gopls`, `pyright-langserver`, `typescript-language-server` — each with **its own** presence
  probe (`gopls version`; `--version` for the others) that prints a skip message and returns when
  the binary is absent, mirroring the existing `rust-analyzer` test. Each asserts diagnostics
  **shape/timing** (a known-broken fixture yields a non-empty diagnostic without `timed_out`), not
  just a navigation round-trip, so the quiescence/timeout behavior is exercised where the
  toolchain exists — not only the happy path.

The offline-determinism suite is untouched: `mcp-lsp` is spawned only by the binary, and no unit
test executes a real language server. **CI process risk (Review resolution R6):** CI images
realistically carry only `rust-analyzer`, so the non-Rust integration tests self-skip there — the
core hypothesis (do pyright/gopls/tsserver return diagnostics under default caps within budget)
is verified only where a developer has the toolchain. Adding at least one non-Rust server to a CI
image is recommended as a follow-up (out of this crate's scope).

## Known limitations / accepted trades

- **`.js`/`.jsx` diagnostics are syntactic-only.** `typescript-language-server` in inferred-project
  mode emits full semantic diagnostics for `.ts`/`.tsx` but syntactic-only for `.js`/`.jsx`
  unless `checkJs`/`// @ts-check` is set — so `lsp.diagnostics` on a plain JS file with type
  errors returns clean. Navigation/hover still work. Documented, not worked around.
- **`tsserver` staged publishes vs the 2s quiescence.** tsserver publishes diagnostics in passes
  (syntactic → semantic → suggestion) as separate `publishDiagnostics`. If the semantic pass lags
  >2s behind syntactic on a large project, the rust-analyzer-tuned `DIAGNOSTICS_QUIESCENCE` (2s)
  can quiesce on the syntactic-only set. The debounce never converts fresh→timeout, so this is a
  completeness limit, not a hang; per-server quiescence tuning is deferred.
- **Single `rootUri` is a scaling hazard for gopls/pyright**, not just a monorepo gap: pointed at
  a large top-level root, `gopls` loads all packages and `pyright` scans the whole tree (memory
  + time, compounding cold-start timeouts). `tsserver` degrades gracefully (it finds the nearest
  `tsconfig.json` per file). Per-sub-project `rootUri` is deferred.

## Non-goals (v2)

- **Per-sub-project rootUri** / multi-root workspaces (see the scaling note above).
- **Server restart/reconnect supervision.** Crashed servers are evicted-and-re-spawned on the
  next call (R4), but there is no background health loop.
- **Incremental `didChange`.** Full-text document sync is retained.
- **Auto-installing missing language servers** (the gate detects presence; it never fetches) and
  **probing language-server runtimes** (`node`/`go`) — a present server with a broken runtime
  fails per-call, per §5.
- **Richer `ClientCapabilities`** (pull diagnostics, config push) — blocked on the reader loop
  answering server→client requests beyond the defensive `MethodNotFound` (§3).
- **Windows PATH resolution** (`PATHEXT`, `.cmd` shims, no executable bit) — the OS sandbox
  targets Linux/macOS; the PATH resolver follows suit.
- **Languages beyond these five** and config-file-driven registration of arbitrary servers.

## Files touched

- `crates/mcp-lsp/src/main.rs` — `ServerSpec` (incl. `first_open_diag_timeout`) +
  `config_for_extension` + `resolved_bin` table; the per-key lazy registry (`ServerState` with
  `Option<Child>`, `slots`, `absent`, `get_or_spawn` with per-key locking + crashed-client
  eviction); `open_if_needed` returns the client and lowercases the extension; `do_*` use it and
  `do_diagnostics` applies the first-open budget; `LspServer::new(root)` + `seed_ready_for_test`;
  the hand-rolled PATH-resolution gate (executable-bit check) in `main()`; all new unit tests +
  per-language integration tests. **Call sites to update:** the `duplex_server` helper and its
  ~10 dependent unit tests (now `LspServer::new(root)` + `seed_ready_for_test`), and the
  `rust_analyzer_integration` test (rewritten to drive `do_diagnostics`/`do_hover` through the
  real `config_for_extension` → `get_or_spawn` dispatch path rather than hand-wiring a client and
  the old `LspServer::new(lsp, root)`).
- `crates/mcp-lsp/src/lsp_client.rs` — `spawn_process` gains `args: &[&str]` (update the
  `spawn_process_with_bogus_binary_errors` call to `&[]`); the reader loop's defensive
  `MethodNotFound` reply to unknown server→client requests; the documented capabilities invariant
  on `initialize`.
- `crates/engine/src/mcp.rs` — a targeted test that a `mcp-lsp`-like child which spawns then
  exits nonzero *before* the MCP handshake surfaces through `connect` as an `Err` (replacing
  inference with verification for the gate's exit path). No production change.
- `CLAUDE.md` — the `mcp-lsp` crate row and the intro paragraph lose "additive and Rust-only in
  v1, multi-language dispatch deferred"; now multi-language (Rust/TS/JS/Python/Go), lazy
  per-language spawn, PATH-gated presence, per-server env overrides + first-open timeouts.
- `docs/ARCHITECTURE.md` — the mcp-lsp line drops "deferred to v2"/"Rust-only v1".
- `docs/superpowers/specs/2026-07-07-mcp-lsp-design.md` — a status note pointing here (its
  "Future generalization" is now built).

## Review resolutions

Consolidated fixes from the three-agent spec review (rust-pro / architect / red-team):

- **R1 — per-key locking (all three, Critical).** The map-wide mutex held across spawn+init
  froze cross-language and warm calls. → §2: brief outer lock to get-or-insert an
  `Arc<Mutex<Option<ServerState>>>` per key; slow spawn+init under only the key's lock.
- **R2 — `Option<Child>` + test seam (rust-pro/red-team, Critical).** `tokio::process::Child` has
  no public constructor, so `Ready(Arc, Child)` was untestable. → §2: `Ready{ client,
  _child: Option<Child> }` + `seed_ready_for_test`.
- **R3 — cold-start diagnostics timeout (red-team, Critical).** 15s default → first-call
  pyright/gopls silently `timed_out` and reads as clean. → §4: per-server `first_open_diag_timeout`.
- **R2b — default-capabilities invariant + dropped server→client requests (red-team, Critical).**
  Undocumented and one capability change from stalling. → §3: documented invariant + defensive
  `MethodNotFound` reply.
- **R4 — Failed/crash handling (red-team/rust-pro, Important).** Permanent-cache only for
  definitely-absent binaries; spawn/init errors stay retry-eligible; crashed `Ready` clients are
  evicted on transport error rather than hanging every call forever.
- **R5 — signatures/call-sites/PATH tests (rust-pro, Important).** `LspServer::new(root)` and
  `seed_ready_for_test` are named; the `duplex_server` helper, its dependents, and the Rust
  integration test are enumerated as rewrites; the PATH resolver gets its own deterministic tests
  and an executable-bit check; the `connect_lsp` pre-handshake-exit path gets a targeted test.
- **R6 — CI coverage + honest limitations (red-team/architect, Important).** `.js` syntactic-only,
  tsserver staged publish vs 2s quiescence, single-rootUri scaling, and the CI-skip risk are
  documented as explicit trades rather than glossed.

## New dependencies

None. No new crates; `spawn_process` args, the defensive reply, and the PATH search are
hand-rolled.
