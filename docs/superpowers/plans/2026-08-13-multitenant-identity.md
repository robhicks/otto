# Multitenant Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give otto a first-class user identity established explicitly by a client, carried on every request, and enforced as the owner of every session — via an `Authenticator` seam in `engine-core`, a new `otto-auth` crate (RFC 6238 TOTP, HS256 JWT with refresh rotation and a `jti` denylist), the `Command::Login`/`Attach`/`Refresh`/`Logout` vocabulary plus `ServerMessage::Hello`/`LoggedIn`/`LoggedOut`, principal resolution on every `serve.rs` route, `otto auth enroll/list/revoke`, and `otto serve --single-user` / `--promotion-receiver` modes — while leaving the offline single-user `otto run` path byte-for-byte unchanged.

**Architecture:** Five crates in strict inward order (`protocol` → `engine-core` → `auth` → `persistence` (already done) → `engine`), plus the workspace-excluded `ui-dioxus/`. Slice 1a (shipped, #123/#124) already delivered `UserId`, the `sessions.owner` column, owner-scoped `SessionStore` reads, `EngineService::authorize`/`authorize_session`, and `run_goal`'s `UserId::local()` — so this plan builds the identity half on top of a finished ownership foundation. `ServeState` gains an `AuthConfig` (mode + `Arc<dyn Authenticator>` + promotion secret); `handle_socket` resolves one connection-scoped principal and threads it through the command loop; every HTTP route authenticates per its mode. The auth store is a second sqlite database in the OS data dir (`OTTO_AUTH_DB`, per A5), never in the workspace, so the sensitive-path floor is untouched.

**Tech Stack:** Rust (edition 2024, toolchain pinned, `rust-version = "1.85"`), `sqlx` 0.8, `jsonwebtoken` **10.3** `default-features = false, features = ["rust_crypto"]` (pinned per the issue's verified MSRV constraint — 11.x declares `rust-version 1.88` and silently breaks the MSRV; the `rust_crypto` backend is mandatory, see Task 4), `hmac = "0.12"`, `sha1 = "0.10"`, `sha2` (refresh hashing), `subtle` (constant-time compare), `data-encoding` (base32), `rand` (secrets), `qrcode` (terminal QR), `anyhow`, `async-trait`, `serde`, `tokio`, `tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-08-01-multitenant-identity-design.md` — read it first. This plan implements it exactly, including the five resolved open issues (§6.5 `--promotion-receiver` opt-in + zero-principal condition; §7.3 per-mode `/workspace` credential table; §7.2 `Hello` always-first; §6.6 widened `ui-dioxus` table; §6.4 corrected Fly citation).

## Global Constraints

- **Dependency flow stays strictly inward** — `protocol` gains no otto dependency; `engine-core` gains **no crypto dependency** (the seam is a trait + plain `String`/`u64` types); `auth` depends on `protocol` + `engine-core` + the crypto crates; `engine` adds `otto-auth`. `engine-core` must never depend on `otto-auth`.
- **The security spine is untouched:** no change to the sensitive-path floor, gated fail-closed Coder edits, or `bash`-only-when-sandboxed. The auth store lives outside the workspace root (A5), so no floor widening is needed.
- **Determinism holds:** no `OTTO_*` read in core logic; the offline suite needs no network, no keys, no auth database. Every serve constructor takes an explicit auth path or an explicit `Authenticator` — `OTTO_AUTH_DB` must never be consulted by a test.
- **`ui-dioxus/` is workspace-excluded**; `cargo build/test --workspace` must never require `dx`. The desktop suite (`cd ui-dioxus && cargo test --features desktop`) is the out-of-band verification for the UI half.
- **No Claude/AI self-attribution** in any commit, comment, or doc.
- Run `cargo fmt --all` before **every** Rust commit; `cargo clippy --workspace --all-targets` before merge.
- **Known pre-existing failure:** `otto-mcp-lsp`'s `rust_analyzer_integration::full_round_trip_against_a_real_rust_analyzer` fails on `main` already (verified). It is not a regression from this work.
- **Security decisions from the spec, non-negotiable:** `Credentials::Debug` redacts the code; `Command`/`ServerMessage` `Debug` redact tokens; failed auth returns one opaque `"authentication failed"` string (A11); TOTP replay rejects `step <= last_step` with a conditional `UPDATE ... WHERE last_step < ?`; JWT verification checks signature → `exp` → denylist in that order; `--single-user` requires a loopback bind; `--promotion-receiver` refuses to start if any principal is enrolled; `otto auth {enroll,list,revoke}` refuse `local`.
- **Versioned contract:** `jsonwebtoken = { version = "=10.3", default-features = false, features = ["rust_crypto"] }` (exact, per the issue's verified MSRV constraint — and the `rust_crypto` backend is mandatory, see Task 4); `hmac = "0.12"`, `sha1 = "0.10"`, `sha2 = "0.10"` reuse the locked `digest 0.10.x` tree.

## Plan-critique resolutions (round 1, applied to this file)

Two blocking findings from plan review, both resolved here; each was confirmed against the worktree.

1. **The seam had no reachable token lifecycle.** The spec's `Authenticator` (`engine-core/src/auth.rs`, §5) exposed only `authenticate`, but `serve.rs` reaches the backend only as `Arc<dyn Authenticator>` and must mint (`Login`), verify (`Attach` + per-command re-check + `/workspace`), rotate (`Refresh`), and revoke (`Logout`) — a concrete `TotpAuthenticator`'s extra methods would be unreachable through the trait object. **Resolution:** widen the seam itself with the four lifecycle methods, all expressed in plain `String`/`u64`/`TokenPair` — so the no-crypto-in-engine-core rule (§1) holds unchanged. The spec's §5 has been updated to match. `FakeAuthenticator` implements the full widened seam, which is what makes Task 6's `login_reaches_ready_and_mints` / `attach_with_a_minted_token` / `logout_invalidates_on_ws_and_workspace` tests implementable.
2. **`testing` as a default-off feature breaks every harness.** A `#[cfg(feature = "testing")]` gate + engine feature-forwarding would leave `cargo test --workspace` unable to compile the seven harnesses (which reference `otto_auth::testing::FakeAuthenticator` unconditionally) and would also break `cargo test -p otto-auth` (Task 5's own gate). **Resolution:** `FakeAuthenticator` lives in a plain `pub mod testing` with **no feature gate** — following the `ScriptedProvider` precedent (a test double in an impl crate's public API, always compiled, does no I/O). No `testing` feature is added anywhere. The spec's §9.1 has been updated to match.

Advisory findings folded in: Task 1 hand-writes `Debug` for the whole `ServerMessage` enum (Rust has no per-variant `Debug`, and the current derived impl already leaks `Promoted.token` — redacting it closes a pre-existing leak for free) **and adds the four new `Command` variants to `serve.rs`'s exhaustive match** (it has no wildcard arm, so Task 1 would otherwise break the library build — the arms reply `Error` until Task 6 wires real auth); Task 3 reuses `persistence`'s `BUSY_BUDGET` bounded busy-retry for `SqliteAuthStore::open` (`otto auth enroll` and `otto serve` racing on a fresh auth DB is exactly that failure mode); Task 4 pins `jsonwebtoken` as `default-features = false, features = ["rust_crypto"]` (10.3.0 has **no** `hmac`/`sha2` features — they are internal deps of the `rust_crypto` feature — and without a crypto backend the crate panics at first sign/verify; HS256-only enforcement comes from the `Validation` config, not the feature set); Task 5's `FakeAuthenticator` gets a `pub fn new(user: UserId)` constructor (its fields are private and both `tests/auth.rs` and the harnesses must construct it, two principals for the cross-tenant tests); Task 6's auth tests land in a **new** `crates/engine/tests/auth.rs` so the task stays compile-green independent of the still-unmigrated `serve.rs` harnesses (the deliberate red window between Tasks 6 and 9 is stated below, not papered over); Task 6's handshake deadline is injectable via `AuthConfig.handshake_deadline` so the timeout tests do not block 10s each; Task 10 updates a **fourth** desktop_boot test (`serve_command_carries_the_given_binary_root_and_token` at `desktop_boot.rs:533`, which asserts the `OTTO_TOKEN` env at `:540`).

## File Structure

| File | Responsibility |
|---|---|
| `crates/protocol/src/lib.rs` | **Modify.** `Credentials`, `AuthMode`, `Command::{Login,Attach,Refresh,Logout}`, `ServerMessage::{Hello,LoggedIn,LoggedOut}`; hand-written redacting `Debug` for the whole `Command`/`ServerMessage` enums + `Credentials`. |
| `crates/engine-core/src/lib.rs` | **Modify.** `pub mod auth;` — re-export the seam types. |
| `crates/engine-core/src/auth.rs` | **Create.** `Authenticator` (identity + token lifecycle), `Principal`, `AuthError`, `TokenPair`, `AuthConfig` — Send + Sync + async, trait-object-friendly, zero crypto deps. |
| `crates/auth/Cargo.toml` | **Create.** New workspace member `otto-auth`. |
| `crates/auth/src/lib.rs` | **Create.** Crate root; re-exports. |
| `crates/auth/src/totp.rs` | **Create.** RFC 6238 (HMAC-SHA1, dynamic truncation, ±1 skew, replay window, lockout, `Clock` trait). |
| `crates/auth/src/jwt.rs` | **Create.** `JwtIssuer` (HS256, `sub`/`iat`/`exp`/`jti`/`kid`, verify signature→exp→denylist). |
| `crates/auth/src/store.rs` | **Create.** `AuthStore` trait + `SqliteAuthStore` (users, signing_keys, refresh_tokens, denylist; `user_version` guard without the legacy arm). |
| `Cargo.toml` | **Modify.** Add `crates/auth` to the workspace members. |
| `crates/persistence` | **No change.** Slice 1a shipped all of §4. |
| `crates/engine/src/serve.rs` | **Modify.** `AuthConfig` in `ServeState`; constant-time compare; `Hello` + `Login`/`Attach`/`Refresh`/`Logout` handshake; connection-scoped principal threaded through the loop; `?token=` deleted; per-mode route auth; `/workspace` per-mode credential; `resolve_session` returns `(SessionId, UserId)` with attach-time ownership. |
| `crates/engine/src/service.rs` | **Modify.** `authorize`/`authorize_session` already exist; keep store() accessor documented. |
| `crates/engine/src/main.rs` | **Modify.** `--single-user`, `--promotion-receiver`; `OTTO_TOKEN` → `OTTO_PROMOTION_SECRET` (required only for promote/promotion-receiver); `otto auth` subcommands; pure guard functions (loopback predicate, zero-principal, promotion-receiver preconditions). |
| `crates/engine/src/loopback.rs` | **Modify.** Thread `AuthConfig` through `LoopbackTarget::new` and `serve::app`. |
| `crates/engine/src/lib.rs` | **Modify.** Re-export `auth` seam if needed; keep `run_goal` untouched (already `UserId::local()`). |
| `crates/remote/src/fly.rs` | **Modify.** `create_machine_body` env map `OTTO_TOKEN` → `OTTO_PROMOTION_SECRET`. |
| `crates/engine/tests/auth.rs` | **Create.** New auth integration harness (Task 6's own tests). |
| `crates/engine/tests/{serve,cors,ui_dir,promote,remote_workspace,vps_promote,microvm}.rs` | **Modify.** Adopt `AuthConfig`/`FakeAuthenticator`; `?token=` auth tests rewritten to prove the query path inert; `/workspace` tests per-mode. |
| `ui-dioxus/src/desktop_boot.rs` | **Modify.** Stop minting + `OTTO_TOKEN`; add `--single-user`; keep `--promote-loopback`. |
| `ui-dioxus/src/net/url.rs` | **Modify.** `build_ws_url` drops the token argument / `?token=`; `LaunchParams.token` stays (empty on desktop). |
| `ui-dioxus/src/app.rs` | **Modify.** Add `Hello`/`LoggedIn`/`LoggedOut` arms to the exhaustive match; drop `tok.is_empty()` gates in `do_connect`/`load_files`/`open_path`. |
| `ui-dioxus/src/components/connection_form.rs` | **Modify.** Comment only (token field stays, unused, for the remote path). |
| `deploy/fly/Dockerfile` | **Modify.** `CMD` gains `--promotion-receiver`; comment `OTTO_TOKEN` → `OTTO_PROMOTION_SECRET`. |
| `CLAUDE.md`, `README.md`, `deploy/fly/README.md` | **Modify.** Rename `OTTO_TOKEN` → `OTTO_PROMOTION_SECRET`; document `--single-user`/`--promotion-receiver`/`otto auth`. |
| `docs/superpowers/specs/2026-08-01-multitenant-identity-design.md` | **Modify.** Status → IMPLEMENTED at close-out. |

## Task Order & Rationale

Forced by the inward dependency rule: `protocol` (Task 1) → `engine-core` (Task 2) → `auth` (Tasks 3–5) → `engine` wiring (Tasks 6–9) → `ui-dioxus` + deploy + docs (Tasks 10–12). Each task leaves `cargo build` green for the crates touched so far, and `cargo test` green for those crates' own suites.

**One deliberate exception is stated, not hidden:** between Task 6 (which changes the `serve_app`/`serve::app` signatures and deletes `?token=`) and Task 9 (which migrates the seven pre-existing integration harnesses), `cargo test --workspace` is **red** — the pre-existing `tests/{serve,cors,ui_dir,promote,remote_workspace,vps_promote,microvm}.rs` files do not compile against the new signatures. That window is intended: Task 6's auth tests live in a new `tests/auth.rs` precisely so the task can be verified independently, and Task 9 closes the window in one commit. The `--lib` run in Task 6 Step 5 confirms the breakage is confined to integration files, never the library.

**Task 1** lands the wire vocabulary. **Task 2** lands the widened seam (no impl). **Tasks 3–5** build `otto-auth` crate-up (store → TOTP/JWT → the `TotpAuthenticator` + `FakeAuthenticator`). **Task 6** rewires `serve.rs` to the three modes, deleting `?token=`; this is the largest task and the one whose integration tests are the spec's success criteria 2–4. **Task 7** adds the CLI (`otto auth`, `--single-user`, `--promotion-receiver`, the rename) and the pure guard functions. **Task 8** threads `AuthConfig` through loopback. **Task 9** migrates the seven pre-existing integration harnesses and closes the red window. **Task 10** keeps the desktop app alive. **Tasks 11–12** deploy + docs.

---

### Task 1: `protocol` — the identity wire vocabulary

**Files:**
- Modify: `crates/protocol/src/lib.rs`

**Interfaces:**
- Consumes: `UserId` (shipped in slice 1a).
- Produces, used by every later task:
  - `pub enum Credentials { Totp { user: UserId, code: String } }` — hand `Debug` redacting `code`.
  - `pub enum AuthMode { SingleUser, Users, Machine }` — `#[serde(rename_all = "snake_case")]`.
  - `Command::{Login { credentials: Credentials }, Attach { token: String }, Refresh { refresh_token: String }, Logout}` — hand `Debug` redacting `Attach.token`/`Refresh.refresh_token` (externally tagged, matching the enum).
  - `ServerMessage::{Hello { auth_mode: AuthMode }, LoggedIn { user, access_token, expires_at: u64, refresh_token }, LoggedOut}` — internally tagged `#[serde(tag = "type", rename_all = "snake_case")]`; `LoggedIn`'s `Debug` redacts both tokens.

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)]` module in `crates/protocol/src/lib.rs`:

```rust
#[test]
fn login_command_round_trips_externally_tagged() {
    let cmd = Command::Login { credentials: Credentials::Totp {
        user: UserId::parse("alice").unwrap(),
        code: "123456".into(),
    } };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.starts_with(r#"{"Login":{"credentials":{"Totp":{"user":"alice","code":"123456"}}}}"#));
    assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
}

#[test]
fn logged_in_frame_is_internally_tagged_and_redacts_debug() {
    let frame = ServerMessage::LoggedIn {
        user: UserId::parse("alice").unwrap(),
        access_token: "access-secret-token".into(),
        expires_at: 1_700_000_000,
        refresh_token: "refresh-secret-token".into(),
    };
    let json = serde_json::to_string(&frame).unwrap();
    assert!(json.starts_with(r#"{"type":"logged_in","user":"alice""#));
    assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), frame);

    let dbg = format!("{frame:?}");
    assert!(!dbg.contains("access-secret-token") && !dbg.contains("refresh-secret-token"),
        "tokens leaked in Debug: {dbg}");
}

#[test]
fn command_debug_redacts_attach_and_refresh() {
    let attach = format!("{:?}", Command::Attach { token: "secret-token".into() });
    assert!(!attach.contains("secret-token"));
    let refresh = format!("{:?}", Command::Refresh { refresh_token: "secret-refresh".into() });
    assert!(!refresh.contains("secret-refresh"));
    let login = format!("{:?}", Command::Login { credentials: Credentials::Totp {
        user: UserId::parse("alice").unwrap(), code: "654321".into() } });
    assert!(!login.contains("654321"));
}

#[test]
fn server_message_debug_redacts_promoted_token() {
    // The hand-written `Debug` for the whole enum must also close the pre-existing
    // `Promoted.token` leak in the derived impl it replaces.
    let promoted = ServerMessage::Promoted {
        session: SessionId::new(),
        endpoint: "ws://x".into(),
        token: Some("fly-secret".into()),
    };
    let dbg = format!("{promoted:?}");
    assert!(!dbg.contains("fly-secret"), "Promoted.token leaked in Debug: {dbg}");
}

#[test]
fn hello_frame_carries_the_mode_snake_cased() {
    let hello = ServerMessage::Hello { auth_mode: AuthMode::SingleUser };
    let json = serde_json::to_string(&hello).unwrap();
    assert_eq!(json, r#"{"type":"hello","auth_mode":"single_user"}"#);
    assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), hello);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-protocol`
Expected: FAIL to compile — `Credentials`/`AuthMode`/new variants not found.

- [ ] **Step 3: Implement**

Add `Credentials` and `AuthMode` near `UserId`'s re-export; add the four `Command` variants and three `ServerMessage` variants. Follow the existing serde conventions (external for `Command`, `tag = "type"` for `ServerMessage`).

**Hand-written redacting `Debug` impls.** Rust has no per-variant `Debug`, so replacing `LoggedIn`'s derived impl means hand-writing `Debug` for the **whole** `ServerMessage` enum — the match must cover `Ready`/`Event`/`Error`/`Promoted`/`Demoted` as well as the three new variants, redacting `LoggedIn.access_token`/`refresh_token` and — because the current derived impl already leaks it into any log line or panic that formats a frame — `Promoted.token` too (`protocol/src/lib.rs:168-172`). Same for `Command` (whole enum, redacting `Attach.token`/`Refresh.refresh_token`) and `Credentials` (redacting `code`). `Event`'s inner `EventKind` keeps its derived `Debug` (no secrets there). Redaction writes `<redacted>` in place of the secret field.

**Keep `cargo build --workspace` green.** `serve.rs`'s command loop matches on `Command` **exhaustively with no wildcard arm** (`crates/engine/src/serve.rs:624`, all ten variants at `:625-740`), and `serve.rs` is library code (`mod serve;` at `crates/engine/src/lib.rs:32`), not an integration test. So this task must also add the four new `Command` variants to that match. They are not-applicable until Task 6 wires auth, so each arm replies `Error { "authentication not enabled" }` and closes (or `break`s the loop), mirroring how an unauthenticated command is treated. This is what keeps Step 5's SUCCESS expectation true; without it the workspace build fails with a non-exhaustive-match E0004.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-protocol`
Expected: PASS — new tests + all pre-existing.

- [ ] **Step 5: Verify the workspace still builds**

Run: `cargo build --workspace`
Expected: SUCCESS — `serve.rs`'s four new `Command` arms (added in Step 3) keep the library compiling; nothing outside `protocol` uses the new types yet (`ui-dioxus`'s exhaustive `ServerMessage` match is workspace-excluded; it is fixed in Task 10).

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add crates/protocol/src/lib.rs crates/engine/src/serve.rs
git commit -m "protocol: add the identity wire vocabulary (Login/Attach/Refresh/Logout, Hello/LoggedIn/LoggedOut)"
```

---

### Task 2: `engine-core` — the `Authenticator` seam (identity + token lifecycle)

**Files:**
- Create: `crates/engine-core/src/auth.rs`
- Modify: `crates/engine-core/src/lib.rs`

**Interfaces:**
- Consumes: `otto_protocol::{UserId, Credentials, AuthMode}`.
- Produces, used by Task 6:
  - `pub struct Principal { pub user: UserId }` — `Debug + Clone + PartialEq + Eq`.
  - `pub enum AuthError { InvalidCredentials, RateLimited { retry_after_secs: u64 }, Backend(anyhow::Error) }`.
  - `pub struct TokenPair { pub access_token: String, pub refresh_token: String, pub expires_at: u64 }` — plain wire types, no crypto.
  - `pub struct AuthConfig { pub mode: AuthMode, pub authenticator: Option<Arc<dyn Authenticator>>, pub promotion_secret: Option<String>, pub handshake_deadline: Duration }` — the one struct the serve constructors take (Tasks 6/8). `authenticator` is `Some` only for `Users`; `promotion_secret` `Some` only for `Machine`/`--promote-*`; `handshake_deadline` defaults to 10s and is injectable so the timeout tests do not block 10s each.
  - `#[async_trait] pub trait Authenticator: Send + Sync` with **the full token lifecycle**, per the spec's §5 (plan-critique resolution — serve.rs reaches the authenticator only as `Arc<dyn Authenticator>`, so mint/verify/rotate/revoke must be on the seam, all in plain `String`/`u64` types to keep engine-core crypto-free):
    - `async fn authenticate(&self, creds: &Credentials) -> Result<Principal, AuthError>` — identity verification (Login).
    - `async fn mint(&self, principal: &Principal) -> Result<TokenPair, AuthError>` — after a successful Login.
    - `async fn verify_access(&self, token: &str) -> Result<Principal, AuthError>` — Attach, per-command re-check, `/workspace`.
    - `async fn rotate_refresh(&self, refresh_token: &str) -> Result<TokenPair, AuthError>` — Refresh; single-use.
    - `async fn logout(&self, access_token: &str) -> Result<(), AuthError>` — Logout; denylist `jti` + revoke refresh.

- [ ] **Step 1: Write the failing test**

Create `crates/engine-core/src/auth.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_seam_is_trait_object_friendly_and_covers_the_token_lifecycle() {
        // The seam must be usable as `Arc<dyn Authenticator>` — this compiles only if the
        // trait is Send + Sync + object-safe. The Stub implements every lifecycle method.
        struct Stub;
        #[async_trait::async_trait]
        impl Authenticator for Stub {
            async fn authenticate(&self, _c: &Credentials) -> Result<Principal, AuthError> {
                Ok(Principal { user: UserId::local() })
            }
            async fn mint(&self, p: &Principal) -> Result<TokenPair, AuthError> {
                Ok(TokenPair { access_token: "at".into(), refresh_token: "rt".into(), expires_at: 0 })
            }
            async fn verify_access(&self, _t: &str) -> Result<Principal, AuthError> {
                Ok(Principal { user: UserId::local() })
            }
            async fn rotate_refresh(&self, _r: &str) -> Result<TokenPair, AuthError> {
                Ok(TokenPair { access_token: "at2".into(), refresh_token: "rt2".into(), expires_at: 0 })
            }
            async fn logout(&self, _a: &str) -> Result<(), AuthError> { Ok(()) }
        }
        let a: Arc<dyn Authenticator> = Arc::new(Stub);
        let p = a.authenticate(&Credentials::Totp { user: UserId::local(), code: "0".into() }).await.unwrap();
        assert_eq!(p.user, UserId::local());
        let pair = a.mint(&p).await.unwrap();
        assert_eq!(a.verify_access(&pair.access_token).await.unwrap().user, UserId::local());
        assert!(a.logout(&pair.access_token).await.is_ok());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p otto-engine-core auth::`
Expected: FAIL to compile — `Authenticator` not found. (`#[tokio::test]` needs `tokio` in dev-deps with `macros`+`rt`; add it to `crates/engine-core/Cargo.toml` if absent.)

- [ ] **Step 3: Implement**

`engine-core/src/auth.rs` — module doc noting (per A9's resolution and the spec's §5) that this is the one seam that must stay crypto-free; the trait; `Principal`; `AuthError`; `TokenPair`; `AuthConfig`. Wire `pub mod auth;` + `pub use auth::{Authenticator, AuthError, Principal, TokenPair, AuthConfig};` into `lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p otto-engine-core`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add crates/engine-core/src/auth.rs crates/engine-core/src/lib.rs crates/engine-core/Cargo.toml
git commit -m "engine-core: add the Authenticator seam with the token lifecycle (no crypto in the core crate)"
```

---

### Task 3: `otto-auth` — the sqlite auth store

**Files:**
- Create: `crates/auth/Cargo.toml`, `crates/auth/src/lib.rs`, `crates/auth/src/store.rs`
- Modify: root `Cargo.toml` (workspace member)

**Interfaces:**
- Consumes: `otto-protocol` (UserId), `otto-engine-core` (seam types for Task 5; store itself only needs protocol).
- Produces, used by Tasks 4–5:
  - `AuthStore` trait: `enroll_user`, `totp_secret`, `last_step`/`set_last_step` (conditional), `record_failure`/`failures_within`, `signing_key(kid)`/`insert_signing_key`, `insert_refresh`/`consume_refresh`/`revoke_user_refresh`, `denylist_insert`/`is_denylisted`/`prune_denylist`, `enrolled_count`, `list_users`, `revoke_user`.
  - `SqliteAuthStore::open(path)` with a `PRAGMA user_version` guard: fresh DB → create tables + stamp; version match → ok; version newer → refuse. **No legacy arm** (this DB is new in this slice).
  - `open` reuses `persistence`'s bounded busy-retry shape (`crates/persistence/src/sqlite.rs:58-76`): `otto auth enroll` and `otto serve` can race to create the same fresh DB, and on a fresh file both `connect_with`'s WAL transition and `init_schema`'s `BEGIN IMMEDIATE` can report `SQLITE_BUSY` despite `busy_timeout`. Wrap the whole open in the same `tokio::time::timeout(BUSY_BUDGET, ...)` loop.

- [ ] **Step 1: Write the failing tests**

Create the crate skeleton with `store.rs`'s test module covering: enroll then read the secret; the conditional `last_step` update (concurrent same-step writes: exactly one wins); failure-count/window bookkeeping; refresh insert/consume/revoke-all; denylist insert/check/prune; `enrolled_count`; a version-mismatch open error; two concurrent `open`s of the same fresh path both succeed. Use `tempfile` for the db path — **never** the `OTTO_AUTH_DB` default.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-auth store::`
Expected: FAIL to compile — crate/types not found. (Add the workspace member to `Cargo.toml` first so the package resolves.)

- [ ] **Step 3: Implement**

`SqliteAuthStore` in the same `init_schema`/`user_version` style as `persistence`'s `sqlite.rs`, but with only the create-and-stamp and forward-version arms, plus the busy-retry. Four tables: `users (id PK, totp_secret, last_step, failure_count, failure_window)`, `signing_keys (kid PK, key, created_at)`, `refresh_tokens (hash PK, user, expires_at, consumed_at)`, `denylist (jti PK, expires_at)`. Store TOTP secrets base32-encoded (RFC 4648, unpadded) and refresh tokens as SHA-256 hashes (a stolen DB yields no usable refresh token).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-auth`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add Cargo.toml crates/auth
git commit -m "auth: add the sqlite auth store (users, signing keys, refresh hashes, jti denylist)"
```

---

### Task 4: `otto-auth` — TOTP and JWT

**Files:**
- Create: `crates/auth/src/totp.rs`, `crates/auth/src/jwt.rs`

**Interfaces:**
- Consumes: `store.rs` from Task 3.
- Produces, used by Task 5:
  - `trait Clock { fn now(&self) -> u64 }` + `SystemClock` + a test fixed clock.
  - `fn totp_at(secret: &[u8], step: u64) -> String` — RFC 4226 dynamic truncation, 6 digits zero-padded.
  - `TotpVerifier` (or functions taking `now`) implementing: compute candidate steps `T-1..T+1`; **any step `<= last_step` rejected before comparison**; on success `UPDATE ... WHERE last_step < step` (exactly one concurrent winner); 5 failures within 15 min → `RateLimited` without computing.
  - `JwtIssuer` over `AuthStore`: 32-byte key generated on first use, stored keyed by `kid`; `mint(user) -> (access, refresh, expires_at)`; `verify_access(token) -> Result<Principal, _>` checking signature → `exp` → denylist in that order; `rotate_refresh(refresh) -> (access, refresh, expires_at)` single-use in a transaction, reused token revokes the user's whole refresh set; `logout(access)` inserts `jti` + `exp`.

- [ ] **Step 1: Write the failing tests**

`totp.rs` tests: **RFC 6238's published test vectors**. The RFC's Appendix B values are **8-digit** (RFC 4226) — `94287082` etc. — so they cannot be asserted against the production 6-digit formatter. Assert them against a raw truncation function exposed for the test (e.g. `fn truncate(secret: &[u8], step: u64) -> u32`, the RFC 4226 31-bit dynamic truncation, formatted `{:08}`), and assert the 6-digit zero-padded production formatting separately against a fixed step: secret `12345678901234567890`, steps `59`, `1111111109`, `1111111111`, `1234567890`, `2000000000`, `20000000000` → `94287082`, `07081804`, `14050431`, `89005924`, `69279037`, `65353130`. Then: ±1 accepted, ±2 rejected; replay (`<= last_step`) rejected; two concurrent same-step logins → exactly one succeeds; lockout after N failures. `jwt.rs` tests: mint/verify round trip; expired rejected; denylisted rejected; denylist row pruned past `exp`; refresh rotation consumes the old token and issues a new pair; a reused (already-consumed) refresh token is rejected and revokes the user's outstanding set.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-auth totp:: jwt::`
Expected: FAIL to compile — modules/types not found.

- [ ] **Step 3: Implement**

`totp.rs`: HMAC-SHA1 via `hmac` + `sha1`; counter as 8-byte big-endian; RFC 4226 truncation; the verification is a pure function of `(secret, last_step, now)` so the RFC vectors are directly testable against a fixed `Clock`. `jwt.rs`: `jsonwebtoken = { version = "=10.3", default-features = false, features = ["rust_crypto"] }`. Note: 10.3.0 has **no** `hmac`/`sha2` features — those are internal deps pulled by the `rust_crypto` feature, and with no crypto backend at all the crate panics on first sign/verify ("make sure exactly one of the 'rust_crypto' and 'aws_lc_rs' features is enabled"). HS256-only enforcement comes from the `Validation` config at the verify call site (`Validation::new(Algorithm::HS256)` — the only algorithm the signer accepts), **not** from the feature set; keep that wording accurate in the commit/docs. `kid` in the header; `sub`/`iat`/`exp`/`jti` claims; no `aud` (reserved, not emitted). Refresh tokens: 32-byte `rand` values, stored SHA-256-hashed.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-auth`
Expected: PASS.

- [ ] **Step 5: Verify the workspace still builds**

Run: `cargo build --workspace`
Expected: SUCCESS — `otto-auth` is a member but nothing depends on it yet.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add crates/auth/src/totp.rs crates/auth/src/jwt.rs crates/auth/Cargo.toml
git commit -m "auth: add RFC 6238 TOTP and HS256 JWT with refresh rotation and jti denylist"
```

---

### Task 5: `otto-auth` — the `TotpAuthenticator` + `FakeAuthenticator`

**Files:**
- Modify: `crates/auth/src/lib.rs`

**Interfaces:**
- Consumes: Tasks 2–4 (seam + store + totp + jwt).
- Produces, used by Tasks 6 and 9:
  - `pub struct TotpAuthenticator { store, jwt }` implementing the full `Authenticator` seam — `authenticate(Credentials::Totp)` → verify code → `Principal`; `mint`/`verify_access`/`rotate_refresh`/`logout` over the `JwtIssuer` + store.
  - `pub mod testing { pub struct FakeAuthenticator { principal: UserId } }` implementing the full seam with a trivial in-memory token map — `authenticate` accepts any `Credentials` and returns the fixed principal; `mint` issues a numbered fake token; `verify_access` accepts tokens this fake minted; `logout` denylists them. **It has a `pub fn new(user: UserId) -> Self` constructor** (its field is private, and both `tests/auth.rs` and the Task 9 harnesses must construct it — two distinct principals for the cross-tenant tests). **It is always compiled — no `#[cfg(feature = "testing")]` gate, no `testing` feature** (plan-critique resolution) — following the `ScriptedProvider` precedent: a test double in an impl crate's public API, harmless in the binary because it does no I/O, and required so that both `cargo test --workspace` (the seven harnesses) and `cargo test -p otto-auth` (this task's own gate) reference it with no feature flags.

- [ ] **Step 1: Write the failing tests**

`authenticate` with a wrong code → `InvalidCredentials` (never `Backend`); with a correct code → `Principal`; with the store locked → `RateLimited`. `verify_access` on a minted token → the principal; on a logged-out token → `InvalidCredentials`; `rotate_refresh` consumes the old token and mints a new pair. The `FakeAuthenticator` returns its fixed principal for any input and verifies only its own minted tokens.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-auth`
Expected: FAIL to compile — `TotpAuthenticator`/`testing` not found.

- [ ] **Step 3: Implement**

`TotpAuthenticator` glues totp + jwt + store behind the seam. `FakeAuthenticator` in a plain `pub mod testing` (no feature gate) implementing the full seam with an in-memory `Mutex<HashMap<String, UserId>>`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-auth`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add crates/auth/src/lib.rs crates/auth/Cargo.toml
git commit -m "auth: add the TotpAuthenticator behind the seam and a FakeAuthenticator test double"
```

---

### Task 6: `serve.rs` — the three modes, the handshake, per-mode routes

**Files:**
- Modify: `crates/engine/src/serve.rs`, `crates/engine/Cargo.toml`
- Create: `crates/engine/tests/auth.rs`

**Interfaces:**
- Consumes: `otto_auth::{TotpAuthenticator, testing::FakeAuthenticator}`; the seam from Task 2; the wire types from Task 1.
- Produces: the spec's success criteria 2–4, and the surface Tasks 7–9 build against.

**Design points this task must implement exactly (from the spec's resolutions):**
- `ServeState` gains `auth: AuthConfig` (the `{mode, authenticator, promotion_secret, handshake_deadline}` struct from Task 2). `authorized`/`authorized_ws` become constant-time (`subtle::ConstantTimeEq`) over `promotion_secret`.
- **`Hello { auth_mode }` is always the first frame** after upgrade, in every mode, even when the `Authorization: Bearer` header pre-resolved a principal.
- `Users`: after `Hello`, the handshake deadline (`AuthConfig.handshake_deadline`, default 10s) for `Login`/`Attach`; header pre-resolution skips the deadline but not the greeting. `Login` → `authenticator.authenticate` + `authenticator.mint` → `LoggedIn` + bind principal; `Attach` → `authenticator.verify_access` (or, on `Machine`, the promotion secret) + bind principal. Failure → `Error { "authentication failed" }` + close. Any non-auth command before authentication is the same failure.
- `SingleUser`: no deadline, no auth frame — every connection is `UserId::local()`, straight to `Ready` after `Hello`.
- `Machine`: promotion secret via header or `Attach`; connection adopts the attached session's owner; **session creation is rejected** (§7.2 step 4); `Hello` then credential then `Ready`.
- `resolve_session` returns `(SessionId, UserId)`; the explicit `?session=` arm is owner-checked (wrong owner == nonexistent); the `None` arm creates a session owned by the principal (rejected on `Machine`).
- **`?token=` is deleted** from `ConnectParams` and `authorized_ws`.
- Inside the loop: `Logout` → `authenticator.logout` (denylist `jti`, revoke refresh), abort the in-flight turn, send `LoggedOut`, close. `Refresh` → `authenticator.rotate_refresh` → reply `LoggedIn`. The access token is re-verified via `authenticator.verify_access` on each command.
- `/workspace`: per-mode table (§7.3) — `SingleUser` ignores the header; `Users` requires a valid access token (`authenticator.verify_access`); `Machine` requires the promotion secret. `/promote`/`/export`: keep the promotion secret constant-time.
- Thread the connection-scoped `owner` through `handle_socket`/`run_turn_loop`/the command arms, replacing the hard-coded `UserId::local()` calls (A9's resolution).

- [ ] **Step 1: Write the failing tests**

Create `crates/engine/tests/auth.rs` — a NEW integration file with its own harnesses, so this task does not depend on `tests/serve.rs`'s still-unmigrated constructors (plan-critique resolution: landing here keeps the task compile-green independently). The harness builds `serve_app` with a `FakeAuthenticator` + `AuthMode::Users` (short `handshake_deadline`), plus a `SingleUser` and a `Machine` app. New tests, each mapping to a success criterion:
- `no_auth_frame_times_out`: connect to a `Users` app with no header, send nothing; assert an `Error` frame then close, and the store has **no** new session.
- `wrong_code_fails_with_the_opaque_message`: `Login` with a bad credential → `Error { "authentication failed" }`, closed, no session.
- `login_reaches_ready_and_mints`: `Login` with the fake credential → `LoggedIn` then `Ready`.
- `attach_with_a_minted_token`: `Attach` with a token minted by a prior `Login` → `Ready`.
- `cross_tenant_attach_is_indistinguishable_from_nonexistent`: session owned by `alice`, connection authenticated as `bob` attaches via `?session=` → byte-identical error to a random id.
- `query_token_no_longer_authenticates`: `?token=` on the URL, no header, no auth frame → the deadline `Error` path (proving the query path inert).
- `logout_invalidates_on_ws_and_workspace`: `Login` → `Logout` → a re-`Attach` and a `/workspace` call with the old token both fail.
- `cross_tenant_replay_is_refused`: after `Login`, attach to another principal's session and `SendPrompt`; assert the shared not-found error and no replayed events (closes spec §9's serve-level replay criterion).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-engine --test auth`
Expected: FAIL to compile — `serve_app` has no auth parameters yet, `Hello`/`Login` don't exist.

- [ ] **Step 3: Implement**

Rework `serve.rs` per the design points above. Add `otto-auth` and `subtle` to `crates/engine/Cargo.toml`. Keep `handle_handover`'s `Promoted.token` logic unchanged (`None` for loopback/vps/microvm, `Some` for Fly).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-engine --test auth`
Expected: PASS.

- [ ] **Step 5: Run the library tests to find the fallout**

Run: `cargo test -p otto-engine --lib`
Expected: PASS — `EngineService`'s signatures are unchanged (slice 1a shipped them), so the library and its own test module are unaffected by this task. The seven pre-existing integration files fail to compile against the new `serve_app` signature — **expected**, they are migrated in Task 9. Confirm the failures are confined to the integration tests, not the library.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add crates/engine/src/serve.rs crates/engine/Cargo.toml crates/engine/tests/auth.rs
git commit -m "engine: resolve a principal on every serve route — Hello/Login/Attach handshake, per-mode auth"
```

---

### Task 7: `main.rs` — `otto auth`, `--single-user`, `--promotion-receiver`, the rename

**Files:**
- Modify: `crates/engine/src/main.rs`

**Interfaces:**
- Consumes: `otto_auth` store + `TotpAuthenticator` from Tasks 3–5.
- Produces:
  - `otto auth enroll <user>` / `otto auth list` / `otto auth revoke <user>` — all refuse `local`; `enroll` re-provisions an existing user unless `--force`; prints the `otpauth://` URI + terminal QR.
  - `otto serve --single-user` / `--promotion-receiver` (requires `--accept-promotions`).
  - `OTTO_TOKEN` → `OTTO_PROMOTION_SECRET`, required **only** when a `--promote-*` or `--promotion-receiver` flag is set; `otto serve` with neither requires no shared secret.
  - Pure, unit-tested guard functions (the `validate_ui_dir` shape): `resolve_bind_host_is_loopback` (parses the resolved `OTTO_HOST` default `"127.0.0.1"` as `IpAddr` and requires `is_loopback()`), `single_user_promote_modes_ok` (only `--promote-loopback`), `users_mode_has_enrolled_principals` (zero-principal refusal naming `otto auth enroll`), `promotion_receiver_preconditions`.

- [ ] **Step 1: Write the failing tests**

Add a `#[cfg(test)]` module (or extend the existing one) in `main.rs` covering the pure guards: loopback predicate accepts `::1`/`127.0.0.2`, rejects `0.0.0.0`/`localhost`/garbage; the zero-principal guard errors when the auth store reports zero enrolled and passes otherwise; the promotion-receiver precondition requires `--accept-promotions` and zero enrolled principals.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-engine --bin otto`
Expected: FAIL to compile — guard functions don't exist.

- [ ] **Step 3: Implement**

Wire the `auth` subcommand dispatcher (the hand-rolled `match` pattern `main.rs:31-58`), `cmd_auth` (enroll/list/revoke against the `OTTO_AUTH_DB` store), the two new serve flags in `cmd_serve`'s flag loop, the env rename + conditional requirement, and the guard functions called before any server setup.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-engine --bin otto`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add crates/engine/src/main.rs
git commit -m "engine: add otto auth enroll/list/revoke, --single-user and --promotion-receiver, OTTO_PROMOTION_SECRET"
```

---

### Task 8: `loopback.rs` — thread `AuthConfig`

**Files:**
- Modify: `crates/engine/src/loopback.rs`

**Interfaces:**
- Consumes: Task 6's `serve::app` signature.
- Produces: a `--single-user` loopback promote that provisions a `SingleUser` engine (spec's §6.5 + the two smaller corrections).

- [ ] **Step 1: Write the failing test**

Extend `crates/engine/tests/promote.rs`: a loopback promote from a `SingleUser` source provisions an engine whose `serve::app` is built with `AuthConfig { mode: SingleUser, .. }` (assert via the provisioned engine's `Hello` frame or the serve constructor's mode). At minimum, the existing promote round-trip test must pass with the new constructor signature.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p otto-engine --test promote`
Expected: FAIL to compile — `LoopbackTarget::new` / `serve::app` signature drift.

- [ ] **Step 3: Implement**

`LoopbackTarget::new(auth: AuthConfig, base_dir, engine_remote)`; `serve::app(service, auth: AuthConfig, capabilities, promote, accept_promotions, …)`. Thread the same mode the source uses.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-engine --test promote`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add crates/engine/src/loopback.rs crates/engine/tests/promote.rs
git commit -m "engine: thread AuthConfig through LoopbackTarget and serve::app"
```

---

### Task 9: Migrate the seven pre-existing integration harnesses

**Files:**
- Modify: `crates/engine/tests/{serve,cors,ui_dir,promote,remote_workspace,vps_promote,microvm}.rs`

**Interfaces:**
- Consumes: the new serve constructors + `FakeAuthenticator`.
- Produces: a workspace-green tree (closes the Task 6–9 red window).

- [ ] **Step 1: Migrate the harness constructors**

Each harness that calls `serve_app`/`serve_app_with_base` gains the `AuthConfig` parameter. Choose per harness:
- `serve.rs` — a `SingleUser`-mode app for the plain command-flow tests (the `?token=` tests, rewritten per Step 2); a `Users`-mode app with `FakeAuthenticator` for any token tests, matching Task 6's `tests/auth.rs`.
- `cors.rs`, `ui_dir.rs`, `remote_workspace.rs` — `SingleUser` mode (no credential, `/workspace` header ignored), keeping their existing assertions; `remote_workspace.rs`'s wrong-token test becomes a wrong-access-token test against a `Users` app or is rewritten per the per-mode table.
- `promote.rs` — **already migrated in Task 8** (its `LoopbackTarget` constructor changed there); here it only needs its `serve_app`/constructor call sites updated to the `AuthConfig` shape. It stays `SingleUser` (loopback promote inherits the source mode per §6.5) — do **not** wire `AuthMode::Machine` into it.
- `vps_promote.rs`, `microvm.rs` — keep `--promote-*` semantics; the **network receiver** side (`POST /promote` on a real `otto serve`) becomes `--promotion-receiver` (`AuthMode::Machine`) with the promotion secret, and `RemoteWorkspace::new(http_base, secret)` against a `Machine` app or a `SingleUser` one.

- [ ] **Step 2: Rewrite the `?token=` auth tests in `serve.rs`**

The five tests (`rejects_missing_token`, `rejects_wrong_token`, `authorizes_via_query_token`, `rejects_wrong_query_token`, `rejects_empty_query_token`) are rewritten, **not deleted** (spec §9): `?token=` must be proven inert. Against a `Users` app, a socket presenting only `?token=` (no header, no auth frame) hits the handshake deadline/`Error`; against a `SingleUser` app, the query parameter is ignored and `Ready` arrives.

- [ ] **Step 3: Run the workspace suite**

Run: `cargo test --workspace`
Expected: PASS except the known `mcp-lsp` rust-analyzer test. This is the commit that closes the Task 6–9 red window.

- [ ] **Step 4: Format and commit**

```bash
cargo fmt --all
git add crates/engine/tests
git commit -m "engine: migrate the serve integration harnesses to AuthConfig and FakeAuthenticator"
```

---

### Task 10: Keep `ui-dioxus` alive — desktop boot, URL, app.rs, connection form

**Files:**
- Modify: `ui-dioxus/src/desktop_boot.rs`, `ui-dioxus/src/net/url.rs`, `ui-dioxus/src/app.rs`, `ui-dioxus/src/components/connection_form.rs`

**Interfaces:**
- Consumes: the new `ServerMessage` variants (Task 1) — `app.rs`'s exhaustive match is a hard compile error until fixed.
- Produces: the desktop app still boots its sidecar with `--single-user`, reaches `Ready`, and loads the file tree over `/workspace` (spec success criterion 7).

- [ ] **Step 1: Write the failing test**

Update `ui-dioxus/src/desktop_boot.rs`'s argv/env-assertion tests — `serve_command_passes_both_desktop_capability_flags`, `serve_command_argv_is_the_expected_serve_invocation`, `serve_command_names_exactly_one_promote_mode` (at `:483,:498,:517`), **and `serve_command_carries_the_given_binary_root_and_token` at `:533`** (which asserts the `OTTO_TOKEN` env at `:540` and fails when it is dropped) — to expect `--single-user` and no `OTTO_TOKEN` env. Add a host-side unit test in the net/url tests that `build_ws_url` no longer carries `?token=`.

- [ ] **Step 2: Run to verify they fail**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: FAIL — argv mismatch; `build_ws_url` still emits `?token=`.

- [ ] **Step 3: Implement**

- `desktop_boot.rs`: stop minting the secret and setting `OTTO_TOKEN`; argv becomes `serve --port … --root … --approve-edits --promote-loopback --single-user`.
- `net/url.rs`: `build_ws_url(base, session, last_seq)` drops the token; `LaunchParams.token` stays (empty on desktop). Keep `redact_token` (still used by `SeamError::new`).
- `app.rs`: add `Hello { auth_mode }` (records the mode — a signal slice 2's login UI reads), `LoggedIn { access_token, .. }` (stores the token), `LoggedOut` (clears auth state) to the exhaustive match; drop the `tok.is_empty()` early-returns in `do_connect`/`load_files`/`open_path` — gate on connection state instead.
- `connection_form.rs`: add the comment that the token field stays, unused, for the remote-server path until slice 2.

- [ ] **Step 4: Run to verify they pass**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS.

- [ ] **Step 5: Verify the web target still compiles**

Run: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`
Expected: SUCCESS.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add ui-dioxus/src
git commit -m "ui-dioxus: run the desktop sidecar in --single-user and handle the new auth frames"
```

---

### Task 11: `deploy/` — Fly image and README

**Files:**
- Modify: `deploy/fly/Dockerfile`, `deploy/fly/README.md`

- [ ] **Step 1: Make the change**

Dockerfile: `CMD` gains `--promotion-receiver`; the `OTTO_TOKEN` comment becomes `OTTO_PROMOTION_SECRET`. README: document the rename and the flag. (The actual injected env rename is in `crates/remote/src/fly.rs` — see Task 12.)

- [ ] **Step 2: Verify**

The change is text-only in the Dockerfile; the injected-env rename is Task 12's `fly.rs` edit. **The Fly axis is fully wired only after Tasks 7 + 11 + 12 land together:** the guest's `--promotion-receiver` flag (11) needs Task 7's flag + `OTTO_PROMOTION_SECRET` read in `main.rs`, and the injected env name (12) must match the Dockerfile comment (11) or the promoted machine will not start. Note that `deploy/fly/Dockerfile` has **no** `OTTO_TOKEN` `ENV` line (the reference at `:51-54` is a comment); the env injection lives in `create_machine_body`'s map (`crates/remote/src/fly.rs`).

- [ ] **Step 3: Format and commit**

```bash
git add deploy/fly
git commit -m "deploy: run the Fly guest in --promotion-receiver mode"
```

---

### Task 12: `remote/fly.rs` env rename + docs (CLAUDE.md, README)

**Files:**
- Modify: `crates/remote/src/fly.rs` (env map in `create_machine_body`), `CLAUDE.md`, `README.md`

- [ ] **Step 1: Make the change**

`create_machine_body`'s `env` map injects `OTTO_PROMOTION_SECRET` (not `OTTO_TOKEN`). Update `CLAUDE.md` (the `engine` crate-table row, the serve command block, and a multitenancy note) and `README.md` for the rename and the two new modes.

- [ ] **Step 2: Verify**

Run: `cargo test -p otto-remote` → PASS (wiremock tests unaffected by the env name). Confirm the docs no longer tell an operator to set `OTTO_TOKEN`.

- [ ] **Step 3: Format and commit**

```bash
cargo fmt --all
git add crates/remote/src/fly.rs CLAUDE.md README.md
git commit -m "remote: inject OTTO_PROMOTION_SECRET into Fly machines and document the auth modes"
```

---

## Phase 5 out-of-band verification (after merge)

- **`ui-dioxus` desktop suite** — `cd ui-dioxus && cargo test --features desktop` (Task 10's gate).
- **wasm bundle** — `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web` (compile check; `./scripts/build-web.sh` if a release bundle is desired).
- **`FakeAuthenticator` always-compiled** — no feature flags needed anywhere; verified implicitly by `cargo test --workspace` compiling the harnesses with default features.
- **Fly image** — `docker build -f deploy/fly/Dockerfile .` must succeed and the guest's `CMD` must include `--promotion-receiver`. This is the changed deploy surface.
- **Determinism** — `cargo test --workspace` with no env set (the default run) stays green; `otto run` is byte-for-byte unchanged (spec success criterion 6).
