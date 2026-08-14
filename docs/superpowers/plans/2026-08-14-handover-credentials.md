# Handover Credentials Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Narrow the handover credential from a machine-wide secret to a per-session secret. `ServerMessage::Promoted.token` becomes a required `String` (always present); every `RemoteTarget` mints a fresh opaque per-session secret at provision time, delivered to the receiver over the provisioning channel (machine env for Fly, the restore-push for vps/microvm) and reported to the client via `Promoted.token`; a long-lived `--accept-promotions`/`--promotion-receiver` receiver keeps a session→secret map consulted by `/export`, `/workspace`, and the `Machine`-mode WS handshake; secrets are disposed when the session is demoted. Carries the slice-1a review items: an actionable `/promote` 400 for pre-ownership bundles, an owner-match check in `accept_demotion`, and a documented source/client-side-only contract for `RemoteWorkspace`.

**Architecture:** The credential is minted by the **source's** target (the pusher), delivered to the remote over the provisioning channel, and relayed to the client as `Promoted.token`. Receivers record `session → secret` at `/promote` time and verify it on every session-scoped operation. Ephemeral single-session machines (Fly/microVM) have machine-secret == session-secret by construction; a long-lived VPS receiver keeps a real map. `PromoteBundle` stays a pure data payload — the secret rides an `X-Otto-Session-Secret` HTTP header on the restore-push, never inside the bundle (which is also the `/export` return type). The map lives in `ServeState` (transport state, like the existing `remotes` map), never in the workspace, so the sensitive-path floor is untouched.

**Tech Stack:** Rust (edition 2024, pinned toolchain), existing deps only — `uuid` (mint), `subtle` (constant-time compare, already a dep), `serde`, `axum`, `wiremock` (tests), `tempfile` (tests). No new crate, no new dependency.

