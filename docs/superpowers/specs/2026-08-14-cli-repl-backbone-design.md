# CLI REPL backbone — the `cli` crate, the `ClientTransport` seam, and conversation history

> **Status:** PROPOSED — slice 1 of the local-development product track.
> **Blocks:** slice 2 (interactive diff review), slice 3 (honest provider config), slice 4
> (two-mode turn routing), slice 5 (session resume). Every later slice hangs off the crate,
> the seam, and the history introduced here.
> **Reserves:** the `cli` crate that `docs/ARCHITECTURE.md:44` has listed as intended-but-absent
> since the architecture doc was written.

otto's engine is far ahead of every surface a local developer touches. The distribution axis is
complete end to end — promote/demote, Fly, microVM, TOTP multitenancy, per-session handover
secrets — while the local loop is still what it was at the beginning: `otto run "<goal>"` runs
exactly one turn, prints `println!("[{:>3}] {:?}", event.seq, event.kind)`, and exits
(`crates/engine/src/main.rs:581`). That is a debug harness, not a product.

This slice builds the backbone of the interactive CLI: a `cli` crate that speaks the wire
protocol through a transport seam, and — the part that cannot be bolted on afterward —
conversation history in the orchestrator spine.

---

## Premise corrections

Each assumption that did not survive contact with the repository, corrected here.

1. **"Add a REPL loop around `run_turn`" is not a small change, because the spine has no memory.**
   `Orchestrator::run_turn(&self, _session: SessionId, goal: &str, emit: &dyn Emitter)`
   (`crates/engine-core/src/orchestrator.rs:80`) takes a bare goal string. The session id is
   underscore-prefixed and **unused**. Every turn is independent: turn 2 has no knowledge that
   turn 1 happened. A REPL over this spine would let a user say "now add tests for that" and
   have the Planner ask "for what?". Conversation history is therefore in scope for slice 1, not
   deferrable to the session-resume slice.

2. **The event log cannot reconstruct the plan.** The natural design — derive history from the
   persisted event log, which already holds everything — does not work as stated. The
   orchestrator emits `EventKind::Log { message: format!("planned {} milestone(s)", …) }` and
   then **discards the milestone text** (`crates/engine-core/src/orchestrator.rs:112`). A
   log-derived history would know that turn 1 planned three milestones and not what any of them
   were. The log also carries no agent output text at all — only agent lifecycle events, edited
   file paths with byte counts, approval requests, verify results, and token meters. History
   must therefore join the log against `TurnRecord`, and the milestones need a persisted home
   (§2.2).

3. **The interactive approval round-trip already exists on the wire; it does not need
   inventing.** `EventKind::ApprovalRequest { id, path, old, new }` carries the file's full
   current and proposed contents — enough to render a real diff — and `Command::ApproveDiff
   { session, id, approved }` is its reply (`crates/protocol/src/lib.rs:240`, `:107`).
   `TurnControls` already exposes `approver: Arc<dyn Approver>` and `pauser: Arc<dyn
   PauseController>` seams with `DenyApprover`/`NeverPause` defaults
   (`crates/engine/src/service.rs:48`). This slice wires the round-trip through the transport
   and leaves it auto-denying; **slice 2 adds the diff renderer.** No new protocol variants
   here.

4. **A WebSocket-backed CLI would have a hostile first run.** `otto serve` defaults to
   `AuthMode::Users` and refuses to start with zero enrolled principals, so a CLI that reached
   the engine through a spawned `serve` sidecar would require `otto auth enroll <user>` before
   `cd repo && otto` worked at all. The default transport must be in-process.

---

## Scope

**In scope**

- A new `crates/cli` library crate; `otto` with no subcommand starts the REPL.
- The `ClientTransport` seam and one implementation, `EmbeddedTransport`.
- Conversation history: `SessionHistory`/`TurnSummary`, derived from `TurnRecord` + the event
  log; milestones persisted into `TurnOutcome`; threaded to the Planner and ContextFinder via
  `AgentCtx`.
- Readable streamed rendering of every `EventKind`.
- Ctrl-C interrupt, Ctrl-D exit, readline history.

**Out of scope (named slices, not omissions)**

- Interactive diff review and the approval UX — **slice 2**. This slice wires the round-trip and
  auto-denies, matching today's headless posture.
- Provider/config story, including the fake-success offline first run — **slice 3**.
- Conversational (non-task) turn routing — **slice 4**. Every REPL input in this slice runs the
  full Plan → Context → Code → Verify spine.
- `--continue` / `--resume` — **slice 5**.
- `WsTransport` — a later slice. The seam ships with one implementation, deliberately.
- `otto run` behavior. Unchanged, byte for byte.

---

## Goal & Success Criteria

