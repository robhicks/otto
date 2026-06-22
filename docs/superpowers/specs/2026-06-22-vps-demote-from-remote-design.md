# vps demote-from-remote — pull a session back off a running receiver

**Date:** 2026-06-22
**Status:** Shipped 2026-06-22 (plan: docs/superpowers/plans/2026-06-22-vps-demote-from-remote.md).
**Depends on:** the `vps` `RemoteTarget` (`VpsTarget`, `PromoteConfig`/`PromoteMode::Vps`,
`accept_promotion`, `POST /promote`) shipped 2026-06-22
(`docs/superpowers/specs/2026-06-20-vps-remote-target-design.md`), and the promote-to-remote
handover (`PromoteToRemote`/`DemoteToLocal`, `handle_handover` in `crates/engine/src/serve.rs`).

## Why

The `vps` axis promotes a session **onto** an already-running `otto serve --accept-promotions`:
the source serve `S` (started with `--promote-vps <R>`) snapshots the session + workspace into a
`PromoteBundle` and `POST`s it to receiver `R`, which restores it through the gated
`accept_promotion`. The client then reconnects to `R` and works there.

The inverse — **demote**, pulling the session back off `R` to local — is the one remaining
honest-refusal in the handover path. Today `handle_handover` returns
`"demote-from-remote not supported in vps mode"` for any vps demote (`serve.rs:544-551`). The code
comment already names the missing piece: *"a reverse snapshot-export RPC."* Loopback demote already
works; this design closes the vps gap and leaves only **machine provisioning** (SSH / cloud-SDK VM
creation) behind `UnsupportedTarget`.

## Goal & non-goals

**Goal.** A client connected to the source serve `S` can issue `DemoteToLocal` and have `S` pull the
session's current state back from receiver `R` and restore it into its own store + workspace, so the
session is local again. Promote is a *push* from `S`; demote is the symmetric *pull* by `S`.

