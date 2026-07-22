# Dioxus runtime-verification spike (#2) — Report

**Date:** 2026-07-21 – 2026-07-22
**Design:** [`2026-07-21-ui-dioxus-runtime-spike-design.md`](2026-07-21-ui-dioxus-runtime-spike-design.md)
**Plan:** [`../plans/2026-07-21-ui-runtime-spike.md`](../plans/2026-07-21-ui-runtime-spike.md)
**Predecessor:** [`2026-07-11-ui-dioxus-spike-report.md`](2026-07-11-ui-dioxus-spike-report.md) (spike #1, "inconclusive, leaning keep")
**Verdict:** **ADOPT Dioxus** — decision taken by the project owner on the web-runtime evidence + spike #1's unification finding, with the desktop-runtime gap explicitly accepted (see §Verdict).

---

## 1. What ran

The spike drove the four UI builds through one frozen 11-step scenario contract
([`2026-07-21-ui-runtime-scenario.md`](2026-07-21-ui-runtime-scenario.md)) against one shared,
offline-deterministic engine configuration (`otto serve --approve-edits --promote-loopback`, both
router slots `LocalProvider`, fresh sqlite store per run). Web clients were driven in-page with
Playwright; assertions were made on rendered DOM, the sqlite event store, and files on disk.

| Build | Runtime-driven? | Outcome |
|---|---|---|
| `leptos-web` | **Yes, full 11-step** | Reference run. 0 runtime bugs. |
| `dioxus-web` | **Yes, full 11-step** | 1 runtime bug found + fixed (+35 LoC). |
| `leptos-desktop` (Tauri) | **Partially** | Launch + shell-script-sidecar acceptance verified on the real session; 11-step scenario not completed. |
| `dioxus-desktop` | **No** | Never run at runtime (adopt decided first). Builds clean. |

**Why the desktop leg is incomplete.** The headless harness (bare Xvfb) cannot render WebKitGTK —
the webview draws solid black and GTK windows malform (1×1, no `WM_CLASS`), even after adding openbox
as a window manager and forcing software rendering. This is a **bare-Xvfb/WebKitGTK limitation shared
by both desktop clients** (both use the `webkit2gtk` backend), not an app defect — confirmed by gdb
backtraces, a custom minimal window manager, and D-Bus tracing during the run. The desktop leg was
retried on the real GNOME/Wayland session (openbox + software rendering confirmed the WM gap was the
issue, not the app); the Tauri app **launched successfully on the real session**, proving app-start
and — resolving the plan's named risk — that **Tauri accepts the shell-script sidecar shim**. Before
the folder-pick interaction completed, the project owner made the ADOPT decision and elected to skip
the remaining desktop runtime steps.

## 2. Step matrix

Full matrix with per-cell notes in [`../spikes/2026-07-21-ui-runtime/results/summary.md`](../spikes/2026-07-21-ui-runtime/results/summary.md). Web clients, condensed:

`leptos-web`:  `1✓ 2✓ 3✓ 4:NV 5✓ 6✓ 7✓ 8✓ 9:NA 10:✓/NV 11✓`
`dioxus-web`:  `1✓ 2✓ 3✓ 4:NV 5✓ 6✓ 7✓ 8:PARTIAL 9:NA 10:✓/NV 11✓`

Both web clients pass every step that the offline-deterministic configuration makes testable. The
non-`PASS` cells are identical in cause across both clients (step 4 abort = offline turn too fast to
interrupt; step 9 = offline Coder proposes no edits; step 10 meter = never emitted offline) — i.e.
they are properties of the shared test configuration, not client differences. The one client
difference is step 8: `dioxus-web`'s minimal editor shows no unsaved/dirty marker (PARTIAL) where the
Leptos editor does.

## 3. Runtime bug log

| Build | Bug | Cause class | Fix |
|---|---|---|---|
| `dioxus-web` | `?ws=…&autoconnect=1` never auto-connected on web | **missing wiring** — `parse_launch_params` ported with passing host tests but only called on the desktop path (dead code on web) | +35 LoC `#[cfg(feature="web")]` mount hook |

`leptos-web`: 0 runtime bugs. Desktop clients: not exercised.

**The two spike-#1 hot spots ran clean on Dioxus at runtime.** Spike #1's three compile-clean bugs
were reactivity-class (tracked-read, positional-hooks, teardown-race), and it warned of an
"unquantified further-bugs risk" in code never exercised. This spike exercised the two riskiest paths:
- **Reconnect replay (step 5)** — the socket-teardown-race path — ran clean: per-session
  `count(*) == count(distinct seq)`, no duplicate DOM rows on replay, **0 console errors during
  teardown+reconnect**.
- **Promote/demote handover (step 11)** — including the `Promoted`-token delivery that was spike #1's
  *Critical* — ran clean: real reconnect (`8899→42847`), seq `34→45` **continuous** across the
  handover, source engine never killed (same pid holding all ports), 0 console errors.

The single runtime bug found was **missing integration wiring, not a reactivity miswrite** — neutral
evidence about Dioxus's reactivity robustness. Notably it is exactly the failure mode spike #1's review
process could *not* have caught by inspection alone and the compiler/unit-tests did *not* catch: a
byte-identical port with passing isolated tests but no call site. It compiled clean and shipped
invisibly until the client was actually run — direct vindication of this spike's premise.

## 4. Measurements

Full table in [`summary.md`](../spikes/2026-07-21-ui-runtime/results/summary.md). Headlines (web,
3-rep medians):

- **First paint:** identical (72 ms both).
- **Event render latency:** comparable (leptos 56.8 ms, dioxus 59.1 ms).
- **Reconnect replay:** comparable (leptos 4.9 ms, dioxus 3.5 ms).
- **Build wall-clock:** Dioxus faster (49 s vs 62 s).
- **wasm bundle:** Dioxus larger — but **its `wasm-opt` crashed**, so it shipped unoptimized wasm
  against an optimized Leptos wasm. The raw ~38% / gzip ~19% gaps overstate the real difference and
  must be re-measured with a working `wasm-opt` before counting as a con.
- **Cold → connected:** **not comparable** — the reference used a tool-contaminated polling method
  (2584 ms) baked into its baseline; the honest in-page Dioxus number is ~51 ms warm-cache. This row
  decides nothing as recorded.
- **Desktop RSS:** never measured (no desktop run completed).

## 5. Findings

**Two mandatory re-checks of spike #1's claims:**

1. **Did any protocol or engine change become necessary?** — **No.** `git status crates/` was empty
   across every run task; the one runtime fix lived entirely in `ui-dioxus/`. Spike #1's headline —
   that otto's protocol/engine boundary is genuinely client-agnostic — **holds at runtime**, now
   demonstrated by a second client actually exercising it, not just compiling against it.

2. **Does the Dioxus desktop app genuinely replace Tauri when launched?** — **Still unproven at
   runtime.** Spike #1 compile-verified the desktop auto-connect and `kill_on_drop` sidecar teardown;
   spike #2 did *not* advance this — the Dioxus desktop app was never runtime-driven. What spike #2 did
   establish on the desktop axis: the app builds clean (24.4 MiB self-contained binary) and the
   shell-script sidecar shim is accepted by Tauri (relevant to the parallel Leptos-desktop path). The
   desktop-replaces-Tauri claim remains exactly where spike #1 left it.

**Byproduct findings surfaced by actually trying to run the shipped stack:**
- **Neither shipped desktop app passes capability flags to its sidecar** — both spawn
  `otto serve --root <picked> --port 8787` only (`ui-dioxus/src/desktop_boot.rs:70-77`,
  `desktop/src-tauri/src/lib.rs:45-53`), so diff-approval and promote/demote are unreachable on desktop
  as shipped (worked around here with a sidecar shim). A real gap in both shipped desktop products.
- **The shipped Tauri app had a placeholder bundle identifier** (`com.tauri.dev`) that blocked release
  bundling until corrected to `dev.otto.desktop`.
- **Dioxus's `wasm-opt` build step crashes** (SIGABRT/DWARF) on this toolchain, silently shipping
  unoptimized wasm — a real build-pipeline issue to fix before shipping a Dioxus web bundle.
- **`dx build` needs `--platform web|desktop`**, not just `--features` — a toolchain nuance.

## 6. Verdict — ADOPT

**The project owner selected ADOPT Dioxus**, on the strength of the fully runtime-verified web
evidence plus spike #1's unification finding, and explicitly accepted skipping the remaining desktop
runtime verification.

The evidence supporting the call:
- **Unification (spike #1's strongest pro-Dioxus result):** one crate structurally replaces
  `ui/` + `desktop/` + Tauri (~80% shared, ~14 cfg edges), compile-verified.
- **Web runtime parity confirmed:** every offline-testable scenario step passes on `dioxus-web`;
  perf is comparable (identical FCP, comparable event/reconnect latency) and build time is faster.
- **Reactivity held up under runtime exercise:** the two paths spike #1 flagged as fragile ran clean;
  the only runtime bug was missing wiring, not reactivity.
- **Client-agnostic boundary confirmed at runtime:** zero protocol/engine changes across both clients.

**The honest counterweight, stated plainly (not buried):** this ADOPT decision is made on web-runtime
evidence. The desktop client — whose replacement of Tauri is the *entire structural point* of adopting
Dioxus — was **never runtime-verified** in either spike. The one negative metric (larger wasm) is
partly a build artifact (`wasm-opt` crash) and unquantified until re-measured. A fully evidence-driven
verdict would have completed the desktop runs first; the owner chose to decide on the available
evidence instead, which is a legitimate call but means the desktop-replaces-Tauri claim carries into
migration **as an assumption to be validated, not a proven result.**

## 7. Disposition

Per the design's ADOPT branch: a **migration plan is written** (see
[`../plans/2026-07-22-ui-dioxus-migration.md`](../plans/2026-07-22-ui-dioxus-migration.md)); `ui/` and
`desktop/` **stay shipped** until it lands. The migration plan's **first task is the deferred desktop
runtime verification** — the Dioxus desktop app must be actually launched, auto-connect confirmed, and
window-close-sidecar-kill exercised on a real session **before** `desktop/` + Tauri are retired, so the
one unproven assumption behind this ADOPT is validated at the start of migration rather than after the
incumbent is gone. The migration also carries the concrete fixes this spike surfaced: the `wasm-opt`
crash, the web-highlighting gap (spike #1's deferred Task 12), the missing dirty-marker in the Dioxus
editor, and the desktop-sidecar capability-flags gap.
