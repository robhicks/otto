# Dioxus UI-axis Spike — Comparison Report

**Date started:** 2026-07-11
**Design:** `2026-07-11-ui-dioxus-spike-design.md`
**Status:** ✅ Complete.

## Per-slice effort log

| Slice | View/reactivity LOC | Pure-logic LOC | Wall-clock | `cfg` edge-gates | Notes |
|---|---|---|---|---|---|
| A — app shell + live session | 246 (`app.rs` 154 + `event_log.rs` 14 + `prompt_bar.rs` 38 + `connection_form.rs` 40) | 0 (reused from Task 2) | ~50 subagent-min (incl. review fix-pass) | 11 total (all in `transport/`; the review fix removed the 3 yield-helper edges) | Both targets compile clean; `net::` regression 45/45. No `Sink`-storage target-split needed. Post-review: added `Sink::close()` + a generation-guarded per-connection drain task (idle poll loop eliminated) — see Fix pass. |
| B — capabilities status strip | 38 (`status_line.rs`) + ~10 (`app.rs` wiring: 1 signal decl + 5 set/clear call-sites + import + render line) ≈ 48 total | 0 new (`capability_segments`/`status_label`/`short_session` reused verbatim from Task 2) | ~15 subagent-min | 11 total, unchanged (no platform edge added — `capabilities` is a plain cross-platform `Signal<Option<CapabilitiesManifest>>`, threaded through the existing generation-guarded drain task exactly like `conn`/`sink`) | Both targets compile clean; `net::` regression 45/45; `cargo fmt --check` clean. One real (non-plan) fix: `last_seq.read().map(...)` doesn't compile directly (`Ref<Option<u64>>` isn't `ToString`) — needed `(*last_seq.read()).map(...)`, matching the deref pattern `app.rs` already uses for `last_seq` elsewhere. |
| D — diff approval panel | 44 (`approval_panel.rs`) + 56 (`app.rs` wiring: 1 signal decl + `EventKind` import + `ApprovalRequest`/`TurnComplete` arms inside the `should_apply` guard + `Error`/`Closed`&#124;`Errored`/`disconnect`/top-of-`do_connect` clear sites + the `decide` closure + render) + 2 (`components/mod.rs` export) ≈ 102 total | 0 new (`diff_lines`/`DiffKind` reused verbatim from Task 2 — pure-logic layer untouched) | ~20 subagent-min | 11 total, unchanged (pure view logic + signal wiring; no new platform edge) | Both targets compile clean; `net::` regression 45/45; `cargo fmt --check` clean. Placed strictly inside the existing generation-guarded per-connection drain task (Task 4's shape), not the plan's single `use_future` — `pending_approval` sets on `ApprovalRequest` and clears on `TurnComplete` both live *inside* the `should_apply` replay guard (mirroring `ui/src/app.rs:85-108` exactly, so a stale replayed request from an already-finished turn is never resurrected); clears additionally on `ServerMessage::Error`, on the `Closed`&#124;`Errored` arm, in `disconnect()`, and at the top of `do_connect()` before a reconnect — five clear sites total, matching Leptos's five. The `decide` closure reads `sink`/`session` directly (not through the existing `send` helper, which discards its `Result`) so it can gate the panel's clear on actual send success, exactly mirroring `ui/src/app.rs:263-282`'s fail-open-panel-stays-up-on-failure contract. |
| E — token/cost meter + pause/resume | 21 (`status_line.rs` diff: `meter` prop + render block) + 9 (`prompt_bar.rs` diff: `paused`/`on_pause`/`on_resume` props + single-button toggle) + 53 (`app.rs` diff: `meter`/`paused` signal decls + `TokenCostMeter` arm inside the `should_apply` guard + `TurnComplete`/`Error`/`Closed`&#124;`Errored`/`disconnect`/top-of-`do_connect` reset sites + `pause`/`resume` closures + `send_prompt`/`abort` resets + render wiring) ≈ 83 total | 0 new (`format_meter`/`cost_estimate` REUSED verbatim from `net::view_model`, already tested in Task 2) | ~20 subagent-min | 11 total, unchanged (no new platform edge — `meter`/`paused` are plain cross-platform `Signal<Option<(u64,u64)>>`/`Signal<bool>`, threaded through the same generation-guarded drain task) | Both targets compile clean; `net::` regression 45/45; `cargo fmt --check` clean. `meter`/`paused` reset at six sites, matching (and in two spots exceeding) Leptos's: top-of-`do_connect`, the `Closed`&#124;`Errored` arm, `disconnect()`, `send_prompt` (new turn), `abort`, plus `TurnComplete` and — one correctness addition beyond the brief's literal list — the `ServerMessage::Error` arm, since an Error frame is turn-terminal exactly like `ui/src/app.rs`'s Error handler (which also clears `paused`/`turn_running`) and skipping it would leave Pause/Resume stuck on "Resume" after a mid-pause turn error. Cost estimate renders only when `capabilities().remote_llm` is true, matching `ui/src/components/status_line.rs`. One-line parity cost: with the pure helpers and the drain-task shape already settled from prior slices, wiring a second turn-scoped signal pair (meter + paused) alongside `pending_approval` was almost free — the pattern (declare, set on event, clear at N reset sites, pass as a prop) repeats verbatim, so the main cost was re-deriving which of Leptos's reset sites are semantically required versus incidental. |
| F — promote/demote + handover reconnect | 78 (`app.rs` diff only: `turn_running`/`reconnect_to` signal decls + `AgentStarted`→true / `TurnComplete`/`Error`/`Closed`&#124;`Errored`/`disconnect`/top-of-`do_connect`→false reset sites + a `Promoted`&#124;`Demoted` drain-task arm setting `reconnect_to` + `promote_remote`/`demote_local` closures + a `use_effect` handover-reconnect trigger + the gated handover buttons' render) | 0 new (`can_promote`/`can_demote` REUSED verbatim from `net::view_model`, already tested in Task 2 — 45/45 includes their existing 2 tests) | ~20 subagent-min | 11 total, unchanged (no new platform edge — `turn_running`/`reconnect_to` are plain cross-platform `Signal<bool>`/`Signal<Option<String>>`) | Both targets compile clean; `net::` regression 45/45; `cargo fmt --check` clean. Two real (non-plan) fixes needed beyond the brief's literal snippet: `url` needed `let mut` to be `.set()`-able from the new effect (E0596), and the pre-existing `SocketEvent::Message(Ok(_)) => {}` catch-all became a genuine `unreachable_patterns` warning once `Promoted`/`Demoted` were matched explicitly (all 5 `ServerMessage` variants are now covered), so it was deleted rather than left as dead code. The handover reconnect **routes through the same hardened `do_connect`** Task 4 built (bumps `generation`, closes the old sink, opens the new socket, spawns a fresh generation-guarded drain task) — no second reconnect path was hand-rolled. Reactivity DX note (a genuine Dioxus gotcha — see Fix pass 2): the effect MUST *reactively read* `reconnect_to` to subscribe to it. `use_effect(move || { if let Some(endpoint) = reconnect_to() { reconnect_to.set(None); url.set(endpoint); do_connect(); } })` — the tracked `reconnect_to()` read is what registers the dependency, so the drain task's later `reconnect_to.set(Some(endpoint))` re-fires the effect. Order is deliberate and bounded: read (subscribe) → `set(None)` (clear) → connect; the `set(None)` re-runs the effect once more, which reads `None`, skips the body, and settles — no loop. This mirrors `ui/src/app.rs`'s handover `Effect` (`reconnect_to.get()` — likewise a tracked read). Two real (non-plan) compile fixes were also needed: `url` needed `let mut` to be `.set()`-able from the effect (E0596), and the pre-existing `SocketEvent::Message(Ok(_)) => {}` catch-all became an `unreachable_patterns` warning once `Promoted`/`Demoted` were matched explicitly (all 5 `ServerMessage` variants are now covered), so it was deleted. **Corrected DX data point:** an initial attempt used `reconnect_to.write().take()` — a *write-guard* access (mem::take through a `Write`), which does NOT subscribe an effect in Dioxus 0.7 (only reactive reads do). That version captured zero deps on its first `None` run and never re-fired, so the reconnect was dead. The fix (tracked read first) is the correct pattern and matches Leptos's tracked-read effect exactly. |
| C.1 — workspace tree + unhighlighted editor (web+desktop) | 55 (`components/file_tree.rs`, new) + 37 (`editor/mod.rs`, new) + 89 (`app.rs` diff: `tree`/`open_file`/`editor_seed` signal decls + `load_files`/`open_path` closures + the auto-load `use_effect` + the workspace/refresh/`FileTree`/`Editor` render block) + 2 (`components/mod.rs` export) + 1 (`main.rs` `mod editor;`) ≈ 184 total | 0 new (`net::tree`'s `TreeNode`/`build_tree`/`decode_or_binary`/`FileBody`/`language_for_path` and `net::url::ws_to_http_base` all REUSED verbatim from Task 3 — this slice is the first thing to actually *call* `language_for_path`'s sibling helpers at runtime, though highlighting itself is deferred to part 2) | ~25 subagent-min | 11 total, unchanged (no new platform edge — the `transport::{list_files,read_file}` facades already existed per-target from Task 3; this slice just calls them from the reactivity spine for the first time) | Both targets compile clean (`web`/wasm and, notably, `desktop` — **the first slice where `app.rs` code actually drives the `reqwest`-backed `/workspace` RPC path**, not just type-checking dead code); `net::` regression 45/45; `cargo fmt --check` clean. Unification data point: the native desktop client's `reqwest::Client` hits `/workspace` directly with no browser origin, so **CORS is irrelevant on this target** — the tower-http CORS layer the engine added for the web client (per `CLAUDE.md`'s sub-project C note) has zero bearing on desktop, one more piece of evidence that a single Dioxus client can absorb what were two separate concerns (web CORS vs. native direct-HTTP) under one code path. Wiring followed the CURRENT generation-guarded `spawn`-per-task app.rs shape, not the brief's `use_future` pseudo-code: `load_files`/`open_path` are ordinary closures that `spawn` a one-shot async RPC call (mirroring the drain task's `spawn` idiom, not a standing loop), and the auto-load effect uses a **tracked read of `conn`** (`use_effect(move || { if matches!(conn(), ConnState::Connected { .. }) { load_files(); } })`) — the same tracked-read discipline the F-slice handover effect already established, called out explicitly in the brief as a Critical-class bug to avoid repeating. `open_path` no-ops unless `Connected`, matching `ui/src/app.rs`'s guard exactly. |
| C.2 — Dioxus-native styled-span editor substrate (web+desktop) | 46 (`editor/mod.rs` diff: `pub mod tokens;` + the `Text` arm rewritten as a two-layer `editor-stack` — `pre.editor-highlight` of `for line`/`for sp` spans overlaid by a transparent `textarea.editor-overlay`, `plain_spans` called as a plain fn inside the match arm; **fix pass** added a 3rd hoisted `use_signal(|| None::<MountedEvent>)` + `onmounted` ref-capture on the `pre` + an `onscroll` scroll-mirror on the textarea) + 61 (`style.css` diff: `.editor`/`.editor-path` base rules + `.editor-stack`/`.editor-highlight`/`.editor-overlay`/`.hl-line` alignment rules + `.tok-plain`/`.tok-keyword`/`.tok-string`/`.tok-comment`/`.tok-type`/`.tok-number` color classes; fix pass added the `.hl-line` line-height-coupling note) ≈ 107 total | 15 (`editor/tokens.rs` new: `Span` struct + `plain_spans`) + 16 (its 2 unit tests) ≈ 31 new (genuinely new this slice — no prior task had a line/span model to reuse) | ~15 subagent-min + ~10 fix-pass | 11 total, unchanged — **including the fix pass**: the scroll-sync uses `MountedData::scroll`/`ScrollData`, implemented on BOTH `dioxus-web` and `dioxus-desktop`, so it's one cross-target `rsx!` path with no `web-sys` branch and no new edge-gate (see Ecosystem / editor) | Both targets compile clean; `editor::tokens::` 2/2 new tests pass; `net::` regression 45/45; `cargo fmt --check` clean (one `cargo fmt --all` pass needed on the new file — the brief's inline snippet formatting for the struct-literal `vec![Span{...}]` calls didn't match rustfmt's wrapped style). Overlay approach: a transparent `textarea.editor-overlay` (input/caret/selection owner, `color: transparent` + `caret-color` restored) is absolutely stacked over a read-only `pre.editor-highlight` of `plain_spans(&buf.read())` inside a `position: relative` `.editor-stack`; both layers share identical font/line-height/padding/border/`white-space: pre-wrap` box metrics so the invisible overlay caret lines up glyph-for-glyph with the visible highlighted text beneath — the standard controlled-highlight editor pattern (CodeMirror/Prism-style), not `contenteditable`, per the brief's steer. **Fix pass** locked the two layers on scroll (an `onscroll` handler mirrors the textarea's `scroll_top`/`scroll_left` onto the `pre` via `MountedData::scroll`) — see the review-fix note below and the Ecosystem / editor finding. `plain_spans` is a plain function call (not a hook) so it lives inside the `FileBody::Text` match arm with no risk to the (now three) hoisted `use_signal`/`use_effect`/`use_signal` hooks at the top of `Editor`, which stay unconditional. Tasks 11/12 swap `plain_spans` for a real tokenizer behind the identical `Vec<Vec<Span>>` shape — no render-layer change expected. |
| C.3 — desktop-native tree-sitter highlighting | 23 (`editor/mod.rs` diff: gated `use`/`mod highlight_native;` + the `Text` arm's span-source split into `#[cfg(feature = "desktop")]`/`#[cfg(feature = "web")]`/`#[cfg(not(any(...)))]` three-way `let spans = …`, the last arm needed so `--no-default-features` with neither target feature still type-checks) ≈ 23 total | 87 (`editor/highlight_native.rs` new, non-test: `language()` id→(Language, query) map for rust/javascript/typescript/python/go, `class_for` capture→CSS-class map, `highlight()` fallback wrapper, `highlight_inner` — parse via `HighlightConfiguration`/`Highlighter`, walk `HighlightEvent`s into a per-byte class map, then `segment_lines`) + 17 (its 2 unit tests: unsupported-lang-falls-back, rust-keyword smoke test) + 29 (`editor/tokens.rs` diff: `segment_lines` moved/added — pure, target-independent, no tree-sitter dep) + 27 (its 1 new unit test) ≈ 160 new | ~20 subagent-min | **12 total (+1 this slice)** — the one new edge is exactly the span-source split flagged in the task brief: desktop calls `highlight_native::highlight`, web keeps `tokens::plain_spans`, and a third `not(any(...))` arm was added (beyond the brief's two-arm sketch) to keep `--no-default-features`-with-neither-feature compiling for the `editor::tokens::`-only gate | `cargo test --no-default-features editor::tokens::` 3/3 as-shipped this slice (2 pre-existing `plain_spans` tests from C.2 + the new `segment_lines_coalesces_equal_classes_per_line`), feature-free — no tree-sitter dep pulled in; **the Task-11 fix pass later added the 2 CRLF/LF `segment_lines` regression tests, bringing `editor::tokens::` to 5/5 (its final state, as the C.4 and Task-13 rows record)**; `cargo build --no-default-features --features desktop` clean (compiles all 5 native grammars + `tree-sitter-highlight` from a cold cache in ~11s, warm-cache incremental in ~3s); `cargo test --no-default-features --features desktop editor::` 5/5 (adds `highlight_native`'s 2 tests, including the **smoke test**: `highlight("fn main() {}", "rust")` → at least one `tok-keyword` span — PASSED, so the grammar+query+segmentation path is proven end-to-end, not just compile-checked); `cargo build --no-default-features --features web --target wasm32-unknown-unknown` clean, and `cargo tree --no-default-features --features web --target wasm32-unknown-unknown \| grep tree-sitter` returns **zero matches** — confirmed tree-sitter-free, as required (web stays on `plain_spans` until Task 12); `net::` regression 45/45; `cargo fmt --check` clean (one `cargo fmt --all` pass, same as C.2, for the multi-line match-arm tuples and wrapped test literals). Crate versions matched `crates/retrieval/Cargo.toml` exactly (`tree-sitter 0.26.9`, `tree-sitter-rust 0.24.2`, `-javascript 0.25.0`, `-typescript 0.23.2`, `-python 0.25.0`, `-go 0.25.0`), plus `tree-sitter-highlight 0.26.10` (not vendored elsewhere in the workspace — resolved fresh, its `tree-sitter ^0.26.10` requirement is satisfied by the `^0.26.9` range so no version bump was needed). No manual GUI drive (out of scope per the brief); see Ecosystem / editor for the API-fit narrative. |
| C.4 — web tree-sitter highlighting (timeboxed spike; **fallback taken**) | 0 (`editor/mod.rs` unchanged — the `#[cfg(feature = "web")]` arm still calls `tokens::plain_spans`; no `highlight_web.rs` created, no `index.html`/`Dioxus.toml` asset wiring added) | 0 new | ~25 subagent-min (feasibility research + report write-up only — no code path was attempted, so there were no build/debug cycles to spend timebox minutes on) | **12 total, unchanged** — no new edge-gate, because no web highlighter was added; the `#[cfg(feature = "web")]` arm from C.3 is untouched | Both targets still compile clean; `cargo tree --no-default-features --features web --target wasm32-unknown-unknown \| grep tree-sitter` still returns zero matches; `editor::tokens::` 5/5; `net::` 45/45; `cargo fmt --check` clean. See Ecosystem / editor for the full feasibility writeup and the headline asymmetry finding. |
| 13 — desktop auto-connect (folder-picker → sidecar → auto-connect) | 98 (`desktop_boot.rs` 63 non-test: module doc + `async fn boot()` + `SidecarGuard`/its `Drop` impl + `app.rs` diff 33: the `let mut token` mutability fix/comment + the desktop-only mount block (`use_signal` for the sidecar guard + `use_future` calling `boot().await`) + `main.rs` diff 2: `#[cfg(feature = "desktop")] mod desktop_boot;`) | 0 new (reuses `net::url::LaunchParams` — the same `{ws, token}` contract the not-yet-wired web/Tauri query-string autoconnect path already defined/tested in an earlier task) + 17 new, desktop-only (`desktop_boot.rs`'s own `#[cfg(test)]` unit test `drop_does_not_panic_on_already_exited_child`, guarding `SidecarGuard::drop`'s `let _ =`-swallowed `kill()`/`wait()` — not part of the host-side `net::`/`editor::tokens::` pure seam, since it needs a real child process) | ~30 subagent-min (incl. an rfd-API doc lookup that came up empty, a Dioxus `use_future`-vs-`use_resource` docs check via `context7`, and a throwaway `rustc` probe of closure-capture mutability) | **14 total (12 → 14, +2 this slice)**: `#[cfg(feature = "desktop")] mod desktop_boot;` in `main.rs` (a single-arm gate, directly analogous to `transport/mod.rs`'s existing lone `mod desktop`/`mod web` gates) + the `app.rs` desktop-only mount block (a single-arm gate — the web target has no boot sequence to run, so there is no web/neither counterpart to pair it with) | Both targets compile clean (`--features desktop` clean; `--features web --target wasm32-unknown-unknown` clean with only pre-existing unrelated dead-code warnings); `net::` 45/45; `editor::tokens::` 5/5; `cargo fmt --check` clean. **rfd:** picked `rfd::AsyncFileDialog::new().pick_folder().await` over the blocking `rfd::FileDialog::pick_folder()` — `context7` has no `rfd` docs indexed, so this was confirmed by reasoning plus a standalone probe rather than doc lookup: the blocking call parks the OS thread the Dioxus/tokio executor runs on for as long as the picker dialog is open, which would stall every other `use_future`/`spawn` task in the app (the socket drain task, `load_files`, etc.), not just the picker's own task. `boot()` is therefore `async fn` — a signature change from the brief's sync sketch, the exact escape hatch the brief pre-authorized ("this may change `boot()`'s signature to async — fine"). **`use_future` vs `use_resource`:** confirmed via `context7`'s Dioxus 0.7 docs that `use_future` (unlike `use_resource`) is the "run once on mount, do not reactively rerun" hook — its own docs' websocket-connect example runs an infinite `while let` loop straight from mount with no dependency re-triggering. This mattered concretely here: the mount block's `do_connect()` call reads `url`/`token` internally, and if that read had made the *outer* future itself reactive (as `use_resource` would), a later `url`/`token` change from an unrelated path (e.g. the promote/demote handover effect) would silently re-fire this block and reopen the folder picker — `use_future`'s non-reactive "fire once" semantics rule that out. **Genuine (non-brief) compile fix:** `token` needed to become `let mut token = use_signal(...)` (previously a plain, read-only `let`) — `Signal::set` requires `&mut self` (confirmed by reading the actual vendored `dioxus-signals-0.7.9` source, `WritableExt::set` in `write.rs`), and a standalone `rustc` probe confirmed a `move`-closure capturing an outer non-`mut` binding cannot call a `&mut self` method on it even though `Signal` is `Copy` — the same class of fix slice F already needed for `url`. **Correctness addition beyond the brief's literal `fn boot() -> Option<(Child, LaunchParams)>`:** wrapped the raw `Child` in a `SidecarGuard` newtype whose `Drop` calls `.kill()` + `.wait()`. A bare `std::process::Child` does **not** terminate its process on drop (only an explicit `.kill()` does), so the brief's literal signature, implemented verbatim, would have silently orphaned the `otto serve` sidecar the moment the signal holding it was ever dropped or replaced — regardless of "window close" — contradicting the brief's own claim that the `Child` would be "killed on drop." Whether Dioxus's desktop shutdown path actually drops root-scope signal values (and thus fires this `Drop`) before the process exits is unconfirmed without a live run — see the Priority-①-gate finding below. **`do_connect` reuse:** the mount block's success path calls the exact same `do_connect` closure the manual `ConnectionForm`'s Connect button and the promote/demote handover effect already call — `generation`-bump, close the old sink, open the new socket, spawn a fresh generation-guarded drain task — no second connect path was hand-rolled. **Hook safety:** the new `use_signal`(sidecar)/`use_future` pair sits in a `#[cfg(feature = "desktop")]` block placed after all of `App`'s other hook declarations and both pre-existing `use_effect`s, so it neither renumbers nor reorders any existing hook on either target; it is unconditional within the desktop compilation (never inside a runtime `if`) and wholly absent from the web compilation, so each target still sees exactly one fixed hook sequence every render. The Editor's three hoisted hooks (C.1/C.2/C.3) are untouched. **Fix pass (review) — three changes superseding the above:** (1) the fixed-sleep-before-connect was replaced with a **ported readiness check** (`is_ready_line`, verbatim from `desktop/src-tauri/src/launch.rs`) — `boot()` now spawns via `tokio::process::Command` with stderr piped and reads lines until otto serve's `"otto serve listening on …"` line, bounded by a 5s `timeout` cap (400ms fixed sleep survives only as a stderr-unavailable fallback), making the reproduction genuinely equivalent to Tauri's startup handshake instead of a weaker shortcut; (2) `SidecarGuard` + its `Drop`/drop-panic test were **removed** in favor of tokio's `Child::kill_on_drop(true)` (cleaner, tokio-documented; the two `is_ready_line` unit tests replace the drop test); (3) `boot()` now returns a `BootOutcome{Cancelled, SpawnFailed(String), Ready(Child, LaunchParams)}` enum so a **spawn failure is surfaced** (`eprintln!` + a `client_error_row` in the mount block) instead of silently falling through to the manual form as a user-cancel would. LOC/test counts in the left columns reflect the first pass; the fix pass roughly **doubled** `desktop_boot.rs` (from ~80 to **155** final `wc -l`) via the ported stderr readiness-wait loop, the `BootOutcome` enum, and the 2 `is_ready_line` unit tests (replacing the removed drop-panic test). `cfg` count unchanged at 14. See the Fix-pass section in `.superpowers/sdd/task-13-report.md`. |

**Leptos baseline (for comparison).** Measured with `wc -l` over every `.rs` file (Task 14):

| Crate | Total LOC | Pure-logic (shared seam) | View/reactivity LOC |
|---|---|---|---|
| `ui/src` (browser only) | 1939 | 945 (`tree.rs` 219 + `url.rs` 209 + `view_model.rs` 517) | 994 (`app.rs` 446 + `components/*` 429 + `main.rs` 14 + `ws.rs` 48 + `workspace.rs` 57) |
| `desktop/src-tauri/src` (native Tauri wrapper, separate crate) | 234 | 0 (no pure-logic seam of its own — thin sidecar/webview wrapper) | 234 (`launch.rs` 74 + `lib.rs` 154 + `main.rs` 6) |
| **Leptos axis total (both targets, two crates)** | **2173** | **945** | **1228** |

`ui-dioxus/src` (one crate, two targets), same method:

| Bucket | LOC | Files |
|---|---|---|
| Pure-logic, **reused byte-identical** from `ui/` (Task 2) | 945 | `net/tree.rs` 219 + `net/url.rs` 209 + `net/view_model.rs` 517 |
| Pure-logic, **new** (editor tokenizer + native highlighter — no Leptos counterpart, see below) | 297 | `editor/tokens.rs` 193 + `editor/highlight_native.rs` 104 |
| Module-wiring glue | 5 | `net/mod.rs` |
| View/reactivity LOC (both web + desktop, one crate) | 1369 | `app.rs` 471 + `components/*` 267 + `desktop_boot.rs` 155 + `editor/mod.rs` 110 + `main.rs` 13 + `transport/*` 353 |
| **Total** | **2616** | |

**The critical apples-to-oranges nuance, stated explicitly:** `ui/`'s editor (`components/editor_pane.rs`,
78 LOC) is a thin wrapper around the **external** `kode-leptos = "0.5.4"` crate dependency, which does
the buffer rendering **and** the actual syntax highlighting itself (via its own `arborium`/
`arborium-highlight` dependencies — confirmed present in `ui/`'s own dependency tree when built for
this task, see Build/toolchain below). None of that highlighting engine's code is counted in `ui/src`
at all — it's off-the-books, reused from crates.io. `ui-dioxus`'s editor, by contrast, is **entirely
self-built**: `editor/mod.rs` (110 LOC, the textarea-over-`pre` overlay + scroll-sync view shell) plus
`editor/tokens.rs` (193 LOC — `Span`/`plain_spans`/`segment_lines`, hand-written) plus
`editor/highlight_native.rs` (104 LOC — the tree-sitter grammar/query/capture-classification wiring,
hand-written) for a total of 407 in-tree LOC doing what `ui/` gets for 78 LOC plus an invisible external
dependency. A raw total-LOC comparison (2616 vs 2173) therefore penalizes Dioxus for work Leptos never
had to do in-repo; the fair comparison isolates the shared pure-logic seam (identical both sides, zero
marginal cost) and treats the 297 LOC of new editor pure-logic as its own line item — see Priority gate ②.

**Reconciliation note — final `wc -l` is authoritative, and it exceeds the summed per-slice diff
deltas for files that later received fix passes.** The gate-② LOC totals and the "+36%" figure are
computed from the **final `wc -l` tree** (measured in this task), *not* from summing the per-slice
"View/reactivity LOC"/"Pure-logic LOC" diff-delta columns in the effort table above — so the headline
figures stand on a direct file measurement. Those two do not fully reconcile, and the notable case is
`editor/tokens.rs`: its per-slice deltas sum to ~87 (C.2 row: ~31 new; C.3 row: ~56 more), but the
final `wc -l ui-dioxus/src/editor/tokens.rs` is **193** (re-measured this task) — a ~106-line gap. The
gap is explained, not an error: the **Task-11 fix pass** rewrote `segment_lines` (to a
`split_inclusive`/`char_indices`, CRLF- and multibyte-safe implementation) and added **2 regression
tests** (`segment_lines_crlf_and_multibyte_are_exact_and_boundary_safe`,
`segment_lines_pure_lf_multibyte_stays_correct`), and the file carries substantial module/function
doc-comment blocks — none of which were logged as separate effort-table row deltas (fix-pass growth
generally wasn't row-logged, matching how the Task-4 and Task-13 fix passes were also folded in-place
rather than given their own delta rows). This is disclosed here in the same spirit as the
cfg-count 14-vs-18 reconciliation below (gate ①): where a tracked running tally and a direct final
measurement diverge, both are stated and the direct measurement is treated as authoritative for the
decisive numbers. Confirmed: `editor/tokens.rs` = **193** final `wc -l`; the 193 feeding the 297-LOC
new-pure-logic bucket and the "+36%" figure is that measured value, unchanged by this reconciliation.

## Narrative evidence (not scored)

### DX / reactivity
_(notes)_

**Synthesis (Task 14) — the model maps cleanly, but it has sharp edges the compiler doesn't guard.**
Across all ten instrumented slices, Dioxus 0.7.9's signal/component surface needed essentially **zero**
adaptation from the 0.6-era plan pseudo-code the brief was written against — every task that checked
(`use_signal`, `.read()`/`.write()`/`.set()`, `use_future` vs `use_resource`, `EventHandler<T>`,
`#[component]`, event `.value()`) confirmed the API unchanged (Tasks 4, 13; see below). That is a
genuinely positive stability signal. Set against it, three **real correctness bugs** surfaced during
this spike — each one compiled clean, shipped no warning, and was caught only by a subsequent human/
opus-assisted review pass, not by `cargo build` or the host test suite:

1. **Write-guard access silently fails to subscribe an effect (Task 8, slice F).** The handover
   reconnect used `reconnect_to.write().take()` inside a `use_effect` — a mutation-shaped read that
   does **not** register as a tracked dependency in Dioxus's reactivity graph. The effect ran once at
   mount, read `None`, and then never fired again — meaning the promote/demote handover reconnect was
   **dead on arrival**, silently. The fix is one token different (`reconnect_to()`, a tracked read,
   read first, then `.set(None)` to clear) — but nothing short of driving the actual handover flow (or
   a reviewer who knew this exact Dioxus gotcha) would have caught the regression; the build was green
   throughout.
2. **A rules-of-hooks violation hidden inside a match arm (Task 9, slice C.1).** `Editor`'s
   `use_signal`/`use_effect` pair for the text buffer lived **inside** the `FileBody::Text` match arm,
   after an early return for `None`. Dioxus's hooks are strictly positional (same hooks, same order,
   every render of a persistent component instance) — this "worked" only because the component's
   render sequence happened to be stable across the tested open/close sequence, but it was one sibling
   hook away from an index-mismatch panic, and the pattern would have propagated into every later
   editor task (C.2/C.3 both add hooks) had it not been hoisted to the top of the component body during
   review.
