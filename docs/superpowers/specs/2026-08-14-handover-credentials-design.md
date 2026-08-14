# Handover credentials — per-session secrets, `Promoted.token` always `Some`

> **Status:** DRAFT — per-session promotion secrets for every `RemoteTarget`, the receiver's
> session→secret map, and the carried-over slice-1a review items. Not yet implemented.
> **Implements:** slice 3 of the multitenancy spine (issue #126) — the handover-credentials half of
> "Suggested first slice" in [#115](https://github.com/robhicks/otto/issues/115).
> **Depends on:** `docs/superpowers/specs/2026-08-01-session-ownership-design.md` (slice 1a, shipped
> in #123) and `docs/superpowers/specs/2026-08-01-multitenant-identity-design.md` (slice 1b, shipped
> in #137).
> **Blocks:** the final multitenancy closure (slice 2's UI login flow lands on a credential model
> that is already session-scoped).

The handover axis carries a single shared bearer today: `OTTO_PROMOTION_SECRET` is the *machine-wide*
credential of every promote receiver, and `ServerMessage::Promoted.token` is `None` for
loopback/vps/microvm — telling the client "reuse the current token". Under a user-identity model,
that reads as *the remote receives a source-valid user credential*: the exact hole issue #115's
Decision 4 closes. This slice narrows the receiver credential to per-session secrets and makes
`Promoted.token` always `Some`.

---

## Premise corrections

Each premise from the issue that did not survive contact with the repository, corrected here.

1. **A `version` field on `PromoteBundle` cannot fix the reported symptom.** The carried-over item
   offers "either a version field or having `/promote` special-case the missing-`owner` deserialize
   error." But the missing-`owner` failure happens *inside* `SessionState`'s deserialization — a
   legacy bundle's `session` object simply lacks the required `owner` key, so serde aborts the whole
   `PromoteBundle` struct before a top-level `version` field is ever read. A `version` field would
   only help for breaks at the `PromoteBundle` top level, and would still not produce the actionable
   message for the break that actually exists. **This slice takes the special-case option**:
   `promote_handler` catches the deserialize error, recognizes the pre-ownership shape, and returns
   the operator-actionable 400 the on-disk break already gets (slice 1a's §4.1 message, adapted to
   the wire).
2. **`VpsTarget::export` cannot survive as-is.** It pulls a session's bundle using the target's
   machine-wide token. Under this slice, an export is authorized by the *session's* per-session
   secret, which a `VpsTarget` (holding the machine-wide pusher credential) does not have — the
   secret lives in the `RemoteHandle` stored per `(session, true)`. The vps demote arm therefore
   stops constructing a fresh `VpsTarget` for the pull and instead reads the stored handle's
   endpoint+secret and calls the shared `export_bundle` directly — exactly the shape the microvm and
   fly demote arms already use. `VpsTarget::export` is removed (its only production caller was that
   arm); the demote pull is one code path for all three network targets. This is a breaking `remote`
   API change (see §8).
3. **The per-session secret is delivered as an HTTP header on the restore-push, not a field on
   `PromoteBundle`.** The issue names "the restore-push for vps/microvm" as the delivery channel.
   A header (`X-Otto-Session-Secret`) keeps the secret out of the serialized bundle — which is also
   the *return* type of `/export` and would otherwise echo the secret back across the wire on every
   pull — and keeps `PromoteBundle` a pure data payload. `push_promote_bundle` gains the header;
   `promote_handler` records it. Fly/microVM send the machine's own env/cmdline secret as the header
   (single-session machines: machine secret == session secret); VPS mints a distinct per-session
   secret at provision time.
4. **`/workspace` on a `Machine` receiver accepts any live session secret, not just the machine
   secret.** `WorkspaceRequest` carries no session id (§2 of the identity spec: `/workspace` is
   machine-scoped, one process-global root). A promoted client reconnects with its per-session
   secret (`Promoted.token`) and drives `/workspace` with it, so the receiver must accept it. The
   check is membership in the session→secret map (constant-time against each entry), plus the
   machine secret kept for the operator/back-compat — consistent with §2's stated
   not-isolated posture.
5. **`RemoteWorkspace` stays source/client-side only, and this slice decides it by documentation.**
   It has no production construction site (only `crates/engine/tests/{promote,remote_workspace,
   vps_promote}.rs`), and nothing in this slice adds one — a promoted machine never constructs it,
   and no promoted-machine code path needs it. The decision is recorded as a doc contract on
   `RemoteWorkspace::new` mirroring identity-spec A13, so the next slice that wires the promoted
   side inherits the constraint instead of rediscovering it.

---

## Scope

**In:**

- `ServerMessage::Promoted.token` becomes a required `String` — always `Some`, never the
  "reuse the current token" signal. Breaking wire change (the `null`/`None` case is removed),
  semver-major per Non-Negotiable Rule 6 → `protocol` 0.1.0 → 0.2.0.
- Every `RemoteTarget` mints a fresh opaque per-session secret at `provision()` time, delivered to
  the receiver over the provisioning channel (machine env for Fly, the machine cmdline/env + the
  restore-push for microVM, the restore-push for VPS), returned in the `RemoteHandle`, and reported
  to the client as `Promoted.token`. `mint_token()` (`crates/remote/src/fly.rs:34-37`) is
  generalized to a shared `mint_session_secret()` in `remote`.
- A long-lived `--accept-promotions` / `--promotion-receiver` receiver keeps a session→secret map
  (`ServeState.session_secrets`), populated on `/promote`, consulted by `/export`, `/workspace`,
  and the `Machine`-mode WS handshake, and disposed when the session is demoted.
- The `Machine`-mode WS credential becomes the attached session's per-session secret (header or
  `Attach` frame), not the machine-wide promotion secret. The `/export` credential becomes the
  requested session's per-session secret; a successful `/export` disposes it (demote consumes the
  credential).
- The carried-over slice-1a review items:
  - `/promote` returns the actionable pre-ownership message instead of a bare `missing field 'owner'`
    400 (premise correction 1).
  - `accept_demotion` refuses a bundle whose owner differs from the local row's current owner
    (closing the remaining half of the overwrite-including-owner hole the id-binding started in
    #123).
  - `RemoteWorkspace` documented source/client-side-only (premise correction 5).
- `ui-dioxus` adopts the token from a `Promoted` frame (the reconnect and `/workspace` calls use it)
  instead of ignoring it.

**Out:**

- The `Users`-mode credentials and the identity half (TOTP/JWT, `Login`/`Attach`) — shipped in slice
  1b, untouched. The `Machine`-mode *connection* still adopts the attached session's owner exactly
  as §6.5 of the identity spec describes; only the credential it verifies changes granularity.
- A `version` field on `PromoteBundle` (premise correction 1 — it cannot fix the reported break).
- Persisting the session→secret map. It is in-memory (`ServeState`); a receiver restart orphans the
  promoted sessions' credentials. Acceptable for the shipped ephemeral/long-lived-operator
  deployments (Fly/microVM die with their machine; a restarted VPS receiver is an operator action
  that can re-seed or the sessions are re-promoted), and recorded in §7.
- Any change to the permission gate, the sensitive-path floor, the sandbox posture, or
  `bash`-only-when-sandboxed. The session-secret map lives in serve state, never in the workspace.
- Per-owner workspace roots (its own slice, per identity-spec §10).

---

## Goal & Success Criteria

Make the handover credential session-scoped: every promoted session is reachable only with a fresh
opaque secret that the *remote* provisioned for that session — never a source-valid user credential
reused from the client — and that secret is disposed when the session is demoted.

1. `cargo test --workspace` is green with no network access and no environment variables set;
   `cargo clippy --workspace --all-targets` and `cargo fmt --all --check` are clean. The only
   pre-existing failure, `otto-mcp-lsp`'s rust-analyzer round-trip, is unaffected.
2. Every `Promoted` frame the server sends carries a non-empty `token` (a fresh 32-hex mint), for
   **all four** targets — loopback, vps, microvm, and fly — asserted by tests that read the frame
   off the socket. A `Promoted` frame with a `null`/missing token no longer deserializes.
3. A `--promotion-receiver` receiver that has accepted a session refuses a WS attach, an `/export`,
   and a `/workspace` call that present the machine-wide promotion secret, and accepts them with the
   session's own secret — asserted with distinct values so the two cannot coincide by accident.
4. A successful `/export` (the demote pull) disposes the session's secret on the receiver: a second
   `/export` or a WS attach with the same secret fails afterwards. The receiver's retained copy of
   the session still exists (copy semantics preserved — asserted against the store).
5. `accept_demotion` refuses a bundle whose `owner` differs from the local row's current owner, with
   `AcceptError::Refused` (asserted).
6. `POST /promote` with a pre-ownership bundle (a hand-built body whose `session` object lacks
   `owner`) returns the actionable message naming the pre-multitenancy break, not a bare
   `missing field 'owner'` (asserted byte-for-byte).
7. `mint_session_secret` is the single mint used by all four targets; fly's `mint_token` is gone
   (asserted by `cargo test -p otto-remote` and a grep-free review).
8. `ui-dioxus` compiles (`cargo build --target wasm32-unknown-unknown --features web`) and its
   desktop suite passes (`cd ui-dioxus && cargo test --features desktop`); the `Promoted` arm adopts
   the token.

---

## Assumptions

Every choice made without asking, with its rationale.

| # | Assumption | Rationale |
|---|---|---|
| A1 | The per-session secret is minted by the **source's** `RemoteTarget` (the pusher), not by the receiver. | The issue: "Every `RemoteTarget` mints a fresh opaque per-session secret, delivered over the provisioning channel... never through the client." The pusher mints, delivers over the channel, and relays to the client via `Promoted.token`. The receiver is a passive record-keeper (`session → secret`). This also keeps the receiver's `/promote` a pure restore+record and needs no response-side mint round-trip. |
| A2 | The `X-Otto-Session-Secret` header is **required** on `/promote`. A pusher that omits it gets a 400. | No installed base (Decision 3), so every pusher is rebuilt in lockstep and sends it. Fail-closed: a session restored without a recorded secret would be unreachable yet exist — worse than refusing the push. |
| A3 | For Fly/microVM (one session per machine), the machine's own promotion secret **is** the session secret: the minted token is injected as `OTTO_PROMOTION_SECRET` and also sent as the session-secret header on the push. | Ephemeral single-session machines need no session→secret *map*; the machine secret and the session secret coincide by construction. The receiver still records `session → token` for uniform checking. Fly already does exactly this (`fly.rs:238`, env injection at `:198`); microVM's `FirecrackerProvisioner` gets the freshly-minted token the same way it gets `cfg.token` today. |
| A4 | `/export` **disposes** the session's secret on success; the source-side demote arms additionally drop the stored handle. | "Secrets disposed on demote." The `/export` pull is the demote primitive, so completing it consumes the credential. Narrow edge case: a demote whose local restore fails after a successful pull cannot retry (the receiver's secret is gone) — the receiver retains the session row (copy semantics) but unreachable; documented in §7 with the operator remedy. |
| A5 | The `Machine`-mode WS handshake requires `?session=` **before** authentication, so it can look up the secret to verify. | A receiver hosts only promoted sessions and rejects session creation (§7.2 step 4 of the identity spec), so there is never a legitimate Machine connection without a `?session=`. Binding the credential check to the named session is the whole granularity win. |
| A6 | `/workspace` on a `Machine` receiver accepts the machine secret **or** any live session secret (membership). | The RPC is machine-scoped with no session id in the request (identity-spec §2). A promoted client reconnects with its session secret, so membership is required; keeping the machine secret preserves the operator path and the existing harnesses. Both are constant-time compared. |
| A7 | The per-session secret is a plain in-memory `String` in a `Mutex<HashMap<SessionId, String>>` on `ServeState`, never persisted, never logged. | The receiver's map is transient transport state, like the existing `remotes` map. Persisting it is out of scope (see Scope **Out**). No `Debug`/log path formats the map. |
| A8 | `Promoted.token` changes type to `String` (breaking, `protocol` bump) rather than staying `Option<String>` and always serializing `Some`. | "Becomes always `Some`" is a type-level invariant, not a convention — the repo's standing preference ("enforced by type rather than convention", e.g. `SeamError`'s private constructor). An `Option` left in place invites a future contributor to re-introduce the `None` reuse-signal. The break is sanctioned by Decision 3 (clients rebuilt in lockstep) and is semver-major per Rule 6. |

