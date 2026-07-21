# Dioxus runtime-verification spike (#2) — Design

**Date:** 2026-07-21
**Status:** ✅ DESIGN COMPLETE — approved during brainstorming. Ready for **writing-plans**.
**Predecessor:** [`2026-07-11-ui-dioxus-spike-design.md`](2026-07-11-ui-dioxus-spike-design.md) /
[`2026-07-11-ui-dioxus-spike-report.md`](2026-07-11-ui-dioxus-spike-report.md)

## Context

Spike #1 built `ui-dioxus/` — a parallel browser+desktop Dioxus client at parity with the shipped
Leptos `ui/` slices A–F, plus a Dioxus-native replacement for the Tauri `desktop/` wrapper. It
returned **"inconclusive, leaning keep-Leptos"**, and named its own decisive weakness: *nothing was
ever run*. No browser was opened, no desktop window was driven, no folder picker was clicked, no
file was typed into either editor. Its strongest pro-Dioxus evidence — Task 13's desktop
auto-connect, the thing that would actually retire Tauri — was compile-verified only. All three
real bugs the spike caught compiled clean, which leaves an unquantified risk that more such bugs
sit in code that has never executed.

Spike #1's own closing recommendation was a second, runtime-driven spike before any adoption
decision. This is that spike.

## Decisions locked during brainstorming

- **Q1 scope = both clients, one scenario.** Drive `ui-dioxus/` *and* the shipped Leptos
  `ui/`+`desktop/` through one identical scenario. This closes the "never ran" gap and
  simultaneously produces the Leptos runtime baseline spike #1 admitted it never had — without it,
  parity effort stays a wash by ignorance rather than by measurement.
- **Q2 desktop verification = automated observable-effects + screenshots.** No human at the
  keyboard; no WebKit-inspector tooling gamble.
- **Q3 scenario = full A–F, including promote/demote.** Specifically includes the handover
  reconnect, the exact path where spike #1 found a compile-clean bug.
- **Q4 verdict form = narrative** (not a pre-registered decision rule). See the caveat below.
- **Q5 evidence = runtime bug log + hard performance numbers.** No visual side-by-side.
- **Q6 bug policy = fix and log, with effort recorded.**
- **Execution approach = C, "written scenario contract, live execution"** — commit the contract
  first, then execute it live against each build (Playwright for web; process/sqlite/disk probes
  plus compositor screenshots for desktop). Not a maintained harness: the contract is a document,
  and the report is the deliverable.

### Caveat on the verdict form

With a narrative verdict and no pre-registered decision rule, the honest failure mode is a second
"inconclusive, leaning keep." The written-first scenario contract is the mitigation — it fixes what
counts as passing while the outcome is still unknown — but it does not fully substitute for a
decision rule. If the evidence comes back genuinely mixed, the report says so plainly rather than
manufacturing a verdict.

## §Goal & non-goals

**Goal:** run all four builds — Leptos web (`ui/`), Leptos desktop (`desktop/`, Tauri), Dioxus web
(`ui-dioxus --features web`), Dioxus desktop (`ui-dioxus --features desktop`) — through one
identical, pre-written scenario contract against one shared engine configuration; collect a runtime
bug log and hard performance numbers; and produce a narrative adopt-vs-keep verdict grounded in
what executed rather than what compiled.

**Non-goals:**

- **Not a migration.** No change to `ui/` or `desktop/` beyond fixing runtime bugs the scenario
  exposes in them.
- **No engine or protocol changes.** Spike #1's headline finding was that none were needed. If this
  spike finds it needs one, that is a finding to surface loudly and escalate — not to quietly make.