3. **Socket teardown needed an explicit close, not just a dropped reference (Task 4, slice A, fix
   pass).** The initial per-connection cleanup dropped the `Rc<dyn Sink>` on disconnect, but the raw
   `web_sys::WebSocket` handlers were `.forget()`'d (leaked, by necessity, so they outlive the closure
   that installed them) — so dropping the `Rc` did not stop the socket from delivering more events, and
   a fast reconnect could race a stale event from the *old* connection into the *new* one's state. The
   fix added a `Sink::close()` (detach handlers, close the socket) plus a `generation: Signal<u64>`
   guard so any already-in-flight stale event is discarded by generation mismatch even if it slips
   through before `close()` takes effect. This is not a Dioxus-specific bug (Leptos's own raw-`web_sys`
   callback pattern has the identical `.forget()`-leak shape and needs the identical discipline — see
   below) but it is a correctness trap the framework does nothing to prevent or flag.

None of these three bugs are "Dioxus is broken" findings — all three are one-line fixes once diagnosed,
and the framework's own docs and `context7`-fetched material do document the tracked-read and
positional-hooks rules explicitly. The finding is narrower and more actionable: **this reactivity model
has real, non-obvious footguns that the type system and `cargo build` do not catch**, so a team adopting
it needs either institutional familiarity with these exact rules or a review discipline (as this spike
used) that specifically checks for them — "it compiles" is not evidence of correctness for effect
dependencies or hook placement in the way it is for most other Rust code.

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

