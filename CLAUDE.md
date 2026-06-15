# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

otto is an agentic coding engine: a deterministic orchestrator drives a spine of atomic
agents (Planner → ContextFinder → Coder → Verifier) over a workspace, routing LLM calls
across local and remote providers. The codebase is being built up plan-by-plan and is
currently a **walking skeleton** — the spine, trait seams, providers, router, tools, and
permission gate are real and tested, but several agents are still stubs (`StubPlanner`,
`StubContextFinder`, `EchoCoder`, `StubVerifier`). Real LLM-backed agents are landing in
Plan 4b (current branch).

`docs/ARCHITECTURE.md` describes the **full intended design**, including crates that do
not exist yet (`mcp-fs`, `mcp-git`, `persistence`, `remote`, `extensions`, `cli`, `ui`,
etc.). Treat it as the destination, not the current state. The per-plan specs in
`docs/superpowers/plans/` and the design spec in `docs/superpowers/specs/` record what was
built and why; check the latest plan to see where the skeleton currently stands.

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
| `engine-core` | The orchestrator state machine + the trait seams: `Agent`, `Workspace`, `Provider`, `Router`, `Tool`/`ToolRegistry`/`PermissionGate`, plus `AgentRegistry` and shared `types`. |
| `agents` | Built-in atomic agents implementing `Agent` (currently stubs). |
| `providers` | `Provider` impls: `LocalProvider` (deterministic), `ScriptedProvider` (canned responses keyed by prompt substring — for testing prompt-and-parse agents), `OllamaProvider`, `AnthropicProvider`. |
| `router` | `SingleProviderRouter` (pass-through) and `BrainBlendRouter` (privacy/complexity routing over a local+remote pool with cross-provider fallback). |
| `tools` | `Tool` impls (`FsRead/Write/ListTool`, `BashTool`), the `DefaultPermissionGate`, and the OS sandbox (`SandboxPolicy`, `os_sandbox_available`). |
| `workspace` | `LocalWorkspace` — edits a real on-disk path in place (no clone). |
| `engine` | Binary `otto` + wiring library (`build_router`, `build_tool_registry`, `run_goal`). |

### The orchestrator spine

`Orchestrator::run_turn` (`crates/engine-core/src/orchestrator.rs`) is the deterministic
control flow: **Plan → Execute (ContextFinder → Coder → apply gated edits → Verify, looping to Repair on failure) → Done**. It
owns control flow and event emission only; all capability lives in the agents. Agents
receive an `AgentCtx` granting scoped access to `router()`, `workspace()`, and `tools()` —
add a capability by extending `AgentCtx` (private fields + accessors), never by widening a
struct's public surface. Events are emitted as bare `EventKind`s; the engine layer
(`run_goal`) assigns the monotonic per-session `seq`.

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

- **Trait seams are remote-ready by design.** When adding to a seam (`Agent`, `Workspace`,
  `Provider`, `Router`, `Tool`), keep it `Send + Sync` and async, and preserve the property
  that the orchestrator only ever holds trait objects — never concrete impls.
- **Determinism is a test invariant.** The default offline path must stay reproducible:
  `LocalProvider`/`ScriptedProvider` do no I/O. Anything reading `OTTO_*` / `ANTHROPIC_API_KEY`
  belongs behind `build_router`, not in core logic.
- **Tests live next to code** (`#[cfg(test)] mod tests`). Provider HTTP behavior is tested
  with `wiremock`; workspace/fs with `tempfile`. New agents should be unit-testable against
  a `ScriptedProvider` and the orchestrator against mocked agents (see
  `orchestrator.rs` tests for the fake-agent pattern).
- Extensible payloads (tool args/results) are JSON `Value`; core message shapes are typed
  records. Adding a tool/agent/event should stay a semver-minor change to the wire types.
