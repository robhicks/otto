# Consolidated results — 2026-07-21 UI runtime spike

Sources: `results/leptos-web.md`, `results/dioxus-web.md`, `results/leptos-desktop.md`,
`results/dioxus-desktop.md`, `results/toolchain.md`.

## Step matrix (11 steps × 4 builds)

Legend: `✓` PASS · `NV` NOT-VERIFIABLE · `NA` NOT-APPLICABLE · `—` not reached · `P` PARTIAL

| # | Step | leptos-web | dioxus-web | leptos-desktop | dioxus-desktop |
|---|---|:--:|:--:|:--:|:--:|
| 1 | Connect | ✓ | ✓ (after fix) | — | — |
| 2 | Status strip (LLM degraded visible) | ✓ | ✓ | — | — |
| 3 | Prompt → live event stream | ✓ | ✓ | — | — |
| 4 | Abort mid-turn | NV¹ | NV¹ | — | — |
| 5 | Reconnect + `last_seq` replay (exactly-once) | ✓ | ✓ | — | — |
| 6 | Workspace tree (`.env` filtered out) | ✓ | ✓ | — | — |
| 7 | Open file in editor | ✓² | ✓² | — | — |
| 8 | Type into buffer | ✓ | P³ | — | — |
| 9 | Diff approval | NA⁴ | NA⁴ | NA⁴ | NA⁴ |
| 10 | Token meter + pause/resume | ✓/NV⁵ | ✓/NV⁵ | — | — |
| 11 | Promote to loopback + demote | ✓ | ✓ | — | — |

**Desktop columns (`—`):** neither desktop client completed the 11-step scenario at runtime. The
harness (bare Xvfb) could not render WebKitGTK; the real-session retry launched the Tauri app
successfully (proving app-start + shell-script-sidecar acceptance) but the user took the ADOPT
decision and skipped the remaining desktop runtime steps before they ran. Desktop runtime remains
**unverified** for both clients — see `leptos-desktop.md` / `dioxus-desktop.md`.

Notes:
1. **Step 4 (abort) NV, both web builds:** offline `LocalProvider` turns complete faster than an
   Abort command's WS round-trip, so a mid-turn interrupt cannot be exercised. Applies to all builds
   equally. (leptos-web additionally recorded that Abort unconditionally closes the WS even
   post-completion — `crates/engine/src/serve.rs:616-619`, pre-existing server design, not a UI defect.)
