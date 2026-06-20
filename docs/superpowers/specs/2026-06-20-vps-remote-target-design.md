# `vps` RemoteTarget — promote a session to a real remote engine

**Date:** 2026-06-20
**Status:** Approved design — ready for implementation plan
**Depends on:** the remote axis (`RemoteTarget`, `promote()`, `LoopbackTarget`,
`UnsupportedTarget` in `crates/engine/src/remote.rs`) and the promote-to-remote handover
(sub-project F — `PromoteToRemote`/`DemoteToLocal`, `PromoteConfig`, `handle_handover`).

## Why

The distribution axis works **end to end on loopback**: `promote()` snapshots a session and its
workspace into a `PromoteBundle` and hands it to a `RemoteTarget`; `LoopbackTarget` provisions a
real second in-process engine and restores the bundle into it. The one missing piece is moving a
session onto an engine **on another machine**. `UnsupportedTarget` marks that boundary today, and
`ARCHITECTURE.md` calls the `vps` impl (long-lived server) "v1-ready".

The key realization: `LoopbackTarget` *creates and restores* an engine, but a real VPS is
**operator-managed and already running**. So the target's job changes from create-and-restore to
**push-the-bundle-to-an-existing-server**. That needs exactly one new engine capability — a
restore-over-the-wire RPC — plus a thin client target. This shrinks the `UnsupportedTarget`
boundary to just "we don't create the machine for you" (SSH / cloud-API machine provisioning stays
external and manual).

## Goal & non-goals

**Goal.** A real `VpsTarget` that promotes a session onto an already-running, bearer-authed
`otto serve` over the network, restoring it there so a client can reconnect and resume via
`Last-Event-ID`.

**Non-goals (explicitly out of scope for this sub-project):**

- **Machine provisioning.** SSH'ing in, shipping/starting the binary, or calling a cloud SDK to
  create a VM. The receiver `otto serve` must already be running. This is the residual
  `UnsupportedTarget` boundary.
- **Demote-from-remote.** Pulling a session *back* off the operator's server needs a reverse
  snapshot-export RPC; it is a separate, larger piece. In `vps` mode `DemoteToLocal` returns an
  honest "demote-from-remote not supported" error. Loopback demote is unchanged.
- **Multi-session receivers.** A receiver `otto serve` has one workspace root, so it hosts one
  promoted *workspace* at a time (its store still holds multiple sessions). A second promote into
  the same receiver overwrites the workspace. Documented limitation; matches serve's existing
  single-workspace model.
- The `remote` crate split-out (`ARCHITECTURE.md`). `VpsTarget` lives alongside `LoopbackTarget`
  in `crates/engine/src/remote.rs`; the crate split is deferred to its own refactor.

## Two roles, two flags

The same `otto serve` binary plays one of two roles in a handover, selected by opt-in flags that
stay fail-closed by default:

- **Receiver (remote) role:** `otto serve --accept-promotions` enables the new inbound restore RPC.
  Absent ⇒ the endpoint rejects all requests (the `/workspace`-style gated posture).
- **Sender (source/local) role:** `otto serve --promote-vps <ws-endpoint>` configures *this* engine
  so a UI's `PromoteToRemote` pushes to `<endpoint>` instead of looping back. Mutually exclusive
  with `--promote-loopback`. The bearer token is **reused from the source** (the same convention
  `LoopbackTarget` already uses — the source and receiver share a trust domain in v1).

A given process can be a receiver, a sender, or (for the loopback round-trip test) both.

## Components

### 1. `Workspace::restore` — promote the inherent method onto the trait

`SessionStore::restore` is already a trait method, but workspace restore is an **inherent** method
on `LocalWorkspace` (only `snapshot()` is on the `Workspace` trait). The receiver handler holds an
`Arc<dyn Workspace>`, so it cannot call the inherent method. Promote `restore` onto the `Workspace`
trait, mirroring `snapshot()`:

```rust
#[async_trait]
pub trait Workspace: WorkspaceRead {
    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot>;
    async fn restore(&self, snapshot: &WorkspaceSnapshot) -> anyhow::Result<()>; // new
}
```

- `LocalWorkspace`'s existing inherent `restore` body moves to the trait impl unchanged — it still
  writes through the gated `apply_edit`, so the **inviolable sensitive-path floor still applies**: a
  bundle carrying `.env`/`.ssh`/`.git`/keys cannot be written. No new bypass.
- `LoopbackTarget` keeps working — it calls `restore` on a concrete `LocalWorkspace`.
- `RemoteWorkspace::restore` returns an error (`anyhow::bail!` — restoring *into* a remote-proxy
  workspace is not a supported direction). This keeps the trait total without inventing behavior.