1. `cd <repo> && otto` opens an interactive prompt with no configuration, no port, no auth
   enrollment, and no sidecar process.
2. A second prompt in the same session reaches the Planner with knowledge of the first — the
   history is populated, bounded, and derived from durable state.
3. Event output is human-readable prose, not `{:?}`.
4. Ctrl-C during a turn cancels it and returns to the prompt; Ctrl-C at an idle prompt, or
   Ctrl-D, exits cleanly.
5. The REPL loop compiles against `Command`/`ServerMessage` only — it must not name
   `EngineService`, `Orchestrator`, or `ToolRegistry` in any signature.
6. **The existing offline suite passes unchanged**, with no test edited to accommodate a changed
   prompt.
7. The whole new suite runs offline: no network, no API keys, no TTY, no PTY.

---

## Assumptions

- `TurnRecord { turn_index, goal, outcome }` (`crates/persistence/src/types.rs:42`) is written
  every turn and is carried inside `SessionState`, so anything stored there survives
  promote/demote for free.
- `SessionStore::replay_since(owner, session, None)` returns the full event log for a session.
- The offline providers (`LocalProvider`, `ScriptedProvider`) perform no I/O, so any
  determinism guarantee here rests on prompt construction alone.

---

## 1. `crates/cli` — the crate boundary

A new **library** crate. Dependency flow stays strictly inward: `cli` → `otto-protocol` +
`otto-engine`. Nothing depends on `cli` except the binary.

The `otto` binary stays in `engine`. `main.rs`'s existing dispatch
(`run`/`serve`/`auth`/`plugin`/`version`/`help`) is untouched; the **no-subcommand** arm — today
an error — becomes `otto_cli::repl(root).await`. Every existing invocation behaves identically.

### 1.1 The crate must not reach past the protocol

```rust
#[async_trait]
pub trait ClientTransport: Send {
    async fn send(&mut self, cmd: Command) -> anyhow::Result<()>;
    async fn recv(&mut self) -> Option<ServerMessage>;
}
```

The REPL loop is written entirely against `Command` / `ServerMessage`. It never sees
`EngineService`, `Orchestrator`, `ToolRegistry`, or `Workspace`.

This is a design constraint with two distinct payoffs, and the second is the load-bearing one:

- It makes the CLI a genuine second client of the protocol rather than a privileged one, which
  is what makes the README's claim — "the frontend never branches on 'local vs remote'" — true
  rather than aspirational. It is currently exercised by exactly one client.
- **It is what makes the REPL loop testable.** Against a fake `ClientTransport` returning canned
  `ServerMessage`s, the loop can be driven with scripted input and no terminal. Without the
  seam, testing the loop means testing a live engine through a PTY.

Enforcement is by convention plus review, as with the other seams — `cli`'s `Cargo.toml` depends
on `otto-engine` (it must, to construct `EmbeddedTransport`), so the compiler cannot forbid the
REPL module from reaching further. The `repl` module's function signatures are the reviewable
surface.

### 1.2 `EmbeddedTransport`

Owns an `EngineService` built exactly as `cmd_run` builds its dependencies today
(`build_router`, `build_composed_tools`, `SqliteStore::open`, `build_retriever`), so extensions,
permissions, hooks, skills, and plugin MCP servers compose identically to `otto run`.

- `send(Command)` dispatches: `CreateSession` → `EngineService::create_session`; `SendPrompt` →
  spawn `run_prompt_with_controls` on a task; `Abort` → `EngineService::abort`; `ApproveDiff` →
  route to the pending approver.
- The turn's `EventSink` pushes each event into an mpsc channel as
  `ServerMessage::Event`; `recv()` reads that channel.
- `TurnControls.approver` is a channel-backed approver of the same shape as `serve.rs`'s
  `InteractiveApprover` (`crates/engine/src/serve.rs:521`): it emits `ApprovalRequest` outward
  and awaits a matching `ApproveDiff`. In this slice the REPL replies `approved: false`
  immediately, preserving the current headless posture until slice 2.
- No socket, no port, no bind address, no `AuthMode`, no promotion secret. The owner is
  `UserId::local()`, matching every other single-machine path.

---

## 2. Conversation history

### 2.1 Shape

```rust
pub struct TurnSummary {
    pub turn_index: u32,
    pub goal: String,
    pub milestones: Vec<String>,
    pub files_edited: Vec<PathBuf>,
    pub verify: Option<VerifySummary>,   // ok + detail
    pub ok: bool,
}

pub struct SessionHistory { turns: Vec<TurnSummary> }
```

**History is derived from `TurnRecord` alone — there is no event-log join.** The orchestrator
already knows every one of these facts at the end of a turn: it produced the milestones, it
emitted each `FileEdit`, and it emitted the `VerifyResult`. So `TurnOutcome` carries them
(§2.2), `record_turn` persists the whole outcome, and building history is a single read of the
session's turn records.