**WASM bundle size (Task 14 — hard numbers, method disclosed).** Neither crate has `dx`/`trunk`
installed in this environment, so both were measured the same way for a fair comparison: a plain
`cargo build --release --target wasm32-unknown-unknown` (with `--no-default-features --features web`
for `ui-dioxus`), then the raw `.wasm` artifact `rustc`/`wasm-bindgen`'s backend emits directly in
`target/wasm32-unknown-unknown/release/*.wasm`, measured with `wc -c`. This is the **pre-`wasm-bindgen`-
JS-glue, pre-`wasm-opt`, pre-gzip** number — i.e. a directional, apples-to-apples "how much code did
the compiler emit" figure, not the final asset a browser would fetch (both `dx bundle` and `trunk
build --release` run `wasm-opt` and/or strip further, and any real deployment gzips over the wire;
neither post-processing step was run here for either crate).

| Crate | Command | Raw `.wasm` size |
|---|---|---|
| `ui-dioxus` (web feature) | `cargo build --release --no-default-features --features web --target wasm32-unknown-unknown` | 2,450,223 bytes (~2.45 MB) |
| `ui` (Leptos CSR) | `cargo build --release --target wasm32-unknown-unknown` | 3,081,572 bytes (~3.08 MB) |

Both builds completed cleanly (`ui-dioxus` in ~45s from a warm registry cache; `ui` in ~1m19s, pulling
in `leptos`/`kode-leptos`/`gloo-net` and, notably, `kode-leptos`'s own `arborium`/`arborium-highlight`
dependencies — the crate that gives `ui/`'s editor its highlighting for free, see the Leptos-baseline
LOC table above). The raw Dioxus artifact is **~21% smaller** than the raw Leptos artifact — but this
is not a clean "framework overhead" comparison: `ui-dioxus`'s web build ships **zero** highlighting
logic (it falls back to `plain_spans`, see Ecosystem/editor), whereas `ui/`'s number already includes
a real, working syntax highlighter (`arborium`) compiled in. Some — not necessarily all — of Leptos's
extra ~630 KB is that highlighter's cost, not framework overhead; disentangling the two would require
building a highlighting-disabled `ui/` variant, which was out of scope here. Stated plainly: this is a
real, measured, disclosed-method data point in Dioxus's favor, but it should not be read as "Dioxus's
runtime is 21% leaner than Leptos's" without that caveat.

**Toolchain (`dx` vs `trunk`).** This spike never invoked `dx serve`/`dx bundle` (Dioxus's own CLI) —
every gate across all 14 tasks used plain `cargo build`/`cargo test`, deliberately, since no GUI/browser
is drivable in this headless environment. So the `dx`-vs-`trunk` developer-loop comparison (hot reload,
asset pipeline, `Dioxus.toml`'s `[web.resource]` — flagged as unexercised back in Task 1) stays
**untested** by this spike; only the underlying `cargo`-level compile experience was compared, and on
that axis both toolchains behaved identically (ordinary `rustc`/`cargo` diagnostics, no custom build
step required for either target to type-check).

**Styling is unconnected — the copied `ui/` chrome stylesheet is effectively orphaned (a known
limitation of the compile-only spike).** The whole-branch review surfaced that this is materially
bigger than the couple of class-name differences earlier slice notes recorded (e.g. `diff-context` vs
`ui/`'s `diff-ctx`): when all of slices A–F are assembled, effectively **none** of the app-chrome CSS
classes the Dioxus components emit have a matching rule in the verbatim-copied `ui/style.css`. The
Dioxus components emit `status-line`, `prompt-bar`, `event-log`, `connection-form`, `approval-panel`,
`meter`/`meter-cost`, `handover`, `status-conn`/`status-session`/`status-seq`, `tree-dir`, `tree-file`
(names chosen fresh while porting each component's `rsx!`); the copied stylesheet instead defines the
original `ui/` names — `status`, `prompt`, `log`, `conn-form`, `cap-*`, `row-*`,
`tree-dir-row`/`tree-file-row`. The **only** classes that actually connect are the editor +
`tok-*` classes, which were added fresh to *both* sides in C.2/C.3 (that's why the editor is the one
place styling was exercised at all, and only in the highlight-span layer). Net effect: a real
`dx serve` visual drive would render the entire app **chrome unstyled** until the class names are
reconciled (either rename the components' classes to match `ui/style.css`, or extend the stylesheet to
the new names). Two related facts compound this and are worth stating together: (1) `style.css` is
pulled in **only** via `Dioxus.toml`'s `[web.resource]` entry — a `dx`-CLI/web-only asset path that no
`cargo build` gate ever exercises (flagged as unexercised since Task 1) — and (2) the **desktop**
webview includes **no stylesheet at all** (no `asset!`/`document::Stylesheet` wiring was added), so
neither target's styling is exercised by any build gate this spike ran. This is a documentation-only
finding: reconciling the class names / wiring the desktop stylesheet is correctly **deferred to the
runtime-driven follow-up the verdict already recommends** (it is exactly the kind of thing a visual
drive would catch on the first frame and a compile-only gate structurally cannot). It does **not**
change the "functionally wired + compile-verified" framing used elsewhere in this report: the client's
*behavior* — connect, stream events, approve diffs, promote/demote, open files into the editor — is
wired and compile-verified; what is unconnected is the app-chrome *presentation* layer. Keep that
distinction crisp: working client logic, unstyled chrome, pending the recommended runtime pass.

**Desktop build.** `cargo build --no-default-features --features desktop` for `ui-dioxus` compiles
clean with no separate wrapper crate or second toolchain — contrast with the Leptos axis, which needs
a `trunk build` of `ui/` **plus** a separate `cargo tauri build`/`tauri build` of `desktop/src-tauri`
(two toolchains, two build invocations, a static-bundle handoff via `ui/dist`) to produce a native
artifact. Task 11 recorded the desktop feature's tree-sitter-grammar compile cost specifically: ~11s
cold-cache, ~3s warm-cache incremental, for all 5 native grammars + `tree-sitter-highlight`. No
installer/signed-bundle size was produced or measured for either axis (`dx bundle` was not run; neither
was a full `tauri build` in this task) — packaging-artifact size is an open question for both sides,
not just Dioxus.

### Ecosystem / editor
_(notes)_

**Synthesis (Task 14) — the headline editor asymmetry, and one honesty check on it.** Desktop gets real
tree-sitter highlighting (5 native grammars, `highlight_native.rs`, Task 11); web falls back to
unhighlighted `plain_spans` (Task 12, fallback taken after a rigorous feasibility spike, not a timebox
punt). This is a **permanent**, not temporary, platform asymmetry: `web-tree-sitter` is a JS library
wrapping a separately-compiled Emscripten wasm module with no Rust-native API and no reusable
highlight-iterator, so closing the gap would mean hand-rolling wasm-bindgen JS interop and
reimplementing a chunk of `tree-sitter-highlight`'s capture-classification logic against an unverifiable
(no-browser-here) runtime — real multi-day work, not a binding fix. One honesty check worth recording
explicitly: building `ui/` for the bundle-size measurement above surfaced that `kode-leptos` (the
external crate `ui/`'s editor wraps) pulls in `arborium`/`arborium-highlight` — meaning **a Rust-native,
wasm-compatible highlighting path does exist in the ecosystem**; it wasn't reused here because this
spike's brief specifically charged the Dioxus editor with reusing `retrieval`'s own tree-sitter grammar
set (Rust/JS/TS/Python/Go) for a same-repo apples-to-apples comparison, not with picking a different
Rust-native highlighter that would fragment the grammar/query set from what `retrieval` already vendors.
So the finding is accurately scoped as "the specific tree-sitter-based approach this spike targeted does
not have a wasm-compatible path today," not "no Rust UI framework can highlight code in the browser" —
the latter is contradicted by `ui/`'s own dependency tree. `.go` files also render unhighlighted on
desktop (a pre-existing, accepted parity constraint: `net::tree::language_for_path`, verbatim-ported
from `ui/` for byte-identical parity, has no `.go` arm — fixing it would mean diverging from the ported
seam, so it was left as a known, minor limitation rather than "fixed" out of scope).

**Slice C.2 (fix pass) — scroll-sync interop maturity data point.** The textarea-over-`pre`
controlled-highlight editor requires mirroring the input textarea's scroll offset onto the
absolutely-positioned highlight `pre` beneath it (otherwise, on any file taller/wider than the
viewport, the highlighted spans and the caret visibly separate the moment the user scrolls — the
classic gotcha of this pattern, and one Tasks 11/12 inherit since they render real highlighting
into that same `pre`). Dioxus 0.7.9 handles this **cleanly and cross-target with no `cfg` split** —
a genuinely positive framework-maturity finding for the spike: the `onscroll` event yields
`Event<ScrollData>` with `.scroll_top()`/`.scroll_left()` (f64), and an element ref captured via
`onmounted` (stored in a hoisted `Signal<Option<MountedEvent>>`) exposes
`MountedData::scroll(PixelsVector2D, ScrollBehavior)` (aliased to DOM `scrollTo`, absolute). Both
`dioxus-web`'s and `dioxus-desktop`'s `RenderedElementBacking` implement `scroll`, so the identical
`rsx!` drives the browser DOM and the desktop webview through one code path — no `web-sys`-only
branch, no new edge-gate (**`cfg` count stays 11**). Two small idiom notes worth recording for the
highlighter tasks: (1) `MountedData::scroll` returns a `Pin<Box<dyn Future>>`, so the `onscroll`
handler must `spawn(async move { … .scroll(…).await })` rather than call it synchronously — cheap,
but non-obvious; and (2) `PixelsVector2D` lives in `dioxus::html::geometry`, **not** the prelude
(unlike `ScrollBehavior`/`MountedEvent`, which come in via `dioxus::prelude`'s `events::*`), so it
needs the fully-qualified path. `ScrollBehavior::Instant` (vs the `Default` `Smooth`) is required to
keep the layers locked frame-for-frame — `Smooth` would visibly lag the highlight behind the caret.
Net: the interop that's fiddly-to-fragile in raw-JS/`web-sys` ports of this pattern is a ~15-line,
single-path, compile-clean affair in Dioxus 0.7 — no rabbit-hole, no approximation needed.

**Slice C.3 — tree-sitter-highlight integration and the native/wasm highlighting divergence.**
The `tree-sitter`/grammar/`tree-sitter-highlight` ecosystem itself integrated **cleanly on the
first try** at these exact versions — no API archaeology needed beyond confirming constant names,
because `crates/retrieval/src/chunk.rs` already had a working, in-repo reference for the
`Parser`/`Language::from(LanguageFn)` half, and `tree-sitter-highlight 0.26.10`'s
`HighlightConfiguration::new(language, name, highlights_query, injection_query, locals_query)` /
`Highlighter::highlight(&cfg, bytes, cancellation, injection_callback)` /
`HighlightEvent::{HighlightStart(Highlight), Source{start,end}, HighlightEnd}` shape matched the
task brief's example verbatim (confirmed by reading the crate's actual `bindings/rust/lib.rs` and
`src/highlight.rs` from the local `~/.cargo/registry/src` cache rather than trusting memory): no
signature drift across the 0.20→0.26 span mattered here because the brief's example was already
written against 0.26's shape. The one real per-grammar wrinkle — `HIGHLIGHTS_QUERY` (rust,
typescript, python, go) vs `HIGHLIGHT_QUERY` (javascript, no `S`) — was exactly as flagged, a
one-token fix, not a design change. The more interesting spike finding is architectural, not
ecosystem-integration difficulty: **this slice is the first place the two Dioxus build targets
render genuinely different content for the same component**, not just different transport/IO
plumbing underneath identical view code. Every prior cross-target `cfg` edge (transport, `bash`
tool access, file RPCs) was infrastructure — the rendered UI was byte-identical regardless of
target. Here, a `.rs`/`.py`/`.ts`/`.js` file opened on desktop gets real token-classified color;
the same file opened in the browser (until Task 12) renders as flat `tok-plain` text. That's a
**visible, user-facing capability gap between targets in one unified codebase** — the kind of
thing a genuinely single-source-tree multi-target framework has to be honest about surfacing (vs.
hiding it behind two entirely separate apps, where a viewer would just assume web "doesn't do
that yet" without it reading as a regression). The `cfg` mechanism handled it fine (one `#[cfg]`
match per span-source branch, no new abstraction needed), but it's worth flagging for the
priority-①-gate scoring: "one crate, one `cfg` graph" does not automatically mean "one feature
set" — native-only system dependencies (here: a C-compiled grammar toolchain gcc provides locally
but a wasm32 target fundamentally cannot link) create real, permanent feature skew, not just
temporary until-the-next-task skew. Task 12's web-side answer (a pure-Rust or WASM-compatible
highlighter, or shipping pre-tokenized spans some other way) will need to close this gap for the
"multi-target unification" verdict to hold water; leaving it open would mean the Dioxus story is
"one codebase, occasionally two different apps," which is a materially weaker claim than the spike
set out to test.

