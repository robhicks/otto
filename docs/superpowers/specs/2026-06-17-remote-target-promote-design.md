# otto Design — RemoteTarget + Promote Flow

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-17

## Goal

Tie the distribution axis together: a `RemoteTarget` seam that provisions a remote engine from
a captured session, a `promote()` orchestration that snapshots a session + its workspace and
provisions it, a `LoopbackTarget` that does this for real on `127.0.0.1` (fully testable), and
an `UnsupportedTarget` that honestly stubs the external cloud provisioner. Fourth and final
remote-axis sub-project — the capstone exercising persistence snapshot/restore, workspace
snapshot/restore, serve, and `Last-Event-ID` reconnect together.

## Context

Every primitive promote needs already exists:
- `SessionStore::snapshot(session) -> SessionState` / `restore(&SessionState) -> SessionId`
  (PR #21).
- `Workspace::snapshot() -> WorkspaceSnapshot` / `LocalWorkspace::restore(&snapshot)` (PR #27).
- `EngineService` + `serve_run(listener, app, tls)` + WS `Ready`/`last_seq` replay (PRs #24/#29).
- `RemoteWorkspace` + `/workspace` RPC (PR #32) — not required by promote but part of the axis.

So this sub-project is mostly orchestration plus the seam. The architecture's `RemoteTarget`
trait (`provision(&SessionState) -> RemoteHandle`, `teardown`) predates the Plan-C split that
moved the workspace out of `SessionState`; this design reunites them in a `PromoteBundle`.

## Decisions (locked during brainstorming)

1. **Everything lives in the `engine` crate for now.** `provision` references
   `persistence::SessionState`, `engine-core::WorkspaceSnapshot`, and `EngineService`/`serve_run`
   — all of which the engine crate already has. The architecture's dedicated `remote` crate is
   the eventual home; splitting it out is deferred.
2. **`provision` takes a `PromoteBundle { session: SessionState, workspace: WorkspaceSnapshot }`**
   (not the architecture's `SessionState`-only signature), reuniting what Plan C split.
3. **`LoopbackTarget` provisions a real second in-process engine** (fresh sqlite + workspace
   temp dirs, both restored, served on `127.0.0.1:0`). This is the testable impl.
4. **`UnsupportedTarget` is an honest stub**: `provision` returns a clear error; no fake cloud
   calls. The real `vps`/`microvm` provisioners are external and manual/integration-only.

## Architecture

### The seam + types (`crates/engine/src/remote.rs`)

```rust
/// A captured session ready to move to another engine: the persisted session state plus the
/// workspace's current files.
pub struct PromoteBundle {
    pub session: SessionState,        // persistence
    pub workspace: WorkspaceSnapshot, // engine-core
}

/// A reachable, provisioned remote engine.
pub struct RemoteHandle {
    pub endpoint: String, // e.g. "ws://127.0.0.1:54321"
    pub token: String,
    // impl-private shutdown state (e.g. the spawned task's abort handle + owned temp dirs)
}

#[async_trait]
pub trait RemoteTarget: Send + Sync {
    /// Provision a remote engine that has `bundle` restored and is serving; return how to reach it.
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle>;
    /// Tear the provisioned remote down.
    async fn teardown(&self, handle: RemoteHandle) -> anyhow::Result<()>;
}
```

`RemoteHandle`'s private shutdown state means the struct's public fields are `endpoint`/`token`
only; impls attach their own teardown mechanism (the plan defines the concrete shape, e.g. an
`Option<JoinHandle>` + `Vec<TempDir>` the loopback impl populates).

### `promote()` orchestration (`crates/engine/src/remote.rs`)

```rust
/// Snapshot a session and its workspace and provision it onto `target`.
pub async fn promote(
    store: &dyn SessionStore,
    workspace: &dyn Workspace,
    session: SessionId,
    target: &dyn RemoteTarget,
) -> anyhow::Result<RemoteHandle> {
    let bundle = PromoteBundle {
        session: store.snapshot(session).await?,
        workspace: workspace.snapshot().await?,
    };
    target.provision(&bundle).await
}
```

Pure glue. The source engine is not stopped here — handover (drop local, reconnect remote) is a
client/UI concern; promote's job is to produce a running, reconnectable remote.

### `LoopbackTarget` (`crates/engine/src/remote.rs`)

`LoopbackTarget::new(token)` holds the bearer token the provisioned remote will require.
`provision(bundle)`:
1. Create a fresh temp dir; open a `SqliteStore` at `<tmp>/sessions.db`; `store.restore(&bundle.session)`.
2. Create a fresh workspace temp dir; `LocalWorkspace::new(<tmp>)`; `restore(&bundle.workspace)`.
3. Build the remote `EngineService`: `build_router()` (offline `LocalProvider`),
   `build_default_registry()`, `build_tool_registry` over the restored workspace, the restored store.
4. `serve_app(service, token)`; bind `std::net::TcpListener` on `127.0.0.1:0` (non-blocking);
   take the port; `tokio::spawn(serve_run(listener, app, None))`.
5. Return `RemoteHandle { endpoint: format!("ws://127.0.0.1:{port}"), token, <abort handle + temp dirs> }`.

`teardown(handle)` aborts the spawned serve task and drops the owned temp dirs (so the sqlite
file + workspace are cleaned up). The temp dirs are owned by the handle so they outlive the
server.

### `UnsupportedTarget` (`crates/engine/src/remote.rs`)

```rust
/// A RemoteTarget that refuses to provision: the real cloud/VPS provisioner needs external
/// infrastructure (a machine, SSH, a deployed engine) and cannot run in-tree or in CI.
pub struct UnsupportedTarget;
// provision -> Err("real VPS provisioning requires external infrastructure; not available in-tree")
// teardown  -> Ok(())
```

This documents the boundary in code rather than pretending a cloud provisioner exists.

## Error handling & determinism

- All loopback, no external network. The promote test binds `127.0.0.1:0`; the suite stays
  offline/deterministic.
- `promote`/`provision` are fail-closed: a snapshot or restore error returns `Err` and no
  `RemoteHandle` is produced (no half-provisioned remote is handed back).
- The restored remote enforces the same bearer auth + permission gate + path containment as any
  served engine (it's a normal `EngineService` behind `serve_app`).

## Testing

- **Capstone integration test (loopback):** on a source engine (store A + workspace A), create a
  session and run a turn through `EngineService` (events + a turn record persist; the scripted
  Coder writes a file into workspace A). Call `promote(store_a, workspace_a, session,
  &LoopbackTarget::new(TOKEN))`. Then a WS client connects to `handle.endpoint` with
  `?session=<id>&last_seq=<k>` and the bearer token and asserts: `Ready { session }` carries the
  **same** session id; the replayed gap equals the source's events with `seq > k`; and the
  restored remote workspace contains the file the source turn wrote (read it via the remote's
  `POST /workspace` RPC using a `RemoteWorkspace` pointed at `handle.endpoint`). Then `teardown`
  and assert a subsequent connect to the endpoint fails.
- **`UnsupportedTarget::provision` errors** with a clear message; `teardown` is `Ok`.
- **`promote` unit-ish:** against a `LoopbackTarget`, `promote` returns a handle whose endpoint
  is reachable (a bare authed connect yields `Ready`), proving the snapshot→provision glue
  without the full reconnect assertions.

## Out of scope (named, not silently dropped)

- **Real `vps`/`microvm` provisioners** — external infra; `UnsupportedTarget` marks the boundary.
  A genuine impl is manual/integration-only, not CI-tested.
- **Client-side handover UX** (drop the local engine, switch the UI to the remote) — a UI/client
  concern; promote produces a reconnectable remote, it does not stop the source.
- **Splitting a dedicated `remote` crate** — lives in `engine` for now.
- **TLS on the provisioned remote** — the loopback target serves plaintext `ws://`; a real
  target would pass a `RustlsConfig` to `serve_run` (the serve sub-project already supports it).
