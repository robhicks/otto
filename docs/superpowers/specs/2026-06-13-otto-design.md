# otto — Design Spec

**Date:** 2026-06-13
**Status:** Draft for review
**Working name:** otto (greenfield successor to `ai-coder`/savvagent, `otto-old`, and `otto`)

## Summary

otto is a coding agent with a VSCode-and-Claude-Code feel that treats **local and remote development as configurations of one app, not two modes**. It is built on a single insight drawn from three prior prototypes: "local vs remote" is not one binary switch but **four independent axes** — where you sit (UI), where the engine runs, where the workspace lives, and where the LLM runs. Each prior project hard-coupled some of these axes and hit a wall:

- **otto** (prior) coupled engine + workspace to *local* → too much emphasis on local development.
- **otto-old** coupled the engine to *remote* and cloned the repo even when a local copy existed → counterproductive.
- **savvagent** (ai-coder) coupled *everything* to the local terminal → not a rich enough experience; the egui escape hatch became a rat hole.

otto decouples all four axes behind **one stable UI↔Engine protocol**. There is one engine binary (embeddable in the desktop app *or* served remotely), one wire protocol, and one Tauri 2 + Leptos frontend that does not care which side the engine is on. The orchestrator remains the heart of the system, driving **atomic agents** as small, swappable, independently testable units.

## Goals

- One codebase serves local development, remote development, and the migration between them.
- Minimalist, terminal-like, lightning-fast UI that is *not* shaped like VSCode but offers a few VSCode-grade affordances (rich prompt editing, light file view/edit/diff).
- The orchestrator is a deterministic, debuggable control loop; capabilities are composed from swappable atomic agents.
- The app detects the limitations of its environment and adjusts how and where it runs.
- Runs on desktop now; the same frontend codebase targets mobile later.

## Non-Goals (v1)

- A full project-wide IDE / file tree / multi-tab editor. otto edits *a couple* of files, diff-first.
- Running a heavy engine or local LLM on mobile (mobile is a thin client to a remote engine — deferred to v2).
- Vector/semantic retrieval in v1 (AST + grep + git is enough; hybrid retrieval is a later phase).
- Multi-tenant SaaS infrastructure.

## The Four Axes

| Axis | Local end | Remote end |
|---|---|---|
| **UI** (where you sit) | desktop app | mobile / browser |
| **Engine** (orchestrator + atomic agents) | localhost process / in-process | server / microVM |
| **Workspace** (the repo) | local folder, edited in place | remote checkout |
| **LLM** | Ollama / local | Claude / Gemini / OpenAI API |

"Local development" and "remote development" are simply different positions on these axes — not different code paths.

## Decisions (from brainstorming)

1. **Greenfield.** New repo at `/home/robhicks/dev/otto-next`. Lift the best of all three: otto's orchestrator/routing/atomic-agents, otto-old's UI↔engine protocol + MCP fleet, savvagent's MCP-everything provider/tool model.
2. **Desktop-first.** Ship desktop with an embedded engine. The protocol, `Workspace`, `Agent`, remote-target, and provider traits are all designed remote-ready in v1, so the remote engine + mobile are **additive in v2, not a rewrite**.
3. **Pluggable remote target.** Promote-to-remote ships against a `RemoteTarget` trait: a long-lived VPS/server impl first, ephemeral microVM impl later.
4. **Native agents now, WASM-pluggable later.** Agents are native Rust behind an `Agent` trait; safety is MCP path-containment + OS sandboxing. The trait is the seam where a `wasm32-wasip2` agent backend slots in later.
5. **Orchestrator model = Approach C.** A deterministic spine drives atomic agents as swappable units; user-defined markdown agents register as additional roles/sub-steps.
6. **Editor = CodeMirror 6** (lighter than Monaco, fits minimalist + future mobile).

## Architecture

### Component map

