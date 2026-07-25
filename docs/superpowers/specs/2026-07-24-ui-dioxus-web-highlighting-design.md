# Web syntax highlighting for `ui-dioxus` — design note

**Status:** DECIDED — option D (hand-rolled lexer, zero new dependencies) implemented.
**Scope:** Phase 1, item 2 of [`../plans/2026-07-22-ui-dioxus-migration.md`](../plans/2026-07-22-ui-dioxus-migration.md).
**Supersedes:** the "permanent platform asymmetry" finding recorded as spike #1's deferred Task 12
(`2026-07-11-ui-dioxus-spike-report.md`, Ecosystem/editor, slice C.4).

## Problem

`ui-dioxus`'s desktop build highlights via native tree-sitter (`src/editor/highlight_native.rs`,
five grammars behind the `desktop` feature). The web build has no equivalent and falls through to
`tokens::plain_spans`, so the browser editor renders as flat, unstyled text. Until this closes, web
is a permanent capability step below desktop and the migration's "one codebase, one feature set"
premise is unmet.

The constraint that shapes every option: **wasm bundle size**. The Dioxus web bundle is already the
migration's weakest number, and a highlighter that triples it trades one embarrassment for a worse
one. So every candidate below was *measured*, not estimated.

## Method

Each candidate was compiled to `wasm32-unknown-unknown` as a `cdylib` under an identical
size-optimized profile (`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`,
`strip = true`), and its `.wasm` compared against an empty control crate built the same way. The
control is **117 bytes**, so the reported figure is effectively the candidate's whole cost.

Reference point for judging those numbers: `ui-dioxus`'s own release wasm today is **2,537,702
bytes** (`cargo build --release --target wasm32-unknown-unknown --features web`). That figure is
pre-`wasm-opt`; a sibling Phase 1 item is fixing the `wasm-opt` crash separately, so no claim here
depends on the optimized number — the *deltas* are what matter and they are measured on the same
footing.

## Options considered

### A. Reuse the desktop path verbatim — the `tree-sitter` Rust crates, compiled to wasm

The ideal outcome: byte-identical output on both targets, one `highlight.rs`, no second grammar set.

**Rejected — does not compile.** tree-sitter grammars are C, built through `cc-rs`, and
`wasm32-unknown-unknown` ships no libc headers:

```
cargo:warning=src/tree_sitter/parser.h:10:10: fatal error: 'stdlib.h' file not found
error occurred in cc-rs: command did not execute successfully (status code exit status: 1):
  clang ... --target=wasm32-unknown-unknown -std=c11 ... -c src/parser.c
```

This is a hard toolchain fact, not a version quirk, and it is the real reason the desktop path
cannot simply be un-gated. Worth recording precisely, because "just build tree-sitter for wasm" is
the first thing anyone will propose when they revisit this.

### B. `web-tree-sitter` (the JS/wasm distribution) via `wasm-bindgen` interop

Spike #1 assessed this and declined; nothing found here changes that verdict, and the size analysis
makes it worse. It requires shipping the tree-sitter runtime wasm plus one compiled `.wasm` grammar
per language as separate fetched assets, an async load/initialize path in a component that is
currently synchronous, hand-rolled `wasm-bindgen` bindings to a JS API with no Rust-native surface,
and a reimplementation of `tree-sitter-highlight`'s capture-classification logic (the JS package
exposes queries but not the highlight iterator). None of that is reachable from `cargo test` — it
can only be verified in a browser — which is precisely the class of untested-glue bug Phase 1 has a
separate item to prevent.

**Rejected:** multi-megabyte asset payload, async complexity, JS interop, and zero unit-testability,
to reach output that still would not match desktop.

### C. `arborium` — Rust-native tree-sitter grammars with a bundled wasm sysroot

The strongest surprise of the design pass, and worth taking seriously: `arborium` (the highlighter
underneath `kode-leptos`, which the incumbent `ui/` editor uses) solves problem A by vendoring an
`arborium-sysroot` crate that supplies the missing libc, so the C grammars *do* compile to
`wasm32-unknown-unknown`. Its `Highlighter::highlight_spans(lang, src) -> Vec<Span>` returns
`{start, end, capture}` byte ranges — an almost perfect fit for our per-byte class map, and it would
give web genuine AST-accurate highlighting.

**Rejected on measured size.** With the five required grammars:

| build | `.wasm` bytes |
|---|---|
| empty control | 117 |
| `arborium` + rust only | 3,008,459 |
| `arborium` + rust/js/ts/python/go | **5,524,663** |

That is **+5.27 MB** on a 2.54 MB application — the highlighter would be more than twice the size of
everything else in the app combined. And trimming the language set is not a fix: a *single*-grammar
build still costs +3.00 MB, because the runtime, the bundled sysroot and the regex engine are a
fixed floor before any grammar is added, with each additional grammar's parse tables layered on top.
Even a Rust-only web editor would more than double the bundle. No amount of correctness justifies
that for a read-mostly code viewer.

### D. A pure-Rust regex/lexer highlighter (e.g. a `syntect` subset)

**Rejected on measured size.** `syntect` with the default syntax and theme sets and the wasm-capable
`regex-fancy` backend:

| build | `.wasm` bytes |
|---|---|
| empty control | 117 |
| `syntect` (default-syntaxes, default-themes, regex-fancy) | **1,498,268** |

