# Dioxus UI-axis Spike — Comparison Report

**Date started:** 2026-07-11
**Design:** `2026-07-11-ui-dioxus-spike-design.md`
**Status:** 🚧 In progress — rows appended as slices land.

## Per-slice effort log

| Slice | View/reactivity LOC | Pure-logic LOC | Wall-clock | `cfg` edge-gates | Notes |
|---|---|---|---|---|---|
| A — app shell + live session | 246 (`app.rs` 154 + `event_log.rs` 14 + `prompt_bar.rs` 38 + `connection_form.rs` 40) | 0 (reused from Task 2) | ~50 subagent-min (incl. review fix-pass) | 11 total (all in `transport/`; the review fix removed the 3 yield-helper edges) | Both targets compile clean; `net::` regression 45/45. No `Sink`-storage target-split needed. Post-review: added `Sink::close()` + a generation-guarded per-connection drain task (idle poll loop eliminated) — see Fix pass. |

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