2. **Step 7:** both web editors render file content as **plain text (unhighlighted)** — a known
   pre-existing gap (`kode-leptos` highlighting was not exercised / Dioxus web tree-sitter was deferred
   in spike #1). Desktop highlighting was never runtime-checked.
3. **Step 8 dioxus-web PARTIAL:** the typed edit appears in the local buffer (sub-clause a ✓), but the
   minimal Dioxus textarea editor shows **no dirty/unsaved marker** (sub-clause b unmet); leptos-web
   showed `src/lib.rs ●`. A never-implemented feature, fully disclosed — not a runtime defect.
4. **Step 9 NA, all builds:** the offline-deterministic Coder proposes no edits, so no
   `ApprovalRequest` ever fires (baseline `grep -c ApprovalRequest` = 0 across two prompts). The
   diff-approval dimension is **untested** on every client, not passed.
5. **Step 10:** pause/resume PASS (the checkpoint fires before the Planner's first `AgentStarted`); the
   token/cost meter is NV because the offline path never emits `TokenCostMeter` (`emit_meter` gated on
   `meter.total() > 0`; offline = 0 tokens). Applies to all builds.

## Measurements

3 reps, median (min–max). `n/a` = not applicable to that build class.

| Measure | leptos-web | dioxus-web | Comparable? |
|---|---|---|:--:|
| wasm bundle raw | 1,568,220 B | 2,164,972 B (unopt⁶) | partial⁶ |
| wasm bundle gzip | 483,131 B | 575,375 B (unopt⁶) | partial⁶ |
| First paint (FCP) | 72 ms (68–76) | 72 ms (72–80) | ✅ identical |
| Cold → connected | 2584 ms (2496–2715)⁷ | ~51 ms warm (51–57)⁷ | ❌ method-mismatch⁷ |
| Event render latency | 56.8 ms (54–58) | 59.1 ms (57–71) | ✅ comparable |
| Reconnect replay | 4.9 ms (4.8–5.2) | 3.5 ms (3.3–4.0) | ✅ comparable |
| Build wall-clock | 62 s | 49 s | ✅ Dioxus faster |
| Desktop RSS | n/a (web) | n/a (web) | — never measured⁸ |
| Desktop binary size | 13,748,208 B (Tauri `app`)⁸ | 25,543,536 B⁸ | build-time only⁸ |

6. **wasm size — partial comparability:** Dioxus's `wasm-opt` step crashed (SIGABRT/DWARF) during the
   build, so its shipped wasm is **unoptimized**, while `trunk` ran `wasm-opt` on the Leptos wasm.
   Dioxus's *optimized* size would be smaller than 2.16 MB; the raw gap (~38%) overstates the real
   difference. The gzipped gap (~19%) is the fairer figure but still inflated. **Flagged, not resolved
   — re-measure with a working `wasm-opt` before treating bundle size as a decision input.**
7. **Cold → connected — NOT comparable:** the `leptos-web` figure (2584 ms) was measured by polling
   *after* navigate, which conflates tool-round-trip latency with wasm-boot time (the contaminated
   method is baked into the leptos-web baseline). `dioxus-web` used a contamination-free in-page
   observer (~51 ms warm-cache). Reproducing the contaminated method on Dioxus gave 4472–5116 ms with
   the connect already complete before the first poll — confirming the reference number is a
   tool-latency floor, not real boot time. **This row cannot decide anything; if cold-start matters,
   re-measure both clients with the in-page method.**
8. **Desktop RSS never measured** (no desktop run completed). Binary sizes are build-time only: the
   Dioxus desktop binary (24.4 MiB) is a single self-contained executable; the Tauri `app` binary
   (13.1 MiB) additionally needs the ~27 MB `otto` sidecar + system WebKitGTK, so a straight
   binary-size diff understates the Tauri shipped footprint. Not a runtime comparison.

## Consolidated bug log

| Build | Step | Symptom | Cause class | Compiler/test-catchable? | Fix | Fix cost |
|---|---|---|---|---|---|---|
| dioxus-web | 1 | `?ws=…&autoconnect=1` did nothing; client never auto-connected on web | **other (missing wiring)** — `parse_launch_params` ported byte-identical with passing host tests, but only the desktop `boot()` path called it → dead code on web | No — compiles clean (no unused-warning since the fn is called on desktop); the unit test exercises the parser in isolation, not the absent web call site | +35 LoC `#[cfg(feature="web")]` `use_future` mount hook, `ui-dioxus/src/app.rs` | one file, one build cycle |

**Per-client bug totals:**
- `leptos-web`: **0** runtime bugs (1 pre-existing server-design observation recorded, not a UI bug).
- `dioxus-web`: **1** runtime bug — a missing-wiring/dead-code defect, **not** a reactivity-class bug.
- Desktop clients: **not exercised** — bug count unknown at runtime.

**Key evidentiary point:** the two paths spike #1 flagged as compile-clean-bug hot spots — reconnect
teardown (step 5) and promote/demote handover incl. the `Promoted`-token delivery that was spike #1's
Critical (step 11) — **both ran clean at runtime on Dioxus** (step 5: per-session `count(*) ==
count(distinct seq)`, no duplicate DOM rows, 0 console errors during teardown; step 11: real reconnect
`8899→42847`, seq `34→45` continuous, same pid holding all ports). The one bug found was missing
integration wiring, which is **neutral** evidence about Dioxus's reactivity robustness, not a further
instance of the reactivity fragility spike #1 warned about.

## Global-constraint compliance

- `git status crates/` was empty across every run task — **no engine or protocol change was needed**
  on any client (spike #1's headline finding holds at runtime).
- No provider API keys were set in any run; every server ran offline-deterministic (both router slots
  `LocalProvider`), which is what makes the event streams byte-comparable across clients.