**Slice C.4 — web tree-sitter spike: timeboxed, fallback taken.** Web tree-sitter spike started
slice C.4; timebox 1 day. Feasibility was assessed against `web-tree-sitter`'s actual published
API (`Parser.init()`, `Language.load(path | Uint8Array)`, `parser.setLanguage`, `parser.parse`,
`Query`/`Query.matches`/`Query.captures` — confirmed via the package's npm/GitHub docs, not
recalled from memory) rather than assumed, and the finding is a **hard architectural mismatch**,
not a soft "would take longer than a day" one — so the timebox was not actually the binding
constraint; the shape doesn't fit regardless of budget:

1. **Two incompatible wasm binaries, not one.** The C-based `tree-sitter` crate doesn't compile to
   `wasm32-unknown-unknown` (the target `dioxus/web` itself builds against) — that's *why* this
   task exists. `web-tree-sitter` sidesteps that by shipping a **prebuilt** `tree-sitter.wasm`
   compiled from the C core via Emscripten (`wasm32-unknown-emscripten`), loaded and driven entirely
   from JS. There is no Rust crate wrapping it — it is a JS library, full stop. So "wiring it in via
   Dioxus JS interop" doesn't mean adding a `wasm-bindgen`-friendly Rust dependency the way
   `highlight_native.rs` added `tree-sitter`/`tree-sitter-highlight`/5 grammar crates on desktop; it
   means hand-writing `#[wasm_bindgen(module = "web-tree-sitter")]` `extern` blocks against a
   moving third-party JS API surface, with the Dioxus app's own `wasm32-unknown-unknown` module and
   `web-tree-sitter`'s Emscripten module coexisting as **two separate wasm instances in the same
   page**, bridged only through JS glue. That is qualitatively more integration surface than any
   prior slice's `cfg` edge — every one of those (transport, `bash`, file RPCs, C.3's own
   desktop/web span-source split) was "same Rust code, different backend"; this would be "call out
   to a foreign wasm runtime through hand-rolled JS bindings."