**Spec:** `docs/superpowers/specs/2026-08-14-handover-credentials-design.md` — read it first. This plan implements it exactly, including the two resolved review rounds (success criterion 3's `/workspace` exemption; `#[serde(default)]` removed from `Promoted.token`; microVM mint pinned to the serve.rs construction site; the §6 `do_connect` claim corrected to the `/workspace` RPCs).

## Global Constraints

- **Dependency flow stays strictly inward.** `protocol` gains nothing; `remote` stays dependent on `protocol` + `persistence`; `engine` depends on both and re-exports `remote`'s new `mint_session_secret`. `engine-core` is untouched by this slice.
- **The security spine is untouched:** no change to the sensitive-path floor, gated fail-closed Coder edits, `bash`-only-when-sandboxed, or the unauthenticated-by-design `--ui-dir` static route. The session-secret map lives in `ServeState`, never in the workspace; no path formats or logs it.
- **Constant-time compares everywhere a credential is compared.** `subtle::ConstantTimeEq` for the session-secret checks, exactly like the existing `authorized`/`secret_matches`.
- **Determinism holds:** no `OTTO_*` read in core logic; `mint_session_secret` is a pure RNG call at handover time (not in the offline spine); the offline suite needs no network, no keys, no auth database.
- **`ui-dioxus/` is workspace-excluded;** `cargo build/test --workspace` must never require `dx`. The desktop suite (`cd ui-dioxus && cargo test --features desktop`) and the wasm check (`cargo build --target wasm32-unknown-unknown --features web`) are the out-of-band verification for the UI half.
- **No Claude/AI self-attribution** in any commit, comment, or doc.
- Run `cargo fmt --all` before **every** Rust commit; `cargo clippy --workspace --all-targets` before merge.
- **Known pre-existing failure:** `otto-mcp-lsp`'s `full_round_trip_against_a_real_rust_analyzer` fails on `main` already. It is not a regression from this work.
- **Semver is a hard requirement.** Two breaking changes, each bumped in the same PR that makes them, and each flagged to the architect reviewer (spec §8):
  - `protocol` 0.1.0 → 0.2.0 — `ServerMessage::Promoted.token: Option<String>` → `String`, `#[serde(default)]` removed (Task 1).
  - `remote` 0.1.0 → 0.2.0 — `VpsTarget::export` removed (Task 2).
  `engine`'s own API is unchanged (`serve::app`, `LoopbackTarget::new`, `serve_app_with_base` signatures stay); it re-exports the changed types but does not bump. The `ui-dioxus` `protocol` dependency is a `path =` dep, so no version string to update there.
- **The `X-Otto-Session-Secret` header is required on `/promote`** (spec A2) — a pusher that omits it gets a 400. Fail-closed.

## File Structure

| File | Responsibility |
|---|---|
| `crates/protocol/src/lib.rs` | **Modify.** `Promoted.token: Option<String>` → `String`, drop `#[serde(default)]`, update the `Debug` arm and tests; add null/missing-token deserialize-failure tests. |
| `crates/protocol/Cargo.toml` | **Modify.** `version` 0.1.0 → 0.2.0. |
| `crates/remote/src/lib.rs` | **Modify.** `pub fn mint_session_secret()` (moved from `fly.rs`); `push_promote_bundle` gains a `session_secret` param + header; `VpsTarget::provision` mints + pushes with the header; `VpsTarget::export` **removed**. |
| `crates/remote/src/fly.rs` | **Modify.** `mint_token` deleted, callers/tests use `crate::mint_session_secret`; `provision` pushes with the header. |
| `crates/remote/Cargo.toml` | **Modify.** `version` 0.1.0 → 0.2.0. |
| `crates/engine/src/serve.rs` | **Modify.** `ServeState.session_secrets` map + helpers; `promote_handler` (header + pre-ownership special-case + record); `export_handler` (session-secret auth + dispose); `workspace_handler` `Machine` arm → membership; `authenticate_connection`/`handshake_frame` session-secret verification; vps demote arm uses the stored handle; `FirecrackerProvisioner::new(config, mint_session_secret())`; `handle_handover` sends `token: tok` unconditionally. |
| `crates/engine/src/loopback.rs` | **Modify.** `provision` mints a fresh secret; handle token = the mint. |
| `crates/engine/src/service.rs` | **Modify.** `accept_demotion` owner-match refusal + unit tests. |
| `crates/engine/src/lib.rs` | **Modify.** Re-export `mint_session_secret`. |
| `crates/workspace/src/remote.rs` | **Modify.** Doc contract on `RemoteWorkspace::new` (source/client-side only). |
| `crates/engine/tests/vps_promote.rs` | **Modify.** Session-secret constant; push/export helpers send/require the header + session-secret bearer; reconnect uses the handle/frame token; new tests (Promoted token non-empty + != machine secret; disposal). |
| `crates/engine/tests/microvm.rs` | **Modify.** `TestServeProvisioner` mints per provision; tests use `handle.token` for export. |
| `crates/engine/tests/promote.rs` | **Modify.** Assert the loopback handle token is a non-empty 32-hex mint (comment flip). |
| `crates/engine/tests/auth.rs` | **Modify.** `Machine`-mode tests push-then-attach with the session secret; machine secret rejected. |
| `crates/engine/tests/serve.rs` | **Modify.** `promote_then_demote_round_trip_preserves_session` asserts a non-empty `Promoted.token`; any other handover assertions updated. |
| `ui-dioxus/src/app.rs` | **Modify.** `Promoted` arm adopts the token (`token.set(token)`) before the reconnect. |
| `CLAUDE.md`, `README.md` | **Modify.** Multitenancy notes: `Promoted.token` always `Some`, per-session secrets, receiver session→secret map, header on `/promote`. |

## Task Order & Rationale

Forced by the inward dependency rule and compile-green discipline: `protocol` (Task 1) → `remote` (Task 2) → `engine` serve/loopback/service (Tasks 3–5) → integration harnesses (Task 6) → workspace doc (Task 7) → `ui-dioxus` (Task 8) → docs (Task 9). Tasks 1–2 are independent of each other but both must precede Task 3 (serve.rs consumes `Promoted.token: String` and `mint_session_secret`). Task 1 edits `serve.rs`'s `Promoted` construction (the type change forces it) but no other engine code; Task 2 edits the vps demote arm (the `VpsTarget::export` removal forces it) but no other engine code. **One deliberate red window is stated, not hidden:** Task 3 changes the *behavior* of `/promote`, `/export`, `/workspace`, and the `Machine` WS handshake, so the pre-existing integration harnesses (`vps_promote.rs`, `microvm.rs`, `auth.rs`, `serve.rs`) fail at **runtime** — they still compile — until Task 6 migrates them. Task 3's own gate is `cargo test -p otto-engine --lib` (library tests) plus `cargo build --workspace`; Task 6 closes the window with `cargo test --workspace`.

---

### Task 1: `protocol` — `Promoted.token` becomes a required `String`

**Files:**
- Modify: `crates/protocol/src/lib.rs`, `crates/protocol/Cargo.toml`, `crates/engine/src/serve.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces, used by every later task:
  - `ServerMessage::Promoted { session, endpoint, token: String }` — the field is now a required `String`; `#[serde(default)]` is **removed** (spec §2): a frame with a `null` token **or** a missing `token` field fails to deserialize. This is the deliberate, semver-major wire break.
  - The `serve.rs` construction site is updated in this task because the type change forces it: `handle_handover`'s reuse branch drops the `handover_token` filter (`(!tok.is_empty() && tok != cfg.token).then(..)`) and the stale "stays `None`" comments (spec §1.6), building `Promoted { token: tok }` unconditionally.

- [ ] **Step 1: Write the failing tests**

In `crates/protocol/src/lib.rs`'s `#[cfg(test)]` module:

- Update `handover_server_messages_round_trip` (`:617-641`): `token: None` → `token: "".into()` for the round-trip list; the JSON-shape assert at `:634-641` uses `token: "".into()`.
- Update `promoted_with_token_round_trips` (`:644-658`): construct `token: "abc".into()`; the match arm asserts `assert_eq!(&token, "abc")`.
- Update `server_message_debug_redacts_promoted_token` (`:743-756`): `token: Some("fly-secret".into())` → `token: "fly-secret".into()`.
- Add two new tests:
  - `promoted_with_null_token_fails_to_deserialize`: hand-build the JSON `{"type":"promoted","session":"<uuid>","endpoint":"ws://x","token":null}` and assert `serde_json::from_str::<ServerMessage>` errors.
  - `promoted_with_missing_token_fails_to_deserialize`: the same JSON **without** the `token` key; assert it errors too.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-protocol`
Expected: FAIL to compile — `Promoted`'s `token` is still `Option<String>`.

- [ ] **Step 3: Implement**

`crates/protocol/src/lib.rs`:
- `Promoted { token: String }` — remove the `#[serde(default)]` attribute on `token`.
- The hand-written `Debug` arm for `Promoted` (`:340-349`) already redacts; adjust the field access from `token.as_deref().map(|_| "<redacted>")` to `&"<redacted>"`.
- Update the `Command::PromoteToRemote` doc comment (`:118-120`) if it mentions "using `token` when present" — it now always uses it.

`crates/engine/src/serve.rs` — `handle_handover` reuse branch (`:1705-1715`):
- Delete the `handover_token` computation and the comment block `:1706-1710`.
- Build `ServerMessage::Promoted { session, endpoint, token: tok }`.
- Delete the stale `// `Promoted.token` stays `None`` comments at `:1637` and the comment at `:1706-1708`.

`crates/protocol/Cargo.toml`: `version = "0.1.0"` → `"0.2.0"`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-protocol`
Expected: PASS — new deserialize-failure tests + all pre-existing.

- [ ] **Step 5: Verify the workspace still builds and the library tests pass**

Run: `cargo build --workspace` then `cargo test -p otto-engine --lib`
Expected: SUCCESS — the `serve.rs` edit is the only non-protocol touch and it compiles; nothing else constructs `Promoted`. The integration harnesses still compile (they read frames, never construct them).

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add crates/protocol/src/lib.rs crates/protocol/Cargo.toml crates/engine/src/serve.rs
git commit -m "protocol: make Promoted.token a required String (always present; wire break to 0.2.0)"
```

---

### Task 2: `remote` — the shared mint, the per-session push header, `VpsTarget` mint

**Files:**
- Modify: `crates/remote/src/lib.rs`, `crates/remote/src/fly.rs`, `crates/remote/Cargo.toml`, `crates/engine/src/serve.rs`

**Interfaces:**
- Consumes: Task 1's unchanged `PromoteBundle`/`RemoteHandle`; `uuid` (existing dep).
- Produces, used by Tasks 3/4:
  - `pub fn mint_session_secret() -> String` in `remote/src/lib.rs` (moved from `fly.rs:34-37`), 32-hex via `Uuid::new_v4().simple()`.
  - `push_promote_bundle(endpoint, bearer, session_secret, bundle)` — gains a `session_secret` param, sent as `X-Otto-Session-Secret` on the `POST /promote` request (spec §1.2).
  - `VpsTarget::provision` mints a fresh secret, pushes with `bearer = self.token` (machine-wide admission) + `session_secret = secret`, returns `RemoteHandle::new(endpoint, secret)`.
  - `VpsTarget::export` **removed** (spec premise correction 2) — the demote pull is the shared `export_bundle`.
  - `FlyTarget::provision` uses `mint_session_secret` and pushes with bearer=token + session_secret=token (spec A3).
  - `MicrovmTarget::provision` pushes with bearer=`machine.token` + session_secret=`machine.token` (spec A3).

- [ ] **Step 1: Write the failing tests**

In `crates/remote/src/lib.rs`'s `#[cfg(test)]` module:
- `mint_session_secret_is_32_hex_and_unique` — the shape of fly's current `mint_token_is_unique_hex`, moved here (the fly test is deleted in Step 3).
- A `push_promote_bundle`-level test can't run without a live server here (it's `pub(crate)`), so the header's presence is asserted at the caller layer in Task 6. Keep Task 2's `remote` tests to the mint + the wiremock fly tests updated in Step 3.