**Non-goals (deliberate, v1):**
- **Move semantics.** Demote is a *copy*: `R` keeps its session row after the pull (exactly as
  promote leaves the source's session in place). Retiring/aborting `R`'s copy is out of scope — see
  *Known limitation*.
- **Cross-process source recovery.** `S` learns `R`'s endpoint from its own live `--promote-vps`
  config. If `S` was restarted (config still set) the pull still works; reconstructing a demote
  against a *different* serve that never promoted is out of scope.
- Machine provisioning, the `microvm` target, the `remote` crate split, multi-session receivers.

## Topology

```
promote (push):   client → S  ──snapshot+POST /promote──→  R   (client reconnects to R)
demote  (pull):   client → S  ──GET bundle via /export───→  R   (client reconnects to S)
                              ←──restore into S's store───
```

The decisive constraints behind "client reconnects to `S`, `S` pulls from `R`":

- `R` is the operator's server and cannot provision a "local" engine on the *user's* machine —
  that is exactly why `R` handling a vps demote is the honest-refusal today.
- `S` already knows `R` (it is `cfg.mode = Vps { endpoint }`) and already shares `R`'s bearer token
  (source and receiver share a trust domain in v1). So `S` needs no new configuration to pull.
- The alternative — `R` pushing the bundle *back* to `S`'s `/promote` — would require `R` to know
  `S`'s address + token and to reach into the user's machine (often behind NAT). Rejected.

## Components

### 1. Receiver side — `POST /export` on `R`

A new axum route on the serve transport, **bearer-authed**, enabled **only when
`--accept-promotions` is set** (reuses the exact flag/guard that gates `/promote`; a receiver that
accepted a promotion may also let it be pulled back). The shared token already gates `/promote`
(push an arbitrary workspace) and `/workspace` (read files), so `/export` adds no material trust
surface.

- **Request:** the session id (JSON body `{ "session": "<uuid>" }`).
- **Response:** `200` with the `PromoteBundle` JSON (`store.snapshot(session)` +
  `workspace.snapshot()`).
- **Backing method:** new `EngineService::export_promotion(session) -> Result<PromoteBundle,
  ExportError>`. It builds the same bundle shape `promote()` builds, minus the target push.
- **Error → status** (mirrors the `/promote` handler):
  - export not enabled (`--accept-promotions` absent) → `403`
  - missing/bad bearer → `401`
  - unknown session → `404`
  - malformed body → `400`
  - snapshot failure → `500`

**Sensitive floor on export.** The service's `workspace.snapshot()` is already gate-filtered
(`service.rs:313-315`): paths the read gate denies (the `.env*`/`.ssh/`/`.git/`/… floor) never
appear in the snapshot. So secrets never leave `R`, *before* the bundle is even transmitted — and
`S` re-validates on restore (defense in depth, below).

### 2. Source side — pull + restore in `handle_handover(to_remote=false)`, vps branch

Replace the honest-refusal branch (`serve.rs:544-555`) with the pull:

1. Read `R`'s endpoint from `cfg.mode = PromoteMode::Vps { endpoint }` and the shared
   `cfg.token`.
2. `VpsTarget::export(session)` — a new reqwest call symmetric with the existing `provision` push:
   `POST {http_base}/export` with `bearer_auth(token)` and the session id, deserialize the
   `PromoteBundle`. Non-2xx surfaces the receiver's body as the error reason (as `provision` does).
3. Restore into `S` via a new **`EngineService::accept_demotion(bundle)`** (see §3). On success,
   reply `ServerMessage::Demoted { session, endpoint: <S's own ws base> }`.

`S`'s `state.remotes` map (keyed by `(session, to_remote)`) is unaffected; loopback demote keeps its
existing provision path. Only the vps branch changes.

### 3. Restore-over — `accept_demotion` + `SessionStore::restore_over`

**The wrinkle.** After a vps promote, `S` *keeps* its copy of the session (copy semantics). So when
`S` pulls the advanced session back from `R` and tries to restore it, the session id **already
exists in `S`'s store**. `SqliteStore::restore` does an `INSERT` that deliberately **fails on
conflict** (test `restore_into_existing_session_is_error`, `crates/persistence/src/sqlite.rs:585`) —
that fail-closed guard protects a *promote receiver* from silently clobbering a session. Demote must
not reuse it: demoting back onto `S` is exactly the intended overwrite of `S`'s own stale copy with
`R`'s advanced state.

So:

- **New trait method `SessionStore::restore_over(&SessionState) -> Result<SessionId>`** — like
  `restore`, but within one transaction first deletes any existing `sessions`/`events`/`turns` rows
  for that session id, then inserts the snapshot. `restore` (fail-on-conflict) is untouched and
  still used by `accept_promotion`.
- **New `EngineService::accept_demotion(bundle) -> Result<SessionId, AcceptError>`** — performs the
  **same whole-bundle sensitive-path floor validation** as `accept_promotion` (every workspace path
  re-checked through the gate before any write), then restores the workspace via the existing
  in-place `Workspace::restore` (already overwrites files in place) and the session via
  `restore_over`. The only difference from `accept_promotion` is `restore_over` vs. `restore`.

Keeping `accept_promotion` (fail-on-conflict) and `accept_demotion` (overwrite-own-copy) as
**distinct** methods preserves the receiver's clobber-protection while letting the source refresh its
own session. The floor is enforced on **both** ends of demote: `R`'s gate-filtered `snapshot()` on
export, and `S`'s re-validation in `accept_demotion` on restore.

### 4. Reply + reconnect

`S` replies `ServerMessage::Demoted { session, endpoint }` with its own ws base (the URL the client
already used to reach `S`). The client reconnects to `S` with the session id; the session is now
local. No new `ServerMessage` variant is needed — `Demoted` already exists and is used by loopback.

## Data flow

```
client ──DemoteToLocal{session}──▶ S.handle_handover(to_remote=false)
                                     │  cfg.mode = Vps{endpoint=R}
                                     ▼
                          VpsTarget::export(session)
                                     │  POST R/export  (bearer)
                                     ▼
                  R: export_promotion(session) ──▶ PromoteBundle
                          (gate-filtered snapshot; secrets excluded)
                                     │  200 + bundle JSON
                                     ▼
                  S: accept_demotion(bundle)
                        ├─ re-validate every path through the floor
                        ├─ Workspace::restore  (overwrite files in place)
                        └─ SessionStore::restore_over  (overwrite S's row)
                                     │
                                     ▼
                  S ──Demoted{session, endpoint=S}──▶ client  (reconnect to S)
```

## Error handling

- **`R` unreachable / non-2xx on `/export`:** `VpsTarget::export` surfaces the status + body; `S`
  replies `ServerMessage::Error` (mirrors the promote path). `S`'s existing session is **untouched**
  — the overwrite in `accept_demotion` happens only after a successful, floor-validated pull.
- **Floor violation in the pulled bundle** (shouldn't occur given `R` filters on export, but
  defense in depth): `accept_demotion` returns `AcceptError::Refused`; `S` replies `Error` and does
  not write.
- **Promote not enabled on `S`** (`cfg` is `None`): unchanged — the existing
  `"remote provisioning unavailable …"` error still fires before the vps branch.

## Testing

End-to-end, mirroring `crates/engine/tests/vps_promote.rs`:

1. **Round-trip:** stand up an in-process receiver `R` with `--accept-promotions`; promote a session
   `S→R`; advance the session on `R` (run a turn / append events); demote `R→S`; assert `S`'s store
   snapshot and workspace now hold `R`'s **advanced** state (not `S`'s pre-promote copy).

Unit tests:

2. `SessionStore::restore_over` overwrites an existing session (and the round-trip in
   `restore_into_existing_session_is_error`'s sibling proves `restore` still rejects).
3. `POST /export` returns `403` without `--accept-promotions`, `404` for an unknown session, `401`
   without the bearer.
4. `accept_demotion` enforces the sensitive-path floor (a bundle carrying a `.env`/`.ssh` path is
   refused), symmetric with `accept_promotion_refuses_sensitive_workspace_entry`.

**Determinism invariant.** `/export` and the demote branch are reachable only with
`--accept-promotions` / `--promote-vps` set; the default offline suite is untouched.

## Known limitation

**Copy semantics → stale duplicate.** After demote, `R` retains its now-frozen session row and
workspace; the live copy is on `S`. The two can diverge if `R` is reused. This mirrors promote
(which also leaves the source's copy in place) and is accepted for v1; **move semantics** (the
export also retiring/aborting `R`'s session, with a partial-failure window between exported and
restored) is a deliberate non-goal.

## Spec coverage check

| Requirement | Component |
|---|---|
| Client→S demote pulls from R; reconnect to S | §2, §4 |
| `POST /export` on R, bearer-authed, gated by `--accept-promotions` | §1 |
| `export_promotion` builds the bundle; error→status mirrors `/promote` | §1 |
| Sensitive floor enforced on export (R) and restore (S) | §1, §3 |
| `VpsTarget::export` pull client | §2 |
| `accept_demotion` + `SessionStore::restore_over` overwrite S's own copy | §3 |
| `accept_promotion`'s fail-on-conflict `restore` left intact | §3 |
| End-to-end advanced-state round-trip + unit tests | Testing |
| Determinism suite untouched | Testing |
| Copy semantics; move-from-remote a non-goal | Known limitation, Non-goals |
