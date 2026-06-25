# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

otto is an agentic coding engine: a deterministic orchestrator drives a spine of atomic
agents (Planner → ContextFinder → Coder → Verifier) over a workspace, routing LLM calls
across local and remote providers. The codebase is being built up plan-by-plan. The
**full single-machine spine is real and tested**: all four agents are LLM-backed (no stubs
remain), the Coder's edits are gated, and the Verifier runs the project's test command in a
sandbox and drives the Repair loop. Agents fall back to a deterministic offline path when no
model answers, so the default test suite needs no network or keys.

The **distribution axis works end to end (on loopback)**: a `persistence` crate persists
sessions and their seq-ordered event log to sqlite; the engine runs turns through an
`EngineService` that streams and records each turn; `otto serve` exposes the `Command`/`Event`
protocol over a bearer-authed WebSocket — plaintext `ws://`, or `wss://` with
`--tls-cert`/`--tls-key` — with `Last-Event-ID` reconnect (replay from the store); a
`RemoteWorkspace` proxies the `Workspace` seam over a gated `POST /workspace` RPC; and
`promote()` + `LoopbackTarget` move a whole session (session snapshot/restore +
`Workspace::snapshot`/`restore`) onto a freshly-provisioned in-process engine that the client
reconnects to. The same protocol runs embedded (CLI) or served.

The **MCP tool server tier is complete** (only `mcp-lsp` is v2): `mcp-fs` (path-contained
`fs.read`/`fs.write`/`fs.list`), `mcp-grep` (ripgrep-style search via the grep/ignore crates),
`mcp-git` (full git/gh: status/diff/log/add/commit/branch/checkout/clone/push/pr_open), and
`mcp-bash` (the sandboxed shell) are all real rmcp stdio servers. The engine's MCP client adapter
spawns each and registers its tools behind the gate; the binary prefers the servers with the
in-process `fs.*`/`bash` tools as fallbacks (`mcp-grep`/`mcp-git` are additive). `mcp-git` is
hardened against agent-input argv injection; `mcp-bash` reuses the shared `run_sandboxed` core and
hardcodes the OS sandbox (the gate + sandbox-only registration are preserved across the move).

The **UI has its first slice** (sub-project A, "app shell + live session"): `ui/` is a
browser-first **Leptos CSR** app (Rust→WASM, built with `trunk`) that connects to `otto serve`
over WebSocket, sends a prompt, renders the live `Event` stream, aborts, and reconnects with
`last_seq` replay. It is a **standalone crate, deliberately excluded from the cargo workspace**
(`exclude = ["ui"]` in the root `Cargo.toml`), depends **only** on `protocol` (compiled to
WASM), and is built/tested from inside `ui/` (`cargo test`, `cargo build --target
wasm32-unknown-unknown`) — so `cargo build --workspace` and the offline determinism suite are
untouched. Enabling it took two additive changes: the WS framing enum `ServerMessage` now lives
in `protocol` (so the UI can deserialize it), and `/ws` accepts the bearer token via a `?token=`
query param (the header path is still preferred). **Sub-project B then shipped** (capabilities +
status strip): the `Ready` frame now carries a `CapabilitiesManifest` (extended with a `remote_llm`
field), derived at serve time by `build_capabilities()`, and the UI replaces its plain status line
with a strip showing engine/LLM/sandbox state — degraded states (offline-deterministic LLM, absent
sandbox) render visibly. **Sub-project C then shipped** (workspace tree + editor): the UI now lists
the served workspace via the bearer-authed `POST /workspace` RPC — unblocked by a new **tower-http
CORS layer** on the engine (the one engine change; not a protocol change) — renders a collapsible
file tree, and opens files into a **`kode-leptos`** editor (native Leptos CSR, syntax-highlighted)
with a local, unsaved buffer; persistence stays deferred to sub-project D. `kode-leptos`/`gloo-net`
are UI-only deps and the `ui/` crate still depends only on `protocol`. **Sub-project D then shipped**
(diff approval): the opt-in `otto serve --approve-edits` flag wires an `ApprovalModeGate` that
upgrades ordinary (non-sensitive) `fs.write` from Allow to `Ask`, and the orchestrator's per-edit
`Ask` branch emits an `ApprovalRequest{id,path,old,new}` event, awaits an async `Approver`, and
applies the edit only on an explicit `ApproveDiff` command (fail-closed on reject/disconnect; the
sensitive floor still Denies first; the headless `DenyApprover` default denies). `serve.rs` now reads
the socket concurrently with the running turn (`split` + `select!`), routing `ApproveDiff` frames
through a per-connection `ApprovalRegistry`/`InteractiveApprover`, and the UI renders the diff with
Approve/Reject buttons. The roadmap and per-slice
spec/plan live in `docs/superpowers/specs/2026-06-17-ui-roadmap.md`; sub-projects E–F are pending.

