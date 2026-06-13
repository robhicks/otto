# otto — Architecture

Structural reference for the otto codebase. For *why* these choices were made, see
`docs/superpowers/specs/2026-06-13-otto-design.md`. This document describes *what* the
pieces are, how they depend on each other, and how data flows between them.

## Guiding principle

One engine, one protocol, four decoupled axes (UI · Engine · Workspace · LLM). The
frontend never branches on "local vs remote" — it speaks one protocol to an engine that
may be embedded in-process or served over the network. Everything below exists to keep
that boundary stable while letting each axis vary independently.

## Crate layout

A single Cargo workspace (edition 2024). Dependencies flow strictly **inward**: leaf type
crates depend on nothing; the orchestrator depends on agents/tools/providers; the
frontend and CLI depend on the protocol only.

```
otto-next/
├── crates/
│   ├── protocol         # Wire types: Command/Event enums, SessionState, capabilities manifest. No I/O.
│   ├── engine-core      # Orchestrator state machine, Agent/Workspace/Provider/RemoteTarget traits, registry.
│   ├── agents           # Built-in atomic agents: planner, context-finder, coder, verifier.
│   ├── providers        # LLM provider libs (in-process): anthropic, gemini, openai, local(ollama).
│   ├── router           # SingleProviderRouter + BrainBlendRouter (privacy/complexity routing) over the Provider pool.
│   ├── tools            # In-process Tool impls (fs.read/write/list) + DefaultPermissionGate (sensitive-path floor).
│   ├── mcp-fs           # MCP stdio server: file read/write/search, path-contained.
│   ├── mcp-git          # MCP stdio server: clone/branch/stage/commit/push/PR.
│   ├── mcp-grep         # MCP stdio server: ripgrep-style search.
│   ├── mcp-bash         # MCP stdio server: shell exec with per-call network + OS sandbox policy.
│   ├── mcp-lsp          # MCP stdio server: LSP bridge (deferred to v2).
│   ├── retrieval        # Tree-sitter chunking, git-history, grep selection (vector index = v2).
│   ├── workspace        # LocalWorkspace + RemoteWorkspace impls of the Workspace trait.
│   ├── persistence      # Session store: sqlite (local) / postgres (remote, optional).
│   ├── remote           # RemoteTarget impls: vps (v1-ready), microvm (v2).
│   ├── extensions       # Loads .claude/ agents, commands, skills, hooks, permissions, plugins.
│   ├── engine           # Binary + library: wires the above; `embedded` and `serve` modes.
│   └── cli              # `otto` binary: `otto engine serve`, headless one-shot runs.
└── ui/                  # Tauri 2 + Leptos frontend (separate build).
```

### Dependency rules

- `protocol` depends on nothing but serde. It is the only crate shared between `engine`
  and `ui`.
- `engine-core` defines the traits; `agents`, `providers`, `workspace`, `remote`
  implement them. `engine-core` must not depend on concrete impls.
- MCP tool crates are standalone binaries; the engine talks to them over stdio JSON-RPC,
  never by linking.
- `ui` depends only on `protocol` (compiled to WASM). It must never link `engine-core`.

## Key trait interfaces

These four traits are the seams that keep the four axes decoupled. They are defined
remote-ready in v1 even where only the local impl ships.

### `Agent` — the atomic-agent seam

```rust
#[async_trait]
trait Agent {
    fn role(&self) -> Role;                       // Planner | ContextFinder | Coder | Verifier | Custom(name)
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> Result<AgentOutput>;
}
```

`AgentCtx` grants scoped access to the LLM router and the MCP tool registry. Native impls
live in `agents`; a `wasm32-wasip2` impl can register against the same trait later. The
orchestrator only knows roles and endpoints — it never knows whether an agent is native,
wasm, or a user-defined markdown SubHost.

### `Workspace` — the workspace-axis seam

```rust
#[async_trait]
trait Workspace {
    async fn read(&self, path: &Path) -> Result<Bytes>;
    async fn apply_patch(&self, diff: &UnifiedDiff) -> Result<()>;
    async fn list(&self, glob: &str) -> Result<Vec<PathBuf>>;
    async fn snapshot(&self) -> Result<WorkspaceSnapshot>;   // uncommitted diffs, for handover
}
```