2. **No synchronous `highlight(text, lang)` fit.** `Parser.init()` and `Language.load()` are both
   `Promise`-returning — i.e. genuinely async, not "async the first time, then a cached sync
   fast-path" the way e.g. a one-time model load can be. That part is tractable in isolation (per
   the brief: resolve once, cache the `Language`/`Parser` handles, then treat later calls as
   synchronous over the cache) and is not, by itself, disqualifying. The deeper problem is that
   `web-tree-sitter` only exposes the **low-level** `Parser`/`Tree`/`Query` API — unlike the native
   `tree-sitter-highlight` crate C.3 used, there is no shipped `Highlighter`/`HighlightEvent`
   iterator on the JS side. `class_for`-style capture→CSS-class classification, injection-query
   handling, and the per-byte highlight-event walk that `highlight_native.rs`'s `highlight_inner`
   gets for free from `tree-sitter-highlight` would all have to be **reimplemented from scratch**
   against `Query.captures()`, either in hand-written JS glue or by shuttling every capture back
   across the wasm-bindgen boundary into Rust. That's not "port the same algorithm to a different
   API," it's "rebuild a chunk of `tree-sitter-highlight`'s logic a second time, against a thinner
   API, in an untested language boundary" — real, multi-day design-and-implementation work, not a
   binding/signature-adaptation exercise like C.3's `HIGHLIGHTS_QUERY`-vs-`HIGHLIGHT_QUERY` naming
   wrinkle.
