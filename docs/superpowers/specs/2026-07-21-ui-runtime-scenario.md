# UI runtime spike #2 — frozen scenario contract

## Status

Written before any client run (Tasks 7–10 of the spike plan). **Frozen once the first client
run (Task 7) starts.** Any change made after that point must be appended below as a **dated
amendment** stating the reason — never a silent edit to the sections above it.

This document is produced by, and satisfies, Task 3 of the spike plan
(`docs/superpowers/spikes/2026-07-21-ui-runtime/`), consuming the ground-truth event
sequences captured in Task 2's server baseline
(`docs/superpowers/spikes/2026-07-21-ui-runtime/baseline/README.md`) and implementing
`docs/superpowers/specs/2026-07-21-ui-dioxus-runtime-spike-design.md`'s §The scenario
contract and §Measurements.

The four build identifiers used throughout this document and every run log are exactly:

- `leptos-web`
- `dioxus-web`
- `leptos-desktop`
- `dioxus-desktop`

## Shared engine configuration

One server configuration across all four runs, so client differences cannot be confounded
by server differences. Verbatim from the design's §Shared engine configuration:

- **Workspace fixture:** a fixed, disposable git repo created fresh per run from the
  committed `docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh <target-dir>` script —
  a Rust crate (`src/lib.rs` with an `add` function, `src/util/mod.rs` in a nested
  directory so the workspace tree has something to expand/collapse), one `.env` file (must
  be listed by the tree but never openable — confirms the sensitive-path floor renders as a
  denial rather than a crash), and a `cargo test`-runnable test the Verifier can actually
  run.
