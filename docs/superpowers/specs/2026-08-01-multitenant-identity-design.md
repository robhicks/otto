# Multitenant identity — the `Authenticator` seam, TOTP + JWT, and session ownership

> **Status:** DRAFT — slice 1 of GitHub issue #115 (multitenancy): identity and session ownership,
> no UI.
> **Implements:** the "Suggested first slice" of [#115](https://github.com/robhicks/otto/issues/115).
> **Blocks:** slice 2 (UI slash commands + `otto login`/`logout` CLI) and slice 3 (handover
> credentials — `Promoted.token` always `Some`, per-session secrets on every target).

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
   (`crates/engine/src/service.rs:647-707`) operates on the single `Arc<dyn Workspace>` the service
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
constant-time (§6.1); and `resolve_session` (`serve.rs:1055-1068`) parses a client-supplied
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
- Ownership checks on session attach, replay, abort, and both handover commands.
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
7. `otto serve --single-user` refuses to bind a non-loopback host (asserted), and the desktop app
   still launches its sidecar and reaches `Ready` — verified out-of-band via
   `cd ui-dioxus && cargo test --features desktop`, since `ui-dioxus/` is workspace-excluded and
   `cargo test --workspace` structurally cannot catch its breakage.

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
| A10 | `CapabilitiesManifest` gains `auth_required: bool`. | The struct is `#[serde(default)]`, so this stays semver-minor for the separately-built UI, and slice 2's login flow needs to know whether to show it. Costs nothing now. |
| A11 | Failed authentication returns one opaque message (`"authentication failed"`) to the client regardless of cause; the specific reason is logged server-side only. | An error distinguishing "unknown user" from "bad code" from "locked out" is an enumeration oracle. |
| A12 | A **`--single-user` mode** is added, and the desktop sidecar uses it, rather than accepting a dead UI between slices. | Without it slice 1 ships a broken application: `ui-dioxus/src/desktop_boot.rs:130` mints a secret and `:219` passes it as `OTTO_TOKEN`, `ui-dioxus/src/net/url.rs:19` appends `?token=`, and both stop working here while the login UI is slice 2 — so the client *cannot* be rebuilt in lockstep, which is the precondition Decision 3's "clean break" relies on. Forcing TOTP on a locally-spawned loopback sidecar is also absurd UX. See §6.5 for why this is not the admin bypass Decision 3 rejects. |
| A13 | `/workspace` accepts a **user access token**, and `RemoteWorkspace` is **source/client-side only** — a promoted machine is never handed a user JWT. | Decision 4: a promoted machine that can verify user JWTs becomes a credential-harvesting oracle against the source. `RemoteWorkspace` has no production construction site today (only `crates/engine/tests/{promote,remote_workspace,vps_promote}.rs`), so this costs nothing now — but it is the constraint slice 3 must honor when it wires promoted machines, and stating it here is what stops the plan encoding the opposite. |

---

## 1. Where each piece lives

Dependencies flow strictly inward. `engine-core` defines the seam and must never depend on a
concrete impl crate; `auth` is an impl crate; `engine` wires them.

| Crate | Addition | Depends on |
|---|---|---|
| `protocol` | `UserId`, `Credentials`, four `Command` variants, two `ServerMessage` variants, `CapabilitiesManifest.auth_required` | unchanged (`serde`, `uuid`) |
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

### 3.5 `CapabilitiesManifest`

One field, `auth_required: bool`, defaulting to `false` via the existing struct-level
`#[serde(default)]`.

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

`README.md`, `CLAUDE.md`, `deploy/fly/Dockerfile`, and `deploy/fly/README.md` are updated, and the
Fly image's `OTTO_TOKEN` env becomes `OTTO_PROMOTION_SECRET` (`crates/remote/src/fly.rs:198` and
`deploy/fly/Dockerfile:52` move together, or the promoted machine will not start).

### 6.5 `otto serve --single-user` — the local deployment shape

`otto serve` gains a mode in which there are no tenants:

- Every connection is bound to `UserId::local()`. No `Authenticator` is constructed, no auth
  database is opened, no token is minted or verified, and `Login`/`Attach`/`Refresh`/`Logout` are
  rejected as not-applicable.
- **The bind host must be loopback.** `--single-user` combined with a non-loopback `OTTO_HOST`
  is a startup error, not a warning. This is the invariant that makes the mode safe, so it is
  enforced in code and asserted by a test (success criterion 7) rather than documented and hoped for.
- Mutually exclusive with the authenticated mode: `--single-user` plus `--accept-promotions` or any
  `--promote-*` flag is a startup error, since handover across machines has no meaning for a
  single-user loopback server and would need a credential the mode does not have.

**Why this is not the admin bypass Decision 3 rejects.** That decision refuses to keep `OTTO_TOKEN`
as "an admin bypass behind a flag" — a *shared secret* that grants *root authority across tenants*,
i.e. a second way to become any principal in a multi-tenant server. `--single-user` has no secret to
leak, no tenants to cross, and no elevation: it is `otto run` with a socket in front of it, owned by
the same reserved `local` principal A2 already establishes for the CLI. The thing Decision 3 wants
gone is a second path to *authority*; this is a mode with no authority to grant. The loopback
enforcement is what keeps those two from converging.

### 6.6 Keeping `ui-dioxus` alive across the slice boundary

`ui-dioxus/` is workspace-excluded, so `cargo test --workspace` cannot observe it breaking — which
makes this the one part of the slice that needs deliberate attention rather than test coverage.
Three touch points, all minimal:

| File | Change |
|---|---|
| `ui-dioxus/src/desktop_boot.rs:130,219` | Stop minting a secret and setting `OTTO_TOKEN`; pass `--single-user` instead. The desktop app is a loopback sidecar — exactly §6.5's shape. |
| `ui-dioxus/src/net/url.rs:19` | Stop appending `?token=…`; the parameter no longer authenticates anything (§6.4). |
| `ui-dioxus/src/components/connection_form.rs` | The token field and `redact_token` **stay**, unused, for the remote-server path. Slice 2 replaces them with the login flow. A code comment says so, so the next reader does not delete them as dead. |

**The honest limitation:** the *desktop* app keeps working end-to-end; the *browser* path against a
remote authenticated `otto serve` does not, because performing `Login` needs the slice-2 UI. That is
a dev-served bundle rather than a shipped artifact, and it is stated in §10 rather than glossed.
Verification is out-of-band by construction (success criterion 7).

---

## 7. `engine` — resolving a principal

### 7.1 `EngineService`

Every public method taking a `SessionId` also takes `owner: &UserId` and calls a private
`authorize(owner, session)` first, which compares against `store.owner_of(session)` and returns the
shared not-found error on mismatch: `run_prompt`, `run_prompt_with_controls`,
`run_command_with_controls`, `run_agent_with_controls`, `abort`, `export_promotion`.
`create_session` takes the owner and passes it through.

`accept_promotion`/`accept_demotion` take the owner from the bundle's `SessionState` (§4.2) — they
are machine-to-machine and have no connected principal.

`workspace_rpc` is unchanged in signature: per premise correction 2 there is nothing session-scoped
to check. Its caller authenticates (§7.3).

### 7.2 The WS handshake

`ws_handler` no longer authenticates at upgrade — it accepts the socket, optionally pre-resolving a
principal from an `Authorization: Bearer` header for non-browser clients. `handle_socket` then:

1. If a header principal was resolved, skip to step 4.
2. Await the first frame under a **10-second deadline** (`tokio::time::timeout`). Nothing else is
   read, no session is resolved, and no store call is made until this succeeds — an unauthenticated
   socket costs one task and one timer.
3. The frame must be `Login` or `Attach`. `Login` runs the `Authenticator` and mints a pair;
   `Attach` verifies an existing access token. Success sends `LoggedIn` (for `Login`) and binds the
   principal to the connection. Failure sends `Error { "authentication failed" }` and closes. **Any
   other command before authentication is the same failure** — including `CreateSession`.
4. Only now is `resolve_session` called, and it is ownership-checked: an explicit `?session=` that
   the principal does not own fails exactly as a nonexistent one does. `None` creates a session
   owned by the principal.
5. `Ready` is sent, `last_seq` replay runs through the owner-scoped `replay_since`, and the command
   loop starts.

Inside the loop, `Logout` denylists the connection's `jti`, revokes its refresh token, aborts that
principal's in-flight turn, sends `LoggedOut`, and closes — the connection does not continue
unauthenticated. `Refresh` rotates and replies with a fresh `LoggedIn`.

The access token is re-verified (signature, `exp`, denylist) **on each command**, not only at
handshake. Otherwise a long-lived socket outlives both expiry and revocation, which would make the
denylist decorative on exactly the connections that matter most.

### 7.3 The three HTTP routes

- **`POST /workspace`** — requires a valid access token (`Authorization: Bearer`), verified
  identically to the WS path. Authenticated, not isolated (§2).
- **`POST /promote` / `POST /export`** — keep the machine credential (§6.4), constant-time compared,
  behind the existing `--accept-promotions` gate. Ownership travels in the bundle. The user-facing
  ownership check for handover is on the **source**, in the WS command loop, where a real principal
  exists: `PromoteToRemote`/`DemoteToLocal` verify the connection's principal owns the session
  before any bundle is built.

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
first connection. The check does not apply to `--single-user`, which has no principals by design
(§6.5). Per §9.1 this guard and §6.5's loopback enforcement are pure functions rather than inline
`cmd_serve` code, so both are testable.

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

`create_session(owner, …)`, the owner-scoped reads, and the `/workspace` auth change reach **six**
integration test files, each of which builds a router/service directly rather than going through
`cmd_serve`, so each needs updating: `crates/engine/tests/{serve,cors,ui_dir,promote,remote_workspace,vps_promote,microvm}.rs`.
Two consequences the plan must budget for rather than discover:

1. **Those harnesses need an `Authenticator`.** They get a `FakeAuthenticator` — a test double in
   `otto-auth` behind `#[cfg(feature = "testing")]` (or a `dev-dependencies`-only module) that
   accepts a fixed credential and returns a fixed `Principal`. Not `SqliteAuthStore`: the suite must
   stay hermetic and fast, and — critically — **A5's `OTTO_AUTH_DB` default must never be consulted
   by a test.** Every constructor takes an explicit path or an explicit authenticator, so a
   developer's real auth database can never be opened, written, or depended on by `cargo test`.
2. **§7.4's zero-principal startup refusal and §6.5's loopback enforcement live in `cmd_serve`,
   which no test exercises.** Both must therefore be extracted as pure, unit-testable functions
   (in the shape of the existing `validate_ui_dir` at `crates/engine/src/main.rs:141`) rather than
   written inline — otherwise two security-relevant guards ship untested.

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
