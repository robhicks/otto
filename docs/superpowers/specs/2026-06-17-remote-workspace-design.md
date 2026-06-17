# otto Design — RemoteWorkspace + Workspace RPC

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-17

## Goal

Make the `Workspace` seam usable over the network: a bearer-authed unary RPC that proxies
`read`/`list`/`apply_edit`/`snapshot` to a remote engine's workspace, and a `RemoteWorkspace`
client that implements the `Workspace` trait by calling it. Third sub-project of the remote
axis — it proves the workspace seam is remote-able. This sub-project builds the seam (client +
server + round-trip test) in isolation; wiring `RemoteWorkspace` into a running remote
orchestrator is deferred to the promote sub-project.

## Context

`WorkspaceRead`/`Workspace` (engine-core) expose `read(path)`, `list(glob)`, `apply_edit(edit)`,
`snapshot()`. `LocalWorkspace` (the only impl) path-contains every operation under its root.
The serve server (`crates/engine/src/serve.rs`) is an axum app with a bearer-authed `/ws`
endpoint; `EngineService` holds `Arc<dyn Workspace>` and `Arc<ToolRegistry>` (whose
`check(name, args) -> Decision` runs the `DefaultPermissionGate` sensitive-path floor). The
current protocol (`Command`/`Event`) is turn-level only — there is no workspace RPC.

## Decisions (locked during brainstorming)

1. **Transport = HTTP unary.** Workspace ops are request→response, so a single bearer-authed
   `POST /workspace` route on the existing axum server (tagged `WorkspaceRequest` JSON →
   `WorkspaceResponse` JSON) is the fit. No multiplexing over the `/ws` event socket.
2. **Gate read + write server-side.** `apply_edit` (and `read`) are routed through the same
   permission gate (`ToolRegistry::check`) before dispatch, proceeding only on `Decision::Allow`.
   The network-exposed write primitive thus cannot write/read sensitive paths (`.env`/`.ssh`/
   `.aws`/`.git`) even though it bypasses the orchestrator. `list`/`snapshot` need no extra gate
   (the list walk already excludes dotfiles/`.git`/`.aws`/`target`/`node_modules`).
3. **Wire types in `protocol`.** They are wire types; `protocol` is their home. Since `protocol`
   cannot depend on `engine-core`, the snapshot response carries the raw
   `Vec<(PathBuf, Vec<u8>)>`; `RemoteWorkspace` maps it to `engine-core`'s `WorkspaceSnapshot`.
4. **`RemoteWorkspace` lives in the `workspace` crate** (per the architecture's crate map,
   alongside `LocalWorkspace`), using `reqwest` as the HTTP client.

## Architecture

### Wire types (`crates/protocol/src/lib.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceRequest {
    Read { path: PathBuf },
    List { glob: String },
    ApplyEdit { path: PathBuf, contents: String },
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceResponse {
    Read { bytes: Vec<u8> },
    List { paths: Vec<PathBuf> },
    ApplyEdit { bytes_written: u64 },
    Snapshot { files: Vec<(PathBuf, Vec<u8>)> },
    Error { message: String },
}
```

`protocol` already depends on `serde`; `PathBuf` serializes as a string (UTF-8 paths, matching
the rest of the system). `ApplyEdit.contents` is a `String` to mirror `Edit.new_contents`.

### Server — the route + `EngineService::workspace_rpc`

- A new `POST /workspace` route on the existing axum app (`serve::app`), behind the same
  bearer-token check as `/ws` (factored so both share one auth helper). Body deserializes to
  `WorkspaceRequest`; the handler calls `state.service.workspace_rpc(req).await` and returns the
  `WorkspaceResponse` as JSON (HTTP 200, including the `Error` variant — transport-level 200,
  application-level error in the body).
- `EngineService::workspace_rpc(&self, req: WorkspaceRequest) -> WorkspaceResponse`:
  - `Read { path }` → gate `check("fs.read", {path})`; on `Allow`, `workspace.read(&path)` →
    `Read { bytes }`; non-`Allow` or a read error → `Error`.
  - `List { glob }` → `workspace.list(&glob)` → `List { paths }` (or `Error`).
  - `ApplyEdit { path, contents }` → gate `check("fs.write", {path})`; on `Allow`,
    `workspace.apply_edit(&Edit { path, new_contents: contents })` → `ApplyEdit { bytes_written }`;
    non-`Allow` (Deny/Ask) → `Error { message: "denied by permission gate" }`; an apply error →
    `Error`.
  - `Snapshot` → `workspace.snapshot()` → `Snapshot { files }` (or `Error`).
  - Gate checks use the existing `ToolRegistry::check` (sync, returns `Decision`); only
    `Decision::Allow` proceeds (Deny/Ask both refuse — fail-closed, matching the orchestrator's
    gated apply).

### Client — `RemoteWorkspace` (`crates/workspace/src/remote.rs`)

```rust
pub struct RemoteWorkspace {
    base_url: String,        // e.g. "http://127.0.0.1:7878" or "https://host:port"
    token: String,
    client: reqwest::Client,
}
```

- `RemoteWorkspace::new(base_url, token)` builds a `reqwest::Client`.
- One private `async fn rpc(&self, req: &WorkspaceRequest) -> Result<WorkspaceResponse>` POSTs
  to `{base_url}/workspace` with `Authorization: Bearer {token}` and JSON body, parses the JSON
  response, and maps a `WorkspaceResponse::Error` into an `anyhow::Error`.
- Trait impls: `read` → `Read` RPC → bytes; `list` → `List` → paths; `apply_edit` → `ApplyEdit`
  → bytes_written; `snapshot` → `Snapshot` → map `files` into `WorkspaceSnapshot`. A response
  variant that doesn't match the request is an `anyhow::Error` (protocol violation).
- The `base_url` scheme selects transport; `reqwest` is built with `rustls-tls` so `https://`
  works (cert-trust configuration for self-signed remotes is a later refinement; v1 is tested
  over `http` loopback).