3. **No runtime verification is possible in this environment.** This spike has no browser and no
   confirmed path to fetch/validate the actual `tree-sitter.wasm` + per-grammar `.wasm` asset
   binaries (network access to npm/CDN asset hosts is not reliably available here). Shipping
   `wasm-bindgen` externs against a JS API this task can't exercise, plus vendored `.wasm` assets
   this task can't confirm even parse correctly in a real `web-tree-sitter` runtime, would produce
   code that *compiles* but has **zero confidence behind it** — untested interop code plus asset
   bloat, which is strictly worse spike output than a rigorous "here is why, and here is what it
   would cost" writeup.
4. **Bundle-size estimate (order-of-magnitude, not fetched/measured).** Community tooling that
   ships `web-tree-sitter` (e.g. editor-in-browser projects distributing prebuilt grammar `.wasm`
   via CDNs such as `unpkg`'s `tree-sitter-wasms` package) commonly cites individual grammar
   `.wasm` files in the **low hundreds of KB** each, with denser grammars (TypeScript, which bundles
   both `.ts` and `.tsx` grammars) running toward **~1 MB**; the `tree-sitter.wasm` runtime itself
   is a comparable additional fixed cost. Scoped to this crate's existing language set (rust,
   javascript, typescript, python, go — 5 languages), a web build would add **on the order of five
   grammar `.wasm` files plus one runtime `.wasm`**, versus desktop's **7 native Cargo crates**
   (`tree-sitter` + `tree-sitter-highlight` + the 5 grammar crates) compiled directly into the
   binary. Neither number is precisely measured here (no network to fetch and weigh the actual
   assets), but the shape of the cost is clear: web would trade "more native code, statically
   linked, zero runtime fetch" for "separate runtime-fetched wasm assets plus hand-rolled JS-interop
   glue," not a smaller version of the same thing.