This replaces an earlier design that joined `TurnRecord` against `replay_since` for the edited
paths and verify result. Deriving both from one row is cheaper (no event-log scan per turn),
simpler to test, and removes a class of skew where the log and the outcome could disagree.

**No new store table, no new event variant, no wire change.** Because `TurnRecord` already rides
inside `SessionState`, history survives promote/demote with no additional work.

### 2.1.1 The store needs one new read method

`SessionStore` can write turn records (`record_turn`) but cannot read them back. The only
accessor is `snapshot()`, which copies the session's entire event log — far too heavy to call
before every turn. `SessionStore` therefore gains:

```rust
/// The session's completed turn records in ascending turn_index order. Scoped by owner:
/// returns an empty Vec for an unknown session and, identically, for a session `owner` does
/// not own — matching `replay_since`'s non-oracle contract.
async fn turns(
    &self,
    owner: &otto_protocol::UserId,
    session: SessionId,
) -> anyhow::Result<Vec<TurnRecord>>;
```

Owner-scoping and the empty-on-unauthorized behavior deliberately mirror `replay_since`, so this
method cannot become an existence oracle for another principal's sessions.

### 2.2 `TurnOutcome` carries the turn's summary

`TurnOutcome` is today `{ ok: bool }` (`crates/engine-core/src/orchestrator.rs:27`). It gains the
three facts history needs, all of which the orchestrator already holds:

```rust
pub struct TurnOutcome {
    pub ok: bool,
    pub milestones: Vec<String>,
    pub files_edited: Vec<PathBuf>,
    pub verify: Option<VerifySummary>,
}
```

`TurnOutcome` is persisted as `TurnRecord.outcome: serde_json::Value`, so this needs **no schema
migration**. One catch the implementation must not miss: `record_turn` currently writes a
hand-built `serde_json::json!({ "ok": outcome.ok })` (`crates/engine/src/service.rs:305`) rather
than serializing the outcome, so that construction site must switch to serializing `TurnOutcome`
or the new fields will be silently dropped on write.

Storing milestones here is deliberately chosen over widening the `Log` message to carry milestone
prose. `TurnOutcome` is structured, already durable, and already promote-safe. Widening `Log`
would be stringly-typed, would have to be re-parsed to be useful, and would put semantic content
into a message that is explicitly untranslated passthrough on the UI boundary.

### 2.3 Threading to agents

Per the CLAUDE.md convention — *"add a capability by extending `AgentCtx` (private fields +
accessors), never by widening a struct's public surface"* — `AgentCtx` gains a private history
field and a `history()` accessor, following the existing `with_retriever` pattern.
`Orchestrator::run_turn` gains the history parameter and `_session` stops being unused.

**Consumers in this slice: `Planner` and `ContextFinder` only.** The Planner needs to know what
already happened; for the ContextFinder, previously-edited files are a strong retrieval signal.
`Coder` and `Verifier` are untouched — they get history when there is evidence they need it, not
before.

### 2.4 Two invariants, enforced as tests

1. **Empty history is byte-identical to today.** With no prior turns, every agent prompt must be
   the exact string it is today. This is the safety rail on a change that touches prompt
   construction for the whole spine, and it is why success criterion 6 forbids editing existing
   tests to accommodate a changed prompt. Implementation: the history block is omitted entirely —
   not rendered as an empty section — when there are no prior turns.
2. **History is bounded.** The last `HISTORY_TURNS` (10) turns, with `files_edited` capped per
   turn. A 200-turn session must not produce a 200-turn prompt. The test asserts prompt length
   stops growing once the cap is reached.

---

## 3. The REPL

- **Entry:** `otto` with no subcommand, in the cwd; `--root <path>` honored.
- **Line editing:** `rustyline` — one new dependency. `inquire` (already in `engine` at 0.7) is a
  prompt library, not a readline: no persistent up-arrow history across prompts, no emacs
  keybindings, awkward multiline. Those are table stakes for a tool used all day. `inquire` is
  not replaced; it stays where it is for the plugin TUI.
- **Session lifecycle:** `Command::CreateSession` on launch, one fresh session per invocation.
- **Turn:** each input is sent as `Command::SendPrompt`; events render as they stream; the prompt
  returns on `TurnComplete`.
- **Interrupt:** Ctrl-C during a turn sends `Command::Abort` and returns to the prompt. Ctrl-C at
  an idle prompt, or Ctrl-D, exits cleanly. Ctrl-C is deliberately **not** mapped to
  `Command::Pause`: in a REPL, Ctrl-C means cancel, and a pause the user must discover how to
  resume is a worse default. `Pause`/`Resume` remain on the wire, unused by this slice.

### 3.1 Rendering is a pure function