`docs/ARCHITECTURE.md` describes the **full intended design**, including crates that do
not exist yet (`mcp-lsp`, `retrieval`, `extensions`, `cli`, etc.), the rest of the UI
(sub-projects C–F: workspace tree/editor, diff approval, token meter,
promote-to-remote, and the Tauri desktop wrapper), and the parts of the remote axis that need
external infrastructure. The **`vps` `RemoteTarget` is shipped**, both directions against a running
receiver: `VpsTarget` promotes a session onto an already-running `otto serve --accept-promotions` via
a gated `POST /promote` restore RPC, and **demote-from-remote** is shipped too — a client on the
source serve issues `DemoteToLocal`; the source pulls the session back via a gated `POST /export` on
the receiver and restores it locally with `accept_demotion` (overwriting its own copy via
`SessionStore::restore_over`, sensitive-floor first; the export is gate-filtered so secrets never
leave the receiver). So machine provisioning (SSH / cloud-SDK / hypervisor VM creation) now lives
behind a **`Provisioner` seam**: the `microvm` axis is **shipped** — `MicrovmTarget` composes any
`Provisioner` with the shared restore-push, `FirecrackerProvisioner` (behind the default-off
`firecracker` feature) boots an ephemeral per-session microVM, and `UnsupportedProvisioner` (which
replaced the old unsupported-target stub) is the single machine-provisioning boundary. **demote-from-microvm
is shipped**: a client on a `--promote-microvm` source issues `DemoteToLocal`; the source pulls the
session's current bundle off the running microVM via the shared `export_bundle` (`POST /export`), restores
it locally with `accept_demotion` (overwriting its own copy via `restore_over`, sensitive-floor first), and
disposes the VM by dropping the live handle. The `remote` crate split is **shipped** (the seam + `VpsTarget`/`MicrovmTarget` +
the `Provisioner`/`UnsupportedProvisioner` seam live in `remote`; `LoopbackTarget` stays in `engine`).
Treat it as the destination, not
the current state. The per-plan specs in `docs/superpowers/plans/` and the design spec in
`docs/superpowers/specs/` record what was built and why; check the latest plan to see where
the build currently stands.

## Commands

```bash
cargo build --workspace            # build everything
cargo test --workspace             # run all tests (fully offline & deterministic by default)
cargo test -p otto-engine-core     # test one crate
cargo test -p otto-tools bash::    # run tests matching a path/name filter
cargo fmt --all                    # format (rustfmt is pinned in rust-toolchain.toml)
cargo clippy --workspace --all-targets   # lint

# Run the engine end-to-end (single turn):
cargo run -p otto-engine -- run "<goal>" [--root <path>]

# Serve the engine over WebSocket (for the UI / remote clients); token is mandatory:
OTTO_TOKEN=<token> cargo run -p otto-engine -- serve [--port <p>] [--root <path>]

# The browser UI (standalone, NOT part of the workspace — run from inside ui/):
cd ui && cargo test                              # pure host-side unit tests
cd ui && cargo build --target wasm32-unknown-unknown   # wasm compile check
cd ui && trunk serve                             # dev server in a browser tab (needs `cargo install trunk`)
```

Toolchain is pinned to stable in `rust-toolchain.toml` (edition 2024, rust-version 1.85).

### Runtime configuration (env vars)

`build_router()` (`crates/engine/src/lib.rs`) selects providers from the environment. With
**no env vars set, the engine runs fully offline and deterministically** (both router slots
use `LocalProvider`) — this is what CI and first-run use, and why the test suite needs no
network or API keys.

