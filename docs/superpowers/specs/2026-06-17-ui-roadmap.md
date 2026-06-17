# otto UI — Build Roadmap

**Date:** 2026-06-17
**Status:** Approved decomposition
**Scope:** The frontend (`ui/`) described in `docs/ARCHITECTURE.md` and the design spec
(`docs/superpowers/specs/2026-06-13-otto-design.md`).

## Why this document

The full otto UI — a **Tauri 2 + Leptos (Rust→WASM)** frontend that speaks the `protocol`
crate's `Command`/`Event` types to the engine — is too large for a single spec. This document
records the agreed **decomposition into sub-projects** and the **build order**, so each
sub-project can be specced, planned, and implemented in its own session with fresh context.

Each sub-project below gets its own design spec (`…-design.md`) and implementation plan.
This file is the index and the rationale for the sequence; it does not itself design anything.

## Fixed decisions

- **Stack:** Tauri 2 + Leptos, per `ARCHITECTURE.md`. The `ui` build depends **only** on
  `protocol` (compiled to WASM); it must never link `engine-core` or any impl crate.
- **Transport:** the UI is a network client to `otto serve` over WebSocket (`/ws`), reusing
  the existing bearer-authed, `Last-Event-ID`-replayable transport. This decouples UI work
  from engine-embedding plumbing.
- **First slice is browser-first.** Sub-project A ships as a plain Leptos CSR app served by
  `trunk` and run in a browser tab. The Tauri desktop wrapper is added in a later sub-project,
  reusing the same WASM bundle.
- **Diff-first, not a VSCode clone.** Per the design spec's non-goals, the "IDE shell" is a
  minimalist, terminal-like surface that edits a couple of files diff-first — not a
  project-wide multi-tab IDE.

## Sub-projects

Ordered so each builds on a working, demoable predecessor.

| # | Sub-project | What it adds | Protocol / engine changes |
|---|---|---|---|
| **A** | **App shell + live session** | `ui/` Leptos CSR project (browser-first). Connect to a `ws://`/`wss://` URL with a bearer token → `Ready{session}`; prompt box sends `SendPrompt`; render the live `Event` stream; `Abort`; reconnect with `last_seq` replay. The reusable shell every later sub-project extends. | Two small **additive** changes: move the WS framing enum (`ServerMessage`) into `protocol` so the UI can deserialize it; accept the bearer token via a `?token=` query param (browser `WebSocket` can't set headers). Header auth and existing tests stay green. |
| **B** | **Capabilities + status strip** | Engine emits `CapabilitiesManifest` on connect; UI status strip shows engine/LLM/sandbox state with **visible** degradation. | `Ready` frame carries the manifest (the type already exists in `protocol`). |
| **C** | **Workspace tree + editor** | File tree (via `POST /workspace` `List`), read-only file view, then CodeMirror 6 editing through a wasm-bindgen JS glue module (`mountEditor`/`getDoc`/`setDoc`/`onChange`). | None for read/list (reuses the `/workspace` RPC); editor is pure frontend + JS interop. |
| **D** | **Diff approval** | Render Coder diffs; an approve/reject gate for `fs.write` `Ask` verdicts, wiring the permission gate's `Ask` path through to the UI. | `ApproveDiff` command; `Diff` + `ApprovalRequest` events; orchestrator/gate wiring to block on the UI's verdict. |
| **E** | **Token/cost meter + Pause/Resume** | Live token/cost meter; pause/resume an in-flight turn. | `TokenCostMeter` event; `Pause`/`Resume` commands; provider-level token accounting. |
| **F** | **Promote-to-remote UX** | Client-driven session handover to a remote engine, reconnect via `Last-Event-ID`. | `PromoteToRemote`/`DemoteToLocal` commands, wired to the existing `promote()` + `LoopbackTarget`. |

## Sequencing rationale

- **A is first** because it needs only minimal, additive transport changes and runs against
  the `/ws` endpoint that already exists and is tested. It gets a real otto UI on screen
  fastest and de-risks the Tauri/Leptos/WASM toolchain before richer pieces depend on it.
- **B, C** are pure frontend + small additive protocol/transport touches; they make the shell
  useful without touching the orchestrator.
- **D, E** reach into the orchestrator/gate/providers and carry the heaviest engine risk, so
  they come after the shell is proven.
- **F** is last because it depends on the remote axis and the most protocol surface.

## What lives where

- **`ui/`** — standalone build (own `Cargo.toml`, **not** a workspace member, so
  `cargo build --workspace` and the offline test suite stay untouched). Path-depends on
  `../crates/protocol`. Built with `trunk`.
- **`protocol`** — the only crate shared between engine and UI. Sub-projects add to it
  additively (semver-minor): new `Command`/`EventKind` variants and the WS framing enum.
