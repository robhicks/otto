# Dioxus UI-axis evaluation spike — Design (WIP)

**Date:** 2026-07-11
**Status:** ✅ **DESIGN COMPLETE — pending user review.** All checkpoints presented and approved
during brainstorming. Ready for a final read-through, then **writing-plans**.

## Context

otto already ships a UI axis: `ui/` (Leptos CSR, browser, slices A–F) + `desktop/` (Tauri 2
wrapper around the same WASM bundle, slice G). Both are workspace-excluded and depend only on
`protocol`. This spike does **not** replace them.

## Decisions locked during brainstorming

- **Q1 driver = B:** evaluate/compare Dioxus vs the incumbent Leptos UI via a spike. Leptos stays.
- **Q2 scope = C:** full parity with Leptos slices **A–F** (complete parallel browser client).
- **Q3 targets = B:** **web (WASM) + native desktop** — tests collapsing `ui/` + `desktop/` into one crate.
- **Q4 editor = C:** **Dioxus-native editor with tree-sitter** highlighting (not textarea, not JS-editor wrap).
- **Q5 scorecard = all dimensions, prioritizing ① multi-target unification and ② parity effort.**
- **Sequencing approach = 1 ("Unification-spine first"):** shell on web+desktop simultaneously
  from day one to prove/measure unification early; then port B→F web-first (desktop follows if
  the seam holds); tree-sitter editor is the final isolated sub-phase; parity effort
  instrumented continuously (per-slice LOC + wall-clock).

## §Goal & non-goals (APPROVED)

**Goal:** a complete parallel browser+desktop Dioxus client reaching parity with Leptos `ui/`
(A–F), instrumented for a Dioxus-vs-Leptos verdict — multi-target unification and parity effort
are the primary scored dimensions.

**Non-goals:** not replacing Leptos (`ui/`/`desktop/` untouched, additive); no engine/orchestrator
behavior changes (pure network client); not a production app (deliverable = working client +
comparison report; polish only where it affects a scored dimension); no mobile target.

## §Crate layout & the protocol seam (APPROVED)

New crate **`ui-dioxus/`**, mirroring existing constraints:
- **Workspace-excluded** (`exclude = ["ui", "desktop", "ui-dioxus"]` in root `Cargo.toml`) — the
  offline determinism suite and `cargo build --workspace` stay byte-for-byte untouched.
- **Depends only on `protocol`** (path dep; WASM for web, native for desktop). Never links
  `engine-core` or any impl crate.
- **One crate, two targets via cargo features:** `web` (`dioxus-web`) + `desktop`
  (`dioxus-desktop`). A single feature-gated crate covering both targets is itself the
  unification test. If it can't stay one crate, that's finding #1.

## §Transport — reused as-is, zero engine changes (APPROVED)

