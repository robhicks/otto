# `Command::RunCommand` on `otto serve` — design

## Status

Sub-project 1 of 2 closing the "`--agent`/`--command` subpaths on serve" thread that has been
explicitly deferred across ~7 prior extensions/serve-path plans (see CLAUDE.md's `extensions`
row and `docs/superpowers/plans/2026-07-04-serve-skills-plugins.md`). This spec covers
**`--command` only**. `--agent` is sub-project 2, tracked separately, because it bypasses the
orchestrator's turn machinery entirely (a single `TaskTool`/`MarkdownAgent` call, no
`Orchestrator::run_turn`) and needs its own event-synthesis design.

## Motivation

`otto run --command <name> [args...]` already works: discover `.claude/commands/*.md`, expand
`$ARGUMENTS`/`$1..$9` and `!bash`/`@file` injections, narrow the tool registry to the command's
`allowed-tools`, pin the router to the command's `model:`, and run the result as a normal spine
turn. `otto serve` has no equivalent — a served session can only submit a plain-string goal via
`Command::SendPrompt`. This closes that gap for commands.

## Non-goals

- `--agent` on serve (sub-project 2).
- Hot-reloading `.claude/commands/*.md` while the server is running — `Extensions` is discovered
  once at `cmd_serve` startup, matching how skills/hooks/plugins already behave on serve. Picking
  up a new/edited command requires a server restart.
- Backporting the serve-only permission-composition improvement (see below) to the CLI path.
- Any change to `otto run --command`'s existing behavior.

## Architecture

Three touch points, no restructuring of existing seams:

### 1. Protocol (`crates/protocol/src/lib.rs`)

Add one new `Command` variant, following the existing internally-tagged, round-trip-tested
pattern (`#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]`, tested in the module's
`#[cfg(test)]` block):

```rust
Command::RunCommand {
    session: SessionId,
    name: String,
    args: Vec<String>,
}
```

No new `EventKind`/`ServerMessage` variants are needed. A `RunCommand` turn is, after
preprocessing, an ordinary goal string run through the ordinary `Orchestrator::run_turn` — so it
produces the exact same event stream (`AgentStarted`/`AgentFinished`, `FileEdit`,
`ApprovalRequest`, `VerifyResult`, `Log`, `TurnComplete`, `TokenCostMeter`) that `SendPrompt`
turns already do.

### 2. `TurnControls` (`crates/engine/src/service.rs`)

Gains two new optional fields:

```rust
pub struct TurnControls {
    pub approver: Arc<dyn Approver>,
    pub pauser: Arc<dyn PauseController>,
    pub tools: Option<Arc<ToolRegistry>>,
    pub router: Option<Arc<dyn Router>>,
}
```

`Default` sets both new fields to `None`. Inside `run_prompt_with_controls`, resolve the
effective tools/router as:

```rust
let tools = controls.tools.clone().unwrap_or_else(|| self.tools.clone());
let router = controls.router.clone().unwrap_or_else(|| self.router.clone());
```

...before constructing the `MeteringRouter` wrapper and the `Orchestrator`. This is the **only**
change to `EngineService`. `SendPrompt` continues to pass `TurnControls::default()` (or a
default-tools/router variant), so its behavior is byte-for-byte unchanged. `RunCommand` supplies
`Some(narrowed_tools)` / `Some(pinned_router)` for that one call only — the server's own
long-lived `EngineService.tools`/`.router` fields are never mutated.

### 3. `ServeState` (`crates/engine/src/serve.rs`)

Gains two new fields, populated once in `cmd_serve` from data it already computes:

```rust
extensions: Arc<otto_extensions::Extensions>,
tools: Arc<ToolRegistry>,
```

`cmd_serve` already calls `otto_extensions::discover(&root, &home_dir())` and
`build_serve_tools(&ext, ...)` before constructing `EngineService::new(..., tools.clone())` — both
values just need to also be cloned onto `ServeState` instead of being dropped after
`EngineService::new` takes its copy. No re-discovery, no new I/O.

## Data flow

