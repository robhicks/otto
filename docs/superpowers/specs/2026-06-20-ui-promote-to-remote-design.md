# Sub-project F — Promote-to-remote UX (design)

**Status:** Approved (2026-06-20). Final UI sub-project of the roadmap
([2026-06-17-ui-roadmap.md](2026-06-17-ui-roadmap.md), row **F**).

## Goal

Let the UI hand a live session off to another engine and back: a **client-driven
handover** to a remote engine with `Last-Event-ID` reconnect, plus the reverse trip
home. New `PromoteToRemote`/`DemoteToLocal` commands are wired to the existing
`promote()` + `LoopbackTarget` machinery in `crates/engine/src/remote.rs`.

## Key insight

The remote axis already has every primitive this needs: `promote()` snapshots a
session + workspace and provisions it onto a `RemoteTarget`, returning a
`RemoteHandle { endpoint, token }`; `LoopbackTarget` provisions a real second
in-process engine on `127.0.0.1:0`; the UI already reconnects with
`session` + `last_seq` replay; and the status strip already renders
`engine_remote`. So **demote is just promote in the other direction** — the same
snapshot→provision→handover mechanism, differing only in the provisioned engine's
`engine_remote` capability flag. No new transport mechanism is introduced; F is
wiring + UX over what exists.

## Fixed decisions (from brainstorming)

1. **Both directions this slice** — full `PromoteToRemote` + `DemoteToLocal` round-trip.
2. **Opt-in `--promote-loopback` flag** — `otto serve` defaults to the
   `UnsupportedTarget` posture (an honest `Error` reply); the flag wires
   `LoopbackTarget` so the handover is exercisable on one machine.
3. **New `ServerMessage::Promoted`/`Demoted`** — connection-level framing, **not**
   persisted to the event log (so stale routing info can never be replayed from the
   store on a later reconnect).
4. **Reuse the same token** — the loopback remote requires the same `OTTO_TOKEN`; the
   client keeps its token and swaps only the endpoint. No secret is echoed over the wire.

## Protocol additions (additive, semver-minor)

In `crates/protocol/src/lib.rs`:

```rust
// Command (frontend → engine)
PromoteToRemote { session: SessionId },
DemoteToLocal   { session: SessionId },

// ServerMessage (engine → frontend; transport framing, NOT a sequenced Event)
Promoted { session: SessionId, endpoint: String },  // reconnect here; engine will report remote
Demoted  { session: SessionId, endpoint: String },   // reconnect here; engine will report local
```

`endpoint` is a `ws://host:port` base (as returned by `RemoteHandle`). The client
reuses its existing token, `session`, and `last_seq`; the new engine's
`CapabilitiesManifest` tells the UI whether it is now remote or local, so the payload
carries no mode flag. `Promoted` and `Demoted` are distinct (mirroring the two
commands) but carry identical fields and drive identical client behavior
("reconnect to `endpoint`").

These are additive enum variants — a semver-minor change to the wire types, consistent
with how every prior sub-project extended `protocol`.

## Server flow

`PromoteToRemote`/`DemoteToLocal` are handled in serve's **outer command loop only**
(i.e. when no turn is in flight). Promoting mid-turn would snapshot partial session
state, so the UI disables the buttons during a turn; a promote/demote that nonetheless
arrives mid-turn is ignored (no-op), matching how the loop treats other off-cadence
commands.

On the command, the handler:

1. If no `PromoteConfig` is present (`--promote-loopback` not set), reply
   `ServerMessage::Error { message: "remote provisioning unavailable (start with --promote-loopback)" }`
   and continue. This is the `UnsupportedTarget` posture surfaced to the UI.
2. Otherwise build a `LoopbackTarget` with `engine_remote: true` (promote) or
   `false` (demote), rooted at a fresh subdir of the config's `base_dir`.
3. Call `promote(service.store(), service.workspace(), session, &target)` →
   `RemoteHandle { endpoint, token }`.
4. **Retain the handle** (see lifecycle below), then send
   `ServerMessage::Promoted { session, endpoint }` (or `Demoted`).

The client then drops its current socket and reconnects to `endpoint`. The source
engine is intentionally **not** stopped (handover is a client concern, per the
existing `promote()` contract); its now-stale session simply stops receiving traffic.

### Handle lifecycle (the one real catch)

`RemoteHandle`'s `Drop` aborts the provisioned engine's task. Because the local
connection ends immediately after handover, dropping the handle there would kill the
remote before the client reaches it. So `ServeState` gains a shared registry:

```rust
remotes: Arc<Mutex<HashMap<SessionId, RemoteHandle>>>
```