---

## 1. `remote` — the shared mint and the per-session push

### 1.1 `mint_session_secret`

`mint_token()` (`crates/remote/src/fly.rs:34-37`) becomes the crate-wide mint, moved to
`crates/remote/src/lib.rs`:

```rust
/// A fresh 32-hex opaque per-session credential. Blast radius of a leak is one promoted session.
pub fn mint_session_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
```

`pub` (re-exported from `otto-engine`) so `LoopbackTarget` (which lives in `engine`) uses the same
mint. Fly's `pub(crate) fn mint_token` is deleted; its callers and tests use the shared function.

### 1.2 `push_promote_bundle` carries the session secret

```rust
pub(crate) async fn push_promote_bundle(
    endpoint: &str,
    bearer: &str,          // the receiver's machine-wide admission secret
    session_secret: &str,  // the fresh per-session secret (X-Otto-Session-Secret header)
    bundle: &PromoteBundle,
) -> anyhow::Result<()>
```

The request sends `Authorization: Bearer <bearer>` (unchanged — `/promote` is admission-authenticated
by the machine secret) plus `X-Otto-Session-Secret: <session_secret>`. Callers:

- **`FlyTarget::provision`** — `let token = mint_session_secret();` (as today), injects it as
  `OTTO_PROMOTION_SECRET` in the machine env, and pushes with `bearer = token`,
  `session_secret = token` (A3). Handle token = `token`.