**+1.50 MB, a 59% increase** on the current bundle, almost all of it the embedded Sublime syntax
dumps for ~100 languages we do not need. Trimming to five hand-picked syntax definitions is possible
in principle but means vendoring and maintaining `.sublime-syntax` assets, and syntect's scope model
would still have to be collapsed down to our five classes — so the remaining cost buys accuracy we
then throw away at the mapping layer.

### E. A hand-rolled lexer matching the existing token model — **CHOSEN**

The existing token model is deliberately small. `style.css` defines exactly five classes plus the
baseline: `tok-keyword`, `tok-string`, `tok-comment`, `tok-type`, `tok-number`, `tok-plain`. And
`highlight_native.rs` already *discards* tree-sitter's full capture vocabulary down to those same
five (`CAPTURES: [&str; 5]`).

That reframes the problem. We are not trying to reproduce a syntax tree in the browser; we are
trying to answer five questions per byte — is this a keyword, a string, a comment, a type name, or a
number? A parser is the wrong tool for that, and options C and D are both paying megabytes for
structural fidelity that `class_for` throws away one function later.

A per-language lexer answers those five questions directly, with **zero new dependencies**:

- **Size:** measured at **+8,624 bytes** on the release wasm (see "Measured result" below) —
  0.34% of the bundle, against +59% for syntect and +218% for arborium.
- **Shares the existing seam exactly.** It emits the same `class_per_byte` map that
  `highlight_native` builds and hands it to the same `tokens::segment_lines`, so both targets
  converge on identical `Vec<Vec<Span>>` construction, the same class vocabulary, and one
  unforked `style.css`. The renderer in `editor/mod.rs` does not change.
- **Unit-testable on the host.** Because it has no dependencies it compiles on every target, so its
  tests run under the ordinary `cargo test --features desktop` command rather than needing a browser
  or a wasm test runner. This is the same "no untested glue" concern Phase 1 raises elsewhere.
- **Synchronous.** No asset fetch, no async init, no change to the component's render path.

**The honest cost:** a lexer is not a parser, so classification is heuristic where tree-sitter's is
structural. Concretely, `tok-type` is assigned by convention (known primitive/builtin type names,
plus identifiers beginning with an uppercase letter) rather than by resolving a type position, so a
constant like `MAX_LEN` is coloured as a type and a lowercase type alias is not. JS regex literals
are not distinguished from division and are left plain rather than risk a runaway string. This is
the standard accuracy floor of lightweight editor highlighting, it is bounded and local (a wrong
colour on one token, never a runaway highlight state), and it is the correct trade for a diff-first
editor whose stated non-goal is VSCode-scale features.

**Desktop is untouched.** The native tree-sitter path remains the higher-fidelity backend on the
target that can afford it; the deliberate asymmetry is now one of *fidelity*, not of *capability*,
and both targets render styled code from one style sheet.

## Decision

**Option E.** Web highlighting ships as `src/editor/highlight_web.rs`: a table-driven lexer over the
same five languages desktop covers (Rust, JavaScript, TypeScript, Python, Go), feeding the existing
`tokens::segment_lines` seam and emitting the existing `tok-*` classes.

Two notes on how it is wired:

- The module carries **no `#[cfg]` gate**, because it introduces no dependency to gate. It compiles
  on every target; only the `#[cfg(feature = "web")]` arm in `editor/mod.rs` *calls* it. Gating the
  module would have made its tests unrunnable under the crate's normal test command for no benefit.
- Unsupported languages (`toml`, `json`, `markdown`, …) fall back to `plain_spans`, matching
  `highlight_native::highlight`'s contract that highlighting is best-effort and must never break the
  editor.

## Measured result

| build | `.wasm` bytes | delta |
|---|---|---|
| `--release --target wasm32-unknown-unknown --features web`, before | 2,537,702 | — |
| same, after | 2,546,326 | **+8,624 (+0.34%)** |

For comparison at the same 2.54 MB base: arborium would have been +5,524,546 (**+218%**), syntect
+1,498,151 (**+59%**). Highlighting five languages on the web target therefore costs about
1/600th of what the next-cheapest working option would have.

### An asymmetry the pinning test surfaced

Writing the desktop no-regression pin turned up something worth recording: `tree_sitter_rust`'s
`HIGHLIGHTS_QUERY` has **no `number` capture**, so desktop renders Rust integer literals as
`tok-plain`. The web lexer classifies them. The two targets genuinely differ here — in web's favour
— which is a useful reminder that "native tree-sitter" is not automatically a superset of "lexer":
the desktop path's fidelity is bounded by each grammar's shipped highlights query, not by
tree-sitter itself. Both behaviours are now pinned by tests
(`highlight_native::tests::desktop_output_is_pinned_for_a_fixed_input`).

## Revisiting this

The size ranking is a property of today's ecosystem, not a law. If tree-sitter's C grammars ever
gain a first-class `wasm32-unknown-unknown` path, option A becomes available and is strictly better
than everything here — one code path, identical output on both targets. Until then the per-byte
class map is the seam that makes swapping the web backend a contained change: any replacement only
has to produce `Vec<&'static str>` of `text.len()`, and `segment_lines` plus the renderer stay as
they are.
