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
│   ├── remote           # RemoteTarget + Provisioner seam; vps + microvm (firecracker, feat-gated). LoopbackTarget stays in engine.
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

`AgentCtx` grants scoped access to the LLM router, the MCP tool registry, and a read-only
view of the workspace. The workspace seam is split into `WorkspaceRead` (`read`, `list`) and
`Workspace: WorkspaceRead` (adds `apply_edit`). `AgentCtx::workspace()` exposes only the
read-only `WorkspaceRead` view, so an agent cannot mutate the workspace directly — writes flow
exclusively through the gated `fs.write` tool and the orchestrator's permission-gated apply. The
orchestrator and the `fs.*` tools hold the full `Workspace`. Native impls
live in `agents`; a `wasm32-wasip2` impl can register against the same trait later. The
orchestrator only knows roles and endpoints — it never knows whether an agent is native,
wasm, or a user-defined markdown SubHost.

The built-in `Planner` and `Coder` are real LLM agents: each builds an instruction prompt
asking the router for a specific JSON shape (milestones / file edits), calls
`ctx.router().complete(...)`, and parses the response with `extract_json` (tolerant of fenced
code blocks + surrounding prose). Prompts describe the JSON schema in PROSE rather than a
literal example, so an offline echo provider yields no parseable JSON and the agents degrade
safely — the Planner treats the whole goal as one milestone, the Coder produces no edits — so
an offline run (no LLM configured) completes a turn but writes nothing. Tests drive a
`ScriptedProvider` (deterministic prompt-keyed mock LLM) to exercise the real parse path. The
Coder's edits still pass the orchestrator's fail-closed permission gate before being written.

The `Verifier` is real and multi-ecosystem: it detects the project type from the workspace root
(`Cargo.toml`, `go.mod`, `package.json`, `pyproject.toml`/`setup.py`, or `Makefile`) via an
ordered recipe table — first match wins, language-native build systems before the generic
`Makefile` — and runs that ecosystem's test command (`cargo test --offline`, `go test ./...`,
`npm test`, `pytest -q`, `make test`) inside the sandboxed `bash` tool. A non-zero exit becomes
`Verify { ok: false }` with the truncated output as detail, which drives the orchestrator's
Repair loop. It degrades safely: no recognized project → "nothing to verify"; `bash` unavailable
(no OS sandbox) → "verification skipped"; the toolchain not on the sandbox PATH (exit 127) →
"verification skipped: <tool> tooling not found". Commands run offline (the sandbox has no
network), so dependencies must already be installed/cached — the accepted v1 posture.

To run `cargo` inside the cleared-env sandbox, `BashTool` passes through the non-secret Rust
toolchain location (`PATH` includes `~/.cargo/bin`; `CARGO_HOME`/`RUSTUP_HOME` point at the
host toolchain); this grants
no new read access (the host FS is already read-only-readable in the sandbox) and network stays
off, so the read-but-no-exfil posture is unchanged.

The `ContextFinder` is real: it enumerates the workspace recursively (the `fs.list` `**` glob,
which skips `.git`/`target`/`node_modules`/dotfiles and does not follow symlinks), scores files
lexically against goal keywords (path matches weighted above content matches), keeps the top
candidates, and asks the model to pick the most relevant subset — falling back to the lexical
top-N when the model does not answer in schema, so the default offline path stays deterministic.
To stay bounded on large repositories, the lexical phase path-scores every file for free, drops
non-text files by extension, and reads file contents only for the top ~200 files by path score
(the rest are scored on their path alone) — so a small repo still reads everything, while a
huge one reads a bounded subset. A file relevant only by content and ranked beyond that budget
may be missed; path-named relevance (weighted higher) is always read.
The `Coder` then reads those files via the gated `fs.read` tool and embeds their contents
(budgeted: at most 8 files, ~8 KB each, ~32 KB total) in its prompt, so edits are grounded in
real file contents. With this, the whole spine — Planner → ContextFinder → Coder → Verifier — is
real, with no stubs remaining.

### `Workspace` — the workspace-axis seam

```rust
#[async_trait]
trait WorkspaceRead {                                        // the agent-facing read-only view
    async fn read(&self, path: &Path) -> Result<Bytes>;
    async fn list(&self, glob: &str) -> Result<Vec<PathBuf>>;
}

#[async_trait]
trait Workspace: WorkspaceRead {                             // orchestrator + `fs.*` tools only
    async fn apply_edit(&self, edit: &Edit) -> Result<u64>;
    async fn snapshot(&self) -> Result<WorkspaceSnapshot>;   // uncommitted diffs, for handover
}
```

The seam is split so agents cannot mutate the workspace directly: `AgentCtx::workspace()`
exposes only the read-only `WorkspaceRead` view, while the orchestrator and the `fs.*` tools
hold the full `Workspace`. Writes therefore flow exclusively through the gated `fs.write` tool
and the orchestrator's permission-gated apply.

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