Reuses the server side the Leptos UI already proved: WS to `/ws` with bearer via `?token=`;
`POST /workspace`/`/promote`/`/export` (CORS already added in slice C); all wire types already
in `protocol`. **No protocol change, no engine change.** A needed protocol addition is a red
flag to surface, not to quietly make (the Leptos client is proof it's unnecessary).

## §Slice-by-slice parity mapping A–F (APPROVED)

Existing `ui/src/*.rs` are the executable spec — port behavior, not code; log effort per slice.

- **A** shell + live session (`app.rs`/`ws.rs`/`view_model.rs`): WS connect, `SendPrompt`, live
  `Event` render, `Abort`, `last_seq` reconnect. Leptos signals + hand-rolled `view_model` vs
  Dioxus `use_signal`/`use_resource` + coroutine for the socket. **Reactivity comparison lives here.**
- **B** capabilities + status strip: decode `CapabilitiesManifest` from `Ready`, render
  engine/LLM/sandbox strip with visible degradation. Mostly view logic — cheap parity.
- **C** workspace tree + editor (`tree.rs`/`workspace.rs`): `POST /workspace` List, collapsible
  tree, open file → editor (editor = own § below).
- **D** diff approval: decode `ApprovalRequest`, render diff (port pure `diff_lines`),
  Approve/Reject → `ApproveDiff`. `diff_lines` is framework-agnostic — ports directly.
- **E** token/cost meter + pause/resume: render `TokenCostMeter`, Pause/Resume → commands. Trivial.
- **F** promote-to-remote (`url.rs`): Promote/Demote gated by `can_promote`/`can_demote`,
  reconnect to handed-back endpoint on `Promoted`/`Demoted`. Tests reconnect under Dioxus lifecycle.

**Reusable seam:** factor the WS decode loop + pure helpers (`diff_lines`, URL/query parsing)
into a plain `protocol`-only module so parity-effort numbers isolate framework view/reactivity
effort from pure-logic effort.

## §The editor — Dioxus-native + tree-sitter (APPROVED)

- **Rendering:** Dioxus component rendering an editable buffer as styled spans; spike the
  simpler controlled-buffer approach first (matches diff-first non-goal; no VSCode-scale features).
- **Highlighting via tree-sitter, split by target (the crux):**
  - **Desktop (native):** Rust `tree-sitter` crate + grammar crates compile natively; reuse the
    exact language set `retrieval` already vendors (Rust/JS/TS/Python/Go) — in-repo precedent.
  - **Web (wasm32-unknown-unknown):** C-based `tree-sitter` crate does not drop onto wasm
    cleanly. Spiked in order: (1) `web-tree-sitter` (official wasm build) via Dioxus JS interop /
    `wasm-bindgen`, grammars as `.wasm`; (2) compile parser+grammars to wasm + FFI if (1) too
    heavy. Timebox (1). **Native-vs-wasm divergence here is a headline unification finding.**
  - **Honest fallback:** if web tree-sitter blows the timebox, web degrades to unhighlighted
    editing while desktop keeps highlighting — the asymmetry is a recorded result, not hidden.

## §Multi-target unification mechanics (APPROVED)

- Shared component tree; `cfg(feature)` only at the edges (socket/platform glue, tree-sitter
  backend). How much must be gated is a measured unification result.
- **Web:** `dx serve` replaces `trunk` (toolchain data point).
- **Desktop:** `dioxus-desktop` native webview; reproduce the Tauri `desktop/` UX (folder-picker
  → workspace root, auto-launch local `otto serve` sidecar on fixed port, auto-connect) **inside
  the one Dioxus crate — no separate wrapper crate, no `ui/dist` sidecar handoff.** Whether this
  genuinely replaces `desktop/`+Tauri is the single most important reported result.

## §Instrumentation & the comparison scorecard (APPROVED)

The scorecard **is** the deliverable — the client is the instrument that produces it.

**Per-slice effort log — recorded live in the report file.** As each slice A–F (and the
editor, as its own line) reaches working parity, append a row directly to the report markdown
(`…-spike-report.md`, below). No end-of-spike reconstruction. Each row records:
- **LOC**, split into **view/reactivity LOC** (framework-specific) vs **pure-logic LOC** (the
  shared `protocol`-only module). This split is what keeps parity-effort honest — we compare
  framework surface, not re-counted `diff_lines`. The report states explicitly where the line
  was drawn.
- **Wall-clock** to working parity for the slice.
- **`cfg(feature)` edge-gate count** — the unification tax, measured per slice.

**Verdict form = narrative + priority gate** (not a weighted numeric sum). The two priority
dimensions **decide** the verdict; the other four are reported as narrative evidence, not scored
numerically — this is honest about what actually drives the decision and avoids laundering a
close call into false precision.

- **Priority gate ① — Multi-target unification:** % shared component tree, edge-gate count, and
  the single yes/no — *does the one Dioxus crate genuinely replace `ui/` + `desktop/` + Tauri?*
- **Priority gate ② — Parity effort:** total view/reactivity LOC + wall-clock vs the Leptos
  baseline (we have `ui/`'s actual LOC to measure against).
- **Narrative evidence (reported, not scored):** DX/reactivity (`use_signal`/`use_resource`/
  coroutine ergonomics vs Leptos signals); build/toolchain (`dx serve` vs `trunk`, **WASM bundle
  size** as a hard number vs the Leptos bundle, desktop build time & artifact size);
  ecosystem/editor (tree-sitter integration reality on both targets, crate maturity); runtime
  perf (event-stream render under load, editor typing latency — qualitative unless a red flag).

**Deliverable location:** `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md` (sibling
to this design), ending in a **verdict**: keep-Leptos / adopt-Dioxus / inconclusive, plus the
evidence that drove it.

## §Testing strategy (APPROVED)

Mirror `ui/`'s posture exactly, so the comparison stays apples-to-apples:
- **Host-side unit tests** (`cargo test` from inside `ui-dioxus/`) for every pure module — the
  shared WS-decode loop, `diff_lines`, URL/query parsing, capability decoding. Most test value
  lives here.
- **WASM compile check:** `cargo build --target wasm32-unknown-unknown --features web`.
- **Desktop build check:** `cargo build --features desktop`.
- **WS/reactive parts:** not unit-tested in `ui/` today; match that (drive manually against a
  live `otto serve`). If Dioxus makes component testing meaningfully easier than Leptos did,
  that's a **DX narrative win worth noting** — but we do **not** build a harness the incumbent
  lacks, or the parity comparison becomes unfair.

## §Build order — Approach 1, unification-spine first (APPROVED)

- **P0 — shell on web + desktop together.** Slice A (WS connect, `SendPrompt`, live `Event`
  render, `Abort`, `last_seq` reconnect) on *both* targets simultaneously. Proves/measures
  unification on day one. If it can't stay one crate → **finding #1**, surfaced immediately.
- **P1 — view slices, web-first:** B (status strip) → D (diff approval) → E (meter +
  pause/resume) → F (promote/demote). Desktop follows each if the seam holds. (C's tree ships
  with the editor.)
- **P2 — editor (slice C) as an isolated sub-phase:** tree + Dioxus-native controlled buffer,
  then tree-sitter — desktop-native first (known-good), then the web/wasm spike under its timebox.
- **P3 — write the verdict** from the accumulated instrumentation table.

## §Risks (APPROVED)

- **tree-sitter-on-wasm timebox blowout.** Mitigation: timebox the `web-tree-sitter` interop;
  honest fallback is web-degrades-to-unhighlighted while desktop keeps highlighting (a recorded
  asymmetry, not hidden).
- **Dioxus desktop packaging maturity.** `dioxus-desktop` webview + sidecar-launch may not
  cleanly reproduce the Tauri folder-picker/auto-connect UX. Mitigation: this *is* the headline
  result — a partial reproduction is still a valid finding, not a failure.
- **`dx` CLI vs `trunk` friction.** New toolchain. Mitigation: it's a scored data point, not a
  blocker.
- **Parity-effort measurement fairness.** The view/pure-logic LOC split is the mitigation; the
  report calls out explicitly where the line was drawn.

---

## Status: design complete — pending user review

All checkpoints presented and approved during brainstorming. Next: user reviews this file, then
invoke **writing-plans** to turn it into a phased implementation plan (P0→P3 above). New crate is
workspace-excluded `ui-dioxus/`, `protocol`-only dep.