`render(&EventKind) -> Vec<Line>`, in its own module, with no terminal I/O and no global state.
Every `EventKind` variant gets a unit test with no TTY and no snapshot harness. Color is
hand-rolled ANSI gated on `NO_COLOR` and isatty, rather than a dependency.

`ServerMessage::Error` and `EventKind::Log` / `VerifyResult.detail` render **verbatim**. They are
server-originated diagnostics, and the CLI does not reformat or interpret them.

### 3.2 The CLI is English-only

`ui-dioxus`'s i18n boundary does not extend to this crate. There is no catalog, no `t`/`tf`, and
no locale. This is stated explicitly so that a later reader of CLAUDE.md's localization rules
does not conclude the catalog is missing keys.

---

## 4. Error Handling & Edge Cases

| Case | Behavior |
|---|---|
| Not a git repo / empty dir | Allowed. The workspace is any directory, as with `otto run`. |
| Store open fails | Fail before the first prompt with the store's error. Do not start a REPL that cannot persist. |
| Invalid `*_BASE_URL` | `preflight_base_urls()` before the first prompt, exactly as `cmd_run` does — refuse to start rather than silently degrade to the canned offline provider. |
| Turn returns `Err` | Render the error, return to the prompt. A failed turn never exits the REPL. |
| Ctrl-C between events | Abort is idempotent; a second Ctrl-C during teardown exits. |
| stdin is not a TTY | Read lines to EOF and run them as turns, then exit. Keeps the REPL scriptable and testable. |
| Approval requested | Auto-denied with a rendered note saying the edit was skipped and that review lands in slice 2. Never silently dropped. |

---

## 5. Semver

- **New crate** `otto-cli` at `0.1.0`.
- `otto-engine-core` — **breaking**: `Orchestrator::run_turn` gains a parameter; `AgentCtx` gains
  a field. Minor bump under the pre-1.0 convention the workspace already uses, flagged in the PR.
- `otto-engine-core` — also breaking: `TurnOutcome` gains three fields, so every construction
  site and exhaustive match updates. Same minor bump.
- `otto-persistence` — **breaking**: `SessionStore` gains `turns()` (§2.1.1). Any implementor
  must add it; within the workspace that is `SqliteStore` plus test fakes. 0.1.0 → 0.2.0.
- **No database schema change, and this is a hard constraint on the design.**
  `TurnRecord.outcome` is an opaque `serde_json::Value`, so the new outcome fields need no
  column, no migration, and **no `PRAGMA user_version` bump**. The store refuses to open when its
  version does not match, so a schema break would force every existing user to delete their
  session database — far too high a price for plan text. Turn rows written before this change
  deserialize with the new fields absent, which `#[serde(default)]` must cover.
- `otto-protocol` — **unchanged.** No new variants, no field changes.

---

## 6. Testing

All offline: no network, no API keys, no TTY, no PTY.

| Layer | Approach |
|---|---|
| Rendering | Pure unit tests over every `EventKind` variant; `NO_COLOR` on and off |
| `EmbeddedTransport` | Driven with `ScriptedProvider`; assert `Command` in → `ServerMessage` out, including the approval round-trip |
| History construction | Fixture store with N `TurnRecord`s + event log → expected `SessionHistory`; the bounding cap |
| Prompt invariant | Empty history yields the byte-identical prompt string to today (§2.4.1) |
| REPL loop | Fake `ClientTransport` + scripted input; asserts turn dispatch, abort, and clean exit |
| Regression | The full existing `cargo test --workspace` suite, unedited |

---

## 7. Risks & Open Questions

1. **Prompt-shape regression risk is the real risk of this slice.** Threading history into the
   Planner and ContextFinder changes prompt construction for the two agents whose offline
   fallback output the existing suite asserts on. §2.4.1 is the mitigation and must be
   implemented first, before any history is threaded — the empty-history-is-identical test should
   be written against the *unmodified* prompt builders and kept green through the change.
2. **History quality is untested against real models.** The summaries are compact by design
   (goal, milestones, files, verify result — no agent prose). Whether that is enough context for
   a real model to handle "now add tests for that" is an empirical question this slice cannot
   answer offline. Accepted: the shape is cheap to widen later, and widening it is additive.
3. **`HISTORY_TURNS = 10` is a guess.** It is a named constant so it can be tuned once there is
   usage evidence.
4. **The seam ships with one implementation.** That is a deliberate YAGNI call, but it does mean
   the abstraction is unvalidated by a second transport until the `WsTransport` slice. The
   testability argument in §1.1 is what justifies it independently.
5. **`stdin`-not-a-TTY behavior overlaps `otto run`.** Piping a goal into `otto` and running
   `otto run "<goal>"` will do nearly the same thing by different paths. Acceptable for now;
   worth revisiting if the two diverge.
