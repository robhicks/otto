# `Command::RunAgent` on `otto serve` — design

## Status

Sub-project 2 of 2 closing the "`--agent`/`--command` subpaths on serve" thread (sub-project 1,
`Command::RunCommand`, shipped in
`docs/superpowers/specs/2026-07-04-serve-run-command-design.md` /
`docs/superpowers/plans/2026-07-04-serve-run-command.md`). This covers `--agent`: dispatching a
discovered `.claude/agents/*.md` custom agent over a served WebSocket connection, the way
`otto run --agent <name> "<goal>"` already does on the CLI.

## Motivation

`otto run --agent <name> "<goal>"` discovers `.claude/agents/*.md`, registers each as
`Role::Custom(name)` backed by a `MarkdownAgent`, builds a `TaskTool` over the base tool
registry, and dispatches the named agent — printing the returned text. `otto serve` has no
equivalent; a served session can only run the fixed spine turn (`SendPrompt`) or, as of
sub-project 1, a discovered command (`RunCommand`). This closes that last gap.

## Why this needed its own design (not just "`RunCommand` again")

`RunCommand` preprocesses a goal string and then runs it through the *ordinary*
`Orchestrator::run_turn` — so it emits the exact same event stream a `SendPrompt` turn does, for
free. Custom-agent dispatch is different in kind: `TaskTool::call` → `MarkdownAgent::run` is a
**single, non-interruptible request/response** (compose `system_prompt` + task prompt → one
`Router::complete` call → return text). There is no `Orchestrator`, no Plan/Execute/Verify
phases, no `fs.write` gate check (`MarkdownAgent` never calls `ctx.tools()` — see
`crates/extensions/src/markdown_agent.rs`; per the custom-agents design's Non-goals, spine-style
autonomous tool-call loops are explicitly out of scope). So there is nothing resembling a turn to
draw `FileEdit`/`ApprovalRequest`/`VerifyResult` events from, and no mid-flight point where
`Pause`/`ApproveDiff` could mean anything. This spec's job is deciding what *does* go over the
wire for a call shaped like that.