In `crates/remote/src/fly.rs` tests:
- `create_machine_body_has_image_env_guest_and_services` — unchanged (the env is still `OTTO_PROMOTION_SECRET`).
- The `provision_*` wiremock tests: assert the `/promote` POST carries the `X-Otto-Session-Secret` header using **`wiremock::matchers::header_exists("x-otto-session-secret")`** (presence-only — the token is minted inside `provision`, so an exact-value matcher mounted before provision cannot know it) optionally chained with `header_regex("x-otto-session-secret", "^[0-9a-f]{32}$")` for the 32-hex shape. **No custom `Match` impl** — that is a needless rabbit hole. The value-equality claim is covered by `create_machine_body_has_image_env_guest_and_services` (env injection) + the existing `handle.token.len() == 32` assert, since header and env share the same mint. Note in the report that `header_exists` asserts presence only.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-remote`
Expected: FAIL to compile — `mint_session_secret` not found, `push_promote_bundle` signature drift, fly's `mint_token` deleted.

- [ ] **Step 3: Implement**

`crates/remote/src/lib.rs`:
- Add `pub fn mint_session_secret() -> String` (doc: fresh 32-hex per-session credential; blast radius one session). `use uuid::Uuid;`.
- `push_promote_bundle(endpoint: &str, bearer: &str, session_secret: &str, bundle: &PromoteBundle)` — add `.header("X-Otto-Session-Secret", session_secret)` to the POST (alongside the existing `.bearer_auth(bearer)`).
- `VpsTarget::provision`: `let secret = mint_session_secret(); push_promote_bundle(&self.endpoint, &self.token, &secret, bundle).await?; Ok(RemoteHandle::new(self.endpoint.clone(), secret))`. Update `VpsTarget::new`'s doc (token is the machine-wide pusher credential; the handle carries the session secret).
- **Remove** `VpsTarget::export` (the whole `pub async fn export` at `:305-307`). In the same edit, update the stale doc on `export_bundle` (`:262-263`, "Shared by `VpsTarget::export` (vps demote)…") — it is now shared by the vps/microvm/fly demote pulls via the stored handle.
- `MicrovmTarget::provision`: `push_promote_bundle(&handle.endpoint, &handle.token, &machine.token, bundle)` — note `handle.token == machine.token`; pass it explicitly as the session secret.

`crates/remote/src/fly.rs`:
- Delete `pub(crate) fn mint_token` (`:34-36`); `provision` uses `crate::mint_session_secret()`.
- `provision`: `push_promote_bundle(&endpoint, &token, &token, bundle)`.
- Delete the `mint_token_is_unique_hex` test (moved to lib.rs).
- Update the `/promote` wiremock mocks per Step 1.

`crates/engine/src/serve.rs` — the vps demote arm (`:1394-1445`):
- Replace the `VpsTarget::new(endpoint, cfg.token) + target.export(session)` construction with the stored-handle pattern the microvm/fly arms use: look up `state.remotes[(session, true)]` → `(endpoint, secret)`; on `None`, reply `Error { "no active vps handover for this session; promote first" }`; else `otto_remote::export_bundle(&endpoint, &secret, session)` → `accept_demotion` → remove the handle → report `Demoted` (spec §1.6). The arm's `PromoteMode::Vps { .. }` match becomes a `Vps { .. }` (the cfg-derived `endpoint` binding is now unused once the pull reads the stored handle — leave it unbound or `clippy -D warnings` fails on the unused variable).

`crates/remote/Cargo.toml`: `version = "0.1.0"` → `"0.2.0"`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-remote`
Expected: PASS — mint tests + updated fly wiremock tests.

