# Multitenant identity — the `Authenticator` seam, TOTP + JWT, and session ownership

> **Status:** DRAFT — being implemented as slice 1b (issue #125); open issues below are resolved.
> **Implements:** the identity half of the "Suggested first slice" of
> [#115](https://github.com/robhicks/otto/issues/115).
> **Depends on:** `docs/superpowers/specs/2026-08-01-session-ownership-design.md` (slice 1a —
> session ownership), **now shipped** in [#123](https://github.com/robhicks/otto/pull/123). It carried
> this spec's §3.1, §4 and §7.1. It also shipped the ownership check on the WS
> `PromoteToRemote`/`DemoteToLocal` arm — which the *ownership* spec's §3.4 had assigned here, not
> the open-issue list below, none of whose five items concerned handover. `POST /promote`,
> `POST /export`, and attach-time `?session=` ownership all land in this slice.
> **Blocks:** slice 2 (UI slash commands + `otto login`/`logout` CLI).
> **Entangled with:** slice 3 (handover credentials) — resolved for this slice by §6.5's
> `--promotion-receiver` (explicit opt-in + zero-principal condition), which slice 3 narrows to
> per-session secrets.

## Why the machine-to-machine credential is scoped here

Three rounds of design review (recorded in the prior version of this file) found that a **machine-wide
promote-receiver credential** could not be made safe as a *default*. The resolution, per issue #125's
open issue 1, is to make the machine mode an **explicit opt-in** and to **condition it on zero
enrolled principals** (§6.5). This slice ships three explicit modes — `--single-user`, the default
`Users`, and `--promotion-receiver` — rather than letting `--accept-promotions` silently imply a
machine-wide root credential. Slice 3 narrows the receiver credential to per-session secrets.

**What slice 1a already shipped** (the ownership foundation this spec's §3.1/§4/§7.1 describe):
the schema, the choke points, and the `UserId` type — with no behavior change and no credential.
This slice builds the identity half on top of it.

## Open issues — resolutions (issue #125)

Each open issue from the third review round, with the resolution this implementation follows:

1. **§6.5 / §2 — `Machine` mode → explicit opt-in, zero-principal condition.** `AuthMode::Machine`
   is selected by a new `--promotion-receiver` flag (requires `--accept-promotions`); it is never
   implied by `--accept-promotions` alone. A `Machine` server **refuses to start if any principal
   is enrolled** — a receiver with real users cannot double as a machine-credential host, which
   closes the "attach to every session regardless of owner" hole at the mode boundary rather than
   relying on discipline inside it. §2's "Not secured" paragraph now names it explicitly. The Fly
   guest and VPS/microVM receivers adopt `--promotion-receiver` (§6.4, §6.5); `Promoted.token` keeps
   its existing shape (`None` for loopback/vps/microvm, `Some` for Fly's fresh per-session mint),
   and slice 3 generalizes it.
2. **§7.3 — `/workspace` per-mode credential, stated.** `SingleUser`: **no credential** — the mode
   is loopback-bound with a single principal and mints nothing, so the route ignores
   `Authorization` entirely (the desktop sidecar sends an empty header; the server does not consult
   it). `Users`: a valid access token (JWT), verified exactly like the WS path. `Machine`: the
   promotion secret, constant-time. This is the per-mode rule §7.3 now states.
3. **§7.2 — `Hello` is always the first frame, in every mode.** `Hello { auth_mode }` is sent
   immediately on upgrade regardless of mode and regardless of whether an `Authorization: Bearer`
   header pre-resolved a principal. A pre-resolved header skips the *deadline*, not the greeting.
   `SingleUser`/`Machine` then proceed straight to `Ready` (Machine after its `Attach`-with-secret
   frame, §6.5); `Users` enters the 10-second `Login`/`Attach` deadline. Uniform first-frame ordering
   is what slice 2's UI depends on.
4. **§6.6 — `ui-dioxus` table widened to cover the exhaustive match and the empty token.**
   `app.rs:129-214` matches `ServerMessage` exhaustively, so the three new frames are added there
   (`Hello` records the mode; `LoggedIn`/`LoggedOut` update an auth signal, wired by slice 2). The
   `tok.is_empty()` gates at `app.rs:93` (`do_connect`), `:346` (`load_files`), and `:368`
   (`open_path`) are removed — under `SingleUser` the token is empty by design, and the guards
   become connection-state-based. Success criterion 7 is widened to exercise `/workspace` (the file
   tree must load under the desktop sidecar).
5. **§6.4 — Fly citation corrected.** The rename touches `create_machine_body`'s `env` map at
   `crates/remote/src/fly.rs:197-201` (`OTTO_TOKEN` → `OTTO_PROMOTION_SECRET`) and the Dockerfile's
   comment at `:51-52`; the Dockerfile `CMD` at `:57` gains `--promotion-receiver`. There is no
   `OTTO_TOKEN` `ENV` line to rename.

Two smaller corrections, adopted: §6.5's loopback predicate runs on the **resolved** bind host
(unset `OTTO_HOST` defaults to `"127.0.0.1"`, `main.rs:820`, and must pass `is_loopback()`); and
`AuthMode` is threaded through `LoopbackTarget::new` (`loopback.rs:31`) and `serve::app`
(`loopback.rs:84`) so a `--single-user` loopback promote provisions a `SingleUser` engine.

otto is single-tenant today: one shared `OTTO_TOKEN` is a root credential for the whole machine,
nothing in the protocol carries an identity, and `sessions` has no owner. This slice introduces a
real principal, makes sessions owned, and adds the `Login`/`Logout` protocol vocabulary.

---

## Premise corrections

Five of the issue's premises do not survive contact with the repository. They are corrected here;
each one changes the design.

1. **There is no migration mechanism in `persistence`, so adding a `NOT NULL` column is not a
   no-op — it is a latent runtime failure.** `SqliteStore::open` calls `init_schema`
   (`crates/persistence/src/sqlite.rs:27`), which issues three bare
   `CREATE TABLE IF NOT EXISTS` statements (`sqlite.rs:31-67`). There is no migrations directory,
   no `user_version` use, and no schema probe. Against a database file that already exists, `IF NOT
   EXISTS` silently keeps the **old** table shape, and every subsequent query naming `owner` fails
   at runtime with a confusing sqlx column error. Issue Decision 3 ("existing local dev sqlite files
   are simply discarded") is the right *policy*, but discarding has to be **enforced and
   diagnosed**, not assumed. **Closed by §4.1** — a `PRAGMA user_version` guard that fails closed
   with an actionable message.

2. **`/workspace` cannot be isolated per tenant in this slice, because it has no session and the
   engine has one process-global root.** The issue asks that `POST /workspace` be "scoped to the
   session's owner and its root". But `WorkspaceRequest` carries no session id
   (`crates/protocol/src/lib.rs:180-185`), `EngineService::workspace_rpc`
   (`EngineService::workspace_rpc`, `crates/engine/src/service.rs`) operates on the single `Arc<dyn Workspace>` the service
   was constructed with, and `otto serve` resolves exactly one `--root` for the process
   (`crates/engine/src/main.rs:638`). Every session on a served engine therefore shares one
   directory. Adding a session field to the RPC would scope the *call* without isolating the
   *data* — security theatre. **This slice authenticates `/workspace`; it does not isolate it**
   (§6.3), and open question 1 is answered in §10: per-owner workspace roots are their own slice.
   The honest consequence is stated in §2 and must not be softened: **slice 1 does not make otto
   safe to run as a shared multi-tenant service.**

3. **Removing `OTTO_TOKEN` outright would break the handover axis, which this slice does not
   otherwise touch.** `PromoteConfig.token` (`crates/remote/src/lib.rs:33-37`) is threaded into
   `push_promote_bundle` (`remote/src/lib.rs:232-251`) and `export_bundle` (`:255-275`), both of
   which `bearer_auth` against the receiver's `ServeState.token`. Deleting the shared secret before
   slice 3 mints per-session ones would leave `/promote` and `/export` with no credential at all and
   break `tests/vps_promote.rs`, `tests/microvm.rs`, `tests/promote.rs`, and the Fly path. Decision 3
   says `OTTO_TOKEN` is removed **as an identity** — which is exactly what §6.4 does: it stops being
   accepted on any user-facing route, is renamed to name what it actually is, and survives only as
   the machine-to-machine promotion credential that slice 3 replaces with per-session secrets.

4. **The two protocol enums use different serde representations, so new variants cannot be written
   uniformly.** `Command` and `EventKind` carry no serde attributes at all and are therefore
   **externally tagged** (`{"SendPrompt":{…}}` — asserted at `protocol/src/lib.rs:324-325`), while
   `ServerMessage` alone is **internally tagged** with `#[serde(tag = "type", rename_all =
   "snake_case")]` (`protocol/src/lib.rs:143-147`, tags asserted at `:232`). New commands follow the
   first convention; the new reply frames follow the second.

5. **`FlyTarget`'s per-session mint is at `fly.rs:34-37`, not `:198`.** The issue cites
   `crates/remote/src/fly.rs:198`, which is where the already-minted token is *injected* into the
   machine's `env`; `mint_token()` itself (a 32-hex `Uuid::new_v4().simple()`) is at `fly.rs:34-37`.
   Slice 3 generalizes that function; noting the real location so slice 3 does not hunt for it.

Two smaller findings, folded into the design rather than listed as corrections: `authorized()`
compares credentials with `==` on `Option<&str>` (`crates/engine/src/serve.rs:73-79`), which is not
constant-time (§6.1); and `resolve_session` (in `serve.rs`) parses a client-supplied
`?session=` uuid and attaches with **no ownership check whatsoever** (§6.2), which is the specific
hole issue #115 opens with.

---

## Scope

**In:**

- The `Authenticator` trait seam + `Principal` in `engine-core`; `UserId`/`Credentials` in `protocol`.
- A new `otto-auth` crate: `TotpAuthenticator` (RFC 6238), JWT minting/verification (HS256),
  refresh-token rotation, the `jti` denylist, and the sqlite-backed store holding all of it.
- Session ownership: `sessions.owner` (`NOT NULL`), `SessionState.owner`, a principal on
  `SessionStore::create_session`, owner-scoped `replay_since`/`session_status`/`snapshot`, and
  `owner_of`.
- Principal resolution on every `serve.rs` route, including a post-upgrade WS authentication
  handshake that replaces the `?token=` query parameter.
- `Command::Login`/`Attach`/`Refresh`/`Logout` and `ServerMessage::LoggedIn`/`LoggedOut`.
- Ownership checks on session **attach** (`resolve_session`'s explicit `?session=` arm). Replay,
  abort, the WS handover commands, and the `POST /promote` / `POST /export` machine credential all
  already shipped in slice 1a — do not re-implement them, and do **not** add an owner check to
  `/promote`/`/export`: those routes are machine-credentialed with no connected principal, and the
  user-facing ownership check for handover lives on the source's WS command loop where a principal
  exists (see §7.1's tautology warning and §7.3's per-mode table).
- `otto auth enroll <user>` — the out-of-band bootstrap that provisions the first principal.
- `otto serve --single-user` — the loopback-only, unauthenticated single-principal mode the desktop
  sidecar runs in (§6.5), plus the two `ui-dioxus/` line changes that keep the shipped desktop app
  working across this slice (§6.6).

**Out** (each with the slice that owns it):

- UI login flow, slash-command dispatcher, signed-in-as / sign-out affordance — **slice 2**.
- `otto login` / `otto logout` CLI for remote servers — **slice 2**.
- `Promoted.token` always `Some`, per-session secrets on every `RemoteTarget`, the receiver's
  session→secret map — **slice 3**.
- **Per-owner workspace roots** — its own slice (§10, open question 1). This is the reason slice 1
  is not sufficient for a shared deployment.
- Audience-scoped (`aud`) JWTs for remote policy enforcement — reserved by the issue, deliberately
  not built (§5.4).
- OIDC / GitHub OAuth device flow — later backends behind the same seam.
- Any change to the permission gate's sensitive-path floor, the sandbox posture, or the
  `bash`-only-when-sandboxed rule. **Explicit non-goal**, and §7 explains how the auth store stays
  out of the workspace without touching the floor.
- Multi-machine tenant scheduling, quotas, billing.

---

## Goal & Success Criteria

Give otto a first-class user identity that is established explicitly by a client, carried on every
request, and enforced as the owner of every session — so that possession of a credential grants
access to *that principal's* sessions rather than to the machine — while leaving the offline
single-user `otto run` path byte-for-byte unchanged.

1. `cargo test --workspace` is green with no network access and no environment variables set;
   `cargo clippy --workspace --all-targets` and `cargo fmt --all --check` are clean.
2. A WS connection that sends no authentication frame within the handshake deadline, or sends bad
   credentials, receives an error and is closed — with **no session created and no event replayed**
   (asserted by a test that inspects the store afterwards).
3. Attaching to, replaying, aborting, or promoting a `SessionId` owned by a different principal
   fails with a message **byte-for-byte identical** to the one for a nonexistent id (asserted by a
   test comparing the two strings), so the API is not an existence oracle.
4. An access token whose `jti` has been denylisted by `Logout` is rejected on every route for the
   remainder of its `exp`, and the denylist row is gone once pruned past `exp` (both asserted).
5. RFC 6238's published test vectors pass; a consumed time-step is rejected on reuse; ±1 step is
   accepted and ±2 rejected; and N consecutive failures lock the user out for the cooldown window.
6. `otto run` with no environment set produces the same event sequence as before this change, and
   `crates/engine/src/lib.rs`'s determinism tests are untouched.
7. `otto serve --single-user` refuses to bind a non-loopback host and refuses `--accept-promotions`
   / non-loopback `--promote-*` (asserted), and the desktop app still launches its sidecar, reaches
   `Ready`, and **loads the file tree over `/workspace`** — verified out-of-band via
   `cd ui-dioxus && cargo test --features desktop`, since `ui-dioxus/` is workspace-excluded and
   `cargo test --workspace` structurally cannot catch its breakage. The `Hello`/`LoggedIn`/`LoggedOut`
   arms are exercised by a desktop-suite unit test over the new `ServerMessage` variants, so the
   exhaustive match in `app.rs` is proven to compile and handle them.

---

## Assumptions

Every choice made without asking, with its rationale.

| # | Assumption | Rationale |
|---|---|---|
| A1 | **This PR implements slice 1 only**; slices 2 and 3 become follow-up issues and #115 stays open. | The issue itself proposes the split and orders it. One PR containing a new crate, a protocol break, a schema change, serve rewiring, a UI dispatcher, a CLI, and per-session handover secrets would be unreviewable. |
| A2 | The single-user CLI path (`otto run`) is owned by a reserved built-in principal, `UserId::local()` == `"local"`, and never authenticates. | Non-goal: the offline deterministic path must be unchanged. A reserved id is simpler than making the owner column nullable, which Decision 3 forbids. `"local"` is rejected by `otto auth enroll`, so it can never collide with a real user. |
| A3 | JWTs are **HS256** with the signing key generated on first use and stored in the auth database, not supplied by an environment variable. | Decision 4 establishes that the key never leaves the source, so symmetric is sufficient. Generating-and-persisting removes an operator step and a class of "weak secret in env" failures. `kid` is carried so rotation is possible without invalidating every live token. |
| A4 | TOTP is built from RustCrypto `hmac` + `sha1` with the RFC 6238 step function written here, rather than a TOTP crate. | No cryptographic primitive is hand-rolled — HMAC-SHA1 comes from a vetted crate. Only the counter/truncation arithmetic is ours (~20 lines), and RFC 6238 publishes test vectors for exactly that. Avoids a heavier dependency whose QR/serde features we do not want. |
| A5 | Auth state lives in its own sqlite database in the **OS data directory** (`dirs::data_dir()/otto/auth.db`), overridable by `OTTO_AUTH_DB`, never in the workspace. | TOTP secrets, the signing key, and refresh-token hashes are credentials. `open_db_path()` defaults the *session* store to `./otto-sessions.db` — i.e. inside the workspace — where the sensitive-path floor would not cover it. Placing auth state outside the root avoids widening the floor, which is an explicit non-goal. Follows the precedent of `retrieval`'s OS-cache-dir index. |
| A6 | Refresh tokens are **in** slice 1, not deferred. | Decision 2 lists short-TTL access tokens plus refresh rotation as something to "design for, not discover later". Without refresh, the access TTL is either short (unusable) or long (an unbounded denylist) — and retrofitting rotation is the exact failure the decision warns about. |
| A7 | Access TTL 15 minutes; refresh TTL 30 days, single-use with rotation; TOTP lockout after 5 failures within 15 minutes. | Ordinary, defensible defaults. All are constants in one module so they are trivially tuned. |
| A8 | The WS handshake accepts credentials **after** upgrade (a `Login`/`Attach` frame) and additionally honours an `Authorization: Bearer` header at upgrade time for non-browser clients. `?token=` is deleted. | The issue requires the credential out of the query string. A browser `WebSocket` cannot set headers, so a post-upgrade frame is the only option that serves both clients from one path. |
| A9 | `EngineService`'s session-bearing methods take an explicit `&UserId` rather than a type-level `AuthorizedSession` token. | Simplest change that is still fail-closed at one choke point. The typed alternative (a `SessionRef` constructible only by `authorize()`) is stronger but a much larger refactor across ~45 existing test call sites; recorded here as the natural follow-up if ownership checks ever get missed. |

> **A9's trigger condition has now fired — this slice re-weighs it.** A9 defers the typed
> `SessionRef` alternative as "the natural follow-up **if ownership checks ever get missed**". One
> was: `serve.rs`'s handover arm reaches `otto_remote::promote` through the `store()` accessor, so it
> bypassed `EngineService` by construction and shipped unchecked until review caught it (see the
> ownership spec's §3.4). The fix was a `pub authorize_session` wrapper — i.e. the invariant is held
> by convention plus a doc comment, not by the type system. This slice adds three more hand-placed
> call sites with the same bypass-by-construction shape (attach, `POST /promote`, `POST /export`).
> **Decision: the full typed `SessionRef` refactor stays out of scope, but the cheap half of the
> recommendation is taken** — `resolve_session` now returns `(SessionId, UserId)` and `handle_socket`
> threads one connection-scoped principal through the loop (replacing the hard-coded
> `UserId::local()` constructions at serve.rs:519, :576, :637, :656, :680, :696, :722, :1098). That
> removes the "ambient authority constructed at every call site" failure mode; the residual
> convention-held invariant is the three new RPCs (`attach`, `POST /promote`, `POST /export`), each a
> single `authorize_session` call documented the same way the handover arm's is.
| A10 | ~~`CapabilitiesManifest` gains `auth_required: bool`.~~ **Withdrawn** — replaced by a pre-auth `ServerMessage::Hello { auth_mode }` frame (§3.5). | The manifest rides only on `Ready`, which §7.2 sends *after* authentication, so the flag would have been unreadable at the one moment a client needs it. A dedicated pre-auth frame also gives `SingleUser`/`Machine` clients a defined greeting, which the field could not. |
| A11 | Failed authentication returns one opaque message (`"authentication failed"`) to the client regardless of cause; the specific reason is logged server-side only. | An error distinguishing "unknown user" from "bad code" from "locked out" is an enumeration oracle. |
| A12 | A **`--single-user` mode** is added, and the desktop sidecar uses it, rather than accepting a dead UI between slices. | Without it slice 1 ships a broken application: `ui-dioxus/src/desktop_boot.rs:151` mints a secret and `:241` passes it as `OTTO_TOKEN`, `ui-dioxus/src/net/url.rs:19` appends `?token=`, and both stop working here while the login UI is slice 2 — so the client *cannot* be rebuilt in lockstep, which is the precondition Decision 3's "clean break" relies on. Forcing TOTP on a locally-spawned loopback sidecar is also absurd UX. See §6.5 for why this is not the admin bypass Decision 3 rejects. |
| A13 | `/workspace` accepts a **user access token**, and `RemoteWorkspace` is **source/client-side only** — a promoted machine is never handed a user JWT. | Decision 4: a promoted machine that can verify user JWTs becomes a credential-harvesting oracle against the source. `RemoteWorkspace` has no production construction site today (only `crates/engine/tests/{promote,remote_workspace,vps_promote}.rs`), so this costs nothing now — but it is the constraint slice 3 must honor when it wires promoted machines, and stating it here is what stops the plan encoding the opposite. |

---

## 1. Where each piece lives

Dependencies flow strictly inward. `engine-core` defines the seam and must never depend on a
concrete impl crate; `auth` is an impl crate; `engine` wires them.

| Crate | Addition | Depends on |
|---|---|---|
| `protocol` | `UserId`, `Credentials`, `AuthMode`, four `Command` variants, three `ServerMessage` variants (`LoggedIn`/`LoggedOut`/`Hello`) | unchanged (`serde`, `uuid`) |
| `engine-core` | `auth::{Authenticator, Principal, AuthError}` | unchanged (already depends on `protocol`) |
| `auth` *(new)* | `TotpAuthenticator`, `JwtIssuer`, `AuthStore`/`SqliteAuthStore`, `Clock` | `otto-protocol`, `otto-engine-core`, `sqlx`, `jsonwebtoken`, `hmac`, `sha1`, `data-encoding`, `subtle`, `rand`, `qrcode`, `anyhow`, `async-trait`, `serde` |
| `persistence` | `owner` on the table, on `SessionState`, and on the scoped trait methods | unchanged |
| `engine` | principal resolution in `serve.rs`, `&UserId` on `EngineService`, `otto auth enroll` | `+ otto-auth` |

`engine-core` gains **no** crypto dependency — the seam is a trait plus two plain types.

**Build order is forced by the inward dependency rule** and this slice touches all five crates, so
tasks must land in exactly this sequence: `protocol` → `engine-core` → `auth` → `persistence` →
`engine` (→ `ui-dioxus`, which is workspace-excluded and built separately). Any other order does not
compile.

---

## 2. What this slice does and does not secure

Stated plainly because it is the single most misreadable thing in this design.

**Secured:** a principal must be established before any command is accepted; sessions have an owner;
attach/replay/abort/promote/demote are ownership-checked and reveal nothing about other tenants'
session ids; credentials are single-use, rate-limited, constant-time compared, and revocable.

**Out of scope by construction:** `--single-user` (§6.5), which has no tenants and therefore nothing
to isolate. Everything below concerns the authenticated multi-user mode.

**Not secured:** the workspace. Every session on a served engine still reads and writes one
process-global `--root` (premise correction 2). A second authenticated principal can therefore still
reach the first principal's files through `fs.*`, `bash`, and `POST /workspace` — not because the
check is missing, but because there is only one directory to check against. **`otto serve` remains
a single-trust-domain deployment until per-owner workspace roots land.** This slice is the identity
and session-isolation foundation that work requires, not a completed multitenancy story.

**Not secured, named:** the `--promotion-receiver` mode (§6.5). Its promotion secret is authority
over every session on that receiver — a holder can attach to any session promoted onto it,
adopting the session's owner. This is bounded (not eliminated) by the zero-principal condition: the
receiver refuses to start if any principal is enrolled, so the machine credential can never be a
backdoor into a server that also has real users. Slice 3 narrows the receiver credential to
per-session secrets, which is the real closure. A reader must not take "attach/replay/abort are
ownership-checked" as a claim that holds inside a `--promotion-receiver` machine.

---

## 3. `protocol` — the wire vocabulary

### 3.1 `UserId`

```rust
/// A principal's stable identifier. Validated on construction: 1–64 characters of
/// `[a-z0-9._-]`, which keeps it safe as a sqlite key, as an `otpauth://` URI label,
/// and in a log line without escaping.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserId(String);

impl UserId {
    pub fn parse(s: &str) -> Result<Self, InvalidUserId>;
    /// The reserved principal that owns sessions created by the offline CLI path.
    /// Never enrollable — `otto auth enroll local` is refused.
    pub fn local() -> Self;
    pub fn as_str(&self) -> &str;
}
```

`try_from = "String"` means the validation runs on **deserialization too** — a hostile client cannot
inject a `UserId` that bypasses the charset rule by sending it over the wire.

### 3.2 `Credentials`

```rust
/// Credentials presented to an `Authenticator`. One variant today; the enum exists so an
/// OIDC or device-flow backend lands additively.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum Credentials {
    Totp { user: UserId, code: String },
}
```

**`Debug` is implemented by hand and redacts `code`** (`Totp { user: alice, code: <redacted> }`).
The derived impl would put a live one-time code into any log line or panic message that formats a
`Command`.

### 3.3 New `Command` variants

Externally tagged, matching the enum's existing convention (premise correction 4).

```rust
Login   { credentials: Credentials },   // authenticate, mint an access + refresh token pair
Attach  { token: String },              // bind an existing access token to this connection
Refresh { refresh_token: String },      // rotate: consume one refresh token, mint a new pair
Logout,                                 // denylist this connection's jti, revoke its refresh token
```

`Attach` exists because a browser `WebSocket` cannot set an `Authorization` header and the access
token must not go in the query string. Keeping it separate from `Login` keeps the `Authenticator`
seam honest: the seam verifies *identity credentials*, never session tokens. `Logout` is
field-free — it acts on the connection's own bound token, so a client cannot revoke a token it does
not hold.

Both `Attach` and `Refresh` carry secrets, so `Command`'s derived `Debug` is replaced with a
hand-written one that redacts those two fields as well.

### 3.4 New `ServerMessage` variants

Internally tagged, `snake_case` — matching the enum's existing convention.

```rust
LoggedIn {
    user: UserId,
    access_token: String,
    /// Unix seconds. `protocol` stays dependency-free — no chrono, no time.
    expires_at: u64,
    refresh_token: String,
},
LoggedOut,
```

Like `Promoted`, these are **connection framing, not sequenced `Event`s**: they carry no `seq`, are
never persisted, and are never replayed. A reconnecting client re-authenticates; it must never be
handed a credential out of an event log. `LoggedIn`'s `Debug` redacts both tokens.

### 3.5 `Hello` — the pre-authentication greeting

`CapabilitiesManifest` **does not** gain an `auth_required` field. It rides only on
`ServerMessage::Ready` (`protocol/src/lib.rs:148-152`, sent at `serve.rs:562`), and §7.2 sends
`Ready` *after* authentication — so a client could only read the flag once it had already logged in,
which is exactly when it no longer needs it. A10 is withdrawn.

Instead, the server sends one frame immediately on upgrade, before any credential is presented:

```rust
Hello { auth_mode: AuthMode },   // AuthMode: single_user | users | machine (§6.5)
```

This does three jobs the withdrawn field could not: it tells slice 2's UI whether to show a login
form, it gives `SingleUser` and `Machine` clients a defined greeting (they proceed straight to
`Ready` with no handshake frame and no 10-second deadline), and it makes the `Users` handshake
explicit rather than something a client must infer from a rejection.

It discloses the server's mode to an unauthenticated prober. That is deliberate and cheap: the mode
is already inferable from one connection attempt, and it names no principal, no session, and no
secret.

---

## 4. `persistence` — session ownership

### 4.1 Schema and the `user_version` guard

`sessions` gains `owner TEXT NOT NULL` (Decision 3: not nullable, no synthetic default, no backfill)
and an index on it, since "list this principal's sessions" is the query ownership implies.

Because `CREATE TABLE IF NOT EXISTS` cannot alter an existing table (premise correction 1),
`init_schema` is preceded by a version probe:

```rust
const SCHEMA_VERSION: i64 = 1;

// PRAGMA user_version is 0 on both a brand-new file and every pre-ownership database.
// The two are told apart by whether `sessions` already exists.
match (user_version, sessions_table_exists) {
    (0, false) => { create_schema(); set_user_version(SCHEMA_VERSION); }
    (0, true)  => bail!(
        "session database at {path} predates multitenancy (issue #115) and has no owner \
         column. otto has no installed base, so there is no migration: delete the file and \
         let otto re-create it."
    ),
    (v, _) if v == SCHEMA_VERSION => {}
    (v, _) => bail!("session database at {path} has schema version {v}, newer than this \
                     otto build understands ({SCHEMA_VERSION}); upgrade otto"),
}
```

Fail-closed and actionable, rather than a sqlx "no such column: owner" surfacing from an unrelated
query hours later. The forward-version arm matters because sqlite will happily let an older binary
open a newer file.

### 4.2 `SessionState`

Gains `pub owner: UserId`, placed so `snapshot`/`restore` carry ownership across a promote — a
restored session belongs to whoever owned it on the source, not to whoever pushed the bundle. This
is a breaking change to `PromoteBundle`'s serialization, which Decision 3 permits.

### 4.3 Trait changes

```rust
async fn create_session(&self, owner: &UserId, goal: &str, config: &Value)
    -> anyhow::Result<SessionId>;

/// The owner of `session`, or `Err` if there is no such session.
async fn owner_of(&self, session: SessionId) -> anyhow::Result<UserId>;

// Owner-scoped. Each returns the *same* error for "no such session" and "not yours".
async fn replay_since(&self, owner: &UserId, session: SessionId, after_seq: Option<u64>)
    -> anyhow::Result<Vec<Event>>;
async fn session_status(&self, owner: &UserId, session: SessionId)
    -> anyhow::Result<SessionStatus>;
async fn snapshot(&self, owner: &UserId, session: SessionId) -> anyhow::Result<SessionState>;
```

Scoped by a `WHERE id = ?1 AND owner = ?2` predicate, so the check is in the same statement as the
read and cannot be forgotten by a caller. One shared constructor produces the not-found error for
both cases, so the two paths cannot drift apart and start distinguishing themselves (success
criterion 3 asserts the strings are identical).

`append_event`, `record_turn`, `next_seq`, `next_turn`, and `set_status` stay unscoped. They are
reachable only from inside a turn that `EngineService` has already authorized (§5.1), scoping them
would triple the churn, and — unlike the five above — none of them returns another tenant's data.
This is a deliberate trade; the module doc records it so a later reader does not mistake it for an
oversight.

`restore`/`restore_over` take the owner from `state.owner`; no signature change.

**`owner_of` is deliberately unscoped, and is a reverse existence oracle.** It distinguishes "exists,
owned by someone else" from "does not exist" — the exact distinction §4.3's scoped reads work to
hide. That is required, because `EngineService::authorize` is its only intended caller and needs the
comparison. It is nonetheless reachable from outside via the public `EngineService::store()`
accessor (`crates/engine/src/service.rs:127`), so the trait's doc comment must state that
**`owner_of` must never back a client-facing path** — the same warning the unscoped writers carry.

---

## 5. `engine-core` — the `Authenticator` seam

Declared in a new `crates/engine-core/src/auth.rs`, following `Retriever`'s shape exactly
(`engine-core/src/retrieval.rs:18-22`): `Send + Sync`, async, trait-object-friendly.

```rust
/// An authenticated principal. Just an identity today; roles and tenancy attributes
/// attach here when they exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal { pub user: UserId }

/// Why authentication failed. The variants exist so the server can log precisely and
/// rate-limit correctly; every one of them renders to the client as the same opaque
/// string (Assumption A11).
#[derive(Debug)]
pub enum AuthError {
    /// Unknown user, wrong code, or a replayed time-step — deliberately one variant, so
    /// a caller cannot accidentally branch on the distinction and leak it.
    InvalidCredentials,
    /// Too many recent failures for this principal; retry after the cooldown.
    RateLimited { retry_after_secs: u64 },
    /// The backend itself failed (database unreachable, clock unavailable).
    Backend(anyhow::Error),
}

#[async_trait::async_trait]
pub trait Authenticator: Send + Sync {
    /// Verify presented credentials and return the authenticated principal.
    async fn authenticate(&self, creds: &Credentials) -> Result<Principal, AuthError>;
}
```

This is the one place the issue's sketch is tightened: it returns `anyhow::Result<Principal>`, which
cannot express "rate limited" without string-matching and risks an internal error message reaching
the client verbatim. A three-variant enum costs nothing and makes A11 enforceable by the type.

---

## 6. `auth` — the first backend

### 6.1 TOTP (RFC 6238)

- **Secret:** 20 random bytes from `rand`, stored base32 (unpadded, RFC 4648) — the encoding
  authenticator apps expect.
- **Enrollment:** renders `otpauth://totp/otto:<user>?secret=<b32>&issuer=otto&algorithm=SHA1&digits=6&period=30`,
  plus a terminal QR via `qrcode`.
- **Verification:** `T = floor(now / 30)`; HMAC-SHA1 over `T` big-endian; RFC 4226 dynamic
  truncation; 6 digits, zero-padded.
- **Skew:** accept `T-1`, `T`, `T+1`.
- **Replay:** the store keeps `last_step` per user; **any candidate step `<= last_step` is
  rejected before comparison**, and a success writes the accepted step back. This is what makes the
  skew window safe — without it, ±1 step means an observed code is replayable for 90 seconds. The
  write is `UPDATE … WHERE user = ? AND last_step < ?`, so two concurrent logins with the same code
  cannot both succeed.
- **Comparison:** `subtle::ConstantTimeEq` over the digit bytes. Never `==`.
- **Throttling:** a per-user failure counter with a timestamp; 5 failures inside 15 minutes returns
  `RateLimited` without even computing a code. A 10^6 keyspace makes this a correctness requirement,
  not a nicety.
- **Time:** all of the above are pure functions taking `now: u64`. A `Clock` trait
  (`SystemClock` in production, a fixed clock in tests) makes the RFC 6238 vectors — which are
  defined at specific timestamps — directly testable, and keeps the crate's tests deterministic.

### 6.2 JWT

HS256. Claims: `sub` (`UserId`), `iat`, `exp`, `jti` (uuid v4), and `kid` in the header.

- The signing key is 32 random bytes, generated on first use and stored keyed by `kid`. Verification
  looks the key up by the presented `kid`, so a second key can be introduced before the first is
  retired.
- `aud` is **reserved and not emitted** — Decision 4 defers audience scoping, and minting a claim
  nothing validates is worse than omitting it.
- Verification checks signature, `exp`, and denylist membership, in that order.

**Revocation.** `Logout` inserts the token's `jti` with its own `exp` as the row's expiry; every
verification consults the table. Rows are pruned opportunistically on write and on startup, so the
table is bounded by the number of logouts within one access TTL rather than growing without limit.

**Refresh.** A 32-byte opaque random string, stored **hashed** (SHA-256) alongside its user and
expiry — a stolen database yields no usable refresh token. `Refresh` consumes the presented token
and issues a new pair in one transaction; a token already consumed is rejected. Reuse of a consumed
refresh token is the classic theft signal, so it revokes that user's whole outstanding refresh set
and is logged.

### 6.3 Store

`AuthStore` (trait) + `SqliteAuthStore`, in the same `CREATE TABLE IF NOT EXISTS` + `user_version`
style as `persistence` (§4.1) — but **without §4.1's `(0, true) => bail!` legacy arm**, which would
be dead code here: this database is new in this slice, so there is no pre-ownership shape it could
ever encounter. It keeps only the create-and-stamp arm and the forward-version guard. Four tables: `users` (id, totp secret, last_step, failure
counter/window), `signing_keys` (kid, key, created_at), `refresh_tokens` (hash, user, expires_at,
consumed_at), `denylist` (jti, expires_at). Located per A5.

### 6.4 What happens to `OTTO_TOKEN`

- **Deleted from every user-facing route.** `/ws` and `/workspace` no longer consult it; the
  `?token=` query parameter and `ConnectParams.token` are removed outright.
- **Renamed to what it is.** `ServeState.token` becomes `promotion_secret`, and the environment
  variable becomes `OTTO_PROMOTION_SECRET`, read only when `--accept-promotions` or a `--promote-*`
  mode is set. `otto serve` with neither flag requires **no** shared secret at all — the meaningful
  end of "the shared token is not an identity".
- **Constant-time compared** via `subtle`, closing the `==` finding.
- Slice 3 replaces it with per-session secrets and the receiver's session→secret map; the rename is
  chosen so that change is a narrowing of an already-correctly-named thing.

`README.md`, `CLAUDE.md`, `deploy/fly/Dockerfile`, and `deploy/fly/README.md` are updated. The Fly
image's injected env becomes `OTTO_PROMOTION_SECRET` at `crates/remote/src/fly.rs:197-201`
(`create_machine_body`'s `env` map) — that and the Dockerfile comment at `deploy/fly/Dockerfile:51-52`
move together, or the promoted machine will not start. The Dockerfile `CMD` at `:57` gains
`--promotion-receiver`. There is **no** `OTTO_TOKEN` `ENV` line in the Dockerfile to rename (the `ENV`
at `:54` sets port/root/host/ui-dir) — open issue 5's correction.

### 6.5 The three authentication modes

`otto serve` today has exactly one deployment shape. It actually has three, and the shared
`OTTO_TOKEN` was papering over the difference. Naming them is what lets the zero-principal guard,
the loopback rule, and the promoted-machine reconnect all be correct at once.

```rust
enum AuthMode {
    /// Loopback-only, no credential, everything owned by `UserId::local()`. The desktop sidecar.
    SingleUser,
    /// The multi-tenant path: user JWTs, ≥1 enrolled principal required.
    Users,
    /// A promote receiver. The promotion secret authenticates `/ws` and `/workspace`, and the
    /// connection adopts the attached session's own owner.
    Machine,
}
```

**`SingleUser`** — `--single-user`.

- Every connection is bound to `UserId::local()`. No `Authenticator`, no auth database, no token
  minted or verified; `Login`/`Attach`/`Refresh`/`Logout` are rejected as not-applicable.
- **The bind host must be loopback**, enforced as a startup error rather than a warning. "Loopback"
  is a predicate, not a string match: parse the **resolved** `OTTO_HOST` (unset defaults to
  `"127.0.0.1"`, `main.rs:820`) as an `IpAddr` and require `is_loopback()`. A value that does not
  parse as an IP is rejected — so `localhost` is refused (it resolves, but not necessarily to a
  loopback address), `::1` and `127.0.0.2` are accepted, and `0.0.0.0` is refused. Per §9.1 this is a
  pure tested function.
- **`--promote-loopback` is allowed; `--promote-vps`/`--promote-microvm`/`--promote-fly` and
  `--accept-promotions`/`--promotion-receiver` are startup errors.** Loopback promote provisions a
  second *in-process* engine on the same machine, inside the same trust domain, so it needs no
  cross-machine credential — the provisioned engine simply inherits `SingleUser` (threaded via
  `AuthMode` through `LoopbackTarget::new`, `loopback.rs:31`, and `serve::app`, `loopback.rs:84`),
  and `Promoted.token` stays `None`. This matters concretely: the desktop sidecar's argv is
  `serve --port … --root … --approve-edits --promote-loopback` (`ui-dioxus/src/desktop_boot.rs:218`),
  three tests assert that flag (`desktop_boot.rs:483,498,517`), and the UI has a live promote feature
  behind it (`ui-dioxus/src/app.rs:187`). Forbidding it would break exactly what §6.6 exists to keep
  working.
- **The ownership check still runs**, bound to `UserId::local()`. This is the property that makes
  the mode defensible rather than a hole: point a `--single-user` server at a store that already
  holds `alice`'s sessions and they are *unreachable*, because every read still goes through §4.3's
  owner-scoped predicate. `SingleUser` narrows *who you can be*; it never skips the check.

**`Users`** — the default. §7.2's handshake, §7.4's zero-principal refusal.

**`Machine`** — selected by the explicit `--promotion-receiver` flag, which requires
`--accept-promotions`. Never implied by `--accept-promotions` alone.

- `/ws` and `/workspace` authenticate with the promotion secret (constant-time), and the connection
  **adopts the owner of the session it attaches to**. No enrolled principals are required.
- **Zero-principal condition:** a `--promotion-receiver` server refuses to start if any principal is
  enrolled. This is the closure of open issue 1's cross-tenant hole — a receiver with real users
  cannot double as a machine-credential host, so the machine secret is never a backdoor into a
  multi-tenant server. §2 names the residual (machine-wide) authority.
- The WS handshake accepts the promotion secret via the `Authorization` header **or** a post-upgrade
  `Attach { token }` frame — `Attach` is what gives a browser reconnecting to a promoted machine a
  channel now that `?token=` is deleted (§7.2).
- This is issue #115 Decision 4's own reasoning, one slice coarser: *"the remote does not need to
  know who the user is: ownership is checked at the source before promotion, and the machine hosts
  exactly one session. Holding that session's secret is authority for that session and nothing
  else."* Slice 1 scopes that authority to the **machine**; slice 3 narrows it to the **session**
  via the receiver's session→secret map. The user's JWT still never leaves the source, which is the
  invariant that actually matters.
- Without this mode, slice 1 would silently brick two shipped things: `deploy/fly/Dockerfile:57`
  runs `otto serve --accept-promotions --port … --ui-dir …` on a guest with zero enrolled
  principals, and the post-promote reconnect (`ui-dioxus/src/app.rs:187`) would have no credential
  the remote is permitted to verify. The Dockerfile `CMD` gains `--promotion-receiver` (§6.4), and
  the VPS/microVM test receivers adopt the flag. Neither is covered by `cargo test --workspace`,
  because all seven harnesses in §9.1 build `serve_app` directly instead of going through
  `cmd_serve` — the same blind spot §9.1 item 2 exists to close.

**Why none of this is the admin bypass Decision 3 rejects.** That decision refuses to keep
`OTTO_TOKEN` as "an admin bypass behind a flag" — a *shared secret granting root authority across
tenants*, i.e. a second way to become any principal on a multi-tenant server. `SingleUser` has no
secret and no tenants to cross. `Machine` has a secret, but it grants authority over sessions that
were already promoted to that machine by an owner the source authenticated — it cannot mint a
principal, cannot reach a session that was never promoted there, and never sees a user JWT. Neither
mode can elevate within `Users`. What Decision 3 wants gone is a second path to *authority over
other tenants*; these are modes with no other tenants to have authority over.

### 6.6 Keeping `ui-dioxus` alive across the slice boundary

`ui-dioxus/` is workspace-excluded, so `cargo test --workspace` cannot observe it breaking — which
makes this the one part of the slice that needs deliberate attention rather than test coverage.
Three touch points, all minimal:

| File | Change |
|---|---|
| `ui-dioxus/src/desktop_boot.rs:151,241` | Stop minting a secret and setting `OTTO_TOKEN`; add `--single-user`. **Keep `--promote-loopback`** (`desktop_boot.rs:218`) — §6.5 permits it, and three tests at `desktop_boot.rs:483,498,517` assert it. Net argv: `serve --port … --root … --approve-edits --promote-loopback --single-user`. `LaunchParams.token` becomes empty (`app.rs:449` sets it). |
| `ui-dioxus/src/net/url.rs:19` | Stop appending `?token=…`; the parameter no longer authenticates anything (§6.4). `build_ws_url` drops the token argument; `LaunchParams.token` stays but is empty on the desktop path. |
| `ui-dioxus/src/components/connection_form.rs` | The token field and `redact_token` **stay**, unused, for the remote-server path. Slice 2 replaces them with the login flow. A code comment says so, so the next reader does not delete them as dead. |
| `ui-dioxus/src/app.rs:129-216` | Add the three new `ServerMessage` arms to the **exhaustive** match — a compile error otherwise: `Hello { auth_mode }` records the mode (a signal slice 2's login UI reads), `LoggedIn { access_token, .. }` stores the token (slice 2 wires the rest), `LoggedOut` clears auth state. `redact_token` keeps its seam-wide use via `SeamError`. |
| `ui-dioxus/src/app.rs:93,346,368` | Drop the `tok.is_empty()` gates in `do_connect`, `load_files`, `open_path` — under `--single-user` the token is empty by design; gate on connection state (`Connected`) instead. Without this the file tree and editor die while the app still reaches `Ready` (open issue 4). |

**The honest limitation:** the *desktop* app keeps working end-to-end (including loopback promote,
and including the post-promote reconnect, since the provisioned engine inherits `SingleUser`). The
*browser* path against a remote `AuthMode::Users` server does not, because performing `Login` needs
the slice-2 UI. That is a dev-served bundle rather than a shipped artifact, and it is stated in §10
rather than glossed. Verification is out-of-band by construction (success criterion 7).

---

## 7. `engine` — resolving a principal

### 7.1 `EngineService`

**Client-facing methods** take `owner: &UserId` and call a private `authorize(owner, session)`
first, which compares against `store.owner_of(session)` and returns the shared not-found error on
mismatch: `run_prompt`, `run_prompt_with_controls`, `run_command_with_controls`,
`run_agent_with_controls`, `abort`. `create_session` takes the owner and passes it through.

**Machine-to-machine methods** derive the owner internally and take no `owner` parameter:
`accept_promotion`/`accept_demotion` read it from the bundle's `SessionState` (§4.2), and
`export_promotion` reads it via `owner_of` — which it needs regardless, because §4.3 makes
`snapshot` owner-scoped.

`export_promotion` belongs in the second group specifically, and this is worth stating because the
opposite is the tempting mistake: its only caller is the `/export` handler
(`crates/engine/src/serve.rs:322`), which is machine-credentialed and has no connected principal
(§7.3). Giving it an `owner` parameter would leave a caller with nothing to pass but
`store.owner_of(session)` — feeding a value derived from the session back in as the check on that
same session. That tautology *looks* like an authorization check and enforces nothing. The real
check for handover lives on the source's WS command loop, where a principal actually exists.

`workspace_rpc` is unchanged in signature: per premise correction 2 there is nothing session-scoped
to check. Its caller authenticates (§7.3).

### 7.2 The WS handshake

`ws_handler` no longer authenticates at upgrade — it accepts the socket, optionally pre-resolving a
principal from an `Authorization: Bearer` header for non-browser clients. **`Hello { auth_mode }` is
always the first frame, in every mode** (open issue 3's resolution): it is sent immediately on
upgrade, before any credential is presented, and regardless of whether the header pre-resolved a
principal — a pre-resolved header skips the *deadline*, not the greeting. `handle_socket` then:

1. Send `Hello { auth_mode }`. If a header principal was resolved, skip to step 4.
2. Await the first frame under a **10-second deadline** (`tokio::time::timeout`). Nothing else is
   read, no session is resolved, and no store call is made until this succeeds — an unauthenticated
   socket costs one task and one timer. (`SingleUser` skips this step — no deadline, no frame, the
   connection is bound to `UserId::local()` and proceeds straight to `Ready`. `Machine` also skips
   the deadline; its credential is the header or an `Attach`-with-promotion-secret frame, §6.5.)
3. The frame must be `Login` or `Attach`. `Login` runs the `Authenticator` and mints a pair;
   `Attach` verifies an existing access token (or, on a `Machine` server, the promotion secret).
   Success sends `LoggedIn` (for `Login`) and binds the principal to the connection. Failure sends
   `Error { "authentication failed" }` and closes. **Any other command before authentication is the
   same failure** — including `CreateSession`.
4. Only now is `resolve_session` called. It returns `(SessionId, UserId)` (the connection-scoped
   principal threaded through the loop, per A9's resolution) and is ownership-checked: an explicit
   `?session=` that the principal does not own fails exactly as a nonexistent one does. `None`
   creates a session owned by the principal — **except on a `Machine` server**, which rejects
   session creation outright (a receiver hosts sessions promoted onto it; creation happens on the
   source). On a `Machine` server the principal is the attached session's own owner (§6.5), so an
   `Attach` without a `?session=` has no owner to adopt and fails.
5. `Ready` is sent, `last_seq` replay runs through the owner-scoped `replay_since`, and the command
   loop starts.

Inside the loop, `Logout` denylists the connection's `jti`, revokes its refresh token, aborts that
principal's in-flight turn, sends `LoggedOut`, and closes — the connection does not continue
unauthenticated. `Refresh` rotates and replies with a fresh `LoggedIn`.

The access token is re-verified (signature, `exp`, denylist) **on each command**, not only at
handshake. Otherwise a long-lived socket outlives both expiry and revocation, which would make the
denylist decorative on exactly the connections that matter most.

### 7.3 The HTTP routes

**The static `--ui-dir` fallback stays unauthenticated, and must not be "fixed".** `serve.rs:169`
mounts `ServeDir` as an unauthenticated fallback *by design* — a browser has to fetch `index.html`
and the wasm before it can possess any credential to present. §2's "a principal must be established
before any command is accepted" is about the command protocol, not about static build output, and a
planner reading §2 alongside a list of routes is exactly the reader who would "complete" the auth
coverage by putting a token check in front of it and break first load. It is named here so that
does not happen. Its separate invariant is unchanged: `--ui-dir` is never defaulted, never inferred,
and never points at a workspace root, because `ServeDir` does not consult the sensitive-path floor.

The three protocol routes, each with its **per-mode credential** (open issue 2's resolution — the
rule below replaces the single "requires a valid access token" sentence):

| Route | `SingleUser` | `Users` | `Machine` |
|---|---|---|---|
| `/ws` | No credential; every connection is `UserId::local()` | Access token (header, or `Login`/`Attach` post-upgrade); the `Hello`/deadline handshake of §7.2 | Promotion secret (header or `Attach`), constant-time; adopts the attached session's owner |
| `POST /workspace` | **No credential** — header ignored entirely; the route is loopback-bound, single-principal, and mints nothing | Valid access token (`Authorization: Bearer`), verified exactly like the WS path | Promotion secret, constant-time |
| `POST /promote` / `POST /export` | Startup error (`--accept-promotions`/`--promotion-receiver` refused under `--single-user`, §6.5) | Promotion secret (machine-to-machine; the flag gate is `--accept-promotions` / `--promotion-receiver`) | Promotion secret, same |

- **`POST /workspace`** — authenticated per the table; *not* isolated (§2). The UI calls it
  unconditionally with `Authorization: Bearer` (`ui-dioxus/src/transport/web.rs:84`,
  `transport/desktop.rs:95`); under `SingleUser` the server simply does not consult the header.
- **`POST /promote` / `POST /export`** — keep the machine credential (§6.4), constant-time compared,
  behind the `--accept-promotions` / `--promotion-receiver` gate. Ownership travels in the bundle.
  The user-facing ownership check for handover is on the **source**, in the WS command loop, where a
  real principal exists: `PromoteToRemote`/`DemoteToLocal` verify the connection's principal owns
  the session before any bundle is built. **That check shipped in slice 1a**
  (`EngineService::authorize_session`). What this slice adds is attach-time `?session=` ownership
  (§7.2 step 4) on the WS path; `/promote`/`/export` stay bearer-only because their machine
  credential and the source-side check are the boundary (A9's resolution documents the convention).

**Which credential `/workspace` accepts, and from whom** (A13 — this is the direction the plan will
encode, so it is stated rather than implied):

| Caller | Credential | Why |
|---|---|---|
| A client or the source engine, talking to a source-side `otto serve` | User access token | It acts for a principal, and there is one to act for. |
| Anything running **on a promoted machine** | The per-session promotion secret — **never** a user JWT | Decision 4: a remote that can verify user JWTs accepts any valid user's token, and a client connecting to it hands one over, turning a compromised ephemeral machine into a credential-harvesting oracle against the source. |

`RemoteWorkspace` (`crates/workspace/src/remote.rs:36-53`) needs no structural change — it already
sends `Authorization: Bearer`. It is **source/client-side only**: it is handed a user access token,
and it must never be constructed on a promoted machine. This costs nothing to assert today, because
`RemoteWorkspace::new` has no production construction site at all — its only callers are
`crates/engine/tests/{promote,remote_workspace,vps_promote}.rs`. It is written down because slice 3
wires the promoted-machine side, and that is where getting this backwards would be expensive.

### 7.4 `otto auth enroll`

```
otto auth enroll <user>     provision a TOTP secret, print the otpauth:// URI + QR
otto auth list              list enrolled principals (no secrets)
otto auth revoke <user>     remove a principal and all their tokens
```

Runs on the host against the auth database directly — it needs no server and no credential, which
is precisely why it is the bootstrap. With `OTTO_TOKEN` gone this is the **only** way a first
principal comes into existence (open question 2, answered). Enrolling an existing user re-provisions
and invalidates the old secret, and requires `--force` so a typo cannot silently lock someone out.
**All three subcommands refuse `local`**, not just `enroll`: it is a reserved built-in (A2), so
`enroll local` cannot create it, `revoke local` cannot delete it, and `list` never shows it as
though it were an enrolled principal. Refusing it in only one place would let the reserved id become
half-real.

`otto serve` refuses to start with **zero** enrolled principals, naming `otto auth enroll` — a
server nobody can log into is always a misconfiguration, and failing at startup beats failing at
first connection.

**The guard applies to `AuthMode::Users` only.** `SingleUser` and `Machine` have no enrolled
principals by design (§6.5), and applying it to them would brick both the desktop sidecar and
`deploy/fly/Dockerfile:57`'s `--accept-promotions` guest. Scoping it to the mode that actually has
principals is the whole distinction §6.5 exists to draw.

Per §9.1 this guard and §6.5's loopback predicate are pure functions rather than inline `cmd_serve`
code, so both are testable — otherwise two security-relevant guards ship with no coverage.

---

## 8. Error Handling & Edge Cases

| Case | Behavior |
|---|---|
| No auth frame within 10s | `Error`, close. No session, no store write. |
| Command before authentication | `Error { "authentication failed" }`, close. |
| Bad TOTP code / unknown user / replayed step | Identical opaque `"authentication failed"`; the real cause logged server-side; the failure counter increments. |
| 5 failures in 15 min | `RateLimited`; further attempts rejected without computing a code. Cooldown is per-user, so one principal cannot lock out another. |
| Clock skew beyond ±1 step | Rejected. Operators see a server-side log naming clock drift, because this is otherwise indistinguishable from a wrong code. |
| Two concurrent logins, same code | Exactly one wins — the `last_step` update is conditional (`WHERE last_step < ?`). |
| Access token expired mid-connection | The next command is rejected; the client is expected to `Refresh`. |
| Denylisted token presented | Rejected on every route until `exp`. |
| Reused refresh token | Rejected, and that user's outstanding refresh tokens are revoked (theft signal), logged. |
| Attach to a session owned by another principal | The nonexistent-session error, byte-for-byte. |
| Promote/demote a session you do not own | Same error, before any bundle is built. |
| Restoring a bundle whose owner is not enrolled locally | Accepted — ownership is data, and demote must work even if the receiver never knew the user. Recorded in §10. |
| Legacy session database | Startup fails with the §4.1 message. |
| Auth database missing/unwritable | `otto serve` fails at startup, not at first login. |
| `otto run` | Unchanged: owner is `UserId::local()`, no authenticator constructed, no auth database opened. |
| `--single-user` on a non-loopback host | Startup error. `OTTO_HOST` must parse as an `IpAddr` and be `is_loopback()`; `localhost` and `0.0.0.0` are refused (§6.5). |
| `--single-user` with `--accept-promotions` or a non-loopback `--promote-*` | Startup error. `--promote-loopback` is permitted. |
| `--single-user` server pointed at a store holding another principal's sessions | Those sessions are unreachable — the owner-scoped predicate still runs, bound to `local` (§6.5). |
| Reconnect to a promoted machine | `AuthMode::Machine`: the promotion secret authenticates and the connection adopts the attached session's owner. The user's JWT never leaves the source (Decision 4). |
| `Login`/`Refresh`/`Logout` sent to a `SingleUser` or `Machine` server | Rejected as not-applicable; the client was told the mode by the `Hello` frame (§3.5). |
| `Attach` sent to a `SingleUser` server | Rejected as not-applicable (no tokens exist in the mode). |
| `Attach` sent to a `Machine` server | **Accepted** — carries the promotion secret (§6.5), the one frame the mode exists to receive. The client was told the mode by `Hello`. |
| `Attach` without `?session=` on a `Machine` server | Rejected — a receiver creates no sessions, so there is no owner to adopt (§7.2 step 4). |
| No auth frame within the 10s deadline (`Users` only) | `Error`, close. No session, no store write. (`SingleUser`/`Machine` have no deadline — §7.2.) |

---

## 9. Testing

- **`protocol`** — round-trip each new `Command`/`ServerMessage` variant and assert the raw JSON
  shape (external tagging for commands, `{"type":"logged_in",…}` for frames), matching the file's
  existing test style. Assert the redacting `Debug` impls do not contain the secret.
- **`auth`** — RFC 6238 vectors against a fixed `Clock`; replay rejection; ±1 accepted / ±2
  rejected; lockout after N; JWT round-trip; expired rejected; denylisted rejected; denylist pruning;
  refresh rotation; reused-refresh revocation; `UserId` validation including rejection via
  deserialization.
- **`persistence`** — `create_session` records the owner; each scoped read returns the row for the
  owner and the identical error for both wrong-owner and nonexistent (asserting string equality);
  `snapshot`/`restore` preserve the owner; the `user_version` guard rejects a hand-built legacy
  database and accepts a fresh one. Uses the existing `temp_store()` fixture shape.
- **`engine`** — extending `crates/engine/tests/serve.rs`, whose five harness constructors and
  `tokio_tungstenite` client helpers already exist. New: no-auth-frame timeout; wrong code;
  `Login` → `Ready`; `Attach` with a minted token; cross-tenant attach indistinguishable from
  nonexistent; cross-tenant replay/abort/promote refused; `Logout` invalidates the token on `/ws`
  **and** `/workspace`; `/workspace` refuses an unauthenticated request; `?token=` no longer
  authenticates anything. The existing `rejects_missing_token` / `authorizes_via_query_token` /
  `rejects_wrong_query_token` tests are rewritten rather than deleted — the query path must be
  proven *inert*, not merely untested.
- **Determinism guard** — `otto run` end-to-end unchanged; the offline suite needs no network, no
  keys, and no auth database.

### 9.1 The integration-test migration is wider than `serve.rs`

**Slice 1a already shipped the `create_session(owner, …)` signature, the owner-scoped reads, and
`run_goal`'s `UserId::local()`** — so the owner-parameter churn this section originally budgeted for
is largely done. What this slice adds at the integration level is the **auth surface**: the serve
constructors gain an `AuthMode`/authenticator parameter, `?token=` stops authenticating, and the WS
handshake grows `Hello` + `Login`/`Attach`. That reaches the same **seven** integration test files,
each of which builds `serve_app`/`ServeState` directly rather than going through `cmd_serve`:
`crates/engine/tests/{serve,cors,ui_dir,promote,remote_workspace,vps_promote,microvm}.rs`.

Two consequences the plan must budget for rather than discover:

1. **Those harnesses need an `Authenticator`.** They get a `FakeAuthenticator` — a test double in
   `otto-auth` behind `#[cfg(feature = "testing")]` (forwarded as `otto-engine`'s own `testing`
   feature, the `firecracker`-forwarding precedent) that accepts a fixed credential and returns a
   fixed `Principal`. Not `SqliteAuthStore`: the suite must stay hermetic and fast, and — critically
   — **A5's `OTTO_AUTH_DB` default must never be consulted by a test.** Every constructor takes an
   explicit path or an explicit authenticator, so a developer's real auth database can never be
   opened, written, or depended on by `cargo test`.
2. **§7.4's zero-principal startup refusal, §6.5's loopback/`--promotion-receiver` enforcement, and
   the `--single-user` mode resolution live in `cmd_serve`, which no test exercises.** They must
   therefore be extracted as pure, unit-testable functions (in the shape of the existing
   `validate_ui_dir` at `crates/engine/src/main.rs:141`) rather than written inline — otherwise
   these security-relevant guards ship untested.

---

## 10. Risks & Open Questions

1. **The workspace is not isolated (§2).** The largest risk in this design is that "multitenant
   identity" is read as "otto is now multitenant". It is not. Mitigated by saying so in §2, in the
   PR body, and in `CLAUDE.md`; resolved by the per-owner-roots slice.
2. **Per-owner workspace roots — open question 1, answered as "its own slice."** The change is
   substantial: `--root` becomes a resolver keyed by owner, `EngineService` holds a workspace per
   session rather than one, `RemoteWorkspace` and `PromoteBundle` gain a root notion, and the
   sandbox's read-only-except-root policy is computed per turn. It cannot ride along here.
3. **A5 is a convention, not an enforced invariant.** An operator who points `OTTO_AUTH_DB` inside a
   workspace puts credentials where agents can read them. Enforcing it means either widening the
   sensitive-path floor (an explicit non-goal) or rejecting such paths at startup. A startup
   rejection is cheap and worth doing if the implementation allows; otherwise it is documented and
   left to slice 2.
4. **HS256 means the signing key verifies and mints.** Correct under Decision 4 (the key never
   leaves the source), and *depends* on it: if a future slice ever lets a remote verify user JWTs,
   the algorithm must move to asymmetric first. Recorded in the `auth` module docs as a condition on
   the choice, not just a preference.
5. **A restored bundle can name an owner the receiver has never enrolled.** Accepted deliberately
   (§8) so demote works, but it means a receiver's `sessions` table can hold owners absent from its
   `users` table. Harmless while ownership is only ever compared, and worth revisiting if ownership
   ever grants anything transitively.
6. **`Logout` cannot revoke another connection's token.** Two sockets sharing one access token: a
   logout on either denylists the shared `jti`, so both die. Correct, and worth stating because it
   looks like a bug from the second connection.
7. **This is a hard protocol break, and Decision 3 only covers part of it.** Decision 3 sanctions
   breaking clients that are *rebuilt in lockstep*. The desktop app is rebuilt in lockstep and is
   handled (§6.6 + `--single-user`). The **browser path against a remote authenticated server is
   not**: performing `Login` needs the slice-2 UI, so between these two slices the web bundle can
   reach a `--single-user` server but cannot authenticate to a multi-user one. Accepted because the
   web bundle is a dev-served artifact rather than a shipped one, and because slice 2 is a hard
   prerequisite for any release that advertises multi-user serve — but accepted *explicitly*, not by
   omission. `ui-dioxus/` being workspace-excluded means no workspace test can detect it, which is
   why success criterion 7 is verified out-of-band.
