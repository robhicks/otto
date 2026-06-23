# demote-from-microvm

**Date:** 2026-06-23
**Status:** Shipped 2026-06-23 (plan: docs/superpowers/plans/2026-06-23-microvm-demote.md).
**Depends on:** the `microvm` axis (`MicrovmTarget` + `Provisioner` seam +
`FirecrackerProvisioner`/`UnsupportedProvisioner` in `crates/remote`, shipped
2026-06-23), the restore-over-the-wire RPCs (`POST /promote` +
`EngineService::accept_promotion`, and `POST /export` + `export_promotion` +
`accept_demotion` + `SessionStore::restore_over`, all shipped with `vps`
demote-from-remote 2026-06-22), and the handover dispatch (`handle_handover` in
`crates/engine/src/serve.rs`).

## Goal

Let a client connected to a source serve `S` (started `--promote-microvm`, having
promoted a session into an ephemeral microVM) issue `DemoteToLocal` and have `S`:

1. pull the session's *current* state back off the running microVM,
2. restore it locally (overwriting `S`'s stale pre-promote copy), and
3. **dispose the microVM**.

This replaces the current honest-refusal branch
(`"demote-from-remote not supported in microvm mode (ephemeral)"`) in
`handle_handover` and closes the "demote-from-microvm is the remaining follow-up"
item in `CLAUDE.md` / the microvm-provisioner-seam spec.

## Why microVM demote differs from VPS demote

VPS demote and microVM demote are the same *shape* — pull a `PromoteBundle` via
`POST /export`, restore it with `accept_demotion`, reconnect the client to `S` —
but three properties differ because a microVM is **ephemeral and owned by `S`**,
whereas a VPS receiver is **operator-managed and long-lived**:

| | VPS demote | microVM demote |
|---|---|---|
| Receiver endpoint | static, from `cfg.mode` (`PromoteMode::Vps{endpoint}`) | dynamic, only known from the live `RemoteHandle` a prior promote stored in `state.remotes` |
| Prior promote required? | no (the receiver runs independently) | **yes** — with no live handle there is no VM to pull from |
| After demote | receiver keeps its copy (no teardown) | **VM is disposed** (dropping the handle runs its disposal task) |

The guest serve runs `otto serve --accept-promotions` (see
`crates/remote/src/firecracker.rs`), so `/export` is live on the microVM and the
pull is the exact inverse of the promote push (`/promote`).

## Components

### 1. `remote` crate — extract a shared pull primitive

Today the `POST /export` pull lives only as `VpsTarget::export` (an inherent
method). microVM demote must pull from a *handle's* endpoint, not a `VpsTarget`,
so the pull is extracted into a free function symmetric with the existing
`pub(crate) push_promote_bundle`:

```rust
/// POST a session id to `{http_base(endpoint)}/export` with `Bearer token` and deserialize the
/// returned `PromoteBundle`. On a non-2xx, bail with the receiver's status + body. Shared by
/// `VpsTarget::export` (vps demote) and the microVM demote pull (handle-sourced endpoint).
pub async fn export_bundle(
    endpoint: &str,
    token: &str,
    session: SessionId,
) -> anyhow::Result<PromoteBundle>
```

`VpsTarget::export` is refactored to delegate to it (one line). The function is
`pub` (not `pub(crate)`) so `serve.rs` in the `engine` crate can call it against a
microVM handle's endpoint+token. Re-exported from `otto_engine` alongside the rest
of the seam.

No new `MicrovmTarget` method is needed: teardown is already "drop the
`RemoteHandle`" (its `shutdown` task aborts the serve / kills the VM via `Drop`),
and the pull is now `export_bundle`.

### 2. `serve.rs` — wire the microVM demote branch

In `handle_handover`, the `if !to_remote` block currently has a `Vps` arm (pull +
restore + reply own base) followed by a `Microvm` arm that refuses. Replace the
refusal with a pull-restore-dispose:

1. **Find the live handle.** Look up `state.remotes` for `(session, true)` — the
   key a prior promote stored under. Clone its `endpoint` + `token`; do **not**
   remove it yet. Absent → reply
   `Error{"no active microvm handover for this session; promote first"}` and
   return. (The lock is taken and released before any `.await`, matching the
   existing idempotent-reuse lookup.)
2. **Pull.** `otto_remote::export_bundle(&endpoint, &token, session).await`. On
   `Err` → reply `Error{e}` and return, **leaving the VM running** (a transient
   pull failure must not lose the session — the user can retry).
3. **Restore locally.** `state.service.accept_demotion(&bundle).await` (existing:
   inviolable sensitive-path floor first, then `restore_over` overwrites `S`'s own
   stale row, then the gate-validated workspace files). On `Err` → reply
   `Error{msg}` and return, again leaving the VM running.
4. **Dispose, then reply — success path only.** Remove the handle from
   `state.remotes` and drop it (its `Drop`/`abort` runs the disposal task → the
   microVM is torn down). Reply
   `Demoted{ session, endpoint: public_ws_base }`. The client reconnects to `S`
   (the session is local again), identical to the VPS-demote reply. If
   `public_ws_base` is `None` (serve not built via `serve_app_with_base`), reply
   the same misconfiguration `Error` the VPS arm already uses.

The generic promote/loopback path lower in `handle_handover` is unchanged and is
never reached for microVM demote (the `!to_remote` Microvm arm returns).

### Data flow (success)

```
client --DemoteToLocal{session}--> S.handle_handover
  S: handle = remotes[(session, true)]          (live promote handle; endpoint+token)
  S --POST /export {session}--> microVM serve    (export_bundle)
  microVM --PromoteBundle (gate-filtered)------> S
  S: accept_demotion(bundle)                      (restore_over + apply gated edits, fail-closed)
  S: drop handle  -> microVM disposed
  S --Demoted{session, endpoint=public_ws_base}--> client
client reconnects to S; session is local again
```

## Error handling (fail-closed, VM-preserving)

- **No prior promote** → `Error`, nothing changed.
- **Pull fails** (VM unreachable / non-2xx) → `Error`, VM left running, local copy
  untouched.
- **Restore refused** (sensitive-path floor) or **restore failed** → `Error`, VM
  left running, `restore_over`/edits are the only mutations and are not reached on
  refusal; a mid-restore `Failed` leaves a partially-overwritten local copy (same
  semantics as VPS demote — `accept_demotion` is not transactional across the
  store + workspace, and this is unchanged).
- The microVM is disposed **only** after a fully successful restore.

## Testing

Mirrors the established no-firecracker boundary (seam happy-path in-process;
serve-wiring only exercises error paths without the feature).

1. **Seam happy path** (`crates/engine/tests/microvm.rs`, CI-able via the existing
   `TestServeProvisioner`): provision an in-process `otto serve
   --accept-promotions`, promote a session into it (or push a bundle), then
   `otto_remote::export_bundle(handle.endpoint, handle.token, id)` pulls it back;
   assert the session id + workspace file round-trip. Drop the handle → the serve
   is unreachable (dispose). Proves pull + dispose against a real serve.
2. **`VpsTarget::export` unchanged** — the existing `vps_promote.rs` export tests
   stay green after the delegation refactor (regression guard on the extraction).
3. **serve-wiring error path** (`microvm.rs`, no feature): replace
   `handover_microvm_demote_is_unsupported` with
   `handover_microvm_demote_without_prior_promote_errs` — `DemoteToLocal` with no
   prior promote yields the new `"no active microvm handover"` error.
4. **`accept_demotion` / `restore_over`** already have unit coverage from the VPS
   demote work; not re-tested here.
5. **Determinism suite untouched** — `--promote-microvm` stays opt-in; default
   offline `cargo test --workspace` is unaffected.

The full serve-level firecracker round-trip (promote into a real VM, demote back)
is inherently un-CI-able — the same boundary as
`handover_microvm_promote_is_unsupported_without_feature`.

## Non-goals

- Re-attaching to a microVM after `S` restarts (the handle, and thus the VM, dies
  with the process — by design for an ephemeral guest).
- Any change to VPS or loopback demote behavior beyond the `export_bundle`
  extraction.
- Making `accept_demotion` transactional across store + workspace (pre-existing).
- A static microVM endpoint (microVM endpoints are dynamic, per-provision).