- [ ] **Step 5: Verify the workspace builds and the engine library tests pass**

Run: `cargo build --workspace` then `cargo test -p otto-engine --lib`
Expected: SUCCESS — the vps demote arm edit is the only engine touch and compiles.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add crates/remote/src/lib.rs crates/remote/src/fly.rs crates/remote/Cargo.toml crates/engine/src/serve.rs
git commit -m "remote: mint per-session secrets and push them over X-Otto-Session-Secret (wire break to 0.2.0)"
```

---

### Task 3: `engine/serve.rs` — the receiver's session→secret map and per-session auth

**Files:**
- Modify: `crates/engine/src/serve.rs`

**Interfaces:**
- Consumes: Tasks 1–2 (`Promoted.token: String`, `mint_session_secret`).
- Produces: the spec's success criteria 3–4 and 6 (per-session `/export`, `/workspace`, `Machine` WS, disposal, pre-ownership message). This task is the security core.

**Design points (spec §3, §4):**

- `ServeState` gains `session_secrets: Mutex<HashMap<SessionId, String>>` (spec §3.1). Helpers:
  - `fn record_session_secret(&self, session, secret)` — `/promote` success.
  - `fn session_secret(&self, session) -> Option<String>` — `/export` + WS attach (clone out under the lock; never hold the guard across an await).
  - `fn dispose_session_secret(&self, session)` — `/export` success.
  - `fn machine_workspace_authorized(&self, headers: &HeaderMap) -> bool` — machine secret OR constant-time match against any live session secret (spec A6).
- `promote_handler` (`:315-345`):
  1. `403`/`401` unchanged (machine secret, constant-time).
  2. Deserialize with the **pre-ownership special-case**: on serde failure, parse the raw body as `serde_json::Value`; if `session` is a present object missing `owner`, return `400` with the §3.2 message (assert byte-for-byte in a test); else the existing `bad request: {e}`.
  3. Read `X-Otto-Session-Secret`; absent **or empty** → `400` (A2: an empty value would be recorded as the session secret, weakening the fail-closed intent — every in-repo pusher sends a 32-hex mint).
  4. `accept_promotion` as today; on `Ok(session)`, `record_session_secret(session, header_secret)`.
- `export_handler` (`:351-379`):
  1. `403` unless `--accept-promotions` (unchanged).
  2. Parse `{ session }`; `session_secret(session)` → `None` ⇒ `401`; compare bearer constant-time against it ⇒ mismatch `401`. **The machine secret no longer authorizes `/export`.**
  3. `export_promotion(session)` as today (its `404` is now a near-dead backstop — spec §3.3 note).
  4. On success, `dispose_session_secret(session)` **before** returning.
- `workspace_handler` `Machine` arm (`:297-302`): `authorized(&headers, promotion_secret)` → `state.machine_workspace_authorized(&headers)`.
- `authenticate_connection` (`:797-839`): signature gains `params: &ConnectParams`. The `Machine` arm:
  - `let Some(session) = params.session.as_ref().and_then(|s| uuid::Uuid::parse_str(s).ok()) else { send AUTH_FAILED; return None }`;
  - `let Some(secret) = state.session_secret(SessionId(session)) else { send AUTH_FAILED; return None }`;
  - header pre-resolution: `authorized(headers, Some(&secret))` → `Some(ConnIdentity { owner: UserId::local(), access_token: None })`;
  - else `handshake_frame(reader, writer, state, Some(&secret)).await`.
- `handshake_frame` (`:846-1011`): gains `expected_machine_secret: Option<&str>`. The `Attach` arm's `Machine` branch (`:945-963`) compares `secret_matches(expected_machine_secret, &token)` instead of `secret_matches(state.auth.promotion_secret.as_deref(), &token)`. `Users` callers pass `None` (unchanged).
- `handle_socket` call site (`:1037-1041`): pass `&params` to `authenticate_connection`.
- The microvm promote arm (`:1652-1658`): `FirecrackerProvisioner::new(config.clone(), cfg.token.clone())` → `FirecrackerProvisioner::new(config.clone(), crate::mint_session_secret())` (spec §1.2; the provisioner is constructed per handover, so this is a fresh per-session mint). Requires `mint_session_secret` re-exported from `otto-engine` (Task 3's `crates/engine/src/lib.rs` edit).

**`crates/engine/src/lib.rs`:** re-export `mint_session_secret` alongside the existing `use otto_remote::{ ... }` block.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module in `crates/engine/src/serve.rs` (or a new `#[cfg(test)]` submodule) unit tests for the pure pieces:
- `machine_workspace_authorized` accepts the machine secret, accepts a recorded session secret, and rejects a wrong secret (build a small `ServeState`-like fixture — note `ServeState` fields are private; if unit-testing is awkward, cover these behaviors in `tests/auth.rs` in Task 6 and skip here).
- The pre-ownership detection helper (a `fn` extracting "is this a legacy bundle?" from raw bytes) — unit-test `is_pre_ownership_bundle(bytes)` against a legacy-shaped body, a garbage body, and a valid bundle.