**The decision, and why it's the headline finding.** Given (1)-(3) above, the timebox was spent on
feasibility analysis rather than an implementation attempt: writing `highlight_web.rs` here would
mean shipping unverified `wasm-bindgen` JS-interop code, unvalidated vendored `.wasm` assets, and a
hand-rolled highlight-iteration reimplementation, all with **no way to confirm any of it actually
runs in a browser** — worse spike output than the fallback per this task's own explicit charter.
So `editor/mod.rs`'s web branch **stays on `tokens::plain_spans`**, matching C.3's status quo
exactly (no diff to that file this slice), and `index.html`/`Dioxus.toml`/the assets directory are
untouched. **Web tree-sitter exceeded feasibility inside the 1-day timebox; web ships unhighlighted,
desktop highlights — a real native/wasm divergence and a headline unification finding.** This
closes the gap C.3 flagged as still-open ("Task 12's web-side answer … will need to close this gap
for the 'multi-target unification' verdict to hold water") with the opposite of what that flag
hoped for: the gap does **not** close. Concretely, for the priority-①-gate scoring: the "one crate,
one `cfg` graph" claim holds structurally (still 12 total edge-gates, still one `rsx!` tree, one
build), but it does **not** imply "one feature set" — a `.rs`/`.py`/`.ts`/`.js` file opens
color-highlighted on desktop and flat `tok-plain` on web, permanently, not just until a follow-up
task lands. The root cause is a genuine, load-bearing platform asymmetry (a C-toolchain-compiled
native library on one target vs. a foreign-runtime JS/wasm library with no native-Rust-shaped API
on the other), not an oversight or a scheduling gap — so unlike every other `cfg` edge in this
spike (transport, `bash`, file RPCs), this one is not expected to close by adding more slices.

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

**Synthesis (Task 14) — what is and isn't measured here.** The one runtime-perf data point this spike
actually produced is the idle-poll-loop finding above (a real, fixed regression, confirmed by
construction and code-reading, not by instrumented timing). Everything else this section's scope calls
for — event-stream render latency under sustained load, and editor typing latency (keystroke →
highlighted-span repaint) for both the textarea-overlay web path and the tree-sitter desktop path — is
**unmeasured**. This spike ran entirely headless (no browser, no drivable desktop GUI in this
environment; every gate across all 14 tasks was `cargo build`/`cargo test`/`cargo fmt --check`), so no
frame-timing or input-latency data was collectable here for either target, on either framework. This is
a real gap in the scorecard, not a claim that Dioxus performs comparably to Leptos at runtime — it is
simply the one narrative dimension where this spike has no evidence at all, disclosed rather than
backfilled with plausible-sounding estimates.

## Priority gate ① — Multi-target unification
_(% shared tree, edge-gate total, and the yes/no: does the one crate replace `ui/` + `desktop/` + Tauri?)_

**Task 13 — desktop auto-connect: the headline unification result.** This task folded the
Tauri `desktop/` wrapper's entire job — pick a workspace folder, launch a local `otto serve`
sidecar, wait for it to bind, auto-connect with no manual URL/token entry — **into the one Dioxus
crate**, with no separate wrapper crate and no `ui/dist` static-bundle handoff. `desktop_boot::boot()`
(folder pick via `rfd::AsyncFileDialog` → `uuid::Uuid::new_v4()` token → `tokio::process::Command`
spawn of `otto serve --port 8787 --root <folder>` with `OTTO_TOKEN` set and **stderr piped** →
async-read the sidecar's stderr until its own readiness line appears → return `LaunchParams{ws,
token}`) plus a desktop-only mount block in `app.rs` (store the sidecar, set `url`/`token`, call the
existing hardened `do_connect`) is **functionally equivalent** to `desktop/src-tauri`'s
sidecar-launch-then-navigate flow — including the **same readiness mechanism** — but collapsed into
the same `rsx!`/reactivity tree that also compiles to `wasm32-unknown-unknown` for the browser. It
compiled cleanly under `--features desktop` with no `Send`/`Sync` friction and no new dependencies
beyond ones already present from Task 1 (`rfd`, `uuid`, `tokio` — the latter just needed its
`process`/`io-util`/`time` features added).

**Readiness detection is ported from Tauri, not shortcut.** The shipped Tauri wrapper
(`desktop/src-tauri/src/launch.rs`) does **not** blind-sleep before connecting — it pipes the
sidecar's stderr and watches for otto serve's own readiness line (`is_ready_line`, matching
`crates/engine/src/main.rs`'s `eprintln!("otto serve listening on {scheme}://{addr}/ws")`). This
task's `boot()` reproduces that exactly: `is_ready_line` is ported verbatim, stderr is piped, and
`wait_for_ready` reads lines with a `tokio::time::BufReader::lines`/`next_line` loop, bounded by a
5s `timeout` safety cap (breaks the instant the line arrives, so the common-case wait is short; the
cap only prevents a hang if the line never comes). A 400ms fixed sleep survives *only* as a
degenerate fallback for the (shouldn't-happen) case where stderr couldn't be piped. So the Dioxus
reproduction is genuinely at parity with Tauri's startup handshake — the earlier fixed-sleep draft
was a strictly weaker mechanism than one already proven in this repo, and it was replaced rather
than rationalized.

**What genuinely closes the gap with `desktop/` + Tauri:** the picker→sidecar→readiness→connect
*logic* now lives in exactly one code path, sharing the web target's manual-form connect path (both
funnel through the same `do_connect`/`build_ws_url`/`transport::connect`), whereas today's shipped
`desktop/` crate is a second, zero-`otto-*`-dependency Rust crate wrapping a *pre-built* `ui/dist`
web bundle in a Tauri webview — two builds, two toolchains (`trunk` + `tauri build`), one extra
crate. This task's version needs none of that: `cargo build --features desktop` is the whole
desktop artifact.

**What is not closed, and must be reported honestly:**
1. **Runtime-unverified here, by design.** This gate is compile-only — the actual
   picker-dialog→sidecar-spawn→readiness→auto-connect sequence has never been driven end-to-end (no
   GUI in this environment). The readiness-line logic (`is_ready_line`) is unit-tested and the wait
   loop is logic-ported from the shipped, tested Tauri path, but whether the whole native flow
   *behaves* equivalently is deferred to the joint desktop drive the task brief calls out.
2. **Sidecar lifecycle on window close is the one real open question.** The sidecar is spawned via
   `tokio::process::Command` with `kill_on_drop(true)`, so the child is killed when the `Child`
   stored in the mount-block signal is dropped — tokio's documented, first-class equivalent of the
   hand-rolled `SidecarGuard` the first draft used (which is why that guard was removed). But
   whether Dioxus-desktop's shutdown path actually drops root-scope signal values (and thus fires
   the kill) before the OS process exits is **unconfirmed without a live run** — a real,
   load-bearing gap, not a formality. Tauri's own sidecar management (`tauri-plugin-shell`) has
   documented kill-on-app-exit semantics; Dioxus-desktop (wry/tao) has no equivalent built-in
   "sidecar" concept, so this crate reproduces that behavior via `kill_on_drop`, and its
   reliability at window-close time can only be confirmed by actually closing a window and checking
   for an orphaned `otto serve` process — not by `cargo build`.
3. **Packaging/bundling parity with Tauri is out of scope here and not claimed.** Tauri's
   `tauri build` produces signed, installer-packaged, auto-updatable native bundles (msi/dmg/
   AppImage) with the sidecar binary embedded as a resolved Tauri "sidecar" resource. This task
   relies on `otto` being separately on `PATH` (or pointed at via `OTTO_BIN`) — there is no
   bundled-sidecar-resource equivalent, code-signing, or updater story wired up. `dx bundle
   --features desktop` (Dioxus's own packaging tool) was not exercised in this task at all.

**Verdict for this task's slice of the unification gate:** structurally *and* mechanically, yes — a
single Dioxus crate reproduces "pick folder → spawn sidecar → wait for the same readiness signal
Tauri waits for → auto-connect" inside one crate, one build, with the exact connect machinery the
manual form already exercises, and it compiles clean. But "replaces `desktop/` + Tauri" as an
unqualified claim is not yet *fully* evidenced: sidecar lifecycle-on-close (now via `kill_on_drop`,
runtime-unverified) and Tauri's packaging/signing/updater story are real, unclosed gaps that a
compile-only gate cannot verify or claim to have replaced. The logic-level replacement is now at
genuine parity with the shipped Tauri path (readiness handshake included); the operational
(window-close kill) and packaging replacements are, respectively, untested and not attempted.

**Gate ① headline numbers (Task 14, final).**

- **`cfg`-edge-gate total: 14**, per the per-slice ledger tracked live throughout the spike (11 after
  the transport seam in Task 3, +3 then −3 net-zero for the Task-4 fix pass, +1 for the C.3 editor
  span-source split → 12, +2 for the Task-13 `desktop_boot` module gate + `app.rs` mount block → 14).
  A direct `grep -rn '#\[cfg(feature\|#\[cfg(not(any(feature' src/` cross-check on the final tree
  returns **18** individual attribute lines — the two numbers differ because the ledger counts
  logical decision points (e.g. "the editor's span-source split" as one edge even where it's written
  as three `#[cfg]` arms), while the grep counts every attribute literally. Both are disclosed here so
  neither reads as cherry-picked; either way, the edges are concentrated in exactly the four places one
  would expect (transport, the editor's highlight backend, the `desktop_boot` module, and one
  desktop-only mount block in `app.rs`) and nowhere else in the ~2600-line crate.
