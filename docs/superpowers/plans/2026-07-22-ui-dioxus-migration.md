# Dioxus UI Migration Plan

> **Status:** APPROVED DIRECTION (ADOPT verdict, spike #2), NOT YET STARTED. Phased plan. Phase 0 is a
> hard gate; several later phases need their own design pass before bite-sized implementation steps are
> written. `ui/` and `desktop/` stay shipped until Phase 4 retires them.

**Goal:** Replace the shipped `ui/` (Leptos CSR) + `desktop/` (Tauri) UI stack with the single
`ui-dioxus/` crate (web + native desktop from one source), retiring two toolchains and the
static-bundle handoff.

**Origin:** [`../specs/2026-07-21-ui-dioxus-runtime-spike-report.md`](../specs/2026-07-21-ui-dioxus-runtime-spike-report.md)
(ADOPT verdict) + [`../specs/2026-07-11-ui-dioxus-spike-report.md`](../specs/2026-07-11-ui-dioxus-spike-report.md)
(unification analysis).

## Global Constraints

- **No engine or protocol change.** Both spikes proved the boundary is client-agnostic; a migration
  that needs a `crates/` change is a red flag to escalate, not absorb.
- **The incumbent stays shipped until Phase 4.** `ui/` + `desktop/` are not touched until `ui-dioxus/`
  has reached verified parity on both targets. No flag-day.
- **`ui-dioxus/` stays workspace-excluded, `protocol`-only** until the moment it replaces `ui/`, so the
  offline determinism suite and `cargo build --workspace` stay untouched during migration.

---

## Phase 0 — Desktop runtime verification (HARD GATE)

**Why first:** the ADOPT decision was made on web-runtime evidence; the desktop client's
replacement of Tauri — the structural point of adopting Dioxus — was never runtime-verified in either
spike. This phase validates that assumption before any incumbent code is retired. **If it fails,
migration pauses here and the verdict is revisited.**

- [ ] Launch `ui-dioxus --features desktop` on a real desktop session (native rendering; the headless
      Xvfb path is confirmed non-viable — WebKitGTK renders black under Xvfb, see spike report §1).
- [ ] Verify the folder picker provisions a workspace root and the bundled `otto serve` sidecar
      auto-spawns and the webview auto-connects (spike #1 compile-verified only — `desktop_boot.rs`).
- [ ] Verify **window-close kills the sidecar** (the `kill_on_drop` claim in `desktop_boot.rs`, spike
      #1's named unresolved risk) — close the window, confirm no orphaned `otto serve` process.
- [ ] Drive the 11-step scenario contract on desktop (native rendering makes the picker/editor
      drivable): connect, prompt→events, tree, open file (**confirm desktop syntax highlighting
      actually renders** — the one editor capability web lacks), promote/demote.
- [ ] Record the results as `dioxus-desktop.md`, replacing its current NOT-RUN status.

**Exit criterion:** the Dioxus desktop app launches, auto-connects, runs a turn, and cleans up its
sidecar on close, on a real session. Only then does Phase 1 begin.

---

## Phase 1 — Close the concrete gaps the spike surfaced

Independent fixes, each shippable on its own, no cutover risk. Each needs a test and a commit.

- [ ] **Fix the `wasm-opt` crash.** Dioxus's build ships unoptimized wasm (SIGABRT/DWARF crash in
      `wasm-opt` on this toolchain) — the web bundle is ~2.16 MB unoptimized vs a fair optimized target.
      Diagnose (DWARF version mismatch is the likely cause) and get `wasm-opt` running, then re-measure
      bundle size for a fair figure. **Gates any bundle-size claim.**
- [ ] **Close the web syntax-highlighting gap** (spike #1's deferred Task 12). The Dioxus web editor
      renders plain text; desktop highlights via native tree-sitter. Options (own mini-design):
      `web-tree-sitter` wasm via JS interop, or a lighter web highlighter. Reaching "one codebase, one
      feature set" requires this — until then web is a permanent capability step below desktop.
- [ ] **Add the editor dirty/unsaved marker** to the Dioxus editor (step 8 PARTIAL in the spike — the
      Leptos editor shows `path ●`, Dioxus shows nothing).
- [ ] **Wire desktop-sidecar capability flags.** Both shipped desktop apps spawn `otto serve` with no
      `--approve-edits`/`--promote-loopback`, so diff-approval and promote/demote are unreachable on
      desktop (`ui-dioxus/src/desktop_boot.rs:70-77`). Decide the desktop capability policy and pass the
      flags (the spike used an external shim; the real fix is in the app's spawn args). Applies to the
      Dioxus desktop app going forward.
- [ ] **Add an automated test for web autoconnect** — the spike's one runtime bug (dead-code parser,
      no web call site) shipped because only an isolated unit test existed. A wasm integration test
      covering the web mount→connect path would have caught it; add it so the class can't recur.
- [ ] **Add an automated test for the desktop sidecar `PR_SET_PDEATHSIG` teardown** (Phase 0 Gate-E
      follow-up; deferred from PR #95's review — finding #4). Gate E was verified manually
      (operator window-close). A subprocess integration test — spawn a helper that installs the guard
      on a child, kill the helper, assert the grandchild dies — would convert the one-time manual
      check into a regression-checked invariant. Same class as the web-autoconnect test above.

---

## Phase 2 — Build & serve story (NEEDS ITS OWN DESIGN PASS)

Replacing `trunk`/`ui/dist` + Tauri's `externalBin` bundling with `dx`. Decisions to settle in a short
design before bite-sized steps:

- [ ] How `otto serve` finds and serves the Dioxus web bundle (today it serves `ui/dist`; the Dioxus
      output lives under `target/dx/.../web/public`). Does the engine's static-serve path change, or does
      the build stage assets into a stable location? (Engine change, if any, must be additive.)
- [ ] Desktop packaging: `dx bundle` vs the Tauri `.deb`/`.rpm` story `desktop/` ships today; installer
      parity (icons, identifier, autostart of the sidecar).
- [ ] CI: replace the `ui/` wasm-build + `desktop/` Tauri-build jobs with the `dx` equivalents; keep the
      workspace determinism suite untouched (both UI crates stay excluded).

---

## Phase 3 — Parity sign-off

- [ ] Re-run the frozen 11-step scenario contract against `ui-dioxus/` on **both** targets (web +
      desktop), now with the Phase 1 gaps closed, and confirm parity with the last recorded `ui/`
      behavior. This is the go/no-go for retiring the incumbent.
- [ ] Confirm no protocol/engine change was needed anywhere in Phases 0–3 (`git status crates/` empty).

---

## Phase 4 — Retire the incumbent

Only after Phase 3 signs off.

- [ ] Repoint any docs/build scripts/CI that reference `ui/dist` or the Tauri app to the Dioxus outputs.
- [ ] Remove `ui/` and `desktop/` (and drop them from the root `Cargo.toml` `exclude` list, leaving
      `ui-dioxus`). Verify `cargo build --workspace` + `cargo test --workspace` still pass.
- [ ] Update `CLAUDE.md`'s UI section: the UI axis is now the single `ui-dioxus/` crate (web + desktop),
      pointing at both spike reports as the decision record.
- [ ] Final whole-branch review + PR.

---

## Notes

- **Spike scaffolding cleanup:** the spike tooling under
  `docs/superpowers/spikes/2026-07-21-ui-runtime/` (fixture, driver, shim, run logs) is the evidence
  trail for this decision; keep it until the migration lands, then it can be pruned or archived.
- **Phases 0, 1, 3, 4 are concretely scoped** and can proceed to bite-sized task plans as each is
  reached. **Phase 2 needs a design pass first** (brainstorming → design → plan) because the build/serve
  cutover has open decisions that shouldn't be guessed.