**Mandatory report note:** if the `machine_workspace_authorized` unit fixture is disproportionate (private fields), that is a *decision to defer to Task 6's integration coverage*, and the implementer must say so explicitly in the report rather than silently skipping. The pre-ownership helper is small and pure; it must be unit-tested here.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-engine --lib`
Expected: the new tests fail to compile (no helpers yet).

- [ ] **Step 3: Implement**

Per the design points above. `use otto_protocol::SessionId` is already imported. All map accesses take the lock briefly and release before any await (clone the secret out).

- [ ] **Step 4: Run the library tests**

Run: `cargo test -p otto-engine --lib`
Expected: PASS.

- [ ] **Step 5: Build the workspace; confirm the integration failures are behavioral, not compile**

Run: `cargo build --workspace --tests` (or `cargo check --all-targets`) — `cargo build --workspace` alone does not compile the `tests/*.rs` targets, so this is the gate that genuinely proves the red window is compile-green.
Expected: SUCCESS — the pre-existing integration harnesses still **compile** (they never construct `Promoted` and never call the changed private helpers). They now fail at **runtime**:
- **`tests/vps_promote.rs`** — genuinely red: `post_promote` sends no `X-Otto-Session-Secret` header → 400; `/export` and post-promote WS reconnects present `Bearer TOKEN` (the machine secret) → 401.
- **`tests/auth.rs`** — genuinely red: the `Machine`-mode tests (`machine_attach_with_the_promotion_secret_adopts_the_session_owner` at `:669`, `machine_attach_with_the_promotion_secret_reaches_ready` at `:978`) attach with `SECRET` and expect `Ready`; a Machine receiver now requires the session's per-session secret → 401/opaque failure.
- **`tests/microvm.rs`** — **stays green**: `TestServeProvisioner` boots with `promotion_secret = TOKEN` and returns `token: TOKEN`, so post-Task-2 `MicrovmTarget::provision` pushes `session_secret = machine.token = TOKEN`; post-Task-3 the receiver records session→TOKEN and `/export` with bearer TOKEN returns 200. Do NOT chase phantom failures here.
- **`tests/serve.rs`** — **stays green**: `promote_then_demote_round_trip_preserves_session` is loopback-only and untouched by Task 3 (the loopback → `SingleUser` engine ignores credentials; `authed_endpoint_request`'s `Bearer TOKEN` is ignored).

**This is the stated red window, closed by Task 6.** Do NOT fix the harnesses here.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add crates/engine/src/serve.rs crates/engine/src/lib.rs
git commit -m "engine: per-session receiver secrets — session→secret map, session-scoped /export/workspace/WS auth"
```

---

### Task 4: `engine/loopback.rs` — mint a fresh secret for loopback

**Files:**
- Modify: `crates/engine/src/loopback.rs`, `crates/engine/tests/promote.rs`

**Interfaces:**
- Consumes: `otto_remote::mint_session_secret` (re-exported via `crate::mint_session_secret`).
- Produces: `Promoted.token` for loopback is a fresh non-empty mint (spec success criterion 2). The provisioned engine is `SingleUser` and ignores credentials, so this is a uniform invariant, not a functional credential.

- [ ] **Step 1: Write the failing test**

In `crates/engine/tests/promote.rs`: the test already drives `LoopbackTarget` and reads `handle`. Assert `handle.token` is 32 hex and non-empty (spec success criterion 2), flipping the current comment at `:97-98` that says "`Promoted.token` stays `None`". (The frame-level assert lives in Task 6's `tests/serve.rs`; here the handle is the observable.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p otto-engine --test promote`
Expected: FAIL — `handle.token` is currently the empty `promotion_secret`.

- [ ] **Step 3: Implement**

`crates/engine/src/loopback.rs`, `provision`:
- Replace the `promotion_secret.clone().unwrap_or_default()` handle-token computation with `let secret = crate::mint_session_secret();`.
- Keep the loopback engine's own `PromoteConfig { token: ... }` wiring as it is (the provisioned engine's inbound `/promote`/`/export` stay disabled; `PromoteConfig.token` there remains the threaded secret — this slice does not change nested-loopback semantics).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p otto-engine --test promote`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add crates/engine/src/loopback.rs crates/engine/tests/promote.rs
git commit -m "engine: mint a fresh per-session secret for loopback promote"
```