- **% shared component tree:** classifying by file (any file compiled for only one target counts as
  platform-specific; files containing inline `#[cfg]` arms but a shared majority count as shared),
  the fully single-target files are `transport/web.rs` (115) + `transport/desktop.rs` (137) +
  `editor/highlight_native.rs` (104, desktop-only) + `desktop_boot.rs` (155, desktop-only) = 511 LOC
  of 2616 total → **~80% of the crate is genuinely shared** across both targets (2105/2616), with the
  remaining ~20% concentrated in transport backends, the native highlighter, and the desktop boot
  sequence — exactly the "edges only" shape the design's unification mechanics called for.
- **Yes/no — does the one crate replace `ui/` + `desktop/` + Tauri?** **Structurally and
  mechanically, yes, with two named, unclosed gaps.** One crate, one `cargo build` per target, no
  wrapper crate, no static-bundle handoff, no second toolchain (`ui/` + `desktop/`'s Tauri axis needs
  `trunk build` **and** `tauri build`, two crates, two build systems). The web editor's permanent
  unhighlighted state (see Ecosystem/editor) is a **feature gap**, not a build/toolchain gap — "one
  `cfg` graph" does not mean "one feature set," and that distinction matters for anyone reading "yes"
  as "byte-for-byte capability parity." The desktop sidecar's window-close-kill behavior and any
  packaging/signing/updater story are **runtime-unverified / not attempted** respectively — real,
  named gaps, not glossed over.

## Priority gate ② — Parity effort
_(total view/reactivity LOC + wall-clock vs the Leptos baseline)_

**Where the view/pure-logic line was drawn (method).** Per the design's explicit instruction, the
shared `net::{url,tree,view_model}` module (945 LOC, reused **byte-identical** from `ui/`'s
`url.rs`/`tree.rs`/`view_model.rs` — confirmed line-for-line equal in the Leptos-baseline table above)
is excluded from both sides' "effort" figures: it cost the Dioxus side nothing to port (a mechanical
copy in Task 2, with 45 pre-existing host tests carrying over unchanged) and it's the *same* code the
Leptos baseline already paid for, so counting it on either side would double-count shared work as if it
were framework-specific effort. What's left after that subtraction is genuinely comparable "did this
framework's view/reactivity layer cost more or less to build": **view/reactivity LOC** (the `rsx!`/
signal/effect layer, transport backends, and desktop boot sequence) and, as its own line item, **new
pure-logic LOC that had no Leptos counterpart at all** (the self-built editor tokenizer/highlighter,
297 LOC — see the apples-to-oranges editor note in the Leptos-baseline section above).

**The numbers, three ways:**

| Comparison | Dioxus (one crate, both targets) | Leptos + Tauri (two crates, two toolchains) | Delta |
|---|---|---|---|
| View/reactivity LOC only | 1369 | 994 (`ui/`) + 234 (`desktop/`) = 1228 | +141 LOC (+11%) |
| + new pure-logic with no Leptos counterpart (self-built editor engine) | 1369 + 297 = 1666 | 1228 + 0 (offloaded to external `kode-leptos`/`arborium`) | +438 LOC (+36%) |
| Shared pure-logic seam (reused both sides, zero marginal cost) | 945 | 945 | 0 |

Read narrowly (view/reactivity only, editor-authoring-approach held equal), unifying both targets into
one Dioxus crate cost **~11% more original LOC** than Leptos's browser crate plus Tauri's native
wrapper combined — a modest "unification tax" for collapsing two crates and two toolchains into one.
Read broadly (counting the editor engine Dioxus had to hand-write because no wasm-compatible
tree-sitter path existed for the exact grammar set this spike targeted), the gap widens to **~36%
more original LOC** — but that entire widening is one identifiable line item (297 LOC), not diffuse
overhead, and it's a direct consequence of the editor-approach decision (§The editor, design doc), not
of Dioxus's component/reactivity model being more verbose.

**Wall-clock.** Summed `subagent-min` across the ten Leptos-parity-mapped slices (A, B, D, E, F, C.1–
C.4, and Task 13's desktop auto-connect — i.e. every row with a direct Leptos-slice analogue, excluding
Tasks 1–3's one-time scaffold/pure-logic-port/transport-seam infrastructure, which has no per-slice
Leptos counterpart to compare against): 50+15+20+20+20+25+25+20+25+30 = **~250 subagent-minutes
(~4.2 hours)** to reach full A–F + editor + desktop-autoconnect parity across both targets, several
rows inclusive of a review fix-pass. **No comparable Leptos wall-clock baseline exists** — `ui/` and
`desktop/` were built in an earlier plan/session with no time instrumentation, so gate ② can report the
Dioxus-side wall-clock as an absolute data point but cannot honestly compute a ratio against Leptos;
stating a fabricated Leptos wall-clock to force a ratio would be exactly the kind of false precision
the design's verdict-form note warns against.

## Verdict
_(keep-Leptos / adopt-Dioxus / inconclusive, + the evidence that drove it)_

**Finding #1, as the design asked to surface explicitly: there was none.** No protocol change and no
engine change was needed anywhere in this spike. The Dioxus client rides the *exact* same server
surface the Leptos client already proved — the same `/ws` bearer-token connect, the same
`Command`/`Event`/`ServerMessage`/`CapabilitiesManifest` wire types, the same gated `POST /workspace`/
`/promote`/`/export` RPCs. This is a genuinely positive result: it means the protocol/engine boundary
otto already built is client-agnostic in practice, not just in intent, and a second, independently-built
UI client validates that boundary without needing to bend it.

**Verdict: inconclusive, leaning keep-Leptos for now — qualified, not a clean "adopt" or "keep."**

The two priority gates point in different directions and neither is decisively strong enough to settle
the question alone:

- **Gate ① (unification) favors adopting Dioxus.** ~80% of the crate is genuinely shared, the
  `cfg`-edge count (14, or 18 by raw grep) is small and concentrated exactly at platform edges, and
  Task 13 proved — compile-verified, logic-ported from the shipped Tauri reference — that one crate can
  structurally replace `ui/` + `desktop/` + Tauri's build topology (one `cargo build` per target instead
  of two toolchains and a static-bundle handoff). This is the strongest result in Dioxus's favor and it
  is real, not aspirational.
- **Gate ② (parity effort) is roughly a wash, trending slightly against Dioxus.** ~11% more
  view/reactivity LOC to cover *both* targets in one crate versus Leptos-browser + Tauri-native
  combined is a genuinely modest unification tax — arguably a good trade for one crate instead of two.
  But the honest total (counting the self-built editor engine) is ~36% more original LOC, and roughly
  4.2 subagent-hours were spent reaching parity with no comparable Leptos baseline to say whether that
  is fast or slow by comparison.

**What tips this to "inconclusive, leaning keep" rather than "adopt":** the unification gate's
strongest evidence — Task 13's desktop auto-connect — is explicitly **compile-verified, not
runtime-verified**. The one thing this spike cannot claim is that the Dioxus client actually *works* end
to end for a human: no browser was ever opened, no desktop window was ever driven, no folder picker was
ever clicked, no file was ever actually typed into either editor. Every one of the three real
reactivity bugs this spike caught (the dead handover reconnect, the latent hooks-panic, the socket
teardown race) compiled clean and would have shipped invisibly without a review pass built specifically
to look for Dioxus's exact tracked-read and positional-hooks rules — which means there is a live,
unquantified risk that further such bugs exist in code that has likewise never been exercised at
runtime (the sidecar's window-close-kill behavior is the most concrete named instance, but not
necessarily the only one). Similarly, the web editor's unhighlighted state is a genuine, permanent
capability gap versus `ui/`'s highlighted editor (via `kode-leptos`), not a "coming soon" — so "one
codebase, two targets" does not currently mean "one codebase, one feature set."

**Given that:** this spike does not produce enough evidence to recommend migrating the shipped `ui/` +
`desktop/` + Tauri stack to Dioxus today. It *does* produce enough evidence to say Dioxus is a
credible, ecosystem-mature, low-API-churn candidate worth a **second, runtime-driven spike** — one that
actually opens a browser and a desktop window and drives the flows this one could only compile-check —
before any adoption decision is made. Keep `ui/` as the shipped client; treat `ui-dioxus/` as a
validated-but-unproven parallel candidate, not dead-end throwaway work: the transport seam, the pure
port of `net::{url,tree,view_model}`, and the desktop-native highlighter are all real, working,
tested code that a follow-up runtime-verification pass could build on directly rather than redo.