The `bash` tool (`BashTool`) runs shell commands confined by a `SandboxPolicy`: on Linux,
`bwrap` mounts the whole filesystem read-only, re-binds only the workspace root writable, and
isolates the network/pid/ipc/session namespaces (`--unshare-net/-pid/-ipc`, `--new-session`);
on macOS, `sandbox-exec` applies an equivalent write-confined, network-denied profile. The
spawned command runs with a cleared, minimal environment (`PATH`/`HOME`/`TERM`, plus the
non-secret Rust toolchain location — `~/.cargo/bin` on `PATH` and `CARGO_HOME`/`RUSTUP_HOME`
pointing at the host toolchain so the Verifier can run `cargo`) so host credentials in env are
never exposed. `HOME` and `TMPDIR` are both pinned to the workspace root — the latter because
the default `/tmp` is on the read-only mount, so toolchain temp files (e.g. `cargo`'s linker
building a registry dependency) must land in the one writable location. The gate classifies `bash` as `Ask` (shell can't be
statically path-vetted), and the engine registers `bash` ONLY when an OS sandbox backend
exists — pairing it with an `AllowListAskResolver` that permits the now-confined `bash`. With
no backend, `bash` is absent and `Ask` stays denied (fail-closed). Output is
`{stdout, stderr, exit_code}`; a timeout kills the whole sandbox process tree via
`kill_on_drop` (the pid-namespace ensures no orphans). Known deferral: unbounded
stdout/stderr buffering (no output cap yet).

The orchestrator also runs every Coder edit through `ToolRegistry::check("fs.write", {path})`
(the same gate, without dispatch) before applying it via the workspace — and only an explicit
`Allow` proceeds (a `Deny` or `Ask` is logged and the edit skipped, fail-closed). So a Coder
cannot write a sensitive path. The `ctx.workspace()` accessor exposes only the read-only
`WorkspaceRead` view, so an agent cannot mutate the workspace directly (the real Coder returns
edits rather than writing them); all writes flow through the gated `fs.write` tool and the
orchestrator's permission-gated apply, which hold the full `Workspace`.

### `RemoteTarget` — the engine-axis seam

```rust
#[async_trait]
trait RemoteTarget {
    async fn provision(&self, state: &SessionState) -> Result<RemoteHandle>;  // returns WSS endpoint
    async fn teardown(&self, handle: RemoteHandle) -> Result<()>;
}
```

`vps` impl (long-lived server) is **shipped**: `VpsTarget` (in the `remote` crate, alongside the
`RemoteTarget` seam; `LoopbackTarget` stays in `engine`) promotes a session onto an already-running, bearer-authed
`otto serve --accept-promotions` by POSTing the `PromoteBundle` to its `POST /promote` restore RPC;
the client then reconnects to the receiver and resumes via `Last-Event-ID`. `teardown` is a no-op
(the target does not own the operator's server). The receiver restore is **gated** — workspace
files land through the permission floor, so a bundle carrying `.env`/`.ssh`/keys is refused.
**demote-from-remote** is **shipped** too: a client on the source serve issues `DemoteToLocal`; the
source pulls the session back via a gated `POST /export` on the receiver and restores it locally with
`accept_demotion` (overwriting its own stale copy via `SessionStore::restore_over`), then replies
`Demoted` and the client reconnects to the source. The export is gated by the same
`--accept-promotions` flag and its workspace snapshot is gate-filtered (secrets never leave the
receiver); the receiver keeps its copy. The machine-provisioning step now lives behind a **`Provisioner` seam** (`provision()` boots a
reachable `otto serve --accept-promotions`; disposal rides the returned task). `MicrovmTarget`
composes any `Provisioner` with the shared `push_promote_bundle` restore-push, and the `microvm`
provisioner is **shipped**: `FirecrackerProvisioner` (behind the default-off `firecracker` cargo
feature) boots an ephemeral per-session microVM and restores the bundle into it via the same gated
`POST /promote`. `UnsupportedProvisioner` (which replaced the old unsupported-target stub) is the single
honest boundary — "no hypervisor / kernel / rootfs in-tree." The seam is proven end-to-end in CI
against an in-process serve; the real VM boot needs operator-supplied images and a host hypervisor.
**demote-from-microvm** is **shipped** too, mirroring vps demote with two differences: the receiver
endpoint comes from the live `RemoteHandle` (the in-memory promote handle, not static config), and a
successful demote disposes the ephemeral VM by dropping that handle. A client on a `--promote-microvm`
source issues `DemoteToLocal`; the source pulls the session's current bundle off the running microVM
via the shared `export_bundle` (`POST /export`) and restores it locally with `accept_demotion`
(overwriting its own copy via `SessionStore::restore_over`, sensitive-floor first); pull/restore
failures leave the VM running. Without the `firecracker` feature the serve-level happy path is not
CI-able (same boundary as microVM promote), but the seam-level pull+dispose is tested in-process.

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

**Repair.** The `Code → apply (gated) → Verify` step is a bounded loop. On a verify failure the
orchestrator increments `prior_failures`, sets the Coder's `feedback` to the failure detail,
emits a "repairing" log, and re-runs the Coder + Verifier — up to `MAX_REPAIRS` (2) repairs
(3 total attempts). `prior_failures` flows into the Coder's `RouteHints`, so Brain-Blend
escalates local→remote on repeated failure. The turn's outcome is the last Verify result, and
the happy path (Verifier passes first try) runs the loop exactly once — so its event sequence
is unchanged. The real bash-backed Verifier (`cargo check --offline` in the sandbox) now
drives this loop; when no project is recognized or `bash` is unavailable it reports success,
so the loop stays dormant in those cases.

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