**Decision:** reuse the existing `EventKind` vocabulary — no new wire variant. A `RunAgent` call
emits, in order: `AgentStarted { role: Role::Custom(name) }`, `Log { message: text }` (the
dispatched agent's full response), `AgentFinished { role }`, `TurnComplete { ok }`. This keeps
`ServerMessage`/`EventKind` byte-for-byte unchanged (only `Command` gains a variant) and gives a
generic client a reasonable rendering for free: the same "role started → output → role finished →
done" shape a spine agent's slice of a normal turn already has. The trade-off is that a UI can't
distinguish "this `Log` is a task result" from an ordinary spine log line by type alone — the
preceding `AgentStarted { role: Custom(_) }` is the only signal. Acceptable: no UI consumer is
built in this pass (see Non-goals), and a future UI slice can special-case `Role::Custom` framing
without a protocol change.

## Non-goals

- Any UI (`ui/`) changes. Precedent: `RunCommand` shipped with zero `ui/` changes; this follows
  the same pattern. A future UI slice can render `RunAgent`/its events once there's a client that
  wants to send it.
- Registering `task` into the served tool registry so a spine agent (Coder, etc.) could dispatch
  a custom agent itself. Explicitly deferred by the original custom-agents design's Non-goals
  ("autonomous dispatch by the spine") and unaffected by this slice — `RunAgent` is a
  client-invoked WS command, exactly as `--agent` is a client-invoked CLI flag.
- Nested/depth>1 dispatch, per-sub-agent `model`, hot-reloading `.claude/agents/*.md` while the
  server runs — all pre-existing, unchanged constraints from the custom-agents design.
- Pause/resume/approval semantics for a `RunAgent` call (see Architecture — there is no
  mid-flight point for either to apply).
- Any change to `otto run --agent`'s existing CLI behavior.

## Architecture

Three touch points, matching `RunCommand`'s shape:

### 1. Protocol (`crates/protocol/src/lib.rs`)

Add one new `Command` variant, following the existing internally-tagged, round-trip-tested
pattern:

```rust
/// Dispatch a discovered `.claude/agents/*.md` custom agent by name as a single, non-interruptible
/// request/response (no orchestrator turn): compose its system prompt with `prompt` and run it
/// through `TaskTool`/`MarkdownAgent`. Emits `AgentStarted`/`Log`/`AgentFinished`/`TurnComplete` —
/// no new `EventKind`. Unknown `name` surfaces as `ServerMessage::Error`; no turn starts, no `seq`
/// is consumed.
RunAgent {
    session: SessionId,
    name: String,
    prompt: String,
},
```

No new `EventKind`/`ServerMessage` variants (see the event-shape decision above).

### 2. `EngineService::run_agent_with_controls` (`crates/engine/src/service.rs`)

A new method, sibling to `run_command_with_controls` but simpler — no `TurnControls`, no
`Orchestrator`, no spawned task, because there is exactly one async call to make and no
approval/pause surface:

```rust
pub async fn run_agent_with_controls(
    &self,
    session: SessionId,
    name: &str,
    prompt: &str,
    sink: &mut dyn EventSink,
) -> anyhow::Result<TurnOutcome>
```

Behavior:

1. `self.extensions.as_ref()` — `None` errors exactly like `run_command_with_controls` ("this
   server was not configured with any extensions"). No turn starts, no `seq` consumed.
2. Find `def` in `extensions.agents` by `name`; not found → error ("no custom agent named
   `<name>` in `~/.claude/agents/` or the project `.claude/agents/`"). No turn starts.
3. Build a fresh `AgentRegistry`, registering **every** discovered `extensions.agents` entry as
   `Role::Custom(def.name) → MarkdownAgent::new(def)` (matches `run_custom_agent_in`'s CLI
   behavior byte-for-byte, including the `allowlists: HashMap<String, Option<Vec<String>>>` built
   alongside it) — not just the one being dispatched. Cheap (defs are already in memory from
   startup discovery) and keeps this path indistinguishable in behavior from the CLI's.
4. Pin the router: `Arc::from(build_router_with_model(def.model.as_deref()))` — always freshly
   built, same convention `run_command_with_controls` already uses (never falls back to
   `self.router`).
5. Base tools for the `TaskTool`: `Arc::clone(&self.tools)` — the server's already-composed
   registry (permissions/hooks/skills/plugin-MCP, per `build_serve_tools`), narrowed per-agent by
   `TaskTool` itself via each def's `tools` allowlist. (Same serve-is-strictly-more-correct-than-CLI
   asymmetry `RunCommand` documented, for the same reason: the CLI `--agent` path narrows from
   `PermissionRules::default()`, this narrows from the fully composed registry.)
6. Read-only workspace view: `self.workspace.clone()` coerced to `Arc<dyn WorkspaceRead>` (`dyn`
   upcasting; already relied on elsewhere, e.g. `crates/engine/src/lib.rs`'s `read_workspace`
   binding) — no second `LocalWorkspace` construction needed, unlike the CLI path (which builds a
   fresh one because it has no long-lived `Workspace` handle to reuse).
7. Construct `TaskTool::new(router, read_ws, Arc::new(registry), base_tools, allowlists)` and call
   it once: `task.call(json!({ "agent": name, "prompt": prompt }))`. An error here (unknown agent —
   already ruled out in step 2, or a provider failure) is surfaced as a failed turn (see below), not
   a pre-turn `ServerMessage::Error` — the boundary is "did we start emitting events," matching how
   `run_prompt_with_controls` treats orchestrator errors after the turn has begun.
8. Emit, through the same persist-then-stream discipline `run_prompt_with_controls` uses (own
   `turn_lock` guard, `next_seq`/`next_turn`, `store.append_event` before `sink.emit`, fail-closed
   on a persist error, `record_turn` + `set_status` at the end):
   - `AgentStarted { role: Role::Custom(name.to_string()) }`
   - On success: `Log { message: text }`, `AgentFinished { role }`, `TurnComplete { ok: true }`
   - On the `task.call` erroring: `AgentFinished { role }`, `TurnComplete { ok: false }` (the error
     is not silently dropped — it's threaded into the final `Err` this method returns, same as an
     orchestrator failure propagates today; the session is marked `Failed`)
9. Return `TurnOutcome { ok }`, record the turn, and set session status — identical bookkeping to
   `run_prompt_with_controls`, so replay (`Last-Event-ID`) and `SessionStatus` behave the same for
   a `RunAgent` call as for any other turn.

No `TurnControls`/`Approver`/`PauseController` involved: there is no `fs.write` gate check to
approve (see the "why this needed its own design" section) and no multi-step turn to pause
between steps of.

### 3. `serve.rs` — `handle_socket`

New `Command::RunAgent { name, prompt, .. }` arm, structured more simply than `SendPrompt`/
`RunCommand`'s arms — no inbound-frame race is needed (nothing to approve or pause), so it does
**not** go through `run_turn_loop`:

```rust
Command::RunAgent { name, prompt, .. } => {
    let mut sink = WsSink { writer: &mut writer };
    let outcome = state
        .service
        .run_agent_with_controls(session, &name, &prompt, &mut sink)
        .await;
    if report_turn_outcome(outcome, &mut writer).await {
        break 'outer;
    }
}
```

`reader` stays free the whole time (no `select!`), so an `Abort`/`Pause` sent mid-dispatch is
simply queued and processed after this call returns — acceptable given the call is a single
bounded LLM completion, not an open-ended multi-step turn.

## Data flow

1. Client sends `{"RunAgent": {"session": "...", "name": "reviewer", "prompt": "look at auth.rs"}}`.
2. `run_agent_with_controls` resolves `extensions`/`def`/router/tools as above; a lookup failure
   returns before any event is emitted → `ServerMessage::Error`, connection stays open, no `seq`
   consumed (mirrors `RunCommand`'s pre-turn-failure contract exactly).
3. Once dispatch starts, events stream in the fixed order above, persisted before being sent
   (same fail-closed discipline every other turn path uses).
4. `report_turn_outcome` sends the terminal frame exactly as it does for `SendPrompt`/`RunCommand`.

## Error handling & concurrency

- Unknown `name`, or no `extensions` attached at all → pre-turn error, no `seq` consumed (same
  contract as `RunCommand`).
- A `Router::complete` failure inside `TaskTool::call` (e.g. remote provider error) → turn marked
  failed (`TurnComplete { ok: false }`, `SessionStatus::Failed`), matching how an orchestrator
  failure is reported for `SendPrompt`/`RunCommand` today. The connection stays open.
- `turn_lock` is still acquired (same field, shared with `run_prompt_with_controls`), so a
  `RunAgent` call and a `SendPrompt`/`RunCommand` turn can never run concurrently against the same
  session's workspace — even though `RunAgent` itself doesn't touch the workspace via `fs.write`,
  the dispatched agent's tool view could still include `fs.read`/`bash`, so serializing against
  other in-flight turns is the safer default and costs nothing (calls are already one-at-a-time
  per session).
- `Abort`/`Pause`/`Resume`/`ApproveDiff` arriving while a `RunAgent` call is in flight are simply
  processed after it completes (see `serve.rs` section) — there is no turn state for them to act
  on mid-call.

## Testing

- Protocol round-trip test for `Command::RunAgent` (serialize/deserialize), alongside the
  existing `Command` variant tests.
- `EngineService` unit tests (mirroring `run_command_with_controls`'s existing test shape):
  - no extensions attached → error, no events emitted, `store.next_seq` unchanged.
  - unknown agent name → error, no events emitted.
  - known agent → events observed in order (`AgentStarted`, `Log` containing the composed
    system-prompt + task-prompt text from a stub `Router`, `AgentFinished`, `TurnComplete { ok:
    true }`); `SessionStatus::Done`.
  - an agent whose `tools` allowlist excludes some base tool, dispatched with a probe agent (as
    `task_tool.rs`'s existing test double does) — confirms narrowing still applies through this
    path, not just the CLI's.
  - simulated `Router` failure → `TurnComplete { ok: false }`, `SessionStatus::Failed`, method
    returns `Err`.
- `serve.rs` integration test: a `.claude/agents/reviewer.md` fixture; drive `Command::RunAgent`
  over a test socket; assert the event sequence and that `SendPrompt`/`RunCommand` still work
  afterward on the same connection (regression: `turn_lock` isn't left poisoned/held).
- Regression: existing `SendPrompt`/`RunCommand` tests pass unmodified.