### 2. `POST /promote` — the restore-over-the-wire RPC (receiver side)

A new axum route on the served app, mirroring the existing `POST /workspace` handler:

- **Auth & gate:** bearer-authed via the existing `authorized(&headers, &state.token)` check, and
  served only when `--accept-promotions` is set. When unset, the route returns
  `403 Forbidden` ("promotion acceptance disabled"). Fail-closed.
- **Body:** the serialized `PromoteBundle` (`{ session: SessionState, workspace: WorkspaceSnapshot }`).
  Both types already derive `Serialize`/`Deserialize`. `PromoteBundle` gains the derives.
- **Handler:** calls a new `EngineService::accept_promotion(&bundle)` which runs
  `store.restore(&bundle.session)` then `workspace.restore(&bundle.workspace)`. Returns
  `200 { "session": <id> }` on success, `409 Conflict` if the session already exists in the
  receiver's store (`SessionStore::restore` already errors on a duplicate — surface it honestly),
  `400` on a malformed body, `500` on a restore failure (e.g. the sensitive-floor refusal).

`ServeState` gains an `accept_promotions: bool` flag; `app()` registers the `/promote` route
unconditionally but the handler short-circuits to `403` when the flag is false (keeps the router
construction simple and the gate explicit in one place).

### 3. `VpsTarget` — the client target (sender side)

A new `RemoteTarget` in `remote.rs`, sibling to `LoopbackTarget`:

```rust
pub struct VpsTarget { endpoint: String, token: String } // endpoint = ws://host:port or wss://...
```