New `Command::RunCommand { session, name, args }` arm in `handle_socket`'s command match,
structured identically to the existing `SendPrompt` arm (same `tokio::select!` race against
inbound `Abort`/`Pause`/`Resume`/`ApproveDiff` frames, same `InteractiveApprover`/
`InteractivePauser` construction):

1. **Look up.** `state.extensions.commands.iter().find(|c| c.name == name)`. Not found →
   `ServerMessage::Error { message }` (wording mirrors the CLI's "no command named `<name>` in
   `~/.claude/commands/` or `<root>/.claude/commands/`"). No turn is started, no `seq` is
   consumed.
2. **Narrow tools.** `def.allowed_tools`:
   - `Some(list)` → `Arc::new(state.tools.subset(list))` (fresh registry, same underlying gate).
   - `None` → `state.tools.clone()` (cheap `Arc` clone, no copy).
3. **Expand.** `expand_args(&def.template, &args)`, then
   `resolve_injections(&expanded, &narrowed_tools).await`. Both are already-public functions in
   `otto_extensions`, directly callable from `serve.rs` (no crate-boundary work needed — `main.rs`
   and `lib.rs` are both part of the same `otto-engine` package and already depend on
   `otto_extensions`). A resolution failure (e.g. a broken `!bash` injection, fail-closed on a
   denied path) → `ServerMessage::Error`, no turn started.
4. **Pin router.** `build_router_with_model(def.model.as_deref())` (already a `pub fn` in
   `crates/engine/src/lib.rs`) → `Arc::from(...)`.
5. **Run.** `state.service.run_prompt_with_controls(session, &goal, &mut sink, TurnControls {
   approver, pauser, tools: Some(narrowed_tools), router: Some(pinned_router) })`, inside the same
   inbound-frame race loop `SendPrompt` uses today.

### A serve-only correctness improvement over the CLI

`otto run --command` narrows starting from `PermissionRules::default()` (empty) — hooks,
skills, permission-rule composition, and plugin MCP servers are all explicitly not applied on
that CLI path (a long-documented, still-open gap). `otto serve`'s `state.tools` is the *already
fully composed* registry (`build_serve_tools`: permissions/`PolicyGate`, `--approve-edits`
composition, hooks, skills, plugin MCP servers — slices 6–11). Narrowing that composed registry
by a command's `allowed-tools`, rather than starting from an empty permission set, means a served
`RunCommand` turn is strictly more correct than the CLI equivalent today. This is intentional and
noted as a known asymmetry, not a bug to reconcile in this pass.

## Error handling & concurrency

No failure modes beyond what `run_command_in` (CLI) already handles — unknown command name,
injection-resolution failure — both mapped to `ServerMessage::Error` on the connection instead of
a process exit, with the connection staying open afterward. `Pause`/`Resume`/`Abort`/
`ApproveDiff` behave identically to `SendPrompt` turns, since they're driven entirely by
`TurnControls.approver`/`.pauser`, which this change does not touch. A second `SendPrompt`/
`RunCommand` arriving mid-turn is still ignored, matching existing `SendPrompt` behavior.

## Testing

- Protocol round-trip test for `Command::RunCommand` (serialize/deserialize), alongside the
  existing `Command` variant tests.
- `TurnControls`-override unit test: `run_prompt_with_controls` with `Some(narrowed_tools)`
  actually restricts what the turn's tools report as available, and `Some(pinned_router)`
  actually routes remote-eligible calls to the pinned model (mirroring the existing
  `narrow_for_command`/`build_router_with_model` unit tests in `main.rs`).
- `serve.rs` integration test: a `.claude/commands/foo.md` fixture with `allowed-tools`, `model`,
  and a `$ARGUMENTS`-bearing template; drive `Command::RunCommand` over a test socket; assert (a)
  the expanded goal matches what `expand_args` alone would produce, (b) the turn's tool registry
  only exposed the allow-listed names, (c) the event stream looks like an ordinary turn
  (`AgentStarted`/.../`TurnComplete`).
- Unknown-command-name → `ServerMessage::Error`, connection stays alive and can still send a
  valid `SendPrompt`/`RunCommand` afterward.
- Regression: existing `SendPrompt`-path tests pass unmodified (the `TurnControls` field
  addition must not change default behavior).