- `OTTO_OLLAMA=1` — use `OllamaProvider` for the local slot (model from `OTTO_OLLAMA_MODEL`, default `llama3.2`).
- `ANTHROPIC_API_KEY` — use `AnthropicProvider` for the remote slot (model from `OTTO_ANTHROPIC_MODEL`, default `claude-haiku-4-5`); this enables the `BrainBlendRouter`. Absent, a `SingleProviderRouter` over the local slot is used.

## Architecture

A single Cargo workspace. **Dependencies flow strictly inward**: `protocol` depends on
nothing but serde; `engine-core` defines the trait seams; the impl crates depend on
`engine-core`; `engine` wires them together. `engine-core` must never depend on a concrete
impl crate.

| Crate | Role |
|---|---|
| `protocol` | Wire types only (`Command`, `Event`/`EventKind`, `Role`, `SessionId`, plus the WS framing enum `ServerMessage` and `CapabilitiesManifest`). No I/O. The crate the `ui/` build shares (compiled to WASM). |
| `engine-core` | The orchestrator state machine + the trait seams: `Agent`, `WorkspaceRead`/`Workspace`, `Provider`, `Router`, `Tool`/`ToolRegistry`/`PermissionGate`, plus `AgentRegistry` and shared `types`. |
| `agents` | Built-in atomic agents implementing `Agent`: `Planner`, `ContextFinder` (lexical prefilter → LLM rank, with bounded per-turn read budget), `Coder`, `Verifier` (data-driven recipe table; runs the detected ecosystem's test command). All LLM-backed, all with a deterministic offline fallback. |
| `providers` | `Provider` impls: `LocalProvider` (deterministic), `ScriptedProvider` (canned responses keyed by prompt substring — for testing prompt-and-parse agents), `OllamaProvider`, `AnthropicProvider`. (`gemini`/`openai` are intended but not yet built.) |
| `router` | `SingleProviderRouter` (pass-through) and `BrainBlendRouter` (privacy/complexity routing over a local+remote pool with cross-provider fallback). |
| `tools` | In-process `Tool` impls (`FsRead/Write/ListTool`, `BashTool`), the `DefaultPermissionGate`, and the OS sandbox (`SandboxPolicy`, `os_sandbox_available`, and the shared `run_sandboxed` core used by both `BashTool` and `mcp-bash`). The fs + bash tools are now the in-process *fallbacks* for `mcp-fs`/`mcp-bash`. |
| `mcp-fs` | Standalone rmcp stdio server binary (`mcp-fs <root>`): path-contained `fs.read`/`fs.write`/`fs.list` over a `LocalWorkspace`. The engine spawns it via its MCP client (`crates/engine/src/mcp.rs`) and registers its tools behind the gate — never linked. |
| `mcp-grep` | rmcp stdio server (`mcp-grep <root>`): a `grep` tool (regex search via the grep/ignore crates), rooted, hidden-skip + sensitive-marker-skip (never returns secret contents), capped results. |
| `mcp-git` | rmcp stdio server (`mcp-git <root>`): full git/gh ops by shelling to `git`/`gh`. Hardened against agent-input argv injection (leading-dash reject, clone URL-scheme allowlist + `clone -- …`); `git.add` enumerates the actually-staged set and refuses sensitive files. |
| `mcp-bash` | rmcp stdio server (`mcp-bash <root>`): a `bash` tool that runs the command via the shared `run_sandboxed` core with `SandboxPolicy::Os` hardcoded (no unsandboxed path; fails closed without a backend). Registered (behind the `bash→Ask` gate + allow-list) only when `os_sandbox_available()`. |
| `workspace` | `LocalWorkspace` — edits a real on-disk path in place (no clone). Implements the writable `Workspace` (incl. `snapshot()` — a full-content capture of the listed files; plus an inherent `restore` that writes a snapshot back through the gated `apply_edit`); agents see only the read-only `WorkspaceRead` view. Also `RemoteWorkspace` — a `Workspace` over the bearer-authed `POST /workspace` RPC (reqwest client). |
| `persistence` | Durable session store. The `SessionStore` trait + a sqlx-backed `SqliteStore`: persists sessions, their seq-ordered event log, and turn records; `replay_since(Option<u64>)` gives the full log or the gap after a seq; `snapshot`/`restore` capture a session as a serializable `SessionState` and atomically re-create it in a fresh store (the promote-to-remote primitive). A leaf crate depending only on `protocol`. |
| `retrieval` | Persistent inverted index behind the `Retriever` seam (defined in `engine-core`). `IndexedRetriever` keeps a standalone sqlite index (token→file postings) in the OS cache dir keyed by workspace root, refreshed stat-incrementally (mtime+size) and atomically per file, and scores content for every indexed file — removing the ContextFinder's per-turn read budget. Tree-sitter symbol chunking (Rust/JS/TS/Python/Go) adds a symbol-name *definition* boost and surfaces matched symbol names per candidate; unsupported languages fall back to whole-file indexing. A bounded per-search git-history recency boost (a `git log`-derived commit-rank tier, added only to files that already match) re-ranks recently-changed candidates upward — a precision re-ranker, never a recall source (it can't surface an unmatched or sensitive file). Mirrors the gate's sensitive-path floor (secrets never indexed; the walk is the sole defense since the index reads files directly). Depends inward on `engine-core`. |
| `extensions` | Loads otto's native extension format (Claude Code's `.claude/`). Slice 1: discovers `agents/*.md` from `~/.claude/` + the project `.claude/` (project wins on name collision), parses Claude-Code-compatible frontmatter into `CustomAgentDef`, and provides a `MarkdownAgent` (`Agent` that runs its body as a system prompt) + a `TaskTool` (`Tool` that dispatches a named custom agent as a depth-1, allowlist-filtered sub-turn via `ToolRegistry::subset`). An agent's `tools` allowlist can only **narrow** the shared gate (sensitive-path floor still inviolable); an **omitted** `tools` field means "all tools" (Claude-Code-compatible). Wired into `otto run --agent <name>`. Depends inward on `engine-core`/`protocol`; invoked only by the binary, so the offline determinism suite is untouched. |
| `remote` | The engine-axis handover seam: `RemoteTarget` (trait) + `RemoteHandle` + `PromoteBundle` + `promote()`, the network-facing `VpsTarget` (promote via `POST /promote`; `export` pulls a bundle back for demote via the shared `export_bundle` `/export` pull, also used by microVM demote), the `Provisioner` seam — `MicrovmTarget` (composes a `Provisioner` with the restore-push), `FirecrackerProvisioner` (behind the default-off `firecracker` feature; boots an ephemeral microVM) and `UnsupportedProvisioner` (the single machine-provisioning boundary) — and the `PromoteConfig`/`PromoteMode` handover config. Dependencies flow strictly inward (`protocol`, `engine-core`, `persistence`). `LoopbackTarget` is **not** here — it boots an in-process engine, so it stays in `engine`. |
| `engine` | Binary `otto` (`run` / `serve`) + wiring library (`build_router`, `build_tool_registry`, `run_goal`). `EngineService` (`create_session`/`run_prompt`/`abort`) holds the store + shared deps and runs a turn by spawning the orchestrator, streaming each event live through an `EventSink` after persisting it (fail-closed), one turn at a time. `serve.rs` is the axum WebSocket transport (bearer auth, `Ready` frame, `last_seq` replay) plus the gated `POST /workspace` RPC, the gated `POST /promote` restore RPC and the gated `POST /export` restore-export RPC (both enabled by `--accept-promotions`; `EngineService::accept_promotion` restores a `PromoteBundle`'s workspace through the permission floor, `export_promotion` builds a gate-filtered bundle for `/export`, and `accept_demotion` restores a pulled bundle, overwriting the source's own copy via `SessionStore::restore_over`); `serve::run` serves plaintext or TLS (`wss://`, via `axum-server` + rustls) from one path. The `RemoteTarget` seam, `promote()`, `VpsTarget`/`MicrovmTarget`, the `Provisioner` seam (`FirecrackerProvisioner`/`UnsupportedProvisioner`), and `PromoteConfig`/`PromoteMode` now live in the `remote` crate; `engine` keeps `loopback.rs` — the `LoopbackTarget` (provisions a real second in-process engine), which implements `otto_remote::RemoteTarget` and is re-exported from `otto_engine` alongside the rest of the seam. `mcp.rs` is the rmcp MCP client adapter (`connect`/`connect_fs`/`connect_grep`/`connect_git`/`connect_bash` spawn an MCP stdio server and wrap its tools as gated `Tool`s); `build_tools_preferring_mcp` registers fs/grep/git/bash over the in-process tools (bash only when a sandbox backend exists). |

