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

- [x] Launch `ui-dioxus --features desktop` on a real desktop session (native rendering; the headless
      Xvfb path is confirmed non-viable — WebKitGTK renders black under Xvfb, see spike report §1).
      — Operator run 2026-07-23 on a real GNOME/Wayland session; recorded in
      `../spikes/2026-07-21-ui-runtime/results/dioxus-desktop.md`.
- [x] Verify the folder picker provisions a workspace root and the bundled `otto serve` sidecar
      auto-spawns and the webview auto-connects (spike #1 compile-verified only — `desktop_boot.rs`).
      — Step 1 **PASS** (auto-connected after folder pick, no manual URL/token entry).
- [x] Verify **window-close kills the sidecar** (the `kill_on_drop` claim in `desktop_boot.rs`, spike
      #1's named unresolved risk) — close the window, confirm no orphaned `otto serve` process.
      — Gate E **FAIL → PASS**: root-caused to `kill_on_drop` not running on a non-unwinding
      teardown, fixed with `PR_SET_PDEATHSIG` and re-verified clean. Closed by PR #95 (`35545d2`);
      converted from a one-time manual check into a regression test by PR #98 (`ec041e5`).
- [x] Drive the 11-step scenario contract on desktop (native rendering makes the picker/editor
      drivable): connect, prompt→events, tree, open file (**confirm desktop syntax highlighting
      actually renders** — the one editor capability web lacks), promote/demote.
      — Steps 1, 2, 3, 6, 7, 11 **PASS** (incl. native tree-sitter highlighting and
      promote→loopback→demote). Steps 4/5/10 (abort, reconnect-replay, pause/resume) **NOT-RUN**,
      explicitly deferred to the Phase 3 re-run below; step 8 NOT-VERIFIABLE externally; step 9
      NOT-APPLICABLE (the offline Coder proposes no edits, so no diff-approval was raised).
- [x] Record the results as `dioxus-desktop.md`, replacing its current NOT-RUN status.
      — `../spikes/2026-07-21-ui-runtime/results/dioxus-desktop.md` now reads
      "RUN AT RUNTIME (2026-07-23) — ALL GATES PASS (Gate E after a fix)".

**Exit criterion:** the Dioxus desktop app launches, auto-connects, runs a turn, and cleans up its
sidecar on close, on a real session. Only then does Phase 1 begin.
**MET** 2026-07-23 (see the results doc's §Status). Phase 1 proceeded.

---

## Phase 1 — Close the concrete gaps the spike surfaced

Independent fixes, each shippable on its own, no cutover risk. Each needs a test and a commit.

**Status: COMPLETE** — all six items shipped as PRs #96–#101 (see the per-item merge notes below).
Phase 2 (build & serve story) is next and still needs its own design pass.

- [x] **Fix the `wasm-opt` crash.** ~~Dioxus's build ships unoptimized wasm (SIGABRT/DWARF crash in
      `wasm-opt` on this toolchain) — the web bundle is ~2.16 MB unoptimized vs a fair optimized target.
      Diagnose (DWARF version mismatch is the likely cause) and get `wasm-opt` running, then re-measure
      bundle size for a fair figure.~~ **Gates any bundle-size claim — now unblocked.**
      **Root cause:** not a DWARF *version* mismatch. The `wasm-release` cargo profile dx uses for
      release web builds emitted ~1.24 MB of DWARF-4 sections, and dx then ran
      `wasm-opt … -Oz --debuginfo`. `--debuginfo` makes dx's bundled binaryen (reports "version 127")
      parse that DWARF, and it aborts with `compile unit size was incorrect (this may be an
      unsupported version of DWARF)` → SIGABRT. `dx` logs that and **still exits 0**, copying the
      unoptimized wasm through — a silent failure.
      **Fix:** `[profile.wasm-release]` in `ui-dioxus/Cargo.toml` — `strip = "debuginfo"` drops the
      DWARF before wasm-opt sees it, plus `opt-level = "s"` because dx only injects its own
      `inherits`/`opt-level` when the manifest does *not* define the profile, so defining it would
      otherwise have silently dropped the build to `release`'s `opt-level = 3` (a ~92 KB regression).
      `debug = false` is deliberately **not** used: it does not work here (A/B verified — the profile
      already resolves to `debug = false` and the DWARF appears anyway).
      **Measured** with `ui-dioxus/scripts/measure-web-bundle.sh` (wipes `target/dx`, then fails
      rather than print a figure if wasm-opt errored, if DWARF survives, or if the wasm blows a size
      ceiling; prints raw + `gzip -9` bytes):

      | Artifact | Before (raw / gzip) | After (raw / gzip) |
      |---|---|---|
      | `otto-ui-dioxus_bg…wasm` | 2,164,985 / 571,929 | **795,188 / 318,320** |
      | `otto-ui-dioxus…js` | 59,927 / — | 59,927 / 13,960 |
      | `style…css` | 3,021 / — | 3,021 / 1,125 |
      | **total** | 2,227,933 / — | **858,136 / 333,405** |

      wasm raw **−63.3%**, gzip **−44.4%**; the bundle drops 2.23 MB → 0.86 MB raw (decimal MB
      throughout). Both columns come from the same tree differing only by this fix, and the same
      command, so the delta is attributable to the fix alone. The JS shim and CSS are unchanged in
      raw size (their pre-fix gzip figures were not captured, hence the dashes). Nothing was tuned
      for size beyond making wasm-opt actually run: wasm-opt's `-Oz` and rustc's `opt-level = "s"`
      are both dx's own defaults.
      **Phase 3 should re-run the script rather than quote these bytes.** They are a point-in-time
      A/B of *this* fix, not a standing figure — every other Phase 1 item moves it. Already true by
      the time this landed: rebased onto the dirty-marker item, the same command measures
      797,380 / 318,370.
      — Closed by PR #100 (`c17a70a`).
- [x] **Close the web syntax-highlighting gap** (spike #1's deferred Task 12). The Dioxus web editor
      renders plain text; desktop highlights via native tree-sitter. Options (own mini-design):
      `web-tree-sitter` wasm via JS interop, or a lighter web highlighter. Reaching "one codebase, one
      feature set" requires this — until then web is a permanent capability step below desktop.
      — Closed by PR #99 (`f706e0e`): a dependency-free web lexer (`src/editor/highlight_web.rs`)
      covering the same five languages, with a test asserting both backends agree on vocabulary and
      line structure.
- [x] **Add the editor dirty/unsaved marker** to the Dioxus editor (step 8 PARTIAL in the spike — the
      Leptos editor shows `path ●`, Dioxus shows nothing).
      — Closed by PR #96 (`8fc7d07`); render tests in `src/editor/dirty.rs`.
- [x] **Wire desktop-sidecar capability flags.** Both shipped desktop apps spawn `otto serve` with no
      `--approve-edits`/`--promote-loopback`, so diff-approval and promote/demote are unreachable on
      desktop (`ui-dioxus/src/desktop_boot.rs:70-77`). Decide the desktop capability policy and pass the
      flags (the spike used an external shim; the real fix is in the app's spawn args). Applies to the
      Dioxus desktop app going forward.
      — **Policy decided by the repo owner: pass BOTH flags.** Diff approval is a safety feature and
      defaulting it off on desktop is the wrong default; leaving promote/demote off would leave the
      shipped sub-project-F promote UI dead on desktop. Closed by PR #101: the argv is
      built by a new pure `serve_command()` in `desktop_boot.rs` and unit-tested (both flags present,
      full argv pinned, exactly one promote mode, and a source guard that `boot()` still routes
      through it). `--accept-promotions` is deliberately **not** passed — it opens inbound
      `/promote`+`/export` for *other* machines, which loopback promotion does not need.
      The spike's `otto-shim.sh` is now redundant for this purpose.
- [x] **Add an automated test for web autoconnect** — the spike's one runtime bug (dead-code parser,
      no web call site) shipped because only an isolated unit test existed. A wasm integration test
      covering the web mount→connect path would have caught it; add it so the class can't recur.
      — Closed by PR #97 (`5f0e2e6`): `src/web_mount_test.rs`, a `wasm-bindgen-test` browser test
      driving the real mount→autoconnect path.
- [x] **Add an automated test for the desktop sidecar `PR_SET_PDEATHSIG` teardown** (Phase 0 Gate-E
      follow-up; deferred from PR #95's review — finding #4). Gate E was verified manually
      (operator window-close). A subprocess integration test — spawn a helper that installs the guard
      on a child, kill the helper, assert the grandchild dies — would convert the one-time manual
      check into a regression-checked invariant. Same class as the web-autoconnect test above.
      — Closed by PR #98 (`ec041e5`): a `spawn_guarded` choke point plus `desktop_boot::pdeathsig_tests`
      covering both the ordinary path and the fork→prctl race, each paired with an unguarded control
      so a vacuous pass fails.

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
