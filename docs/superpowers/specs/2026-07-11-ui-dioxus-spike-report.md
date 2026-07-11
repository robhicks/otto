# Dioxus UI-axis Spike — Comparison Report

**Date started:** 2026-07-11
**Design:** `2026-07-11-ui-dioxus-spike-design.md`
**Status:** 🚧 In progress — rows appended as slices land.

## Per-slice effort log

| Slice | View/reactivity LOC | Pure-logic LOC | Wall-clock | `cfg` edge-gates | Notes |
|---|---|---|---|---|---|
| A — app shell + live session | 246 (`app.rs` 154 + `event_log.rs` 14 + `prompt_bar.rs` 38 + `connection_form.rs` 40) | 0 (reused from Task 2) | ~50 subagent-min (incl. review fix-pass) | 11 total (all in `transport/`; the review fix removed the 3 yield-helper edges) | Both targets compile clean; `net::` regression 45/45. No `Sink`-storage target-split needed. Post-review: added `Sink::close()` + a generation-guarded per-connection drain task (idle poll loop eliminated) — see Fix pass. |
| B — capabilities status strip | 38 (`status_line.rs`) + ~10 (`app.rs` wiring: 1 signal decl + 5 set/clear call-sites + import + render line) ≈ 48 total | 0 new (`capability_segments`/`status_label`/`short_session` reused verbatim from Task 2) | ~15 subagent-min | 11 total, unchanged (no platform edge added — `capabilities` is a plain cross-platform `Signal<Option<CapabilitiesManifest>>`, threaded through the existing generation-guarded drain task exactly like `conn`/`sink`) | Both targets compile clean; `net::` regression 45/45; `cargo fmt --check` clean. One real (non-plan) fix: `last_seq.read().map(...)` doesn't compile directly (`Ref<Option<u64>>` isn't `ToString`) — needed `(*last_seq.read()).map(...)`, matching the deref pattern `app.rs` already uses for `last_seq` elsewhere. |
| D — diff approval panel | 44 (`approval_panel.rs`) + 56 (`app.rs` wiring: 1 signal decl + `EventKind` import + `ApprovalRequest`/`TurnComplete` arms inside the `should_apply` guard + `Error`/`Closed`&#124;`Errored`/`disconnect`/top-of-`do_connect` clear sites + the `decide` closure + render) + 2 (`components/mod.rs` export) ≈ 102 total | 0 new (`diff_lines`/`DiffKind` reused verbatim from Task 2 — pure-logic layer untouched) | ~20 subagent-min | 11 total, unchanged (pure view logic + signal wiring; no new platform edge) | Both targets compile clean; `net::` regression 45/45; `cargo fmt --check` clean. Placed strictly inside the existing generation-guarded per-connection drain task (Task 4's shape), not the plan's single `use_future` — `pending_approval` sets on `ApprovalRequest` and clears on `TurnComplete` both live *inside* the `should_apply` replay guard (mirroring `ui/src/app.rs:85-108` exactly, so a stale replayed request from an already-finished turn is never resurrected); clears additionally on `ServerMessage::Error`, on the `Closed`&#124;`Errored` arm, in `disconnect()`, and at the top of `do_connect()` before a reconnect — five clear sites total, matching Leptos's five. The `decide` closure reads `sink`/`session` directly (not through the existing `send` helper, which discards its `Result`) so it can gate the panel's clear on actual send success, exactly mirroring `ui/src/app.rs:263-282`'s fail-open-panel-stays-up-on-failure contract. |
| E — token/cost meter + pause/resume | 21 (`status_line.rs` diff: `meter` prop + render block) + 9 (`prompt_bar.rs` diff: `paused`/`on_pause`/`on_resume` props + single-button toggle) + 53 (`app.rs` diff: `meter`/`paused` signal decls + `TokenCostMeter` arm inside the `should_apply` guard + `TurnComplete`/`Error`/`Closed`&#124;`Errored`/`disconnect`/top-of-`do_connect` reset sites + `pause`/`resume` closures + `send_prompt`/`abort` resets + render wiring) ≈ 83 total | 0 new (`format_meter`/`cost_estimate` REUSED verbatim from `net::view_model`, already tested in Task 2) | ~20 subagent-min | 11 total, unchanged (no new platform edge — `meter`/`paused` are plain cross-platform `Signal<Option<(u64,u64)>>`/`Signal<bool>`, threaded through the same generation-guarded drain task) | Both targets compile clean; `net::` regression 45/45; `cargo fmt --check` clean. `meter`/`paused` reset at six sites, matching (and in two spots exceeding) Leptos's: top-of-`do_connect`, the `Closed`&#124;`Errored` arm, `disconnect()`, `send_prompt` (new turn), `abort`, plus `TurnComplete` and — one correctness addition beyond the brief's literal list — the `ServerMessage::Error` arm, since an Error frame is turn-terminal exactly like `ui/src/app.rs`'s Error handler (which also clears `paused`/`turn_running`) and skipping it would leave Pause/Resume stuck on "Resume" after a mid-pause turn error. Cost estimate renders only when `capabilities().remote_llm` is true, matching `ui/src/components/status_line.rs`. One-line parity cost: with the pure helpers and the drain-task shape already settled from prior slices, wiring a second turn-scoped signal pair (meter + paused) alongside `pending_approval` was almost free — the pattern (declare, set on event, clear at N reset sites, pass as a prop) repeats verbatim, so the main cost was re-deriving which of Leptos's reset sites are semantically required versus incidental. |
| F — promote/demote + handover reconnect | 78 (`app.rs` diff only: `turn_running`/`reconnect_to` signal decls + `AgentStarted`→true / `TurnComplete`/`Error`/`Closed`&#124;`Errored`/`disconnect`/top-of-`do_connect`→false reset sites + a `Promoted`&#124;`Demoted` drain-task arm setting `reconnect_to` + `promote_remote`/`demote_local` closures + a `use_effect` handover-reconnect trigger + the gated handover buttons' render) | 0 new (`can_promote`/`can_demote` REUSED verbatim from `net::view_model`, already tested in Task 2 — 45/45 includes their existing 2 tests) | ~20 subagent-min | 11 total, unchanged (no new platform edge — `turn_running`/`reconnect_to` are plain cross-platform `Signal<bool>`/`Signal<Option<String>>`) | Both targets compile clean; `net::` regression 45/45; `cargo fmt --check` clean. Two real (non-plan) fixes needed beyond the brief's literal snippet: `url` needed `let mut` to be `.set()`-able from the new effect (E0596), and the pre-existing `SocketEvent::Message(Ok(_)) => {}` catch-all became a genuine `unreachable_patterns` warning once `Promoted`/`Demoted` were matched explicitly (all 5 `ServerMessage` variants are now covered), so it was deleted rather than left as dead code. The handover reconnect **routes through the same hardened `do_connect`** Task 4 built (bumps `generation`, closes the old sink, opens the new socket, spawns a fresh generation-guarded drain task) — no second reconnect path was hand-rolled. Reactivity DX note (a genuine Dioxus gotcha — see Fix pass 2): the effect MUST *reactively read* `reconnect_to` to subscribe to it. `use_effect(move || { if let Some(endpoint) = reconnect_to() { reconnect_to.set(None); url.set(endpoint); do_connect(); } })` — the tracked `reconnect_to()` read is what registers the dependency, so the drain task's later `reconnect_to.set(Some(endpoint))` re-fires the effect. Order is deliberate and bounded: read (subscribe) → `set(None)` (clear) → connect; the `set(None)` re-runs the effect once more, which reads `None`, skips the body, and settles — no loop. This mirrors `ui/src/app.rs`'s handover `Effect` (`reconnect_to.get()` — likewise a tracked read). Two real (non-plan) compile fixes were also needed: `url` needed `let mut` to be `.set()`-able from the effect (E0596), and the pre-existing `SocketEvent::Message(Ok(_)) => {}` catch-all became an `unreachable_patterns` warning once `Promoted`/`Demoted` were matched explicitly (all 5 `ServerMessage` variants are now covered), so it was deleted. **Corrected DX data point:** an initial attempt used `reconnect_to.write().take()` — a *write-guard* access (mem::take through a `Write`), which does NOT subscribe an effect in Dioxus 0.7 (only reactive reads do). That version captured zero deps on its first `None` run and never re-fired, so the reconnect was dead. The fix (tracked read first) is the correct pattern and matches Leptos's tracked-read effect exactly. |
| C.1 — workspace tree + unhighlighted editor (web+desktop) | 55 (`components/file_tree.rs`, new) + 37 (`editor/mod.rs`, new) + 89 (`app.rs` diff: `tree`/`open_file`/`editor_seed` signal decls + `load_files`/`open_path` closures + the auto-load `use_effect` + the workspace/refresh/`FileTree`/`Editor` render block) + 2 (`components/mod.rs` export) + 1 (`main.rs` `mod editor;`) ≈ 184 total | 0 new (`net::tree`'s `TreeNode`/`build_tree`/`decode_or_binary`/`FileBody`/`language_for_path` and `net::url::ws_to_http_base` all REUSED verbatim from Task 3 — this slice is the first thing to actually *call* `language_for_path`'s sibling helpers at runtime, though highlighting itself is deferred to part 2) | ~25 subagent-min | 11 total, unchanged (no new platform edge — the `transport::{list_files,read_file}` facades already existed per-target from Task 3; this slice just calls them from the reactivity spine for the first time) | Both targets compile clean (`web`/wasm and, notably, `desktop` — **the first slice where `app.rs` code actually drives the `reqwest`-backed `/workspace` RPC path**, not just type-checking dead code); `net::` regression 45/45; `cargo fmt --check` clean. Unification data point: the native desktop client's `reqwest::Client` hits `/workspace` directly with no browser origin, so **CORS is irrelevant on this target** — the tower-http CORS layer the engine added for the web client (per `CLAUDE.md`'s sub-project C note) has zero bearing on desktop, one more piece of evidence that a single Dioxus client can absorb what were two separate concerns (web CORS vs. native direct-HTTP) under one code path. Wiring followed the CURRENT generation-guarded `spawn`-per-task app.rs shape, not the brief's `use_future` pseudo-code: `load_files`/`open_path` are ordinary closures that `spawn` a one-shot async RPC call (mirroring the drain task's `spawn` idiom, not a standing loop), and the auto-load effect uses a **tracked read of `conn`** (`use_effect(move || { if matches!(conn(), ConnState::Connected { .. }) { load_files(); } })`) — the same tracked-read discipline the F-slice handover effect already established, called out explicitly in the brief as a Critical-class bug to avoid repeating. `open_path` no-ops unless `Connected`, matching `ui/src/app.rs`'s guard exactly. |

**Leptos baseline (for comparison):** `ui/` totals — measure with
`tokei ui/src` or `wc -l ui/src/**/*.rs` and record here once, split the same way.

## Narrative evidence (not scored)

### DX / reactivity
_(notes)_

**Slice A (this task).** The Dioxus analogue of Leptos's `forget()`-leaked-closures-writing-signals
pattern is a single long-running `use_future` that owns a `loop { … }` draining an
`UnboundedReceiver<SocketEvent>` held *inside a signal* (`incoming`). `connect()` (an ordinary
event handler) installs a fresh receiver by `incoming.set(Some(rx))`; the drain future notices it
on its next poll via `incoming.write().take()`, then owns the receiver locally (not through the
signal) for the lifetime of that connection, `.await`ing `rx.next()` in a plain `while let`. On
`Closed`/`Errored` it `break`s back to the outer `loop`, drops the exhausted receiver, and goes
back to polling `incoming` for the next `connect()`.

Compared to Leptos's approach in `ui/src/app.rs` (raw `web_sys::WebSocket` callbacks registered
with `set_onmessage`/`set_onclose`/`set_onerror`, then `.forget()`'d so they outlive the closure
that created them, each one calling `RwSignal::set` directly from the callback) the Dioxus version
is **structurally simpler and safer**: there is exactly one long-lived task instead of three
independently-leaked closures per connection, no `Closure::forget()` (a real leak Leptos's own
comments call out and work around by manually detaching the old socket's handlers before
reconnecting), and no risk of a late callback from a stale socket firing after a new `connect()`
— the drain loop only ever owns one receiver at a time, and the *old* receiver was already dropped
via `take()` before the new one is installed, so nothing needs the old socket's callbacks to be
manually unhooked. The tradeoff is one extra layer of indirection to reason about (a receiver
*stored in a signal*, taken out into a local variable, so `incoming` is empty most of the time —
by design it holds a value only in the narrow window between `connect()` writing it and the drain
future's next poll picking it up); once that shape is understood it reads as a mailbox handoff,
not magic.

One correction versus the brief's literal code: the brief's loop called
`futures_util::future::poll_fn(|_| Poll::Ready(())).await` immediately before the yield helper —
that's a no-op (it resolves instantly, contributing nothing beyond what the yield already does), so
it was dropped; the yield call alone is sufficient to hand control back to the executor between
poll attempts.

A second correction: the brief's closures `send`/`send_prompt` needed `let mut send = move |..|`
(not a bare `let send = ...`) even though the closure only touches `Signal`s (which mutate via
`Copy` + interior mutability, not `&mut self`) — the compiler still classifies a closure that calls
another *previously-defined* closure by value (`send(cmd)` inside `send_prompt`'s body) as needing
mutable capture of that inner closure binding. This is ordinary Rust closure-capture behavior, not
a 0.6→0.7 API change, but it's an easy trap when porting the brief's pseudo-code verbatim — `cargo
build` catches it immediately as a hard error, so it never silently ships.

**Slice B (this task).** A near-mechanical parity port. The Leptos `StatusLine` in `ui/src/components/status_line.rs` is `RwSignal`-based and additionally renders a token/cost meter (a later Leptos slice); the Dioxus port scopes to this task's two signals (`conn`, `last_seq`, `capabilities`) and skips the meter row, matching the brief exactly. All three pure helpers (`capability_segments`, `status_label`, `short_session`) were already written and tested in Task 2's `net::view_model` port, so this slice was pure view + wiring: one new 38-line component plus five call-sites in `app.rs` (one `use_signal` declaration, one `Ready`-arm bind, three clear-sites mirroring the existing `sink`/`conn` reset pattern). No new `cfg` edge, no new dependency, no new async shape — the generation-guarded drain task from Task 4's review fix already had a slot for exactly this kind of "set on Ready, clear on every disconnect path" signal, so `capabilities` just slots in alongside `session`/`conn`. This is the DX data point worth recording: once the pure view-model layer is shared and the drain-task shape is settled, a Leptos-parity status strip is a ~15-minute port, not a re-design — the Dioxus and Leptos versions of this slice differ only in `rsx!`-vs-`view!` syntax and signal-read ergonomics (`Ref` deref vs `.get()`), not in structure.

### Build / toolchain (incl. WASM bundle size)
_(notes)_

**Slice A (this task).** Confirmed the pinned Dioxus 0.7.1 API against `context7`'s docs before
porting the brief's 0.6-era pseudo-code; the signals API (`use_signal`, `.read()`/`.write()`/
`.set()`/`.toggle()`, calling a signal like `signal()` to clone), `use_future` (fire-and-forget,
non-reactive — unlike `use_resource` it does not auto-rerun when a signal it reads changes, which
is exactly the "runs once, loops forever internally" shape the drain loop needs), `EventHandler<T>`
props, and event `.value()` on form events are all **unchanged from 0.6** for this task's purposes
— no signature adaptations were needed there. The one real research finding: async code must never
hold a signal `.read()`/`.write()` guard across an `.await` point (the docs call this out
explicitly and show it panics) — the brief's `incoming.write().take()` is safe because the guard
is a temporary dropped at the end of that statement, before the `while let Some(ev) = rx.next()`
loop's `.await`.

**Sink storage:** the brief flagged a suspected `Send` requirement for signal-held values on
`dioxus-desktop`'s multithreaded runtime, with a target-split (`Rc` web / `Arc<dyn Sink + Send +
Sync>` desktop) as the fallback. Tried the simplest thing first — a single `Signal<Option<Rc<dyn
Sink>>>` used on both targets — and it **compiled clean on `--features desktop`** with no
`Send`/`Sync` diagnostic at all; the only errors that build surfaced were ordinary borrow-checker/
closure-mutability issues (see DX notes), unrelated to signal storage. So Dioxus 0.7.9's signal
backing does not require `Send` for desktop in this configuration — no target split was needed,
and the suspected `cfg` edge for it never materialized. Flagging this as unconfirmed-at-runtime
(compile-only verified per this task's scope) in case interactive desktop testing later reveals a
`Send`-adjacent panic the type system didn't catch.

**Yield helper:** resolved as directed — `gloo_timers::future::TimeoutFuture::new(0)` on web,
`tokio::task::yield_now()` on desktop — plus a third arm, `std::future::pending::<()>()`, for the
"neither feature" configuration the `net::` regression gate builds under (mirrors Task 3's
"`transport/mod.rs` must type-check with no transport feature enabled" fix; without it the whole
crate — `main.rs` unconditionally has `mod app;` — wouldn't compile for `cargo test
--no-default-features net::`). `gloo-timers` (`features = ["futures"]`) was added as an optional
dep gated into the `web` feature; `futures-util` was promoted from desktop-only to a top-level
non-optional dependency since the drain loop's `StreamExt::next()` is now used on both targets.
One brief-vs-code cleanup: the brief's loop body called an extra
`futures_util::future::poll_fn(|_| Poll::Ready(())).await` immediately before the yield — a no-op
that does nothing the yield doesn't already do — so it was dropped rather than transcribed
verbatim.

`cargo fmt --all` reflowed the new files with no manual pass needed; `cargo build` for both targets
and `cargo test --no-default-features net::` (45/45, unchanged from Task 3's baseline) all ran in
well under a minute each on this machine — no `dx serve`/wasm-bundle-size data yet, since this
task's gate is compile-only (manual browser/desktop drive is explicitly deferred to a later joint
pass per the task instructions).

### Ecosystem / editor
_(notes)_

### Runtime perf
_(notes)_

**Slice A (post-review).** The original slice-A drain design was a single long-lived `use_future`
with an `outer loop { … cooperative-yield }` polling a signal-held receiver — which, while idle
(no connection), **idles as a throttled poll loop** (a zero-delay `TimeoutFuture`/`yield_now` every
wake), unlike Leptos's raw `web_sys` callbacks which are genuinely zero-poll (the browser event
loop wakes them; nothing spins). That was a real, measurable DX/perf regression versus the Leptos
baseline. The review fix **eliminated it**: the reworked design spawns a fresh drain task *per
connection* (dioxus `spawn`) that owns its receiver and blocks in `rx.next().await` — so while
disconnected there is **no task at all**, and while connected the task is parked on the receiver
(woken only by an actual inbound frame), exactly like the Leptos callbacks. Net result: the poll
loop is gone, the cooperative-yield helper (and its `gloo-timers` dep + 3 `cfg` edges) is deleted,
and Dioxus's idle/connected runtime-poll profile now matches Leptos's zero-poll callbacks. This is
a win worth recording — the drain-loop-over-a-signal-held-receiver pattern is *not* required; a
per-connection spawned task is both simpler and free of the idle spin.

## Priority gate ① — Multi-target unification
_(% shared tree, edge-gate total, and the yes/no: does the one crate replace `ui/` + `desktop/` + Tauri?)_

## Priority gate ② — Parity effort
_(total view/reactivity LOC + wall-clock vs the Leptos baseline)_

## Verdict
_(keep-Leptos / adopt-Dioxus / inconclusive, + the evidence that drove it)_