### Auth

The `/workspace` route requires `Authorization: Bearer <OTTO_TOKEN>`, the same guard as `/ws`.
The bearer check is factored into a shared helper so the two routes can't drift.

## Error handling & determinism

- Workspace/gate failures become `WorkspaceResponse::Error { message }` (HTTP 200) so the client
  surfaces a clean `anyhow::Error`; a malformed request body is a 400; a missing/wrong token is
  a 401 before any dispatch.
- The server's `LocalWorkspace` still enforces path containment regardless of the gate, so a
  `..`/absolute path is rejected even if the gate somehow allowed it.
- The default offline test path is unaffected; the RemoteWorkspace round-trip test runs on a
  `127.0.0.1:0` ephemeral port over plaintext `http` (pure loopback — no external network), so
  the suite stays deterministic and key-free.

## Testing

- **Round-trip parity (loopback):** start the axum app with `/workspace` backed by a
  `LocalWorkspace` over a tempdir (seed some nested files); point a `RemoteWorkspace` at the
  bound port; assert `read`/`list`/`apply_edit`/`snapshot` through the client produce the same
  results as the underlying `LocalWorkspace` (e.g. a `RemoteWorkspace::apply_edit` then a direct
  `LocalWorkspace::read` shows the write; `RemoteWorkspace::snapshot().files` equals
  `LocalWorkspace::snapshot().files`).
- **Gated write denied over RPC:** a `RemoteWorkspace::apply_edit` targeting `.env` (a sensitive
  path) returns an error and writes nothing — proving the server-side floor holds over the wire.
- **Gated read denied over RPC:** a `RemoteWorkspace::read` of a sensitive path errors.
- **Auth:** a request with a missing/wrong bearer token is rejected (401), surfaced as a client
  error.
- **`EngineService::workspace_rpc` unit tests** (no socket) cover each variant + the gate denial
  directly, so the dispatch/gating logic is tested independently of the HTTP layer.

## Out of scope (named, not silently dropped)

- **Wiring `RemoteWorkspace` into a running remote orchestrator / promote-to-remote** — the
  next sub-project (`RemoteTarget` + promote).
- **Large-file / streaming transfer** — whole-value JSON bodies for v1 (the snapshot/read
  `Vec<u8>` JSON-bloat caveat from the snapshot sub-project applies; base64/streaming is a
  shared later refinement).
- **https client cert-trust configuration** for self-signed remotes — `reqwest` supports it;
  v1 tests plaintext loopback. TLS on the server already exists (serve sub-project).
- **A separate `remote` crate** — `RemoteWorkspace` lives in `workspace` per the architecture;
  `RemoteTarget` (the `remote` crate) is the next sub-project.