The handler inserts the handle **before** sending `Promoted`/`Demoted`, so the
provisioned engine outlives the local connection. The provisioned engine is itself
wired with a (nested) `PromoteConfig` so it can promote/demote again — that is what
makes the full round-trip work.

**Accepted limitation (stated honestly):** provisioned loopback engines accumulate in
their parent's registry until process exit; there is no cross-engine teardown
handshake. True teardown of a provisioned remote is the same external/manual boundary
that `UnsupportedTarget` already marks in-tree. This is acceptable for a one-machine
loopback feature and is documented, not hidden.

## Engine / wiring changes

- **`EngineService::workspace(&self) -> &dyn Workspace`** — a read accessor so the
  serve layer can pass the workspace to `promote()` (the store accessor already exists).
- **`LoopbackTarget`** gains an `engine_remote: bool` (promote → `true`, demote →
  `false`); it already sets the provisioned manifest's `engine_remote`, so this just
  becomes configurable. Its `provision()` additionally wires the provisioned engine's
  `app()` with a `PromoteConfig` (nested `base_dir`, same token) so the new engine can
  itself hand the session on.
- **`PromoteConfig { token: String, base_dir: PathBuf }`** — a small struct carried by
  `ServeState` as `Option<PromoteConfig>`. `Some` ⟺ promotion enabled.
- **`serve::app` / `serve_app`** grow one optional `Option<PromoteConfig>` parameter
  threaded into `ServeState`. Existing callers pass `None` (no behavior change).
- **`main.rs cmd_serve`** parses `--promote-loopback`; when set, it builds a
  `PromoteConfig` with the serve token and a base dir under the work root, and passes
  `Some(..)` to `serve_app`.

`build_router`/determinism are untouched: promotion is opt-in and command-driven; the
offline path never constructs a `PromoteConfig` and never emits a promote event.

## UI changes (`ui/`, depends only on `protocol`)

- A **"Promote to remote"** button: shown when connected, the manifest reports
  `engine` local, and no turn is running. On click → `Command::PromoteToRemote { session }`.
- A **"Demote to local"** button: shown when connected and the manifest reports
  `engine` remote (and no turn running). On click → `Command::DemoteToLocal { session }`.
- On `ServerMessage::Promoted { endpoint, .. }` / `Demoted { endpoint, .. }`: set the
  connection URL to `endpoint`, keep token + session + `last_seq`, tear down the
  current socket, and reconnect through the existing reconnect path. Replay fills the
  gap; the status strip flips local↔remote from the new manifest.
- A `ServerMessage::Error` reply (promotion unavailable) renders in the event log via
  the existing error row.

A small **pure helper** in `ui/src/view_model.rs` computes the next connection target
from a `Promoted`/`Demoted` message (so the reconnect-target logic is host-testable),
plus button-visibility predicates over `(ConnState, CapabilitiesManifest, turn_running)`.

## Testing

- **Engine integration (loopback E2E), in `crates/engine/tests/serve.rs`:** with
  `--promote-loopback` wired, connect to L → send a prompt (turn runs on L) →
  `PromoteToRemote` → receive `Promoted { endpoint }` → reconnect to `endpoint` with
  `last_seq` → assert replay continuity and `engine_remote = true` → send another
  prompt (turn runs on the remote) → `DemoteToLocal` → receive `Demoted { endpoint }`
  → reconnect → assert `engine_remote = false` and the full event history is intact.
- **Unsupported posture:** without the flag, `PromoteToRemote` yields
  `ServerMessage::Error` and the session keeps working locally.
- **Protocol:** serde round-trip for the four new variants.
- **UI host tests:** the pure reconnect-target helper and the button-visibility
  predicates.
- **Determinism:** unchanged — no new env reads in core logic, no new offline-path
  events.

## Build sequence

1. `protocol`: the two `Command` + two `ServerMessage` variants (+ serde tests).
2. `engine`: `EngineService::workspace()`; `LoopbackTarget` `engine_remote` flag +
   nested promote-config wiring; `PromoteConfig`.
3. `engine/serve`: `ServeState` registry + `Option<PromoteConfig>`; route
   Promote/Demote in the outer loop; `app`/`serve_app` parameter.
4. `engine/main`: `--promote-loopback` flag.
5. `ui`: buttons, `Promoted`/`Demoted` handling + reconnect, view_model helpers.
6. Tests: loopback E2E + unsupported posture + UI host tests.

## Out of scope (boundaries)

- Real VPS/microVM provisioning — external infra, stays behind `UnsupportedTarget`.
- Cross-engine teardown handshake (provisioned engines live until process exit).
- Promoting mid-turn (disabled; finish or abort the turn first).
- Multi-host token management (loopback reuses one token by decision).
