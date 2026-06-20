# otto UI — Build Roadmap

**Date:** 2026-06-17
**Status:** Approved decomposition — **Sub-projects A–C shipped** (A: PR #47, 2026-06-18; B: capabilities + status strip, 2026-06-18; C: workspace tree + editor, 2026-06-18 — [design](2026-06-18-ui-workspace-tree-editor-design.md) · [plan](../plans/2026-06-18-ui-workspace-tree-editor.md); D: diff approval, 2026-06-19 — [design](2026-06-19-ui-diff-approval-design.md) · [plan](../plans/2026-06-19-ui-diff-approval.md); E: token/cost meter + pause/resume, 2026-06-19 — [design](2026-06-19-ui-token-meter-pause-resume-design.md) · [plan](../plans/2026-06-19-ui-token-meter-pause-resume.md); F: promote-to-remote UX, 2026-06-20 — [design](2026-06-20-ui-promote-to-remote-design.md) · [plan](../plans/2026-06-20-ui-promote-to-remote.md)). **All sub-projects A–F shipped.**
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
| **A** ✅ | **App shell + live session** *(shipped — [design](2026-06-17-ui-shell-live-session-design.md) · [plan](../plans/2026-06-17-ui-shell-live-session.md))* | `ui/` Leptos CSR project (browser-first). Connect to a `ws://`/`wss://` URL with a bearer token → `Ready{session}`; prompt box sends `SendPrompt`; render the live `Event` stream; `Abort`; reconnect with `last_seq` replay. The reusable shell every later sub-project extends. | **Done:** moved the WS framing enum (`ServerMessage`) into `protocol` so the UI can deserialize it; `/ws` accepts the bearer token via a `?token=` query param (browser `WebSocket` can't set headers) with the header path still preferred. Header auth and existing tests stayed green. |
| **B** ✅ | **Capabilities + status strip** *(shipped — [design](2026-06-17-ui-capabilities-status-strip-design.md) · [plan](../plans/2026-06-18-ui-capabilities-status-strip.md))* | Engine emits `CapabilitiesManifest` on connect; UI status strip shows engine/LLM/sandbox state with **visible** degradation. | **Done:** extended `CapabilitiesManifest` with `remote_llm` (so the strip distinguishes offline-deterministic from remote-backed); the `Ready` frame now carries the manifest; `build_capabilities()` derives it from the serve environment. |
| **C** ✅ | **Workspace tree + editor** *(shipped — [design](2026-06-18-ui-workspace-tree-editor-design.md) · [plan](../plans/2026-06-18-ui-workspace-tree-editor.md))* | File tree (via `POST /workspace` `List`), file view, and editing via a **`kode-leptos`** `CodeEditor` (native Leptos CSR component — no JS-glue seam). Editing is local-only; persistence deferred to D. | **Done:** added a tower-http CORS layer to /workspace so the browser can call it cross-origin (no protocol change). Editor is **kode-leptos** (native Leptos CSR component) — no JS-glue seam needed. |
| **D** ✅ | **Diff approval** *(shipped — [design](2026-06-19-ui-diff-approval-design.md) · [plan](../plans/2026-06-19-ui-diff-approval.md))* | Render Coder diffs; an approve/reject gate for `fs.write` `Ask` verdicts, wiring the permission gate's `Ask` path through to the UI. | `ApproveDiff` command; `Diff` + `ApprovalRequest` events; orchestrator/gate wiring to block on the UI's verdict. **Done:** added an `ApproveDiff` command + `ApprovalRequest{id,path,old,new}` event (additive, semver-minor); an async `Approver` seam + fail-closed `DenyApprover` default; the orchestrator's per-edit `Ask` branch now reads the old contents, emits `ApprovalRequest`, awaits the approver, and applies the edit only on approval (Allow/Deny unchanged, sensitive floor still Denies first); an opt-in `otto serve --approve-edits` gate (`ApprovalModeGate` upgrades non-sensitive `fs.write` Allow→Ask, wired by `build_tool_registry_approving`); and serve now reads the socket concurrently with the running turn (`split` + `select!`), routing `ApproveDiff` frames through a per-connection `ApprovalRegistry`/`InteractiveApprover` (fail-closed on reject/disconnect). The UI decodes `ApprovalRequest`, renders the diff (pure `diff_lines`) with Approve/Reject, and sends `ApproveDiff`. |
| **E** ✅ | **Token/cost meter + Pause/Resume** *(shipped — [design](2026-06-19-ui-token-meter-pause-resume-design.md) · [plan](../plans/2026-06-19-ui-token-meter-pause-resume.md))* | Live token/cost meter; pause/resume an in-flight turn. | **Done:** `Pause`/`Resume` commands + `TokenCostMeter` event (additive, semver-minor); `Usage` on `CompleteResponse` reported by Anthropic/Ollama (offline providers report none); a `MeteringRouter` decorator tallies usage into a per-turn `TokenMeter`; the orchestrator emits cumulative `TokenCostMeter` at phase boundaries (offline emits none, so the determinism suite is unchanged); a `PauseController` seam (`NeverPause` default) is checked at phase boundaries, bracketing a park with `Log` "turn paused"/"turn resumed"; `run_prompt_with_controls` wires a per-turn meter + the pauser via `TurnControls`; serve routes `Pause`/`Resume` through the existing `select!` over a connection-scoped `PauseState` (AtomicBool+Notify), releasing on disconnect/abort; the UI shows a token/cost meter in the status strip and a Pause/Resume button. |
| **F** ✅ | **Promote-to-remote UX** *(shipped — [design](2026-06-20-ui-promote-to-remote-design.md) · [plan](../plans/2026-06-20-ui-promote-to-remote.md))* | Client-driven session handover to a remote engine, reconnect via `Last-Event-ID`. | **Done:** `PromoteToRemote`/`DemoteToLocal` commands + `Promoted`/`Demoted` handover frames (additive, semver-minor); `EngineService::workspace()` accessor; `PromoteConfig` threaded through `serve_app`/`ServeState` with a connection-scoped registry that retains each `RemoteHandle` so the provisioned engine outlives the local connection; `LoopbackTarget` gained an `engine_remote` flag (promote→remote, demote→local) and wires its provisioned engine promote-capable for the round-trip; `handle_handover` routes both commands between turns (idempotent re-promote returns the existing endpoint — no engine abort), fail-closed with an `Error` reply when promotion is disabled; opt-in `otto serve --promote-loopback` enables it; the UI shows Promote/Demote buttons (gated by `can_promote`/`can_demote`) and reconnects to the handed-back endpoint on `Promoted`/`Demoted`, flipping the status strip local↔remote. Real VPS provisioning stays the external/manual `UnsupportedTarget` boundary; provisioned loopback engines live until process exit. |

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
