# Dioxus UI-axis Spike — Comparison Report

**Date started:** 2026-07-11
**Design:** `2026-07-11-ui-dioxus-spike-design.md`
**Status:** 🚧 In progress — rows appended as slices land.

## Per-slice effort log

| Slice | View/reactivity LOC | Pure-logic LOC | Wall-clock | `cfg` edge-gates | Notes |
|---|---|---|---|---|---|
| _(rows appended per slice)_ | | | | | |

**Leptos baseline (for comparison):** `ui/` totals — measure with
`tokei ui/src` or `wc -l ui/src/**/*.rs` and record here once, split the same way.

## Narrative evidence (not scored)

### DX / reactivity
_(notes)_

### Build / toolchain (incl. WASM bundle size)
_(notes)_

### Ecosystem / editor
_(notes)_

### Runtime perf
_(notes)_

## Priority gate ① — Multi-target unification
_(% shared tree, edge-gate total, and the yes/no: does the one crate replace `ui/` + `desktop/` + Tauri?)_

## Priority gate ② — Parity effort
_(total view/reactivity LOC + wall-clock vs the Leptos baseline)_

## Verdict
_(keep-Leptos / adopt-Dioxus / inconclusive, + the evidence that drove it)_