`LocalWorkspace` edits a real on-disk path in place (no clone). `RemoteWorkspace` operates
on a checkout living on the remote engine. The orchestrator only ever holds
`Box<dyn Workspace>`.

### `Provider` — the LLM-axis seam

```rust
#[async_trait]
trait Provider {
    fn id(&self) -> ProviderId;                   // anthropic | gemini | openai | local
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteStream>;
}
```

In-process by default; an optional HTTP shim wraps the same impl for out-of-process
debugging. The Brain-Blend router holds a pool of providers and selects one per task.

### `Router` — the agent-facing completion seam

Agents call a `Router` (via `AgentCtx::router()`), not a single `Provider`. `engine-core`
owns the trait: `async fn complete(&self, CompleteRequest, RouteHints) -> Result<CompleteResponse>`.
`otto-router` provides `SingleProviderRouter` (pass-through) and `BrainBlendRouter`
(privacy-forced-local + complexity-scored selection over a local/remote provider pool, with
cross-provider fallback that never crosses the privacy boundary). This keeps Brain-Blend behind
a stable seam: adding providers or changing routing never touches `engine-core` or the agents.

### `Tool` — the tool-call seam (+ the permission gate)

Agents call tools via `AgentCtx::tools()` → `ToolRegistry::call(name, json_args)`. Tools are
MCP-shaped (`fn name()`, `async fn call(Value) -> Result<Value>`), so an MCP-stdio tool
(rmcp client to external/Claude Code servers) registers behind the same `Tool` trait later.
Every call passes a deterministic `PermissionGate` (otto's guardrail) before dispatch:
`DefaultPermissionGate` denies sensitive paths (`.env`, `.ssh/`, `.git/`, `.aws/`, ssh keys) as
an inviolable, case-insensitive floor. An `Ask` verdict is resolved by an `AskResolver` —
`DenyAsk` in headless mode; the interactive UI supplies a prompting resolver later.

### `RemoteTarget` — the engine-axis seam

```rust
#[async_trait]
trait RemoteTarget {
    async fn provision(&self, state: &SessionState) -> Result<RemoteHandle>;  // returns WSS endpoint
    async fn teardown(&self, handle: RemoteHandle) -> Result<()>;
}
```

`vps` impl (long-lived server) is v1-ready; `microvm` impl (ephemeral, per-session) is v2.

## Protocol message catalog

`protocol` defines two enums plus the manifest and session-state types.

| Direction | Type | Variants (representative) |
|---|---|---|
| UI → Engine | `Command` | `CreateSession`, `SendPrompt`, `ApproveDiff`, `Pause`, `Resume`, `Abort`, `OpenFile`, `EditFile`, `ListWorkspace`, `PromoteToRemote`, `DemoteToLocal` |
| Engine → UI | `Event` | `AgentState`, `TokenCostMeter`, `FileEdit`, `Diff`, `ToolCall`, `VerifyResult`, `ApprovalRequest`, `Log` |

- Every `Event` carries a monotonic `seq: u64`. Reconnecting clients send the last seq as
  `Last-Event-ID`; the engine replays the gap from its event store.
- Core message shapes are stable typed records. Extensible payloads (MCP tool args/results,
  custom-agent I/O, event bodies) are JSON-encoded strings — adding a tool/agent/event is a
  semver-minor change, not a breaking wire change.
- Transport is abstracted: local uses localhost/IPC + an in-memory bus backed by sqlite;
  remote uses WSS + (optionally) Postgres `pg_notify`. The `Command`/`Event` types are
  identical across both.

## Deployment topologies

The same `engine` code runs in three shapes, selected at startup:

```
EMBEDDED (v1 default)
  ┌────────── Tauri app ──────────┐
  │  Leptos UI ──IPC/localhost──► engine (in-process / sidecar) ──► LocalWorkspace
  └────────────────────────────────┘                          └──► provider pool (local + remote)

SERVED (v2)
  Tauri/mobile UI ──WSS──► `otto engine serve` (VPS or microVM) ──► RemoteWorkspace
                                                                └──► provider pool

PROMOTED (v2 handover)
  UI ──IPC──► local engine ──snapshot SessionState──► RemoteTarget.provision()
  UI ──drops local, reconnects WSS──► remote engine (resumes via Last-Event-ID)
```

## Core data flows

### A single turn (plan → execute → verify → repair)

```
UI: SendPrompt
  └─► Orchestrator [Plan]
        └─► Planner agent ──► milestones
      Orchestrator [Execute]
        ├─► ContextFinder agent ──► minimal file set (AST + git + grep via retrieval)
        ├─► Brain-Blend router picks provider (local vs remote) per task
        ├─► Coder agent ──► unified-diff patch
        │     └─► Guardrail gate validates every tool call before it runs
        └─► Workspace.apply_patch()  ──► Event::FileEdit / Event::Diff to UI
      Orchestrator [Verify]
        └─► Verifier agent ──► run lint/type/test in Podman (or host fallback)
              ├─ pass ──► [Done] ──► Event::VerifyResult(ok)
              └─ fail ──► [Repair] feed failure to Coder, bounded retry w/ backoff
                            └─ repeated identical error hash ──► deadlock, surface to UI
```

### Promote-to-remote

```
UI: PromoteToRemote
  └─► engine: Workspace.snapshot() + serialize SessionState (state-machine pos,
              context, history, uncommitted diffs, llm/agent config)
        └─► RemoteTarget.provision(state) ──► remote engine boots, reconstitutes
              RemoteWorkspace from patch bundle / scratch branch
        └─► returns WSS endpoint ──► UI reconnects, replays events from Last-Event-ID
```

## Claude Code compatibility

otto's native extension format is Claude Code's `.claude/` convention. The `extensions`
crate discovers `.claude/` (project) and `~/.claude/` (user-global) at session start and
registers each artifact into an existing otto primitive — there is no parallel otto-only
format:

- `agents/*.md` → `AgentRegistry` as `Role::Custom(name)` (honors per-agent tool allowlist + model).
- `commands/*.md` → command registry (prompt templates for the palette).
- `skills` (`SKILL.md` + resources) → loadable skills exposed via a built-in `Skill` tool.
- `settings.json` hooks → `HookRegistry`, fired at the same lifecycle points.
- `settings.json` permissions → composed into the Layer-2 permission gate.
- plugins (`.claude-plugin/plugin.json`) → manifest parsed; each bundled component registered
  via the rows above; **bundled MCP servers route straight into otto's MCP client unmodified.**

`extensions` depends on `engine-core` (registry/traits), the MCP client, and the permission
gate. It therefore lands as a dedicated plan after those seams exist. The `Agent` trait being
the single registration seam (Plan 1) is what makes this additive rather than invasive.

## Editor interop (frontend)

CodeMirror 6 (JS) is mounted into a Leptos-owned DOM node, not reimplemented in Rust. A
bundled JS glue ES-module exposes `mountEditor(element, opts)`, `getDoc()`, `setDoc(text)`,
and an `onChange` callback; Rust imports it via `#[wasm_bindgen(module = "...")]`. A Leptos
component owns the mount element through a `node_ref`, instantiates the editor in a creation
effect, tears it down on unmount, and bridges changes into Leptos signals via `onChange`. The
same module is used unchanged in the Tauri desktop and Tauri 2 mobile webviews.

## Capability negotiation

On startup the engine emits a `CapabilitiesManifest` (Ollama present? sandbox available?
engine local/remote? GH token? cpu/disk). The UI combines this with its own form factor
and composes behavior from the intersection — e.g. no local LLM ⇒ router forces remote;
mobile + remote engine ⇒ editor collapses; no sandbox ⇒ host-runner verify with a warning.
Degradation is always visible in the status strip, never silent.

## Security layering

1. **Path containment** — `mcp-fs`/`mcp-bash` reject `..`, symlink escapes, out-of-root.
2. **Permission gate** — host-side allow/ask/deny; sensitive-path floor (`.env*`, `.ssh/`)
   inviolable; "always/never" decisions persist.
3. **OS sandbox** — opt-in bwrap (Linux) / sandbox-exec (macOS).
4. **Guardrail agent** — deterministic, non-LLM, in the orchestrator spine; vets every tool
   call before execution.

## Testing seams

- `Provider` has a deterministic `LocalProvider` impl for CI — the full spine runs with no
  external API calls.
- Each `Agent` is unit-tested in isolation; the orchestrator is tested with mocked agents.
- `Workspace` is tested against both impls via a shared fixture-repo conformance suite.
- `protocol` has round-trip and version-compatibility tests.
- MCP tools are tested specifically for containment/sandbox-escape resistance.
