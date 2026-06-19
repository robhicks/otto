# Sub-project E — Token/Cost Meter + Pause/Resume (design)

**Date:** 2026-06-19
**Roadmap:** `docs/superpowers/specs/2026-06-17-ui-roadmap.md` (row **E**)
**Status:** Approved design — ready for an implementation plan.

## Goal

Two live affordances for an in-flight turn:

1. **Token/cost meter.** As the turn runs, surface how many tokens it is consuming. The engine
   accounts real token usage from the providers that report it and emits a cumulative
   `TokenCostMeter` event; the UI shows running input/output token counts and an approximate
   dollar cost.
2. **Pause/Resume.** A human can pause a running turn and resume it. The turn parks at the next
   phase boundary and stays parked until resumed (or the connection drops).

Headless/CLI behavior and the offline determinism suite are unchanged: no usage data on the
offline path means no meter events, and the default `PauseController` never pauses.

## Where the current design shapes this

Five facts about the existing code drive every decision below:

1. **Usage is dropped at the provider boundary.** `Provider::complete`
   (`crates/engine-core/src/traits.rs`) returns `CompleteResponse { text }` — just text. The
   real Anthropic response carries `usage.{input,output}_tokens` and Ollama carries
   `prompt_eval_count`/`eval_count`, but both impls (`crates/providers/src/anthropic.rs`,
   `ollama.rs`) parse only the text and discard usage. `LocalProvider`/`ScriptedProvider` (the
   offline path) have no usage at all.

2. **Agents drop usage too.** Agents call `ctx.router().complete(req, hints).await?` and use
   only `completion.text`. Usage never reaches the orchestrator through the agent path, so the
   meter cannot piggy-back on `AgentOutput`.

3. **The router is a clean pass-through seam.** `SingleProviderRouter` and `BrainBlendRouter`
   (`crates/router/src/lib.rs`) return the provider's `CompleteResponse` unchanged. A decorating
   `Router` is the natural place to observe every completion without touching agents.

4. **`run_turn` is a straight sequential async fn.** `Orchestrator::run_turn`
   (`crates/engine-core/src/orchestrator.rs`) runs Plan → ContextFinder → Coder → gate edits →
   Verify, looping to Repair, with inline `await`s — not a state machine. The boundaries
   *between* agent calls are clean checkpoints. The orchestrator already owns **all** event
   emission (`emit.emit(EventKind)`); agents and tools never emit.

5. **Serve already reads the socket concurrently with the turn.** `handle_socket`
   (`crates/engine/src/serve.rs`) splits the socket and drives the turn inside a `tokio::select!`
   loop that also reads inbound `ApproveDiff`/`Abort` frames (sub-project D). `Pause`/`Resume`
   route through the exact same loop with no new structural change. Note: `abort` today does
   **not** cancel the running task — it marks session status and the turn runs to completion.
   Pause/Resume inherits that model (it parks the task; it does not kill it).

## Decisions (locked)

- **Offline meter: none.** `usage` is an `Option` that only metered providers populate
  (Anthropic, Ollama). `LocalProvider`/`ScriptedProvider` return `None`. With no usage, the
  cumulative total stays 0 and the orchestrator emits **no** `TokenCostMeter` event — the offline
  determinism event stream is byte-for-byte unchanged. The meter is meaningful only when real
  tokens are spent.
- **Cost lives in the UI; the wire carries integer tokens.** `TokenCostMeter` carries cumulative
  `input_tokens`/`output_tokens` as `u64`. The UI derives an approximate dollar cost from the
  remote-model identity it **already holds** — sub-project B's `CapabilitiesManifest.remote_llm`,
  delivered in the `Ready` frame. `protocol` stays serde + integer-only; no floats, no price
  table in the engine.
- **Meter granularity: per phase boundary.** The orchestrator emits an updated cumulative meter
  after each `AgentFinished` (so 4–6 updates per turn). Emission stays in the orchestrator, where
  all events originate — the metering router only tallies, it never emits.
- **Pause granularity: cooperative, at phase boundaries.** The orchestrator checks a shared pause
  signal between agents and parks there until resumed. It cannot interrupt an in-flight LLM HTTP
  call; a pause requested mid-phase takes effect when that phase ends.
