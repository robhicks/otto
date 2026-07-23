# Run log — `dioxus-desktop`

## Status: RUN AT RUNTIME (2026-07-23, real GNOME/Wayland session) — ALL GATES PASS (Gate E after a fix)

Executed as **Phase 0** of the Dioxus migration plan
(`docs/superpowers/plans/2026-07-22-ui-dioxus-migration.md`), the hard gate that runtime-verifies
the desktop client's Tauri-replacement claim before any incumbent code is retired. This replaces the
prior NOT-RUN status.

**Outcome:** launch / auto-connect / turn / tree+highlight / promote-demote all **PASS** on the first
runtime verification of the Dioxus desktop client. **Sidecar-cleanup-on-close (Gate E) initially
FAILED** (a closed window orphaned the `otto serve` sidecar), was root-caused to `kill_on_drop`'s
dependence on Drop running, **fixed** with a kernel-level `PR_SET_PDEATHSIG` guard, and **re-verified
PASS** against the bundled release binary. Phase 0's exit criterion is therefore **MET**. See §Bugs
and §Disposition.

## Build

- Launched via `dx serve --platform desktop --features desktop` (Dioxus CLI **0.7.9**, `3e43ffa`) —
  **dev/hot-reload mode**, not a clean release bundle. Reported incremental times this run: App 0.9 s,
  Bundle 0.4 s, serving in 1.3 s (`shots/dioxus-desktop-step-e-close-orphan.png`).
- Sidecar: `otto` release binary `target/release/otto` (27,353,040 bytes), reached via the spike
  shim `otto-shim.sh` (`OTTO_BIN`) so the app's spawned `otto serve` gains `--approve-edits
  --promote-loopback`.
- Clean-release desktop binary size (prior build, unchanged this run): **25,543,536 bytes** (~24.4 MiB),
  single self-contained executable.
- **Caveat:** running under `dx serve` means the app ran as a child of the dx dev-supervisor. This is
  material to Gate E (see §Bugs) — the shipped product runs the bundled binary directly, a different
  process-teardown path.

## Environment

- Provider keys all unset for the run (`env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GEMINI_API_KEY`);
  both router slots resolve to `LocalProvider`. Status strip confirmed **`LLM: offline
  (deterministic)`** (`shots/dioxus-desktop-step1-connected.png`).
- `OTTO_DB=/tmp/otto-ui-spike/dioxus-desktop.db`.
- Sidecar bound `ws://127.0.0.1:8787` (fixed port from `desktop_boot.rs`).
- Workspace fixture: `/tmp/otto-ui-spike/fixture` (from `fixture.sh`).

## Steps

| # | status | evidence | notes |
|---|---|---|---|
| 1 | PASS | Window auto-connected after folder pick; status strip `connected a0da…seq — engine: local, LLM: offline (deterministic), sandbox: on` (`shots/dioxus-desktop-step1-connected.png`) | Folder picker → pick fixture → auto-connect, no manual URL/token entry. |
| 2 | PASS | Same screenshot: LLM indicator renders **degraded/offline** (bold `offline (deterministic)`), engine `local`, sandbox `on` — not blank, not "healthy" | Slice-B degraded-state render confirmed. |
| 3 | PASS | Event log rendered the full ordered spine sequence — Planner started / planned 1 milestone(s) / Planner finished / ContextFinder started / finished / Coder started / finished / Verifier started / **Verify cargo test passed** / Verifier finished / **TurnComplete ok**; status strip `seq 10` (`shots/dioxus-desktop-step3-turn-complete.png`) | 11-frame sequence, ends at seq 10, matches baseline frame-for-frame. Rendered live in-window. |
| 4 | NOT-RUN | — | Abort not exercised this Phase-0 gate run (deferred to Phase 3 full parity pass). |
| 5 | NOT-RUN | — | Reconnect/replay not exercised this run (deferred to Phase 3). |
| 6 | PASS | Tree shows `src` → `util` → `mod.rs`, `lib.rs`, `Cargo.toml`, `README.md`; **`.env` absent** from the rendered tree (`shots/dioxus-desktop-step1-connected.png`) | Sensitive-path floor filtered `.env` server-side; `src/util` expand/collapse is NOT-VERIFIABLE externally but was visually confirmed. |
| 7 | PASS | `src/lib.rs` opened and rendered **syntax-highlighted** (native tree-sitter) | Confirmed by operator; the one editor capability web lacks. |
| 8 | NOT-VERIFIABLE | (desktop — local unsaved buffer has no external artifact) | Per frozen contract. |
| 9 | NOT-APPLICABLE | offline Coder proposes no edits | Per frozen contract. |
| 10 | NOT-RUN | — | Pause/resume not exercised this gate run (deferred to Phase 3). |
| 11 | PASS | Promote → loopback handoff shown, seq continued (no reset); source sidecar stayed alive; Demote returned control | Confirmed by operator. `--promote-loopback` reached via shim. |
| **E** | **FAIL → PASS (after fix)** | **First run (`dx serve`):** closing the window left an orphaned `otto serve` sidecar (operator observed it running post-close; `dx serve` reported `Application [linux] exited gracefully` while the sidecar persisted — `shots/dioxus-desktop-step-e-close-orphan.png`). **Re-test (bundled release binary + `PR_SET_PDEATHSIG` fix):** window-close left **no** `otto serve` process and freed :8787 — operator confirmed CLEAN. | The migration plan's Phase 0 exit criterion (sidecar cleanup on close) is now met. Root cause + fix in §Bugs. |

