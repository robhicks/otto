# otto Extensions Slice 1 Design — `extensions` crate + custom agents (Task dispatch)

**Status:** Approved design.
**Date:** 2026-06-24.

## Why this document

`ARCHITECTURE.md` ("Claude Code compatibility") describes a single `extensions` crate that
discovers `.claude/` (project) and `~/.claude/` (user-global) at session start and registers
each artifact — agents, commands, skills, hooks, permissions, plugins — into an existing otto
primitive. That is a multi-sub-project effort (three of the six consuming primitives —
`HookRegistry`, a command registry, a `Skill` tool — do not exist yet), so it is decomposed
the way the UI roadmap was (A–F). This is **slice 1**: the `extensions` crate scaffold plus the
**custom agents** artifact, wired end to end as a dispatchable subagent.

## Scope

Build, end to end:

1. A new `extensions` crate: discovery of `~/.claude/agents/*.md` and `<project>/.claude/agents/*.md`
   (project overrides user by name) + Claude-Code-exact `agents/*.md` frontmatter parsing.
2. A `MarkdownAgent` (`Agent` impl) that answers a free-form task by running its markdown body as
   a system prompt through the router, honoring its `model` hint.
3. A built-in `TaskTool` that dispatches a named custom agent as a depth-1 sub-turn with a
   tool view filtered to the agent's `tools` allowlist.
4. A CLI entry — `otto run --agent <name> "<goal>"` — that runs a discovered custom agent through
   the dispatch path (the user-visible, verifiable demonstration).
5. Wiring in `otto run`/`serve`: discover, register each custom agent as `Role::Custom(name)`,
   register the `TaskTool`.

## Fixed decisions (from brainstorming)

- **First artifact = custom agents** (the consuming primitive `AgentRegistry`/`Role::Custom`
  already exists).
- **Discovery scope = both, project wins.** Discover `~/.claude/agents/` and
  `<project>/.claude/agents/`; on a name collision the project agent overrides the user one.
- **Schema = match Claude Code exactly.** Real `agents/*.md` frontmatter (`name`, `description`,
  `tools`, `model`) parses unmodified.
- **Invocation = full subagent model (Task tool).** Register the custom agent **and** ship a
  built-in tool that runs it as a sub-turn. (Distinct from register-only.)

## Non-goals (explicitly out of scope — later slices)

- **Autonomous dispatch by the spine.** otto's spine agents (Planner/ContextFinder/Coder/Verifier)
  do not run open-ended LLM tool-call loops — the Coder applies `fs.write` through specific logic,
  not a general "the model emits a tool call" loop. So registering a `task` tool does not, by
  itself, get it invoked. This slice ships the dispatch **mechanism** (the `TaskTool` + filtered
  sub-turn) and a concrete invocation path (`--agent`); a spine agent *deciding on its own* to
  dispatch requires giving spine agents tool-call loops, which is a separate, larger slice.
- **Nested subagent dispatch (depth > 1).** A dispatched agent's tool view excludes `task`, so it
  cannot re-dispatch. Prevents fork bombs; multi-level dispatch is a later slice.
- **The other five artifact types** — commands, skills, hooks, permissions, plugins — each its own
  later slice.
- **Retrieval/RemoteWorkspace concerns** — unchanged by this slice.

## Architecture

Dependencies flow strictly inward, matching the existing crate graph:

```
extensions  ──depends on──►  engine-core  (Agent, Role, AgentCtx, Router, ToolRegistry, types)
   ▲
   │ wired only by
engine (otto binary: run / serve)
```

`extensions` is a leaf like `retrieval`: it depends on `engine-core` and serde; it is **never**
linked into `engine-core` and is invoked **only** from the `engine` binary paths. The offline
determinism suite (which runs the orchestrator core with no env and no `.claude/`) is therefore
untouched.

### Components

#### `extensions` crate

- `CustomAgentDef` — a parsed agent: `name: String`, `description: String`,
  `tools: Option<Vec<String>>` (None = all available tools; Some = allowlist),
  `model: Option<String>`, `system_prompt: String` (the markdown body).