```
┌──────────────────────────── Frontend (Tauri 2 + Leptos) ─────────────────────────────┐
│  Conversation pane │ Rich prompt editor │ File peek/edit/diff │ Status strip          │
│  Adaptive layout driven by capabilities manifest + form factor                        │
└───────────────▲───────────────────────────────────────────────────────────────────────┘
                │  one protocol (Commands req/resp + Events stream, Last-Event-ID replay)
                │  local: localhost/IPC      remote: WSS
┌───────────────▼───────────────────────────── Engine (Rust) ──────────────────────────┐
│  Orchestrator  (deterministic state machine: Idle→Plan→Execute→Verify→Repair→Done)    │
│     ├─ Brain-Blend LLM router (local vs remote per task)                               │
│     ├─ Security guardrail gate (deterministic, non-LLM)                                │
│     └─ Agent registry  (role → endpoint; built-in + user-defined)                      │
│  Atomic agents (Agent trait): Planner · ContextFinder · Coder · Verifier               │
│  MCP tool fleet: mcp-fs · mcp-git · mcp-grep · mcp-bash · mcp-lsp                       │
│  Workspace trait: LocalWorkspace (edit in place) | RemoteWorkspace (checkout)          │
│  Providers (in-process libs): anthropic · gemini · openai · local(ollama)              │
│  Session persistence: sqlite (local) / postgres (remote, optional)                     │
│  RemoteTarget trait: VPS impl (v1-ready), microVM impl (later)                         │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

### Engine: embedded vs served

The `engine` crate runs in two deployment modes from the same code:

- **Embedded** — linked into / spawned as a sidecar by the desktop app; the frontend talks to it over localhost or an in-process channel.
- **Served** — `otto engine serve` on a VPS or microVM; the frontend talks over WSS.

Both expose the **identical protocol**, so the frontend never branches on local vs remote.

### Protocol

A dedicated, versioned `protocol` crate (the lineage of otto-old's `contracts`). Two channels:

- **Commands** (UI → Engine, request/response): `create_session`, `send_prompt`, `approve_diff`, `pause` / `resume` / `abort`, `open_file`, `edit_file`, `list_workspace`, `promote_to_remote`, `demote_to_local`, …
- **Events** (Engine → UI, stream): `agent_state`, `token_cost_meter`, `file_edit`, `diff`, `tool_call`, `verify_result`, `approval_request`, `log`. Each event carries a monotonic sequence id; clients reconnect with `Last-Event-ID` for resumable replay.

Locally the event stream is backed by an in-memory bus + sqlite (no Postgres dependency for local use). Remotely it may use Postgres `pg_notify` (as otto-old did) but that is an implementation detail behind the same protocol.

**Versioning:** core message shapes are stable typed records; extensible edges (MCP tool args/results, custom-agent I/O, event payloads) are JSON-encoded so adding a tool/agent/event is a minor version bump, not a breaking change.

### Orchestrator (deterministic spine)

A state machine: `Idle → Plan → Execute → Verify → Repair → Done`. The spine owns, predictably and non-negotiably:

- **Brain-Blend routing** — which LLM (local vs remote) handles each task.
- **Security guardrail gate** — deterministic, non-LLM evaluation of tool calls against allow/block lists and sensitive-path floors.
- **Retry / deadlock detection** — bounded self-correction (max retries with backoff); identical repeated error hashes trip deadlock detection and surface to the user.

### Atomic agents (Approach C)

Every role is an `Agent` behind a uniform trait: it takes a typed request, returns structured output, and may call tools (MCP) and the LLM (via the router). Each agent is small, single-purpose, and independently testable.

- **Built-ins:** `Planner` (decompose goal → dependency-ordered milestones), `ContextFinder` (select minimum file set via AST import-trace + git history + grep), `Coder` (surgical unified-diff patches), `Verifier` (run lint/type/test in sandbox, feed failures back to Coder).
- **Registry:** maps `role → agent endpoint`. Native impls in v1; the trait is the seam for a WASM (`wasm32-wasip2`) backend later.
- **User-defined agents:** markdown files (savvagent SubHost style — own scoped session, optional model override, filtered tool view, depth-bounded recursion) register as additional roles or as sub-steps the Coder can delegate to.

This is the synthesis: the spine gives otto's reliability and bounded cost; the swappable agents give savvagent's composability. Because every agent speaks the same interface, the WASM-pluggable goal falls out naturally.

### Tools = MCP

Tools follow savvagent's "everything is MCP" model: `mcp-fs`, `mcp-git`, `mcp-grep`, `mcp-bash`, `mcp-lsp` (jira optional/later). Spawned per session as stdio children, path-contained to the workspace.

### LLM router (Brain Blend)

Per-task routing between local (Ollama) and remote (Claude / Gemini / OpenAI) by a complexity score (token volume, file span, edit locality, task kind, prior failure count) plus privacy flags (sensitive files forced local) plus a fallback chain (local fails N times → escalate to remote). Providers are in-process libraries by default; remote HTTP shims are optional for debugging/isolation.

### Retrieval

v1: Tree-sitter chunking + git history + grep, feeding `ContextFinder`. Hybrid BM25 (tantivy) + vector (HNSW) + RRF is **deferred** to a later phase; `ContextFinder` works either way and gains the vector index without changing its interface.

### Workspace abstraction

A `Workspace` trait with two impls:

- **`LocalWorkspace`** — a real path on disk; the engine edits **your actual folder directly, no clone**. This is the direct fix for otto-old's "clone a remote repo when a local copy exists" awkwardness.
- **`RemoteWorkspace`** — a checkout living on the remote engine.

The orchestrator only ever knows "the workspace." Local engine + `LocalWorkspace` = zero-ceremony local dev. Remote engine + `RemoteWorkspace` = full remote dev. Same code path.

### Promote-to-remote (handover)

Generalizes otto's cloud handover:

1. Engine serializes `SessionState`: orchestrator state-machine position + context bin + chat history + **uncommitted diffs** + LLM/agent config.
2. Ships it to the `RemoteTarget` (VPS impl first, microVM later). The workspace is reconstituted as a `RemoteWorkspace` via a patch bundle / scratch branch.
3. The same Leptos UI drops its local connection and reconnects to the remote WSS endpoint; the event stream resumes via `Last-Event-ID`. **The UI does not re-render or fork — it just reconnects.**
4. The reverse path (`demote_to_local`) pulls the session back down.

### Frontend (Tauri 2 + Leptos)

Minimalist, terminal-like, *not* VSCode-shaped but VSCode-*capable*. Surfaces:

- **Conversation / command pane** — the Claude-Code-like core: prompt, streaming agent output, inline tool calls + diffs.
- **Rich prompt editor** — CodeMirror 6 for multi-line prompt composition with file-mentions, slash commands, syntax.
- **File peek / edit** — light viewer + diff + occasional edit for a couple of files (CodeMirror). Not a project-wide tree.
- **Status strip** — agent state, Brain-Blend mode (local/remote LLM), token/cost meter, git + verify status.

Desktop now; iOS/Android later from the same Leptos + Tauri 2 codebase.

### Capability negotiation (the "adjusts to its environment" mechanism)

On startup the engine emits a **capabilities manifest**: Ollama present? sandbox (bwrap/Podman) available? engine local or remote? GH token present? cpu/disk budget? The UI also knows its own form factor. The app composes behavior from the **intersection**:

- No local LLM → Brain Blend routes remote; UI states so.
- Mobile + remote engine → editor collapses to conversation + diffs only.
- Local engine + no sandbox → verify falls back to host runner with a warning.

This is the concrete answer to "recognizes limitations of the environment and adjusts how and where it runs."

### Security

savvagent's three layers plus otto's guardrail:

1. **Path containment** — `mcp-fs` / `mcp-bash` reject `..`, symlink escapes, out-of-root paths.
2. **Permission prompts** — host-side allow / ask / deny; sensitive-path floor (`.env*`, `.ssh/`) inviolable; persists "always/never" decisions.
3. **OS sandbox** — opt-in bwrap (Linux) / sandbox-exec (macOS).
4. **Deterministic guardrail agent** in the spine evaluates every tool call before execution.

Verification runs in Podman containers when available, with a host runner fallback (otto-old graceful degradation).

## Error handling

- **Agent/tool failures** feed back into the orchestrator's Repair state as structured observations; bounded retries with backoff; deadlock detection on repeated identical errors.
- **Provider failures** trigger the Brain-Blend fallback chain (local → remote, or remote → alternate remote).
- **Connection loss** (especially remote): the UI reconnects and replays missed events via `Last-Event-ID`; in-flight approvals are re-surfaced.
- **Capability gaps** degrade gracefully and visibly (host verify instead of container; remote routing when no local LLM) rather than failing hard.

## Testing strategy

- **Atomic agents** are independently unit-testable behind the `Agent` trait, with a deterministic `LocalProvider` for CI (savvagent/otto pattern).
- **Orchestrator** state machine tested as a unit with mocked agents.
- **Protocol** round-trip + version-compatibility tests.
- **MCP tools** tested against path-containment and sandbox escapes.
- **Workspace** trait tested with both `LocalWorkspace` and a fixture `RemoteWorkspace`.
- **Integration** tests run the full spine against a fixture repo with the deterministic provider — no external API calls required.

## v1 scope

**Ships:** engine (embedded + `serve`), `protocol` crate, deterministic orchestrator spine, `Agent` trait + Planner/ContextFinder/Coder/Verifier, MCP fs/git/grep/bash, LLM router with Claude + Ollama, `LocalWorkspace`, sqlite persistence, Tauri + Leptos desktop UI (all four surfaces), capability manifest + adaptive layout, three-layer security + guardrail, Podman + host verify.

**Deferred to v2+ but designed-for now** (traits defined remote-ready in v1): remote engine target + promote-to-remote, mobile build, vector/hybrid retrieval, WASM agents, microVM `RemoteTarget`, mcp-lsp, jira.

## Open questions

- Final project name (working name "otto" collides with the prior repo).
- Exact Leptos editor integration approach for CodeMirror 6 inside the Tauri webview.
- Whether user-defined agents reuse `.claude/agents/` markdown for compatibility (savvagent did).
