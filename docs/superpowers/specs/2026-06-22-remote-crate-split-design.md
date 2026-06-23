# `remote` crate split-out — design

## Problem

The remote/engine-axis seam (`RemoteTarget`, `RemoteHandle`, `PromoteBundle`, `promote()`),
its two network-facing targets (`VpsTarget`, `UnsupportedTarget`), the in-process test
provisioner (`LoopbackTarget`), and the handover config (`PromoteConfig`/`PromoteMode`) all
live in one file, `crates/engine/src/remote.rs`. `ARCHITECTURE.md` calls for a dedicated
`remote` crate ("`RemoteTarget` impls: vps, microvm"), and the two most recent vps specs
explicitly deferred the split "to its own refactor". With vps promote **and** demote shipped,
this is now the only buildable-in-tree remaining remote-axis item — and it is the natural home
for the future `microvm` target and a mockable provisioner seam.

## The coupling that shapes the split

`remote.rs` is **bidirectionally** coupled to the rest of `engine`, so it cannot be moved
wholesale into a leaf crate:

- **Outward (the seam → consumers):** `serve.rs` imports `LoopbackTarget`, `PromoteConfig`,
  `PromoteMode`, `PromoteBundle`, `RemoteHandle`, `RemoteTarget`, `VpsTarget`, `promote`;
  `service.rs` consumes `PromoteBundle` (accept/export/demote); `main.rs` builds a
  `PromoteConfig`.
- **Inward (`LoopbackTarget` → the engine):** `LoopbackTarget::provision` builds a full
  `EngineService` (`build_router`/`build_tool_registry`/`build_default_registry`/
  `build_capabilities`) and serves it via `crate::serve::{app, run}`. It **depends on the
  engine**.

A single `remote` crate holding `LoopbackTarget` would therefore need to depend on `engine`,
while `engine` depends on `remote` — a cycle.

## Decision

Split along the dependency grain:

- **New `otto-remote` crate** (`crates/remote/`) holds everything whose dependencies flow
  strictly inward: `RemoteTarget` (trait), `RemoteHandle`, `PromoteBundle`, `promote()`,
  `UnsupportedTarget`, `VpsTarget`, `PromoteConfig`, `PromoteMode`, plus the two existing unit
  tests (`unsupported_target_refuses_to_provision`, `vps_http_base_maps_ws_schemes`).
  Dependencies: `otto-protocol` (`SessionId`, `CapabilitiesManifest` is **not** needed here),
  `otto-engine-core` (`Workspace`, `WorkspaceSnapshot`), `otto-persistence` (`SessionStore`,
  `SessionState`), plus `async-trait`, `anyhow`, `serde`, `serde_json`, `reqwest`, `tokio`.
- **`LoopbackTarget` stays in `engine`**, moved out of `remote.rs` into a new
  `crates/engine/src/loopback.rs`, implementing `otto_remote::RemoteTarget`. It keeps using the
  engine's builders + `serve`. This makes the dependency one-directional: **`engine →
  otto-remote`**, never the reverse. This is also semantically right — `LoopbackTarget` is an
  in-process test/dev provisioner that *boots an engine*; the architecture's `remote` crate is
  for the *real* targets (vps, microvm), none of which boot an in-process engine.

### Crossing the crate boundary: `RemoteHandle` constructors

`RemoteHandle.shutdown` is a private field, set today inside `remote.rs` by both targets. After
the split, `LoopbackTarget` (in `engine`) can no longer name that private field on a type
defined in `otto-remote`. So `RemoteHandle` gains a small public API in `otto-remote`:

- `RemoteHandle::new(endpoint, token)` — no backing task (used by `VpsTarget`; today's
  `shutdown: None`).
- `RemoteHandle::with_task(endpoint, token, JoinHandle<()>)` — owns an in-process task to abort
  (used by `LoopbackTarget`; today's `shutdown: Some(task)`).
- `RemoteHandle::abort(&mut self)` — take-and-abort the backing task, idempotent; called by both
  `Drop` and `LoopbackTarget::teardown`.

The `endpoint`/`token` fields stay `pub` (serve.rs reads them on reconnect). Behavior is
identical: `Drop` still aborts loopback tasks; `VpsTarget`/`UnsupportedTarget` still own no task.

### Backward-compatible public surface

`otto_engine` continues to re-export every moved name
(`pub use otto_remote::{PromoteBundle, PromoteConfig, PromoteMode, RemoteHandle, RemoteTarget,
UnsupportedTarget, VpsTarget, promote};` + `pub use loopback::LoopbackTarget;`). So the
integration tests (`promote.rs`, `vps_promote.rs`, `serve.rs`) and `main.rs`, which import via
`otto_engine::…`, need **no changes**. Only the two internal modules that referenced
`crate::remote::…` (`serve.rs`, `service.rs`) switch to `otto_remote::…` (and `serve.rs` imports
`LoopbackTarget` from `crate::loopback`).

## Non-goals

- The `microvm` target, real machine provisioning, and a mockable provisioner seam — all still
  ahead; this split is the enabling step, not those features.
- Moving `LoopbackTarget` out of `engine` (would require inverting the dependency via a
  provisioner-factory trait — YAGNI until a second engine-booting target exists).
- Any behavior change. The full offline-deterministic suite (`promote.rs`, `vps_promote.rs`,
  `serve.rs`, and the moved unit tests) must stay green with identical semantics.
- Removing `reqwest` from `engine`'s manifest: its tests (`vps_promote.rs`) still use
  `reqwest::Response`, so it stays a dependency.

## Verification

`cargo build --workspace`, `cargo test --workspace`, `cargo fmt --all`, and `cargo clippy
--workspace --all-targets` all clean. The dependency direction is confirmed one-way: `otto-remote`
has no path to `otto-engine`.