### The orchestrator spine

`Orchestrator::run_turn` (`crates/engine-core/src/orchestrator.rs`) is the deterministic
control flow: **Plan → Execute (ContextFinder → Coder → apply gated edits → Verify, looping to Repair on failure) → Done**. It
owns control flow and event emission only; all capability lives in the agents. Agents
receive an `AgentCtx` granting scoped access to `router()`, `workspace()` (the read-only
`WorkspaceRead` view — agents never get the writable `Workspace`), and `tools()` — add a
capability by extending `AgentCtx` (private fields + accessors), never by widening a
struct's public surface. The only path to disk writes is the gated `fs.write` tool. Events
are emitted as bare `EventKind`s; the engine layer (`EngineService::run_prompt`, which
`run_goal` wraps) assigns the monotonic per-session `seq` (sourced from the store cursor so it
continues across turns/reconnects) and persists each event (fail-closed) before streaming it.

The ContextFinder sources candidates from the indexed `Retriever` when one is wired (the
engine builds an `IndexedRetriever` for `run`/`serve`) and falls back to its inline lexical
pipeline — the deterministic offline path — when absent. The retriever now boosts files that
define a symbol named after a goal term and lists matched symbol names in the select prompt
(tree-sitter chunking over Rust/JS/TS/Python/Go).