---

### Task 5: `engine/service.rs` — `accept_demotion` owner match

**Files:**
- Modify: `crates/engine/src/service.rs`

**Interfaces:**
- Consumes: the existing id-binding in `accept_demotion` (spec §4).
- Produces: refusal of a demotion bundle whose owner differs from the local row's current owner — closing the overwrite-including-owner hole.

- [ ] **Step 1: Write the failing tests**

In `crates/engine/src/service.rs`'s `#[cfg(test)]` module, after the existing `accept_demotion`/`accept_promotion` tests:
- `accept_demotion_refuses_a_bundle_with_a_different_owner`: create a session as `alice`, promote a bundle for it, then `accept_demotion` with the same id but `bundle.session.owner = bob` → `Err(AcceptError::Refused(_))`, and the store row still shows `alice`.
- `accept_demotion_accepts_a_bundle_with_the_same_owner`: the same id, same owner → `Ok`, row owner unchanged.
- `accept_demotion_refuses_an_unknown_local_row`: `accept_demotion` for a session id with no local row → `Err` (the `owner_of` call fails closed).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p otto-engine --lib`
Expected: FAIL — the owner check does not exist yet.

- [ ] **Step 3: Implement**

In `accept_demotion` (`:694-723`), after the existing `id != expected` check and before `validate_workspace_edits`:
```rust
let current_owner = self.store.owner_of(expected).await.map_err(AcceptError::Failed)?;
if bundle.session.owner != current_owner {
    return Err(AcceptError::Refused(format!(
        "demotion bundle is owned by {}, but the local copy of {} is owned by {}",
        bundle.session.owner.as_str(), expected.0, current_owner.as_str(),
    )));
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p otto-engine --lib`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add crates/engine/src/service.rs
git commit -m "engine: refuse a demotion bundle whose owner differs from the local row's"
```

---

### Task 6: Migrate the integration harnesses and add the per-session tests (closes the red window)

**Files:**
- Modify: `crates/engine/tests/vps_promote.rs`, `crates/engine/tests/microvm.rs`, `crates/engine/tests/auth.rs`, `crates/engine/tests/serve.rs`, `crates/engine/tests/promote.rs` (if Task 4 didn't fully cover it)

**Interfaces:**
- Consumes: the Task 3 behavior (session-secret `/promote`/`/export`/`/workspace`/WS auth, disposal, `Promoted.token: String`).
- Produces: a workspace-green tree (`cargo test --workspace` passes except the known `mcp-lsp` failure), asserting the spec's success criteria 2–6.

- [ ] **Step 1: Migrate `tests/vps_promote.rs`**

- Add a session-secret constant, e.g. `const SESSION_SECRET: &str = "session-secret";`, distinct from `TOKEN` (the machine secret) — the whole point is proving the two differ (spec success criterion 3).
- `post_promote(base, token, body)` gains a session-secret param and sends `X-Otto-Session-Secret`; every call passes `SESSION_SECRET` (or a per-test fresh value).
- `post_export(base, token, session)` now authenticates with the **session secret** for `/export`; calls that currently use `TOKEN` switch to `SESSION_SECRET` where the pushed session used `SESSION_SECRET`. **Watch for status-code expectation changes, not just credential swaps:** the new `session_secret` check precedes `export_promotion`, so `export_unknown_session_is_not_found` (currently 404) becomes 401 (a never-promoted session has no recorded secret → 401, spec §3.3). Update every `/export` call site's expected status accordingly.
- WS reconnects to the receiver: `authed_ws_request` currently sends `Bearer TOKEN`. For post-promote reconnects that must authenticate as a Machine receiver, use the session secret from the promote (the `Promoted` frame or the `handle.token`). The harness-level `start_receiver` still boots with `promotion_secret = TOKEN`; session secrets are minted/recorded per push.
- `vps_target_provisions_against_a_receiver` / `vps_target_teardown_does_not_stop_the_receiver` / `vps_target_provision_errors_on_non_2xx`: pass a session secret to the push; assert `handle.token` is a fresh 32-hex != `TOKEN`.
- `handover_vps_promote_points_at_receiver`: assert the `Promoted` frame's `token` is a non-empty string and != `TOKEN` (spec criterion 2).
- `handover_vps_demote_pulls_session_back_to_source` / `vps_demote_round_trip_brings_advanced_state_back_to_source`: after promote, the demote arm uses the stored handle's secret for the pull — the test flow is unchanged (the source's `handle_handover` handles it), but the final `post_export(&recv_http, Some(TOKEN), &session)` in the round-trip test (copy-semantics assertion) must switch to a fresh push+secret or assert against the store directly (the receiver's retained copy exists but its original secret was disposed by the demote — spec §7). Update to assert copy semantics via the store (`SqliteStore::open(db_dir...)` + `session_status`), not a second `/export`.
- `vps_promote_resumes_session_and_workspace_on_receiver`: the reconnect to the receiver uses the handle's session secret (from `promote()`'s returned handle), not `TOKEN`; the `RemoteWorkspace::new(recv_http, ...)` uses the session secret.

- [ ] **Step 2: Migrate `tests/microvm.rs`**

- `TestServeProvisioner::provision` mints a fresh per-provision secret (spec §1.2), boots the serve with it as `promotion_secret`, and returns it in `ProvisionedMachine.token`.
- `microvm_target_seam_round_trip`: the `/export` after provision authenticates with the **session secret** from the provisioned machine (`provisioner`'s token, or the handle's token), not `TOKEN`.
- `microvm_demote_pull_then_dispose`: `export_bundle(&handle.endpoint, &handle.token, id)` already uses the handle token — verify it's the fresh mint, not `TOKEN`.
- `microvm_target_teardown_stops_the_machine`: unchanged (the POST `/promote` after teardown just needs a valid bearer; use the machine's session secret).
- `handover_microvm_*`: the source's `handle_handover` drives the mint; assertions unchanged except any that mention the machine secret.

- [ ] **Step 3: Migrate `tests/auth.rs`**

- **`start_machine` (`:190-202`) must build with `accept_promotions = true`** — it currently passes `false` as the last arg to `serve_app(service, auth, test_capabilities(), None, false)`. Under slice 3 the Machine-mode tests seed `session_secrets` via `POST /promote`, which 403s unless acceptance is on. Flipping it is safe for the helper's other consumers (`machine_rejects_session_creation` and `machine_handshake_has_a_deadline` don't depend on the flag).
- The `Machine`-mode tests that currently attach with the **machine secret** (`SECRET`) and expect `Ready` — **`machine_attach_with_the_promotion_secret_adopts_the_session_owner` (`:669`) AND `machine_attach_with_the_promotion_secret_reaches_ready` (`:978`)** — must switch to the session's **per-session** secret. Rework each: push a bundle via `POST /promote` (with `X-Otto-Session-Secret: <secret>`) to seed `session_secrets`, then:
  - attach with `<secret>` → `Ready`, adopts the session owner (the `:669` adoption assertion stays).
  - attach with `SECRET` (the machine secret) → opaque `authentication failed` (spec criterion 3).
  - attach with a wrong `<other>` → opaque failure.
- `machine_rejects_session_creation` (`:711`): still true — but the `Attach` without `?session=` now fails at the secret lookup (A5); assert the opaque error + the store stays empty.
- `machine_handshake_has_a_deadline` (`:953`): its assertions still pass (no `?session=` → immediate opaque failure instead of a deadline wait), but its premise comment is now stale — update the one-line comment to note the Machine credential is the per-session secret, so a header-less `?session=`-less connection now fails at the lookup rather than the deadline.
- Any test that pushes to a `Machine` receiver directly (`POST /promote`) gains the header.

- [ ] **Step 4: Migrate `tests/serve.rs`**

- `promote_then_demote_round_trip_preserves_session` (`:1236`): after reading the `Promoted` frame (`:1271`), assert `frame["token"]` is a non-empty string (spec criterion 2 for loopback via the socket). The reconnect uses `authed_endpoint_request` with `Bearer TOKEN` — under loopback→`SingleUser` this is ignored, so it still works; keep it.
- Any other handover assertions updated to the always-`Some` frame.

- [ ] **Step 5: Add the new integration tests**

In `tests/vps_promote.rs` (or a new `tests/handover_credentials.rs` if the file grows too large — prefer the existing file per the repo's harness-adjacent convention):
- `/export` with the machine secret → `401` (spec criterion 3).
- `/export` with the session secret → `200`; a **second** `/export` with the same secret → `401` (disposal; spec criterion 4), and the receiver's store still holds the session (`session_status` via `SqliteStore::open`).
- `/workspace` on a `Machine` receiver with a live session secret → accepted; with the machine secret → accepted (A6).
- `POST /promote` without `X-Otto-Session-Secret` → `400`.
- `POST /promote` with a hand-built pre-ownership bundle (JSON whose `session` object has no `owner`) → `400` whose body contains "predates session ownership" (spec criterion 6, byte-for-byte).
- A `Promoted` frame from a vps promote carries a fresh 32-hex token != the machine secret.

- [ ] **Step 6: Run the workspace suite**

Run: `cargo test --workspace`
Expected: PASS except the known `otto-mcp-lsp` rust-analyzer test. This commit closes the Task 3–6 red window.

- [ ] **Step 7: Format and commit**

```bash
cargo fmt --all
git add crates/engine/tests
git commit -m "engine: migrate the handover harnesses to per-session secrets and add the session-map tests"
```

---

### Task 7: `workspace/remote.rs` — the `RemoteWorkspace` construction contract

**Files:**
- Modify: `crates/workspace/src/remote.rs`

**Interfaces:**
- Produces: the spec §5 doc contract on `RemoteWorkspace::new` (source/client-side only; never constructed on a promoted machine). Documentation only — no structural change (there is no production construction site to remove).

- [ ] **Step 1: Make the change**

On `RemoteWorkspace::new` (`:21-36`), add the doc block from spec §5 verbatim.

- [ ] **Step 2: Verify**

Run: `cargo test -p otto-workspace`
Expected: PASS (docs-only change; nothing else touched).

- [ ] **Step 3: Format and commit**

```bash
cargo fmt --all
git add crates/workspace/src/remote.rs
git commit -m "workspace: document RemoteWorkspace as source/client-side only (spec A13)"
```

---

### Task 8: `ui-dioxus` — adopt the promoted token

**Files:**
- Modify: `ui-dioxus/src/app.rs`

**Interfaces:**
- Consumes: `Promoted.token` is now a required `String` (Task 1; `ui-dioxus`'s `path =` dep resolves it).
- Produces: the `Promoted` arm stores the token so the post-promote `/workspace` RPCs (`load_files`/`open_path`) present the session secret (spec §6). The desktop path (loopback → `SingleUser`) is behaviorally unchanged.

- [ ] **Step 1: Write the failing test**

There is no host-side unit test that pins the `Promoted` arm's token handling (it lives in the wasm-only `web_mount_test` flow). Add the minimal assertion the slice can support: in `ui-dioxus/src/app.rs`'s existing host-test surface if any touches `Promoted`; otherwise rely on the compile-time type change (the arm must destructure `token`) plus the desktop suite. If no existing test covers it, note in the report that the arm change is verified by the wasm compile + desktop suite, not a unit test.

- [ ] **Step 2: Implement**

`ui-dioxus/src/app.rs` `:199-210`:
```rust
SocketEvent::Message(Ok(ServerMessage::Promoted { endpoint, token, .. })) => {
    token.set(token);                 // switch the credential before the reconnect
    reconnect_to.set(Some(endpoint));
}
```
The current arm is a **combined or-pattern** `Promoted { endpoint, .. } | Demoted { endpoint, .. }` — it must be **split** into two arms so the `token` field can bind in the `Promoted` arm (the compiler will force this once `token` is destructured; the `Demoted` arm keeps `{ endpoint, .. }`).

- [ ] **Step 3: Verify the desktop suite and the wasm compile**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS.
Run: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`
Expected: SUCCESS.

- [ ] **Step 4: Format and commit**

```bash
cargo fmt --all
git add ui-dioxus/src/app.rs
git commit -m "ui-dioxus: adopt the Promoted frame's session token for the reconnect"
```

---

### Task 9: Docs — CLAUDE.md, README, and the record-as-shipped commit

**Files:**
- Modify: `CLAUDE.md`, `README.md`

**Interfaces:**
- Produces: the multitenancy notes describe the new credential model. The spec's `Status` flip to IMPLEMENTED and the plan checkboxes are the record-as-shipped commit, which per house style happens **after merge** in a fresh worktree — this task only updates the prose docs in the feature PR.

- [ ] **Step 1: Update `CLAUDE.md`**

In the `engine` crate-table row and the serve command block: note that `Promoted.token` is always `Some` (a fresh per-session secret minted by the target), that `--promotion-receiver` receivers keep a session→secret map consulted by `/export`/`/workspace`/the `Machine` WS handshake, and that the per-session secret rides `X-Otto-Session-Secret` on the `/promote` push. Keep the `OTTO_PROMOTION_SECRET` description accurate (it is now the *admission* credential for `/promote`, no longer a session credential). **Also update the `remote` crate-table row**, which currently describes `VpsTarget` as having an `export` that "pulls a bundle back for demote" — that method is removed by Task 2; the row should say the demote pull uses the shared `export_bundle` with the stored handle's session secret.

- [ ] **Step 2: Update `README.md`**

The same notes at the summary level (promote flow: fresh per-session secret, always-present `Promoted.token`).

- [ ] **Step 3: Verify no stale claims remain**

Run: `grep -rn "Promoted.token\|reuse the current token\|stays .None" CLAUDE.md README.md docs/superpowers/specs/2026-08-14-handover-credentials-design.md`
Expected: no "reuse the current token" or "stays None" claims anywhere except historical references clearly marked as superseded.

- [ ] **Step 4: Format and commit**

```bash
git add CLAUDE.md README.md
git commit -m "docs: describe per-session handover secrets and always-Some Promoted.token"
```

---

## Phase 5 out-of-band verification (after merge)

- **`ui-dioxus` desktop suite** — `cd ui-dioxus && cargo test --features desktop` (Task 8's gate).
- **wasm bundle** — `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web` (compile check; `./scripts/build-web.sh` if a release bundle is desired).
- **Feature-gated builds** — the `firecracker` feature's `FirecrackerProvisioner` construction changed (Task 3's mint at the serve.rs arm): verify `cargo build --workspace --features firecracker` compiles. The `candle` feature is untouched by this slice but is exercised by the same command.
- **Wire breaks** — `protocol` 0.2.0 and `remote` 0.2.0 are consumed by path-deps across the workspace; `cargo test --workspace` resolving cleanly confirms no stale version string.
- **`deploy/fly` container image** — **no change needed, and none expected:** the image builds `otto serve` from the same commit as the pushers, so pusher/receiver lockstep (the required `X-Otto-Session-Secret` header and the per-session `Machine` handshake) holds by construction. Verify the `deploy/fly` directory is untouched in the PR diff.
