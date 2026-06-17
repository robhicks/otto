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

`docs/ARCHITECTURE.md` describes the **full intended design**, including crates that do
not exist yet (`mcp-fs`, `mcp-git`, `mcp-grep`, `mcp-bash`, `retrieval`, `extensions`, `cli`,
`ui`, etc.) and the parts of the remote axis that need external infrastructure — a real
`vps`/`microvm` `RemoteTarget` provisioner (`UnsupportedTarget` marks that boundary in-tree),
the client-side handover UX, and a split-out `remote` crate. Treat it as the destination, not
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
| `protocol` | Wire types only (`Command`, `Event`/`EventKind`, `Role`, `SessionId`). No I/O. The crate a future UI shares. |
| `engine-core` | The orchestrator state machine + the trait seams: `Agent`, `WorkspaceRead`/`Workspace`, `Provider`, `Router`, `Tool`/`ToolRegistry`/`PermissionGate`, plus `AgentRegistry` and shared `types`. |
| `agents` | Built-in atomic agents implementing `Agent`: `Planner`, `ContextFinder` (lexical prefilter → LLM rank, with bounded per-turn read budget), `Coder`, `Verifier` (data-driven recipe table; runs the detected ecosystem's test command). All LLM-backed, all with a deterministic offline fallback. |
| `providers` | `Provider` impls: `LocalProvider` (deterministic), `ScriptedProvider` (canned responses keyed by prompt substring — for testing prompt-and-parse agents), `OllamaProvider`, `AnthropicProvider`. (`gemini`/`openai` are intended but not yet built.) |
| `router` | `SingleProviderRouter` (pass-through) and `BrainBlendRouter` (privacy/complexity routing over a local+remote pool with cross-provider fallback). |
| `tools` | `Tool` impls (`FsRead/Write/ListTool`, `BashTool`), the `DefaultPermissionGate`, and the OS sandbox (`SandboxPolicy`, `os_sandbox_available`). In-process today; the MCP-server form (`mcp-fs` etc.) is the destination. |
| `workspace` | `LocalWorkspace` — edits a real on-disk path in place (no clone). Implements the writable `Workspace` (incl. `snapshot()` — a full-content capture of the listed files; plus an inherent `restore` that writes a snapshot back through the gated `apply_edit`); agents see only the read-only `WorkspaceRead` view. Also `RemoteWorkspace` — a `Workspace` over the bearer-authed `POST /workspace` RPC (reqwest client). |
| `persistence` | Durable session store. The `SessionStore` trait + a sqlx-backed `SqliteStore`: persists sessions, their seq-ordered event log, and turn records; `replay_since(Option<u64>)` gives the full log or the gap after a seq; `snapshot`/`restore` capture a session as a serializable `SessionState` and atomically re-create it in a fresh store (the promote-to-remote primitive). A leaf crate depending only on `protocol`. |
| `engine` | Binary `otto` (`run` / `serve`) + wiring library (`build_router`, `build_tool_registry`, `run_goal`). `EngineService` (`create_session`/`run_prompt`/`abort`) holds the store + shared deps and runs a turn by spawning the orchestrator, streaming each event live through an `EventSink` after persisting it (fail-closed), one turn at a time. `serve.rs` is the axum WebSocket transport (bearer auth, `Ready` frame, `last_seq` replay) plus the gated `POST /workspace` RPC; `serve::run` serves plaintext or TLS (`wss://`, via `axum-server` + rustls) from one path. `remote.rs` is the `RemoteTarget` seam + `promote()` + a `LoopbackTarget` (provisions a real second in-process engine) + `UnsupportedTarget` (marks the external-VPS boundary). |

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