- **`provision(bundle)`:** derive the HTTP base from `endpoint` (`ws→http`, `wss→https`),
  reqwest-`POST` the JSON-serialized bundle to `{base}/promote` with
  `Authorization: Bearer <token>`. On a 2xx response, return
  `RemoteHandle { endpoint: <ws endpoint>, token, shutdown: None }`. On any non-2xx or transport
  error, return `Err` — which `handle_handover` already turns into a client `Error` reply
  (fail-closed; nothing is half-promoted from the client's view).
- **`teardown`:** a **no-op**. `VpsTarget` does not own the operator's long-lived server, so it must
  not abort it. This is the honest behavioral difference from `LoopbackTarget` (which aborts the
  engine task it spawned). Documented in the impl.
- The reqwest client mirrors the existing `RemoteWorkspace` RPC client (same TLS posture; `wss://`
  endpoints map to `https://`).

### 4. Target selection in `handle_handover`

`PromoteConfig` currently carries loopback-specific fields (`token`, `base_dir`). Generalize the
*which-target* decision with a mode enum while keeping the shared `token`:

```rust
pub struct PromoteConfig { pub token: String, pub mode: PromoteMode }
pub enum PromoteMode {
    Loopback { base_dir: PathBuf },
    Vps { endpoint: String },
}
```

`handle_handover` builds the matching target:

- `Loopback { base_dir }` → `LoopbackTarget::new(token, base_dir, to_remote)` (unchanged behavior).
- `Vps { endpoint }`:
  - `to_remote == true` (promote) → `VpsTarget { endpoint, token }`.
  - `to_remote == false` (demote) → reply with an `Error`
    ("demote-from-remote not supported in vps mode") and return. (Per non-goals.)

The existing idempotency cache (`remotes` keyed by `(session, to_remote)`) and the
"retain-before-reply" ordering carry over unchanged. For `vps` the retained `RemoteHandle` has a
no-op `shutdown`, so retention is cheap and dropping it never touches the operator's server.

## CLI wiring

`otto serve` gains:

- `--accept-promotions` (bool) → `ServeState.accept_promotions = true`.
- `--promote-vps <ws-endpoint>` (string) → `PromoteConfig { token, mode: Vps { endpoint } }`.
- Existing `--promote-loopback` → `PromoteConfig { token, mode: Loopback { base_dir } }`.
- `--promote-loopback` and `--promote-vps` are mutually exclusive (clap conflict / explicit check).

## Data flow (promote to a real remote)

```
UI (connected to SOURCE serve, started with --promote-vps wss://host:port)
  └─ Command::PromoteToRemote
        └─ handle_handover  (PromoteMode::Vps)
              └─ promote(store, workspace, session, &VpsTarget{endpoint, token})
                    ├─ store.snapshot(session)      → SessionState
                    ├─ workspace.snapshot()         → WorkspaceSnapshot
                    └─ VpsTarget::provision(bundle)
                          └─ POST {https}/promote  (Bearer token, JSON bundle)
                                 │
RECEIVER serve (--accept-promotions) ◄─────────────┘
   /promote handler → service.accept_promotion(bundle)
        ├─ store.restore(session)        (409 if already present)
        └─ workspace.restore(snapshot)   (gated apply_edit; sensitive floor enforced)
   → 200 { session }
        └─ RemoteHandle{ endpoint: wss://host:port, token, shutdown: None }
  └─ ServerMessage::Promoted{ session, endpoint } → UI
        └─ UI drops SOURCE socket, reconnects to RECEIVER (?last_seq=) → resumes
```

## Error handling & security

- **Fail-closed gate.** `/promote` rejects with `403` unless `--accept-promotions`; rejects with
  `401` without a valid bearer. Identical posture to the `/workspace` RPC.
- **Sensitive-path floor preserved.** Workspace restore goes through `LocalWorkspace::apply_edit`,
  so a malicious or buggy bundle cannot write `.env`/`.ssh`/`.git`/ssh keys — the restore of those
  entries is refused and surfaces as a `500`/error, never a silent write.
- **No partial promote on the client.** A non-2xx receiver response makes `provision` return `Err`;
  `handle_handover` sends `Error` and does not cache a handle, so the source session stays put and
  the user can retry.
- **Duplicate-session honesty.** Re-restoring an existing session id into a receiver is a `409`, not
  a silent overwrite (`SessionStore::restore` already errors on duplicates).
- **TLS.** `wss://` endpoints map to `https://`; the receiver runs `otto serve --tls-cert/--tls-key`
  for a real deployment. Token-in-URL caveats already documented for `/ws` are unchanged.

## Testing (real coverage, no external infra)

The crux: a `VpsTarget` pointed at a **second in-process `otto serve --accept-promotions` on a
`127.0.0.1:0` ephemeral port** exercises the real network restore RPC end to end. No external box,
fully deterministic, runs in CI.

1. **End-to-end promote round-trip.** Start receiver serve B with `accept_promotions = true`. Create
   a session with a turn + workspace files in store A. `promote()` it through `VpsTarget` at B.
   Connect a WS client to B with `?last_seq=0` and assert the session resumes — the event log
   replays and the restored workspace files are listable via B's `/workspace`.
2. **Restore-RPC handler unit tests:**
   - `POST /promote` with a valid bundle → `200`; store + workspace on B are restored.
   - `POST /promote` without `--accept-promotions` → `403`.
   - `POST /promote` without/with a wrong bearer → `401`.
   - `POST /promote` with a bundle whose `WorkspaceSnapshot` includes a `.env` entry → the sensitive
     floor refuses it (error response; the file is not written).
   - `POST /promote` re-restoring an already-present session id → `409`.
3. **`VpsTarget::teardown` is a no-op** — after `teardown`, the receiver serve B is still reachable
   (asserts it does not abort the operator's server).
4. **`handle_handover` in vps mode:** `PromoteToRemote` → `Promoted{endpoint}` pointing at the
   receiver; `DemoteToLocal` → an `Error` ("not supported in vps mode").
5. **`Workspace::restore` trait move:** existing `LocalWorkspace` snapshot/restore round-trip and
   path-escape/non-utf8 rejection tests keep passing through the trait method; add a
   `RemoteWorkspace::restore` returns-error test.
6. **Determinism suite untouched.** No new default behavior — both new flags are opt-in, all paths
   offline, so `cargo test --workspace` with no env vars is unchanged.

## Files touched (anticipated)

- `crates/engine-core/src/traits.rs` — add `restore` to the `Workspace` trait.
- `crates/workspace/src/lib.rs` — move `LocalWorkspace::restore` onto the trait impl.
- `crates/workspace/src/remote.rs` — `RemoteWorkspace::restore` (erroring impl).
- `crates/engine/src/service.rs` — `EngineService::accept_promotion(&PromoteBundle)`.
- `crates/engine/src/remote.rs` — `VpsTarget`; `PromoteBundle` serde derives; `PromoteConfig` +
  `PromoteMode` refactor.
- `crates/engine/src/serve.rs` — `/promote` route + handler; `ServeState.accept_promotions`;
  `app()` signature; `handle_handover` target selection.
- `crates/engine/src/main.rs` (or wherever `serve` CLI is wired) — `--accept-promotions`,
  `--promote-vps`, mutual-exclusion with `--promote-loopback`.
- `docs/ARCHITECTURE.md` / roadmap note — record `vps` shipped; `UnsupportedTarget` boundary now
  only machine-provisioning; demote-from-remote is the next follow-up.