- **Pause scope: connection-scoped, in-memory.** No `Paused` session status, no checkpoint
  persistence, no resume-across-reconnect. On disconnect/abort the pause is released so the parked
  task can unwind (matching today's "abort doesn't truly cancel" semantics).
- **Pause uses `Log` events, no new event variant.** The orchestrator emits `Log { "turn paused" }`
  when it parks and `Log { "turn resumed" }` when it unparks, so the persisted log records it. No
  dedicated pause event.

## Wire changes (`crates/protocol`, additive / semver-minor)

```rust
// Command — two new variants
Pause  { session: SessionId },
Resume { session: SessionId },

// EventKind — one new variant
/// Cumulative token usage for the current turn, emitted as the turn progresses. Only fires
/// when a metered provider reported usage; the offline path emits none. The UI renders the
/// counts and derives an approximate cost from the remote model in the capabilities manifest.
TokenCostMeter { input_tokens: u64, output_tokens: u64 },
```

Both ride the existing `Command` / `ServerMessage::Event` JSON paths — no transport-framing
change. Round-trip tests added beside the existing ones. An out-of-step peer that never sends
`Pause`/`Resume` sees a normal turn; one that never reads `TokenCostMeter` simply ignores it.

## Token accounting (`engine-core`, `providers`, `router`)

### Usage on the response

`crates/engine-core/src/types.rs`:

```rust
/// Token usage reported by a provider for one completion. Absent for providers that do not
/// report it (the offline `LocalProvider`/`ScriptedProvider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

pub struct CompleteResponse {
    pub text: String,
    pub usage: Option<Usage>,   // <-- new; None on the offline path
}
```

- **Anthropic** (`anthropic.rs`): parse the response's `usage { input_tokens, output_tokens }`
  into `Some(Usage { .. })`. (`MessagesResponse` gains a `usage` field.)
- **Ollama** (`ollama.rs`): map `prompt_eval_count` → input, `eval_count` → output into
  `Some(Usage { .. })` (both default to 0 if the field is absent).
- **Local / Scripted**: `usage: None`.

Every construction of `CompleteResponse` in the tree is updated (mechanical: add `usage: None`
to the test/offline ones).

### The shared meter

`crates/engine-core` gains a small concrete accumulator (atomics, cheap to share):

```rust
#[derive(Default)]
pub struct TokenMeter { input: AtomicU64, output: AtomicU64 }

impl TokenMeter {
    pub fn add(&self, u: &Usage) { /* fetch_add both */ }
    pub fn snapshot(&self) -> (u64, u64) { /* (input, output) */ }
    pub fn total(&self) -> u64 { /* input + output, to gate emission */ }
}
```

### The metering router decorator (`crates/router`)

```rust
pub struct MeteringRouter { inner: Arc<dyn Router>, meter: Arc<TokenMeter> }

#[async_trait]
impl Router for MeteringRouter {
    async fn complete(&self, req: CompleteRequest, hints: RouteHints)
        -> anyhow::Result<CompleteResponse>
    {
        let resp = self.inner.complete(req, hints).await?;
        if let Some(u) = &resp.usage { self.meter.add(u); }
        Ok(resp)   // pass through unchanged
    }
}
```

It observes every completion, tallies real usage, and is invisible to agents (they still get
`CompleteResponse`). No agent or `AgentOutput` change.

### Emission (`orchestrator.rs`)

`Orchestrator` gains `meter: &'a TokenMeter`. After each `AgentFinished` emission, it emits a
cumulative meter **only when there is usage to report**:

```rust
let (input_tokens, output_tokens) = self.meter.snapshot();
if self.meter.total() > 0 {
    emit.emit(EventKind::TokenCostMeter { input_tokens, output_tokens });
}
```

Offline (`Local`/`Scripted`, `usage: None`) the total stays 0 and nothing is emitted — the
offline event stream is unchanged. The emit stays synchronous through the existing `Emitter`, so
ordering, seq assignment, and persistence are untouched.

## Pause/Resume (`engine-core`, `engine`)

### The pause seam (`engine-core`, beside `Approver`)

```rust
#[async_trait]
pub trait PauseController: Send + Sync {
    /// Sync peek at a phase boundary: is a pause currently requested?
    fn should_pause(&self) -> bool;
    /// Park until resumed (or released on disconnect/abort). Returns promptly if not paused.
    async fn wait_for_resume(&self);
}

/// Default: never pauses (CLI/headless/offline).
pub struct NeverPause;
```

`Orchestrator` gains `pauser: &'a dyn PauseController`. At each phase boundary (before Planner,
before ContextFinder, and at the top of each Coder/Verify repair iteration) it runs one
checkpoint helper:

```rust
if self.pauser.should_pause() {
    emit.emit(EventKind::Log { message: "turn paused".into() });
    self.pauser.wait_for_resume().await;
    emit.emit(EventKind::Log { message: "turn resumed".into() });
}
```

`NeverPause::should_pause()` returns `false`, so the helper is a no-op on every non-serve path —
the offline stream and CLI behavior are unchanged.

### Turn controls bundle (`engine`)

The turn now carries two injected controls. To avoid signature sprawl, bundle them:

```rust
pub struct TurnControls {
    pub approver: Arc<dyn Approver>,
    pub pauser: Arc<dyn PauseController>,
}
impl Default for TurnControls { /* DenyApprover + NeverPause */ }
```

`EngineService::run_prompt_with_approver` is generalized to
`run_prompt_with_controls(session, goal, sink, controls: TurnControls)`; the convenience
`run_prompt` passes `TurnControls::default()`. In the spawned turn task, service.rs wraps the
shared router in `MeteringRouter`, constructs a per-turn `TokenMeter`, and hands the orchestrator
`&metering_router`, `&meter`, `controls.approver.as_ref()`, and `controls.pauser.as_ref()`.
Sub-project D's `run_prompt_with_approver` callers/tests migrate to `TurnControls` (mechanical).

### Connection-scoped pause state + routing (`serve.rs`)

Mirrors D's `ApprovalRegistry`/`InteractiveApprover`:

```rust
struct PauseState { paused: AtomicBool, resume: Notify }   // Arc-shared, connection-scoped
struct InteractivePauser(Arc<PauseState>);                  // impl PauseController

// PauseController::should_pause  -> self.0.paused.load(...)
// PauseController::wait_for_resume -> while paused { self.0.resume.notified().await }
```

In the `select!` loop, alongside `ApproveDiff`/`Abort`:

| Inbound frame | Action |
|---|---|
| `Pause`  | `pause_state.paused.store(true)` |
| `Resume` | `pause_state.paused.store(false)`; `pause_state.resume.notify_waiters()` |
| disconnect / `Abort` | release: `paused.store(false)` + `notify_waiters()` (so a parked turn unwinds) — in addition to the existing `approvals.clear()` |

The `PauseState` is created once per connection (like `ApprovalRegistry`) and an
`InteractivePauser` over it is placed in `TurnControls` for each `SendPrompt`. Disjoint-borrow
reasoning is identical to D: the turn future borrows the writer; `reader.next()` borrows the
reader; the pause state is `Arc`-shared and mutated from the reader arm.

## UI (`ui/`)

- **Decode** `TokenCostMeter` in `ws.rs`; hold a `meter: Option<(u64, u64)>` signal in `app.rs`,
  updated on each event.
- **Meter readout** in the existing status strip (sub-project B): running
  `↑ <input> ↓ <output> tok` plus `~$<cost>` when derivable. Cost comes from a pure
  `fn cost_estimate(input: u64, output: u64, remote: &RemoteLlm) -> Option<f64>` keyed off the
  capabilities manifest the UI already holds; unknown/absent remote model ⇒ tokens only.
- **Pause/Resume control** beside the existing Abort button: a local `paused` signal toggles the
  button label and sends `Command::Pause` / `Command::Resume` over the existing socket. The
  `Log { "turn paused" }` / `"turn resumed"` lines appear in the event stream as confirmation.
- **Pure, host-tested helpers** in `view_model.rs`: `fn format_meter(input, output) -> String`
  and `fn cost_estimate(..)`, tested like `describe_event` / `diff_lines`. The `ui` crate still
  depends only on `protocol` (+ existing `kode-leptos`/`gloo-net`).

## Testing

- **providers:** wiremock responses including `usage`/`eval_count` fields → assert
  `CompleteResponse.usage == Some(Usage { .. })`; Local/Scripted → `None`.
- **router:** `MeteringRouter` over a scripted inner provider — usage tallies into the shared
  `TokenMeter`; a `None`-usage response adds nothing; pass-through returns text unchanged.
- **engine-core (`orchestrator.rs`):**
  - A scripted provider returning `Usage` → assert `TokenCostMeter` emitted after agents with
    cumulative (monotonic) totals; with `usage: None` (offline default) → **no** `TokenCostMeter`
    in the stream (guards the offline invariant).
  - A fake `PauseController` that reports paused for the first checkpoint then resumes → assert
    `Log{"turn paused"}` / `Log{"turn resumed"}` bracket the park and the turn still completes;
    `NeverPause` → neither log, stream unchanged.
- **engine (serve integration, ephemeral port):** with a usage-returning scripted Coder —
  connect, prompt, observe `TokenCostMeter` events; send `Pause`, observe the stream stalls (no
  further `AgentStarted`), send `Resume`, observe completion; a disconnect-while-paused run does
  not hang (release path).
- **ui (host):** `format_meter` formatting; `cost_estimate` over priced/unknown remote models;
  the meter reducer (set on event) and the `paused` toggle.

## Out of scope (deferred)

- Persisted pause (`Paused` status), resume-across-reconnect, and true mid-call cancellation.
- Per-provider / per-model cost breakdown — the UI estimate applies one remote rate to all
  counted tokens (a clearly-labeled approximation; mixed local+remote turns slightly overstate).
- Per-agent or per-call meter granularity (we emit per phase boundary).
- A persisted price table or cost figure on the wire (cost stays a UI-derived display value).

## Invariants this must not break

- **Offline determinism is untouched:** no usage ⇒ no `TokenCostMeter`; `NeverPause` ⇒ no pause
  logs. Default `serve`, CLI `run`, and the offline suite produce the same event stream as before.
- The orchestrator remains the **sole** emitter of events; the metering router only tallies.
- `protocol` depends only on serde and stays integer-only on the wire; `ui` depends only on
  `protocol` (+ its existing UI deps).
- Pause is cooperative and fail-safe: a parked turn is always released on disconnect/abort, so a
  paused turn can never wedge the connection.
- The security spine is untouched: the edit gate, sensitive-path floor, and D's approval path are
  unchanged; metering and pause add no new tool or disk path.