### Permission gate (security spine — get this right)

Every tool call routes through `ToolRegistry` → `PermissionGate` before dispatch.
`DefaultPermissionGate` enforces an **inviolable, case-insensitive sensitive-path floor**
(`.env*`, `.ssh/`, `.git/`, `.aws/`, ssh keys) that always denies. An `Ask` verdict is
resolved by an `AskResolver`: `DenyAsk` (headless default, fail-closed) or
`AllowListAskResolver`.

Two rules that are easy to break:

1. **Coder edits are gated, fail-closed.** Before applying any edit, the orchestrator calls
   `tools.check("fs.write", {path})` and applies the edit **only on an explicit `Allow`** —
   a `Deny` *or* an `Ask` is logged and skipped. Do not relax this to "apply unless denied".
2. **`bash` is registered only when an OS sandbox backend exists.** The gate classifies
   `bash` as `Ask` (a shell can't be statically path-vetted). `build_tool_registry` registers
   `BashTool` *only* when `os_sandbox_available()` is true, pairing it with an
   `AllowListAskResolver` permitting the now-confined `bash`. With no backend, `bash` is
   absent and `Ask` stays denied. Never wire `SandboxPolicy::None`; never register `bash`
   unconditionally.

The sandbox (`crates/tools/src/sandbox.rs`) uses `bwrap` on Linux / `sandbox-exec` on macOS:
filesystem read-only except the workspace root, network/pid/ipc isolated, minimal env
(`PATH`/`HOME`/`TERM` only), process tree killed on timeout via the pid namespace.

## Conventions

- **Trait seams are remote-ready by design.** When adding to a seam (`Agent`,
  `WorkspaceRead`/`Workspace`, `Provider`, `Router`, `Tool`), keep it `Send + Sync` and
  async, and preserve the property that the orchestrator only ever holds trait objects —
  never concrete impls.
- **Determinism is a test invariant.** The default offline path must stay reproducible:
  `LocalProvider`/`ScriptedProvider` do no I/O. Anything reading `OTTO_*` / `ANTHROPIC_API_KEY`
  belongs behind `build_router`, not in core logic.
- **Tests live next to code** (`#[cfg(test)] mod tests`). Provider HTTP behavior is tested
  with `wiremock`; workspace/fs with `tempfile`. New agents should be unit-testable against
  a `ScriptedProvider` and the orchestrator against mocked agents (see
  `orchestrator.rs` tests for the fake-agent pattern).
- Extensible payloads (tool args/results) are JSON `Value`; core message shapes are typed
  records. Adding a tool/agent/event should stay a semver-minor change to the wire types.