## Measurements

Phase-0 **gate** run, not the full Phase-3 measurement pass — the eight-measure, three-repetition
matrix (RSS, first-paint/Ready timings, event-render latency, reconnect-replay time) was **NOT
CAPTURED** here and is deferred to Phase 3 parity sign-off. Captured this run:

- Desktop binary size (clean release, unchanged): **25,543,536 bytes**.
- Build wall-clock: not a clean-release measure this run (dev `dx serve`; App 0.9 s / Bundle 0.4 s
  incremental).
- Desktop RSS: NOT CAPTURED (Phase 3).

## Bugs

- **Failing step:** E (window-close sidecar cleanup).
- **Symptom:** closing the app window leaves an orphaned `otto serve` sidecar bound to :8787.
- **Cause class:** Dioxus **teardown** (per spike #1's taxonomy). The sidecar `Child` is held in a
  root-level `use_signal` (`ui-dioxus/src/app.rs:405`), spawned `kill_on_drop(true)`
  (`desktop_boot.rs:79`). `kill_on_drop` only kills the child when the signal's value is **dropped**,
  which requires the app's component teardown to run destructors on close. If the process is torn down
  without unwinding (SIGKILL / `std::process::exit` / dev-supervisor kill), Drop never runs and the
  sidecar orphans. Relying solely on `kill_on_drop` for sidecar lifetime is the fragile part.
- **Could a compiler or test plausibly have caught it?** No — it is a runtime process-lifecycle
  behavior invisible to the type system and to unit tests; only a live window-close exercises it.
  This is exactly the gap Phase 0 existed to close, and it did.
- **Dev-mode confound (resolved):** the failure was first seen under `dx serve` (hot-reload
  supervisor). The re-test used the **bundled release binary** (`dx build --release --features desktop
  --platform desktop`) run directly — removing the confound — and passed, so the fix holds on the
  shipped-shape artifact, not just the dev server.
- **Fix:** `ui-dioxus/src/desktop_boot.rs` gains `install_pdeathsig()`, which sets
  `PR_SET_PDEATHSIG → SIGKILL` on the sidecar via `pre_exec` (between fork and exec), so the kernel
  kills `otto serve` when the app process dies for *any* reason — independent of Drop running.
  Includes a `getppid() == 1` race guard. `kill_on_drop(true)` is retained for the graceful in-app
  disconnect; the two are complementary. Linux-only (no-op fallback elsewhere); `libc` added as a
  desktop-only dep. Desktop unit suite stays green (54 passed).
- **Re-verified:** bundled binary, window-close → no `otto serve`, :8787 free (operator-confirmed).

## Disposition

**A/B/C(steps 1–3,6,7,11) PASS: the structural Tauri-replacement thesis is runtime-confirmed** — the
Dioxus desktop app launches, auto-connects with no manual entry, runs a full offline turn, renders the
filtered tree and a natively-highlighted file, and promotes/demotes to loopback. This is the first
runtime evidence for the desktop client (spike #1 was compile-only).

**Gate E was fixed and re-verified, so Phase 0's exit criterion ("launches, auto-connects, runs a
turn, and cleans up its sidecar on close") is now MET** — on the bundled release binary, the
shipped-shape artifact. The ADOPT verdict stands on runtime evidence for the desktop client for the
first time, not just spike #1's compile-only check. **Phase 1 is unblocked.**