- `parse_agent_md(text) -> Result<CustomAgentDef>` — splits YAML frontmatter from body. The
  frontmatter shape mirrors Claude Code: `name`, `description`, optional `tools` (accepts both a
  comma-separated string and a YAML list), optional `model`. Missing `name`/`description` is an
  error for that file (skipped, logged); the file is never fatal to discovery.
- `discover(project_root: &Path, home: &Path) -> Extensions` — reads `<home>/.claude/agents/*.md`
  then `<project_root>/.claude/agents/*.md`, parsing each. Returns `Extensions { agents:
  Vec<CustomAgentDef> }` with project entries overriding user entries of the same `name`. `home`
  is an explicit parameter, never read ambiently — keeps tests hermetic and the default suite
  deterministic.

#### `engine-core` additions (additive, semver-minor)

- `AgentRequest::Task { prompt: String }` and `AgentOutput::Task { text: String }` — the free-form
  request/response shape a custom agent answers. The fixed spine never constructs these; they are
  the seam for free-form subagents.
- `ToolRegistry::subset(&self, allowed: &[String]) -> ToolRegistry` — a new registry holding only
  the named tools, sharing the **same** `gate` and `ask` resolver `Arc`s. The sensitive-path floor
  and every gate decision are therefore identical to the parent; an allowlist can only **narrow**
  the available tool set, never widen it or bypass gating.

#### `MarkdownAgent` (`Agent` impl)

Lives in `extensions` (it depends only on `engine-core` seams). Constructed from a
`CustomAgentDef`. `run(AgentRequest::Task { prompt }, ctx)`:

1. Compose the completion prompt from `system_prompt` + the task `prompt`.
2. Call `ctx.router().complete(...)` with route hints carrying the agent's `model` preference
   (the router maps an unknown/None model to its default slot — no hard failure).
3. Return `AgentOutput::Task { text }`.

It reads `ctx.tools()` for any tool use it performs; the dispatcher supplies a filtered registry,
so the agent can only reach its allowlisted tools.

#### `TaskTool` (built-in `Tool`, in `engine`)

A `Tool` named `task`, holding `Arc<dyn Router>`, `Arc<dyn WorkspaceRead>`, `Arc<AgentRegistry>`
(the registered custom agents), and `Arc<ToolRegistry>` (the base tools — fs/grep/git/bash —
**without** `task`). `call(args)`:

1. Parse `{ "agent": String, "prompt": String }`.
2. Look up `Role::Custom(agent)` in the registry; error if absent.
3. Build the sub-tool view: `base_tools.subset(def.tools)` (None ⇒ all base tools). The subset
   never includes `task`, so the dispatched agent cannot re-dispatch (depth 1).
4. Build a sub-`AgentCtx::new(router, workspace, &sub_tools)` and run the agent with
   `AgentRequest::Task { prompt }`.
5. Return `{ "text": output_text }`.

The base tool registry passed to `TaskTool` is constructed **without** `task` to break the
otherwise-circular `Arc` (main registry holds `task`; `task` holds the base registry).

### Wiring (in `engine`)

- `otto run`/`serve` call `extensions::discover(root, home_dir)` (using the process `HOME`).
- For each `CustomAgentDef`, register `Arc::new(MarkdownAgent::new(def))` into the `AgentRegistry`
  under `Role::Custom(def.name)`.
- Build the base tool registry as today; construct `TaskTool` over a clone of it (base, no `task`);
  register `TaskTool` into the registry the spine uses.
- `otto run --agent <name> "<goal>"`: instead of the fixed spine turn, run the named custom agent
  directly through the dispatch path (look up `Role::Custom(name)`, build its filtered ctx, run
  `Task { prompt: goal }`), printing its output. Absent `--agent`, `otto run` is unchanged.
- All of the above is a no-op when no `.claude/agents/` directory exists.

## Gate classification

`task` is classified **Allow** by `DefaultPermissionGate`. This is safe because the tool does not
touch the filesystem itself — it only orchestrates an LLM call plus **already-gated** sub-tool
calls. Every action the dispatched agent takes re-enters the gate independently, and the shared
gate keeps the sensitive-path floor inviolable regardless of the agent's allowlist. (A custom
agent that lists `fs.write` in `tools` still cannot write `.env`.)