- **`VpsTarget::provision`** — mints `let secret = mint_session_secret();`, pushes with
  `bearer = self.token` (the receiver's machine secret, the pusher's admission credential) and
  `session_secret = secret`, returns `RemoteHandle::new(endpoint, secret)`.
- **`MicrovmTarget::provision`** — the provisioner returns `ProvisionedMachine { token }` (the
  freshly-minted secret booted into the machine); pushes with `bearer = machine.token` and
  `session_secret = machine.token` (A3); handle token = `machine.token`.

### 1.3 `VpsTarget` changes

- `provision` mints + pushes per §1.2 (the issue's premise-correction-2 shape).
- `export` is **removed** (premise correction 2). The demote pull is the shared `export_bundle`
  everywhere. Breaking `remote` API change (§8).

### 1.4 `LoopbackTarget` (in `engine`)

`provision` mints a fresh secret with `otto_remote::mint_session_secret()` and returns it in the
handle. The provisioned engine is `SingleUser` (identity-spec §6.5) and ignores credentials, so the
secret is a uniform invariant, not a functional credential. The loopback engine's own
`PromoteConfig`/`accept_promotions` wiring is unchanged.

### 1.5 `export_bundle`

Signature unchanged (`export_bundle(endpoint, token, session)`); callers now pass the **session
secret** (from the stored handle) instead of the machine-wide token.

---

## 2. `protocol` — `Promoted.token` is a required `String`

```rust
Promoted {
    session: SessionId,
    endpoint: String,
    token: String,   // was Option<String>; always present now (A8)
}
```

- The `#[serde(default)]` attribute stays (an absent token — from a hypothetical pre-slice peer —
  defaults to the empty string, which fails every check), but a serialized `null` no longer
  deserializes: `"token": null` → serde type error. This is the deliberate break.
- `Debug` redacts `token` (already does).
- Tests: `handover_server_messages_round_trip` / `promoted_with_token_round_trips` /
  `server_message_debug_redacts_promoted_token` construct `token: String` now; a new test asserts a
  `null` token fails to deserialize.
- Version bump `protocol` 0.1.0 → 0.2.0 (Rule 6, flagged to the architect reviewer). `ui-dioxus`
  depends on `protocol` — its `Cargo.toml` dependency follows the bump (workspace-excluded, so the
  workspace build is unaffected by the version change, but the desktop suite pins it).

---

## 3. `engine/serve.rs` — the receiver's session→secret map

### 3.1 `ServeState.session_secrets`

```rust
session_secrets: Mutex<HashMap<SessionId, String>>,
```

Helper methods (all constant-time where they compare):

```rust
fn record_session_secret(&self, session: SessionId, secret: String);   // /promote success
fn session_secret(&self, session: SessionId) -> Option<String>;        // /export + WS attach
fn dispose_session_secret(&self, session: SessionId);                  // /export success
fn machine_workspace_authorized(&self, headers: &HeaderMap) -> bool;   // /workspace
```

`machine_workspace_authorized` = `authorized(headers, promotion_secret)` OR a constant-time match
against **any** live session secret in the map (A6).

### 3.2 `promote_handler`

1. `403` unless `--accept-promotions`; `401` unless the machine secret matches (unchanged).
2. Deserialize `PromoteBundle`. On failure, **special-case the pre-ownership shape**: parse the raw
   body as `serde_json::Value`, and if `session.owner` is absent, return `400` with the actionable
   message (mirroring slice 1a's §4.1, adapted for the wire): *"promote bundle predates session
   ownership (issue #115): its session carries no owner. otto has no installed base, so there is no
   migration — re-promote from a current otto."* Any other deserialize failure keeps the existing
   `bad request: {e}` 400 (premise correction 1 / success criterion 6).
3. Require the `X-Otto-Session-Secret` header (`400` if absent — A2).
4. `accept_promotion` as today; on `Ok(session)`, `record_session_secret(session, header_secret)`.

### 3.3 `export_handler`

1. `403` unless `--accept-promotions` (unchanged).
2. Parse `{ session }`; look up `session_secret(session)`; `401` if absent (never promoted here, or
   already disposed) or if the bearer does not match it (constant-time). **The machine-wide secret
   no longer authorizes an export** (success criterion 3).
3. `export_promotion(session)` as today (`404` on unknown).
4. On success, `dispose_session_secret(session)` **before** returning the bundle (A4) — demote
   consumes the credential.

### 3.4 `/workspace` — `Machine` arm

`authorized(headers, promotion_secret)` → `state.machine_workspace_authorized(&headers)` (A6).

### 3.5 `Machine`-mode WS handshake

`authenticate_connection`'s `Machine` arm and `handshake_frame`'s `Attach` handling both change from
verifying against `state.auth.promotion_secret` to verifying against the **session's** per-session
secret (A5):

```rust
AuthMode::Machine => {
    // The credential is the per-session secret for the session named in ?session=. A receiver
    // creates no sessions, so a Machine connection without a ?session= has no secret to check.
    let Some(session) = params.session.as_ref().and_then(|s| uuid::Uuid::parse_str(s).ok()) else {
        // opaque AUTH_FAILED + close
    };
    let Some(secret) = state.session_secret(SessionId(session)) else {
        // opaque AUTH_FAILED + close  (never promoted here, or demoted/disposed)
    };
    if authorized(headers, Some(&secret)) {
        return Some(ConnIdentity { owner: UserId::local(), access_token: None });
    }
    handshake_frame(reader, writer, state, Some(&secret)).await
}
```

`handshake_frame` gains the expected Machine secret (an `Option<&str>`, `None` on the `Users` path,
which is unchanged); its `Attach` arm for `Machine` compares against that secret
(`secret_matches(Some(&secret), &token)`), not the promotion secret. `ConnIdentity.owner` stays the
`UserId::local()` placeholder and `resolve_session` adopts the attached session's owner as before —
unchanged.

---

## 4. `engine/service.rs` — `accept_demotion` owner match

`accept_demotion(expected, bundle)` currently refuses a bundle whose id ≠ `expected`. Add the
carried-over second half: after the id check, refuse a bundle whose owner differs from the local
row's current owner:

```rust
if id != expected { /* existing Refused */ }
let current_owner = self.store.owner_of(expected).await.map_err(AcceptError::Failed)?;
if bundle.session.owner != current_owner {
    return Err(AcceptError::Refused(format!(
        "demotion bundle is owned by {}, but the local copy of {} is owned by {}",
        bundle.session.owner.as_str(), expected.0, current_owner.as_str(),
    )));
}
```

`restore_over` overwrites the local row *including its owner*; the id-binding already prevents a
receiver from answering an export for X with a bundle for Y, and this closes the residual path
where the bundle names X but carries a tampered/different owner. `owner_of` is the reverse-existence
oracle, so its result is never returned verbatim — the refusal text re-derives both ids from the
bundle and the known-good expected id (success criterion 5).

---

## 5. `workspace/remote.rs` — `RemoteWorkspace` construction contract

`RemoteWorkspace::new` gains a doc comment recording the constraint (premise correction 5 / A13):

> `RemoteWorkspace` is **source/client-side only**. It is constructed by a client (or a source
> engine acting for a client) to reach a served engine over the `/workspace` RPC. It must never be
> constructed on a promoted machine: a remote that can reach yet another machine's workspace, or
> that holds a source-valid credential, is a pivot. It has no production construction site on a
> promoted machine; keep it that way.

No structural change — there is no production construction site to remove, and the slice adds none.

---

## 6. `ui-dioxus` — adopt the promoted token

`app.rs`'s `Promoted` arm (`:199-201`) currently ignores the token:

```rust
SocketEvent::Message(Ok(ServerMessage::Promoted { endpoint, token, .. })) => {
    token.set(token);                 // switch the credential before the reconnect
    reconnect_to.set(Some(endpoint));
}
```

The reconnect (`do_connect`) and the `/workspace` RPCs (`load_files`/`open_path`) read the `token`
signal, so adopting it here is what lets a post-promote client present the per-session secret to the
remote. Under the desktop sidecar (loopback → `SingleUser` engine) the token is minted but ignored,
so behavior there is unchanged. `Demoted` reconnects to the source, whose credential is the
connection's own — slice 2 owns the full credential lifecycle; this slice only stops discarding the
handover secret.

---

## 7. Error Handling & Edge Cases

| Case | Behavior |
|---|---|
| `/promote` from a pre-ownership source | `400` with the actionable §3.2 message (not `missing field 'owner'`). |
| `/promote` without the session-secret header | `400` (A2). |
| `/promote` with the wrong machine secret | `401`, unchanged. |
| `/export` for a session never promoted here | `401` (no recorded secret) — indistinguishable from a disposed one. |
| `/export` with the machine secret | `401` — the machine secret is admission-only now (success criterion 3). |
| `/export` of a disposed secret (post-demote) | `401`. The receiver's retained copy still exists (copy semantics), unreachable. |
| WS `Machine` attach without `?session=` | Opaque `authentication failed`, closed (A5). |
| WS `Machine` attach with a wrong/foreign secret | Same opaque failure (the map lookup fails or the compare fails) — never an existence oracle. |
| WS `Machine` attach with the machine secret | `401` — the machine secret is no longer a session credential (success criterion 3). |
| `/workspace` on `Machine` with any live session secret | Accepted (A6). |
| Demote restore fails after a successful pull | Source reports the error; the receiver's secret is already consumed (A4). The session stays local-and-stale on the source; the receiver retains an unreachable copy. Operator remedy: re-promote the source's copy under a fresh secret is blocked by `AlreadyExists` on the receiver, so the receiver's stale row must be cleared first — documented in the PR body. |
| `accept_demotion` owner mismatch | `AcceptError::Refused` (400), nothing written. |
| Receiver restart (long-lived VPS) | The in-memory map is lost; promoted sessions are unreachable until re-promoted (Scope **Out** — persistence of the map is a follow-up). |
| `Promoted.token` serialized as `null` (hypothetical old peer) | Deserialization error in a new client — the deliberate break (A8). |

---

## 8. Semver

Two breaking changes, both semver-major (the 0.x minor-position bump) and both named here for the
architect reviewer:

1. **`protocol` 0.1.0 → 0.2.0** — `ServerMessage::Promoted.token: Option<String>` → `String` (§2).
2. **`remote` 0.1.0 → 0.2.0** — `VpsTarget::export` removed (premise correction 2); `remote` also
   gains the additive `mint_session_secret` and the (internal) `push_promote_bundle` header.

`engine` re-exports the changed `remote`/`protocol` types but its own API surface is unchanged
(`serve::app`, `LoopbackTarget::new`, etc. keep their signatures), so `engine` itself does not bump.
The `ui-dioxus` `protocol` dependency follows the bump.

---

## 9. Testing

- **`protocol`** — `Promoted { token: String }` round-trips; a `null`/missing `token` fails to
  deserialize; `Debug` redacts it. Existing `Promoted` tests updated to the `String` shape.
- **`remote`** — `mint_session_secret` is 32-hex and unique; `push_promote_bundle` sends the
  `X-Otto-Session-Secret` header (wiremock `header` matcher); `VpsTarget::provision` mints a fresh
  secret distinct from the machine secret and pushes with bearer=cfg.token +
  header=session_secret; `FlyTarget` wiremock path passes the minted token as both bearer and
  header; fly's `mint_token` test becomes `mint_session_secret`'s.
- **`engine` (serve)** — the session→secret map: `/promote` records it; `/export` requires the
  session secret (machine secret → 401) and disposes it (second export → 401, retained copy still in
  the store); `/workspace` `Machine` accepts a live session secret and the machine secret; WS
  `Machine` attach requires the session's secret (`?session=` present, wrong secret → opaque
  failure, machine secret → 401); `Promoted.token` is non-empty for loopback (via
  `tests/serve.rs`'s promote test) and vps (via `tests/vps_promote.rs`).
- **`engine` (service)** — `accept_demotion` refuses an owner-mismatched bundle and accepts a
  matching one; the refusal is `AcceptError::Refused`.
- **`engine` (integration)** — `tests/vps_promote.rs`, `tests/microvm.rs`, `tests/promote.rs` updated
  to the per-session model (pushers send the header; pulls use the handle's secret; the machine
  secret no longer authorizes export/attach). `tests/remote_workspace.rs` unchanged (no `Machine`
  receiver there).
- **`ui-dioxus`** — `app.rs` `Promoted` arm adopts the token; desktop suite green; wasm compile
  check green.

### 9.1 The push header reaches the harnesses

All four test harnesses build `serve_app` directly. The push-side harnesses (`vps_promote.rs`,
`microvm.rs`) gain a session-secret constant distinct from `TOKEN` where the test must prove the two
credentials differ, or reuse `TOKEN` where the machine secret and session secret coincide (the
single-session microVM case). The `post_promote`/`post_export` helpers send/require the header and
the session-secret bearer respectively.

---

## 10. Risks & Open Questions

1. **The `/export`-consumes-the-secret design strands a demote whose restore fails.** A4 accepts the
   edge case; the operator remedy (clear the receiver's stale row) is documented but adds an
   operational step. A follow-up could add a source-confirmed release step (a demote-complete RPC)
   if the flow ever needs to be retry-proof.
2. **The in-memory session→secret map does not survive a receiver restart.** Fly/microVM die with
   their machine; a long-lived VPS receiver that restarts orphans its promoted sessions' credentials.
   Persisting the map (or keying it off a durable secret column) is the natural follow-up.
3. **`Promoted.token` as a required `String` breaks any peer that still serializes `None`.** There is
   no such peer (lockstep, no installed base), and the break is explicit + version-bumped. Any
   future out-of-band client must be rebuilt against `protocol` 0.2.0.
4. **`/workspace` accepting any session secret is a widening of who can reach the machine's
   workspace RPC** relative to "exactly the operator's machine secret" — but `/workspace` was never
   session-isolated (identity-spec §2), and the set of session-secret holders is a strict subset of
   "machines that successfully promoted onto this receiver". Stated so it is not mistaken for a
   tenant boundary.
5. **The `Machine`-mode WS handshake now depends on `?session=` being present and mapped** — a
   behavior tightening over identity-spec §6.5's "promotion secret via header or Attach". No
   legitimate Machine connection exists without a `?session=` (receivers create no sessions), so
   nothing shipped breaks.
