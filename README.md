# otto

An agentic coding engine. A deterministic orchestrator drives a spine of small, atomic
agents — **Planner → ContextFinder → Coder → Verifier** — over a workspace, routing LLM
calls across local and remote providers behind a stable set of trait seams. The frontend
(future) never branches on "local vs remote": it speaks one protocol to an engine that may
be embedded in-process or served over the network.

> **Status: the single-machine spine is real.** The orchestrator, all four agents
> (Planner, ContextFinder, Coder, Verifier), trait seams, providers, router, tools, sandbox,
> and permission gate are implemented and tested — no stubs remain. The agents are LLM-backed
> with a deterministic offline fallback, so the test suite runs without keys or network. Still
> ahead: the remote/distribution axis (`serve` mode, `RemoteWorkspace`), MCP tool servers,
> retrieval, persistence, extensions, and the UI. The full intended design lives in
> [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); per-plan history is in `docs/superpowers/plans/`.

## Quick start

Requires the pinned stable Rust toolchain (`rust-toolchain.toml`; edition 2024, rust 1.85).

```bash
cargo build --workspace
cargo test --workspace          # offline & deterministic — no API keys or network needed
```

Run a single turn against a directory:

```bash
cargo run -p otto-engine -- run "add a hello function" --root /path/to/repo
```

This prints the sequenced event stream (`AgentStarted`, `FileEdit`, `VerifyResult`,
`TurnComplete`, …) and exits non-zero if the turn fails.

With no environment configured, the engine runs **fully offline and deterministically** —
both router slots use the in-process `LocalProvider`. To use real models:

| Variable | Effect |
|---|---|
| `OTTO_OLLAMA=1` | Local slot uses Ollama (`OTTO_OLLAMA_MODEL`, default `llama3.2`). |
| `ANTHROPIC_API_KEY=…` | Remote slot uses Anthropic (`OTTO_ANTHROPIC_MODEL`, default `claude-haiku-4-5`); enables the Brain-Blend router. |

## How it works

The engine is one Cargo workspace whose dependencies flow strictly **inward**: `protocol`
depends on nothing, `engine-core` defines the trait seams, the impl crates depend on
`engine-core`, and `engine` wires everything together.

```
protocol        wire types (Command/Event/Role/SessionId) — no I/O
engine-core     orchestrator state machine + the trait seams + AgentRegistry
  ├─ agents     Agent impls (Planner / ContextFinder / Coder / Verifier) — all LLM-backed
  ├─ providers  Provider impls: Local, Scripted, Ollama, Anthropic
  ├─ router     SingleProviderRouter, BrainBlendRouter (privacy/complexity routing)
  ├─ tools      fs.* + bash tools, permission gate, OS sandbox
  └─ workspace  LocalWorkspace (edits a real folder in place; agents see a read-only view)
engine          the `otto` binary + wiring (build_router, build_tool_registry, run_goal)
```

**A turn** (`Orchestrator::run_turn`) is deterministic control flow: Plan → Execute (find
context → code → apply gated edits → Verify, looping to Repair on failure) → Done. The
Verifier detects the project's ecosystem (Rust / Go / Node / Python / Make) and runs its
test command in the sandbox. Agents hold no global state; they receive an `AgentCtx`
granting scoped access to the router, a **read-only** workspace view, and the tool registry.

**Every tool call and every Coder edit passes a permission gate** before it touches disk.
A sensitive-path floor (`.env*`, `.ssh/`, `.git/`, `.aws/`, ssh keys) is always denied, and
edits apply only on an explicit `Allow` (an `Ask` or `Deny` is skipped — fail-closed). The
`bash` tool is registered only when an OS sandbox backend (`bwrap`/`sandbox-exec`) is
present, and runs network-isolated with the workspace root as the only writable path.

The trait seams — `Agent`, `WorkspaceRead`/`Workspace`, `Provider`/`Router`, `Tool` — are
the extension points. They are `Send + Sync` async traits, and the orchestrator only ever
holds trait objects, so new agents, providers, routing policies, or tools slot in without
touching the spine.

## Developing

```bash
cargo test -p otto-engine-core            # one crate
cargo test -p otto-tools bash::           # filter by test path/name
cargo fmt --all
cargo clippy --workspace --all-targets
```

Tests live alongside the code (`#[cfg(test)] mod tests`). Patterns to follow:

- **Agents** are unit-tested against a `ScriptedProvider` (canned responses keyed by a
  prompt substring) — no network, fully reproducible.
- **The orchestrator** is tested with mocked agents (see the fake-agent structs in
  `crates/engine-core/src/orchestrator.rs`).
- **Provider HTTP behavior** is tested with `wiremock`; **workspace/fs** with `tempfile`.

Keep the offline default deterministic: `LocalProvider`/`ScriptedProvider` perform no I/O,
and anything reading environment variables belongs behind `build_router`, not in core logic.

See [`CLAUDE.md`](CLAUDE.md) for the working conventions and the security rules that are
easy to get wrong, and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design.

## License

MIT OR Apache-2.0.