- **Not closing the web-highlighting gap** (spike #1's deferred Task 12). It remains a known
  asymmetry and is recorded as one.
- **Not a maintained test suite.** The scenario contract is a document, not code that outlives the
  decision.
- **No new target, no new slice.** Same two targets, same slices A–F.

**Deliverable:** `docs/superpowers/specs/2026-07-21-ui-dioxus-runtime-spike-report.md`, carrying the
verdict, cross-linked with spike #1's report so the two read as one thread.

## §Shared engine configuration

One server configuration across all four runs, so client differences cannot be confounded by server
differences.

- **Workspace fixture:** a fixed, disposable git repo created fresh per run from a committed setup
  script — a few source files in a language the tree-sitter set covers (Rust/JS/TS/Python/Go), one
  nested directory (exercises the collapsible tree), one `.env` file (confirms the sensitive-path
  floor renders as a denial rather than a crash), and a test command the Verifier can actually run.
- **LLM:** fully offline-deterministic — no `OTTO_*` vars, no provider keys, both router slots
  `LocalProvider`. This makes event streams byte-comparable across clients, and makes the status
  strip's degraded-LLM state render, which is slice B's interesting case.
- **Web runs:** `otto serve --root <fixture> --port <p> --approve-edits --promote-loopback` with a
  fixed `OTTO_TOKEN`.
- **Desktop runs:** the app spawns its own sidecar, so flags cannot be passed directly. A wrapper
  script is staged in the sidecar path — `OTTO_BIN` for Dioxus
  (`ui-dioxus/src/desktop_boot.rs:70`), the `binaries/otto-<triple>` file for Tauri
  (`desktop/build-sidecar.sh`) — which execs the real binary with the app's own args plus
  `--approve-edits --promote-loopback`. Identical mechanism on both; no app code is changed.
- **Fallback:** if the wrapper shim misbehaves, the desktop scenario degrades to steps 1–8 and 10 on
  *both* desktop builds equally, and the missing coverage is recorded rather than papered over.

### Finding recorded regardless of outcome

Neither shipped desktop app can reach diff approval or promote/demote, because neither passes the
flags that enable them: both spawn `otto serve --root <picked> --port 8787` and nothing more
(`ui-dioxus/src/desktop_boot.rs:70-77`, `desktop/src-tauri/src/lib.rs:45-53`). This is a gap in the
shipped Tauri product, not a Dioxus problem, and the spike surfaced it purely by asking what it
would take to actually run the thing.

## §The scenario contract

Committed **before any run** to `docs/superpowers/specs/2026-07-21-ui-runtime-scenario.md`. Numbered
steps, each with an explicit pass assertion.

| # | Slice | Step | Pass assertion |
|---|---|---|---|
| 1 | A | Connect (URL+token; autoconnect on desktop) | `Ready` received; session id displayed |
| 2 | B | Status strip renders | engine/LLM/sandbox states shown; LLM shows degraded/offline visibly |
| 3 | A | Send prompt | `AgentStarted` etc. stream live, in order, incrementally |
| 4 | A | Abort mid-turn | stream stops; UI returns to idle, not wedged |
| 5 | A | Kill socket, reconnect with `last_seq` | replayed events appear exactly once — no duplicates, no gap |
| 6 | C | Expand workspace tree | nested dir expands/collapses; `.env` listed but unopenable |
| 7 | C | Open a source file | content renders; highlighted on desktop, plain on web (known gap) |
| 8 | C | Type into buffer | edits appear locally; unsaved state honest |
| 9 | D | Trigger an edit → approve one, reject one | approved edit lands on disk; rejected one does not |
| 10 | E | Token meter + pause/resume | meter updates; pause halts stream, resume continues |
| 11 | F | Promote to loopback, run a turn, demote back | reconnects to handed-back endpoint; seq continues across handover; state returns |

Steps 5 and 11 carry the most weight: they are where spike #1's compile-clean bugs lived.

## §Execution mechanics

Four runs, each executing the same contract:

1. **Leptos web** — `trunk build --release`, served; driven via Playwright.
2. **Dioxus web** — `dx build --release --features web`, served; driven via Playwright.
3. **Leptos desktop** — `desktop/build-sidecar.sh` then `cargo tauri build`; launched directly.
4. **Dioxus desktop** — `dx build --release --features desktop`; launched directly.

Web steps are driven in-page: click, type, assert on rendered DOM. Desktop steps are driven by
observable effects, since the WebKitGTK webview cannot be attached to — each assertion is checked
against what the system actually did: sidecar process present and bound, session/event rows in the
sqlite store, edited file contents on disk, process tree gone after window close. Compositor
screenshots confirm the window rendered and reached the expected state. Steps that are purely
in-window with no external effect (step 8's local unsaved buffer) are marked **not verifiable on
desktop** rather than assumed passing.

All four artifacts are built **before** any scenario is driven, so toolchain friction surfaces early
and cheap rather than mid-run.

## §Measurements

Release builds, same machine, no competing load, three repetitions, median reported with spread
noted.

| Measure | Where | How |
|---|---|---|
| Web bundle size | after build | wasm + js + css bytes, raw and gzipped |
| Cold start → first paint | step 1 | in-page timing |
| Cold start → `Ready` handled | step 1 | in-page timing / first event timestamp |
| Event render latency | step 3 | first-event-received → last-event-painted, one fixed turn |
| Reconnect replay time | step 5 | socket-open → replay complete |
| Desktop RSS | after step 3 | app process tree only; sidecar measured separately and excluded |
| Desktop binary size | after build | shipped artifact bytes |
| Build wall-clock | per build | clean release build; toolchain data point |

Two honesty constraints:

1. **Desktop RSS excludes the sidecar.** Both apps spawn the same `otto serve`; including it would
   silently equalize the Tauri-vs-Dioxus shell overhead that is the actual thing being compared.
2. **Event latency is comparable only because the LLM is offline-deterministic** — the same prompt
   must yield the same event count on both clients. If it does not, the measure is void and is
   reported as void.

## §Bug policy

Fix and log. Each runtime bug records: the failing step, the cause class (for Dioxus, the
tracked-read / positional-hooks / teardown classes spike #1 identified), whether a compiler or test
could plausibly have caught it, and the fix wall-clock. Fixes land in the client crate only.

A bug that can **only** be fixed by changing `protocol` or the engine stops the spike and is
escalated — that would overturn spike #1's headline finding that the server boundary is genuinely
client-agnostic.

Bugs found in `ui/`/`desktop/` are logged and fixed identically. If the incumbent has runtime bugs
too, that is directly relevant evidence; counting only the challenger's would be dishonest.

## §Report structure

Written incrementally as runs complete, not reconstructed at the end (spike #1's discipline, which
worked):

1. **What ran** — the four builds, versions, machine, which steps executed and which were marked
   not-verifiable and why.
2. **Step matrix** — 11 steps × 4 builds: pass / fail / not-verifiable.
3. **Runtime bug log** — per the bug policy, with cause class and fix cost.
4. **Measurements** — medians with spread.
5. **Findings** — including the two spike-#1 claims this spike must re-check at runtime: did any
   protocol/engine change become necessary (spike #1: no), and does the Dioxus desktop app genuinely
   replace Tauri when actually launched (spike #1: compile-verified only).
6. **Verdict** — narrative: adopt / keep / mixed, reasoning from sections 2–5.
7. **Disposition** — per the rules below.

## §Disposition

Named up front, while the outcome is unknown, so the verdict does not get to invent its own
consequences:

- **Adopt** → a follow-up migration plan is written (not executed in this spike); `ui/` and
  `desktop/` stay shipped until it lands.
- **Keep** → `ui-dioxus/` is **deleted**, with the report as the durable record of why. Parking a
  second unmaintained client is how a repo accumulates a graveyard; the report is what has lasting
  value, not the code.
- **Mixed** → say so plainly, name the specific remaining unknown, and recommend either a bounded
  third probe of just that unknown or dropping the question — never a general "spike again."

## §Risks

- **WebKitGTK / `dx` toolchain friction on Fedora** could consume the desktop leg. Mitigated by
  building all four artifacts before driving any scenario.
- **The wrapper-script shim** may not survive Tauri's sidecar validation. Mitigated by the
  documented equal degradation on both desktop builds.
- **Driver bias** — knowing `ui-dioxus/`'s internals better than `ui/`'s could mean trying harder on
  one. Mitigated by the contract being written first with identical assertions for both; any
  deviation in driving is recorded.