- **LLM:** fully offline-deterministic — no `OTTO_*` vars, no provider keys
  (`ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/`GEMINI_API_KEY` all unset, e.g. via `env -u`), both
  router slots resolve to `LocalProvider`. This makes event streams byte-comparable across
  clients, and makes the status strip's degraded-LLM state render (`capabilities.local_llm:
  false, remote_llm: false` in every `ready` frame) — slice B's interesting case.
- **Web runs:** `otto serve --root <fixture> --port <p> --approve-edits --promote-loopback`
  with a fixed `OTTO_TOKEN` and a fresh `OTTO_DB` (per the Task 2 baseline capture: `OTTO_TOKEN=spike-token
  OTTO_DB=/tmp/otto-ui-spike/<run>.db ./target/release/otto serve --root
  /tmp/otto-ui-spike/fixture --port 8899 --approve-edits --promote-loopback`, adjusting the
  DB filename per run so runs never share event-log state).
- **Desktop runs:** the app spawns its own sidecar, so flags cannot be passed directly. A
  wrapper script is staged in the sidecar path — `OTTO_BIN` for Dioxus
  (`ui-dioxus/src/desktop_boot.rs:70`), the `binaries/otto-<triple>` file for Tauri
  (`desktop/build-sidecar.sh`) — which execs the real binary with the app's own args plus
  `--approve-edits --promote-loopback`. Identical mechanism on both; no app code is changed.
- **Fallback:** if the wrapper shim misbehaves, the desktop scenario degrades to steps 1–8
  and 10 on *both* desktop builds equally, and the missing coverage is recorded rather than
  papered over.

### Finding recorded regardless of outcome

Neither shipped desktop app can reach diff approval or promote/demote as shipped, because
neither passes the flags that enable them: both spawn `otto serve --root <picked> --port
8787` and nothing more (`ui-dioxus/src/desktop_boot.rs:70-77`,
`desktop/src-tauri/src/lib.rs:45-53`). This is a gap in the shipped Tauri product, not a
Dioxus problem, and the spike surfaced it purely by asking what it would take to actually
run the thing. The wrapper-shim workaround above exists solely to let this spike drive
steps 9–11 on desktop; it is not a proposed product fix.

## The 11-step scenario contract

Committed **before any run**. Each step's pass assertion below is rewritten from the
design's generic form to name the concrete `EventKind` variants observed in the Task 2
server baseline (`baseline/turn.json`, `baseline/abort.json`, `baseline/approve.json`,
`baseline/promote.json`) — not an invented generic sequence.

Baseline fact carried through steps 3, 4, 9, 11: the offline-deterministic turn against the
fixture prompt (`Add a doc comment to the add function in src/lib.rs`) emits exactly this
ordered 11-frame `EventKind` sequence, `lastSeq: 10`:

```
AgentStarted -> Log -> AgentFinished ->
AgentStarted -> AgentFinished ->
AgentStarted -> AgentFinished ->
AgentStarted -> VerifyResult -> AgentFinished ->
TurnComplete
```

(one `AgentStarted`/`AgentFinished` pair per spine stage — Planner, ContextFinder, Coder,
Verifier — plus Planner's one `Log` and Verifier's one `VerifyResult`.)

| # | Slice | Step | Pass assertion |
|---|---|---|---|
| 1 | A | Connect (URL+token; autoconnect on desktop) | A `ready`/`Ready` frame is received (`capabilities.local_llm: false, remote_llm: false`); session id displayed in the UI |
| 2 | B | Status strip renders | engine/LLM/sandbox states shown; with both router slots offline, the LLM indicator visibly renders degraded/offline (not silently blank or "healthy") |
| 3 | A | Send prompt | The event stream renders live and in order, incrementally, starting with the turn's first frame (`AgentStarted`) and ending with its last (`TurnComplete`), matching the 11-frame sequence above frame-for-frame with no reordering, drop, or duplicate, ending at `lastSeq: 10` |
| 4 | A | Abort mid-turn | After `Abort` is sent, no further event frames arrive (the baseline observed no distinct "abort acknowledged" event — the stream simply stops emitting, as in `baseline/abort.json`'s truncation after the 4th `AgentStarted`, `lastSeq: 7`); the UI stops rendering new events and returns to an idle/ready state, not a wedged "waiting" state |
| 5 | A | Kill socket, reconnect with `last_seq` | On reconnect with `Last-Event-ID`/`last_seq`, the replayed tail of the same 11-frame sequence appears exactly once each — no duplicated frames, no gap in `seq` — ending again at `TurnComplete`/`lastSeq: 10` |
| 6 | C | Expand workspace tree | The nested `src/util/` directory expands/collapses; `.env` is listed in the tree but attempting to open it is denied (sensitive-path floor), not crashed |
| 7 | C | Open a source file | `src/lib.rs` content renders; syntax-highlighted on desktop, plain text on web (known, already-recorded gap — not a new bug) |
| 8 | C | Type into buffer | Typed edits appear in the local unsaved buffer; the buffer is visibly marked unsaved/dirty (honest about not having persisted) |
| 9 | D | Trigger an edit -> approve one, reject one | **NOT-APPLICABLE for all four builds** — see dedicated section below |
| 10 | E | Token meter + pause/resume | The token/cost meter updates as `AgentStarted`/`AgentFinished`/`VerifyResult` frames arrive; `Pause` halts further event rendering mid-stream, `Resume` continues it to `TurnComplete` |
| 11 | F | Promote to loopback, run a turn, demote back | After `TurnComplete`, sending `PromoteToRemote` yields a `promoted` `ServerMessage` frame (as in `baseline/promote.json`, `lastSeq: 10`); the UI reconnects to the handed-back endpoint, `seq` continues monotonically across the handover (no reset to 0, no gap), and engine/LLM/sandbox state in the status strip returns to the same rendered state as before promotion; a subsequent demote returns control to the original endpoint with the same continuity guarantee |

## How driven (web) / How asserted (desktop), per step

Web steps are driven in-page via Playwright: click, type, read the rendered DOM/console.
Desktop steps cannot attach to the WebKitGTK webview, so they are asserted by observable
external effects only: the sidecar process in the process table, rows in the `OTTO_DB`
sqlite store, files/edits on disk, and process-exit behavior. Any step whose only effect is
inside the window, with nothing observable outside it, is declared **NOT-VERIFIABLE
(desktop)** here, in advance, rather than discovered later.

| # | How driven (web) | How asserted (desktop) |
|---|---|---|
| 1 | Playwright navigates to the served page, fills URL+token fields (or confirms autoconnect), asserts the `Ready`/session-id text node appears in the DOM within a timeout | Confirm the app's own sidecar process (the wrapper shim exec'ing `otto serve`) is running and bound to its port (`ss`/`lsof` or `ps` on the PID the app spawned) and that the `OTTO_DB` sqlite file's `sessions` table gained exactly one new row at launch |
| 2 | Playwright reads the status-strip DOM nodes/classes for engine/LLM/sandbox and asserts the LLM node carries the degraded/offline visual state (class name or text) | Compositor screenshot of the desktop window showing the same degraded/offline status-strip rendering (visual-only; no external artifact exists for "did the strip render a certain way" beyond the screenshot itself, so this line **is** the desktop assertion, not a NOT-VERIFIABLE) |
| 3 | Playwright fills the prompt, submits, and asserts the sequence of rendered event DOM nodes matches the 11-frame sequence in order, with an in-page timestamp captured at first and last event for the latency measurement | Query `OTTO_DB`'s event-log table for the session: assert the persisted `EventKind` rows for the turn match the 11-frame sequence exactly and end at `seq = 10`; a compositor screenshot at the end confirms the window rendered the completed turn |
| 4 | Playwright triggers Abort mid-stream (after the 4th rendered `AgentStarted`, matching the baseline driver's trigger condition) and asserts no new event DOM nodes are appended for a fixed quiet window, then asserts the UI's idle/ready indicator is shown | Query `OTTO_DB` for the session's event-log row count before and after the abort-plus-quiet-window; assert it stops growing (no rows appended after the abort), matching the baseline's `lastSeq: 7` truncation-and-stop behavior |
| 5 | Playwright closes the WebSocket via `page.evaluate`, reopens the app/reconnect flow, and asserts each replayed event DOM node appears exactly once (no duplicate node, no `seq` gap) with an in-page timestamp for the reconnect-replay-time measurement | Query `OTTO_DB`'s event-log table for the session: assert row `seq` values are a contiguous, non-duplicated sequence spanning the pre-disconnect and post-reconnect events (the store, not the window, is authoritative for "exactly once") |
| 6 | Playwright clicks the `src/util` tree node, asserts it expands (child node visible) then collapses on a second click; asserts a click on `.env` yields a denial (error toast/inline message), not a rendered file body | NOT-VERIFIABLE (desktop) — tree expand/collapse and the `.env` denial are purely in-window UI state with no external artifact; recorded here in advance rather than discovered later |
| 7 | Playwright clicks `src/lib.rs` in the tree, asserts the file body text renders in the editor pane, and asserts a `<pre>`/highlighted-token structure on web vs desktop per the known highlighting gap | Compositor screenshot of the opened file pane confirms syntax highlighting rendered (this is the desktop evidence for the known web/desktop asymmetry — a screenshot, not a NOT-VERIFIABLE, since "does highlighting render" is exactly what the screenshot shows) |
| 8 | Playwright types into the editor buffer, asserts the typed text appears in the DOM and an unsaved/dirty indicator is shown | NOT-VERIFIABLE (desktop) — a local unsaved buffer has no external effect (nothing is written to `OTTO_DB` or disk until a save action that this scenario does not exercise); declared here in advance per the design's explicit call-out of step 8 |
| 9 | N/A — see NOT-APPLICABLE section below | N/A — see NOT-APPLICABLE section below |
| 10 | Playwright reads the token/cost meter DOM node before/during/after the turn and asserts it updates; clicks Pause mid-stream and asserts event-node appending halts; clicks Resume and asserts it continues to `TurnComplete` | Query `OTTO_DB` event-log row count: assert it stops growing while paused (no new rows appended) and resumes growing to completion after Resume — same external-effect test as step 4's abort, applied to pause/resume |
| 11 | Playwright submits a turn, asserts `TurnComplete` rendered, clicks Promote, asserts the UI reconnects and a "promoted" indicator/new endpoint is shown with `seq` continuing (not resetting) in the rendered event list; repeats for demote | Query the **loopback** target's `OTTO_DB` for a newly-created session with the handed-off event history (the promote primitive copies session state per `docs/ARCHITECTURE.md`'s promote/`LoopbackTarget` design); confirm the original sidecar process is still alive (loopback promotes to an in-process engine, it does not kill the source) and that a subsequent demote restores the session back into the source's `OTTO_DB` via `restore_over` |

## Step 9 — NOT-APPLICABLE

**Step 9 (trigger an edit -> approve one, reject one) is `NOT-APPLICABLE (offline Coder
proposes no edits)` for all four builds.**

Reason, per the Task 2 baseline (`baseline/README.md`, "ApprovalRequest decision gate"):
against the fixture prompt (`Add a doc comment to the add function in src/lib.rs`), and
against a second prompt tried specifically to provoke an edit (`Implement a new multiply
function in src/lib.rs that does not exist yet, with a doc comment.`), the
offline-deterministic Coder proposed **zero** edits both times — `grep -c ApprovalRequest
baseline/approve.json` returned `0` for both prompts, and the observed event sequence was
identical to the no-edit `turn`/`promote` captures. No `ApprovalRequest` event can fire on
this path, and no file is ever written by a turn under this configuration.

Consequently, every run log's step-9 row must record `NOT-APPLICABLE` with this reason —
never `PASS` and never a fabricated approve/reject observation. The diff-approval dimension
(slice D) is reported in the final spike report as **untested**, not as passing or failing,
across all four builds equally.

## Run-log schema

Every `docs/superpowers/spikes/2026-07-21-ui-runtime/results/<build>.md` (`<build>` one of
`leptos-web`, `dioxus-web`, `leptos-desktop`, `dioxus-desktop`) must contain exactly these
sections, in this order:

### `## Build`

Command used to build the artifact, wall-clock time for that build, resulting artifact
size(s) (wasm+js+css for web; binary for desktop), and toolchain/dependency versions
(rustc, `trunk`/`dx`/`cargo tauri` version, etc.).

### `## Environment`

Explicit confirmation that no provider keys were set (`ANTHROPIC_API_KEY`/
`OPENAI_API_KEY`/`GEMINI_API_KEY` all absent/unset for this run), the `OTTO_DB` path used,
and the port the server bound.

### `## Steps`

One row per step, table form:

| Column | Contents |
|---|---|
| `N` | step number, 1–11 |
| status | one of `PASS` / `FAIL` / `NOT-VERIFIABLE` / `NOT-APPLICABLE` |
| evidence | pasted output — DOM assertion result, sqlite query output, screenshot filename/path, process-table excerpt, etc. — not a paraphrase |
| notes | anything relevant: deviation from the "how driven/asserted" plan above, timing oddity, etc. |

Step 9 is always `NOT-APPLICABLE` (see above). Steps 6 and 8 are always `NOT-VERIFIABLE`
on desktop rows (see the per-step table above); they are driven and asserted normally on
web rows.

### `## Measurements`

One sub-entry per measure from the design's §Measurements (eight measures — see the
cross-check below), each showing all three repetitions and the reported median, with
spread noted:

- Web bundle size (wasm + js + css bytes, raw and gzipped) — web builds only
- Cold start -> first paint (in-page timing)
- Cold start -> `Ready` handled (in-page timing / first event timestamp)
- Event render latency (first-event-received -> last-event-painted, one fixed turn)
- Reconnect replay time (socket-open -> replay complete)
- Desktop RSS (app process tree only, sidecar excluded) — desktop builds only
- Desktop binary size (shipped artifact bytes) — desktop builds only
- Build wall-clock (clean release build)

A measure that is void per the design's honesty constraints (e.g. event latency voided by
a non-matching event count between clients) is recorded as `VOID` with the reason, not
omitted.

### `## Bugs`

One entry per runtime bug found, each recording:

- failing step (1–11)
- symptom
- cause class (for Dioxus: tracked-read / positional-hooks / teardown, per spike #1's
  taxonomy, or a new class if none fit; for Leptos/incumbent bugs: whatever class applies)
- could a compiler or test plausibly have caught it? (yes/no + why)
- fix commit
- fix wall-clock

## Cross-check against the design spec

Re-checked against `docs/superpowers/specs/2026-07-21-ui-dioxus-runtime-spike-design.md`'s
§The scenario contract and §Measurements:

**Steps** — all 11 present above:

1. Connect (URL+token; autoconnect on desktop) — covered
2. Status strip renders — covered
3. Send prompt — covered, rewritten with the concrete `AgentStarted`.../`TurnComplete`
   sequence and `lastSeq: 10`
4. Abort mid-turn — covered, rewritten to assert stream-stops/idle-return rather than an
   "abort acknowledged" event, matching the baseline's observed behavior
5. Kill socket, reconnect with `last_seq` — covered
6. Expand workspace tree — covered
7. Open a source file — covered
8. Type into buffer — covered, explicitly marked NOT-VERIFIABLE (desktop) in advance
9. Trigger an edit -> approve one, reject one — covered, marked NOT-APPLICABLE with the
   baseline's grounding
10. Token meter + pause/resume — covered
11. Promote to loopback, run a turn, demote back — covered

**Measurements** — the design lists eight measures; all eight appear in the run-log schema
above and are individually named:

1. Web bundle size
2. Cold start -> first paint
3. Cold start -> `Ready` handled
4. Event render latency
5. Reconnect replay time
6. Desktop RSS
7. Desktop binary size
8. Build wall-clock

Nothing missing on either list.