## Data flow

```
otto run --agent reviewer "audit auth.rs"
  └─► extensions::discover(root, HOME) ──► [CustomAgentDef{name:"reviewer", tools:["fs.read","grep"], model:..}]
        └─► AgentRegistry.register(Role::Custom("reviewer"), MarkdownAgent)
        └─► dispatch path: base_tools.subset(["fs.read","grep"]) ─► sub-AgentCtx
              └─► MarkdownAgent.run(Task{prompt:"audit auth.rs"}, ctx)
                    └─► ctx.router().complete(system_prompt + prompt, model hint)
                    └─► (any tool use re-gated; secrets still denied by the floor)
              └─► AgentOutput::Task{ text } ──► printed
```

The in-spine path (a future tool-calling agent calling the `task` tool) reuses the identical
`TaskTool.call` machinery.

## Error handling

- Malformed/missing-field `agents/*.md` → skipped, logged; discovery continues.
- Missing `.claude/agents/` (either root) → empty contribution, no error.
- `task` called for an unknown agent name → tool returns an error `Value`/`Err` (gated call surfaces
  it to the caller; it does not panic).
- Router failure inside a dispatched agent → propagates as the tool's `Err`, same as any agent's
  router failure today.
- Name collision across user/project → project wins (defined precedence, not an error).

## Security

- **Allowlist narrows only.** `subset` shares the parent gate/resolver; no allowlist can widen the
  tool set or bypass a gate decision. The sensitive-path floor (`.env*`, `.ssh/`, …) still denies.
- **Depth 1.** The dispatched agent's tool view excludes `task` — no nested dispatch, no fork bomb.
- **`model` selects a slot only.** It cannot reach a provider that `build_router` did not wire.
- **Attacker-controlled system prompt** runs under the same sandbox/gate as the spine — a hostile
  `agents/*.md` gains no capability beyond its (gated, allowlisted) tools.
- **Hermetic discovery.** `home` is an explicit parameter; tests never read the developer's real
  `~/.claude`, and the orchestrator core never calls discovery, so the determinism suite is
  unchanged.

## Testing

- **`extensions`:** `parse_agent_md` round-trips frontmatter (CSV and YAML-list `tools`, optional
  `model`, body-as-system-prompt); malformed/missing-field files are skipped; `discover` over two
  tempdir roots applies project-over-user precedence; absent dirs yield empty.
- **`engine-core`:** `ToolRegistry::subset` exposes only named tools and preserves gate behavior
  (a denied/sensitive call is still denied through the subset); `AgentRequest::Task`/`AgentOutput::Task`
  round-trip.
- **`MarkdownAgent`:** against a `ScriptedProvider`-backed router, `run(Task{..})` returns the
  scripted text and passes the model hint; with a tempfile workspace + a filtered registry it can
  reach an allowlisted tool and cannot reach an excluded one.
- **`TaskTool`:** dispatches a registered custom agent end to end (filtered tools applied, output
  returned); unknown-agent name errors; a dispatched agent cannot call `task` (absent from its view).
- **`engine`:** `otto run --agent <name>` over a tempdir `.claude/agents/` runs the agent and emits
  its output; the existing offline determinism suite stays green (no `.claude/` ⇒ no-op).

## What this unblocks

With the `extensions` crate, hermetic discovery, the `Task` request seam, and `ToolRegistry::subset`
in place, later slices slot in without touching the orchestrator spine:

- **commands** (`commands/*.md` → a command/prompt registry),
- **skills** (`SKILL.md` → a built-in `Skill` tool),
- **hooks** (`settings.json` hooks → a new `HookRegistry`),
- **permissions** (`settings.json` permissions → composed into the gate),
- **plugins** (`.claude-plugin/plugin.json` → fan out to the above; bundled MCP servers → the MCP client),
- and **autonomous spine dispatch** (a spine agent gains a tool-call loop and calls the existing `task` tool).
