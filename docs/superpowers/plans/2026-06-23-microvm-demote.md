# demote-from-microvm Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a client connected to a source serve `S` (started `--promote-microvm`, having promoted a session into an ephemeral microVM) issue `DemoteToLocal` and have `S` pull the session's current state back off the running microVM, restore it locally, and dispose the VM.

**Architecture:** Extract the `POST /export` pull (today only `VpsTarget::export`) into a `pub` free function `export_bundle` in the `remote` crate, symmetric with the existing `push_promote_bundle`. Then replace the microVM demote refusal in `serve.rs`'s `handle_handover` with: find the live promote handle in `state.remotes`, pull the bundle via `export_bundle`, restore locally via the existing `accept_demotion` (fail-closed sensitive floor + `restore_over`), and on success only, drop the handle (disposing the VM) and reply `Demoted{endpoint: public_ws_base}`.

**Tech Stack:** Rust (edition 2024, async/tokio), axum transport, reqwest client, sqlx/SQLite persistence, integration tests with `tokio-tungstenite` + an in-process `otto serve` provisioner.

**Design spec:** `docs/superpowers/specs/2026-06-23-microvm-demote-design.md`

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/remote/src/lib.rs` | Modify | Add `pub async fn export_bundle`; refactor `VpsTarget::export` to delegate; unit test for `export_bundle` against an in-process serve is covered at the engine integration layer (Task 3) since `remote` has no serve to talk to |
| `crates/engine/src/lib.rs` | Modify | Re-export `export_bundle` from `otto_remote` |
| `crates/engine/src/serve.rs` | Modify | Replace the microVM demote refusal branch in `handle_handover` with pull → restore → dispose |
| `crates/engine/tests/microvm.rs` | Modify | Seam happy-path pull+dispose test via `export_bundle`; replace `handover_microvm_demote_is_unsupported` with a no-prior-promote error test |
| `docs/ARCHITECTURE.md`, `CLAUDE.md`, the design spec | Modify | Record shipped; demote-from-microvm no longer "the remaining follow-up" |

`VpsTarget` is the only current caller of the export pull, so the extraction forces no other updates.

---

### Task 1: Extract `export_bundle` in the `remote` crate

**Files:**
- Modify: `crates/remote/src/lib.rs` (add `export_bundle` after `push_promote_bundle` at line 241; refactor `VpsTarget::export` at lines 264-283)

- [ ] **Step 1: Add the `export_bundle` free function**

In `crates/remote/src/lib.rs`, immediately after `push_promote_bundle` (which closes at line 241), add:

```rust
/// POST a session id to `{http_base(endpoint)}/export` with `Bearer token` and deserialize the
/// returned `PromoteBundle` (the demote pull). On a non-2xx, bail with the receiver's status +
/// body (operator diagnostics). Shared by `VpsTarget::export` (vps demote) and the microVM demote
/// pull (whose endpoint comes from the live `RemoteHandle`, not a static config).
pub async fn export_bundle(
    endpoint: &str,
    token: &str,
    session: SessionId,
) -> anyhow::Result<PromoteBundle> {
    let url = format!("{}/export", http_base(endpoint));
    let resp = build_promote_client()
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "session": session.0.to_string() }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("export rejected by remote: HTTP {status}: {body}");
    }
    Ok(resp.json().await?)
}
```

> `SessionId`, `PromoteBundle`, `build_promote_client`, `http_base`, and `serde_json` are all already in scope in this file.

- [ ] **Step 2: Refactor `VpsTarget::export` to delegate**

Replace the body of `VpsTarget::export` (lines 267-282 — the `let url = …` through `Ok(resp.json().await?)`) so the whole method reads:

```rust
    /// Pull a session's `PromoteBundle` back from the receiver (the demote primitive). Delegates to
    /// the shared `export_bundle` so vps and microVM demote use the identical gated pull.
    pub async fn export(&self, session: SessionId) -> anyhow::Result<PromoteBundle> {
        export_bundle(&self.endpoint, &self.token, session).await
    }
```

> This drops `VpsTarget`'s use of its own `self.client` for the export call; `self.client` is still used by `provision` indirectly? No — `VpsTarget::provision` calls `push_promote_bundle` (which builds its own client). Confirm whether `self.client` is now unused. If `cargo build` warns `field 'client' is never read`, remove the `client` field from the `VpsTarget` struct (line 250), the `client: build_promote_client(),` initializer in `VpsTarget::new` (line 260), and adjust. If it is still read elsewhere, leave it. Resolve this during implementation based on the actual warning — do not leave dead code.

- [ ] **Step 3: Build to verify it compiles**

Run: `cargo build -p otto-remote`
Expected: builds clean (no warnings — if `client` became unused, you removed it in Step 2).

- [ ] **Step 4: Run the remote crate tests**

Run: `cargo test -p otto-remote`
Expected: PASS — the existing `http_base_maps_ws_schemes`, `microvm_target_*`, and `unsupported_provisioner_refuses` tests are unaffected.

- [ ] **Step 5: Commit**

```bash
git add crates/remote/src/lib.rs
git commit -m "refactor(remote): extract export_bundle pull, shared by vps and microvm demote"
```

---

### Task 2: Re-export `export_bundle` from `otto_engine`

**Files:**
- Modify: `crates/engine/src/lib.rs:33-36`

- [ ] **Step 1: Add `export_bundle` to the re-export list**

In `crates/engine/src/lib.rs`, the `pub use otto_remote{...}` block (lines 33-36) currently reads:

```rust
pub use otto_remote::{
    MicrovmConfig, MicrovmTarget, PromoteBundle, PromoteConfig, PromoteMode, ProvisionedMachine,
    Provisioner, RemoteHandle, RemoteTarget, UnsupportedProvisioner, VpsTarget, promote,
};
```

Add `export_bundle` (keep the list alphabetical-ish, matching the existing `promote` lowercase-fn placement):

```rust
pub use otto_remote::{
    MicrovmConfig, MicrovmTarget, PromoteBundle, PromoteConfig, PromoteMode, ProvisionedMachine,
    Provisioner, RemoteHandle, RemoteTarget, UnsupportedProvisioner, VpsTarget, export_bundle,
    promote,
};
```

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build -p otto-engine`
Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/lib.rs
git commit -m "chore(engine): re-export export_bundle from otto_remote"
```

---

### Task 3: Seam happy-path test — pull + dispose via `export_bundle`

This proves the pull primitive and disposal against a real in-process serve, before wiring `serve.rs`. It reuses the existing `TestServeProvisioner` and `sample_bundle` helpers in `microvm.rs`.

**Files:**
- Test: `crates/engine/tests/microvm.rs` (add one test; the imports `MicrovmTarget`, `RemoteTarget`, `PromoteBundle` already exist; add `export_bundle` to the `otto_engine` import on line 11)

- [ ] **Step 1: Add `export_bundle` to the test's imports**

In `crates/engine/tests/microvm.rs`, the `use otto_engine::{...}` block (lines 11-14) imports the seam types. Add `export_bundle`:

```rust
use otto_engine::{
    EngineService, MicrovmTarget, PromoteBundle, ProvisionedMachine, Provisioner, RemoteTarget,
    UnsupportedProvisioner, build_default_registry, build_tool_registry, export_bundle, serve_app,
    serve_run,
};
```

- [ ] **Step 2: Write the failing test**

Add this test to `crates/engine/tests/microvm.rs` (after `microvm_target_seam_round_trip`, which closes at line 130):

```rust
#[tokio::test]
async fn microvm_demote_pull_then_dispose() {
    // Provision an in-process serve (the CI stand-in for a microVM), promote a session into it,
    // then pull it back via the shared export_bundle primitive and dispose the machine.
    let provisioner = Arc::new(TestServeProvisioner::new());
    let endpoint = provisioner.endpoint.clone();
    let http_base = provisioner.http_base();
    let target = MicrovmTarget::new(provisioner.clone());

    let id = SessionId::new();
    let bundle = sample_bundle(id, vec![("pulled.txt", b"FROM_VM")]);
    let handle = target.provision(&bundle).await.unwrap();
    assert_eq!(handle.endpoint, endpoint);

    // Pull the session back off the provisioned serve using the handle's endpoint + token.
    let pulled = export_bundle(&handle.endpoint, &handle.token, id).await.unwrap();
    assert_eq!(pulled.session.id, id);
    assert!(
        pulled
            .workspace
            .files
            .iter()
            .any(|(p, b)| p == &PathBuf::from("pulled.txt") && b == b"FROM_VM"),
        "pulled workspace should contain pulled.txt: {:?}",
        pulled.workspace.files
    );

    // Dispose: dropping the handle aborts the serve task → the endpoint stops listening.
    drop(handle);
    tokio::task::yield_now().await;
    let result = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap()
        .post(format!("{http_base}/export"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "session": id.0.to_string() }))
        .send()
        .await;
    assert!(result.is_err(), "serve should be unreachable after the handle is dropped");
}
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p otto-engine --test microvm microvm_demote_pull_then_dispose`
Expected: PASS — `export_bundle` pulls the promoted session; dropping the handle makes the serve unreachable. (If it fails to compile on `export_bundle`, recheck Task 2's re-export and Step 1's import.)

- [ ] **Step 4: Commit**

```bash
git add crates/engine/tests/microvm.rs
git commit -m "test(engine): microvm demote pull-then-dispose via export_bundle"
```

---

### Task 4: Wire the microVM demote branch in `handle_handover`

**Files:**
- Modify: `crates/engine/src/serve.rs:677-688` (replace the refusal block)

- [ ] **Step 1: Replace the microVM refusal block**

In `crates/engine/src/serve.rs`, inside `handle_handover`'s `if !to_remote { … }` block, the microVM arm currently refuses (lines 677-688):

```rust
        if let otto_remote::PromoteMode::Microvm { .. } = &cfg.mode {
            // microVMs are ephemeral; pulling a session back off a torn-down guest is a follow-up.
            let _ = send_msg(
                writer,
                &ServerMessage::Error {
                    message: "demote-from-remote not supported in microvm mode (ephemeral)"
                        .to_string(),
                },
            )
            .await;
            return;
        }
```

Replace it with the pull → restore → dispose flow:

```rust
        if let otto_remote::PromoteMode::Microvm { .. } = &cfg.mode {
            // Source the live microVM endpoint+token from the handle a prior promote stored under
            // (session, true). No handle ⟹ nothing to pull from. Take the lock only to clone the
            // endpoint/token, releasing it at the `;` before any await.
            let live = state
                .remotes
                .lock()
                .unwrap()
                .get(&(session, true))
                .map(|h| (h.endpoint.clone(), h.token.clone()));
            let Some((endpoint, token)) = live else {
                let _ = send_msg(
                    writer,
                    &ServerMessage::Error {
                        message: "no active microvm handover for this session; promote first"
                            .to_string(),
                    },
                )
                .await;
                return;
            };

            // Pull the current bundle off the microVM. On failure, leave the VM running (a transient
            // pull error must not lose the session) and report the error.
            let bundle = match otto_remote::export_bundle(&endpoint, &token, session).await {
                Ok(b) => b,
                Err(e) => {
                    let _ = send_msg(writer, &ServerMessage::Error { message: e.to_string() }).await;
                    return;
                }
            };

            // Restore into THIS engine, overwriting our stale pre-promote copy (fail-closed
            // sensitive-path floor first). On failure, leave the VM running and report.
            if let Err(e) = state.service.accept_demotion(&bundle).await {
                let msg = match e {
                    crate::service::AcceptError::Refused(m) => m,
                    crate::service::AcceptError::Failed(err) => err.to_string(),
                    // unreachable: accept_demotion uses restore_over (overwrite), never AlreadyExists
                    crate::service::AcceptError::AlreadyExists => {
                        "demote restore conflict".to_string()
                    }
                };
                let _ = send_msg(writer, &ServerMessage::Error { message: msg }).await;
                return;
            }

            // Success only: drop the handle to dispose the microVM, then tell the client to
            // reconnect to us (the session is local again).
            state.remotes.lock().unwrap().remove(&(session, true));
            match &state.public_ws_base {
                Some(base) => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Demoted { session, endpoint: base.clone() },
                    )
                    .await;
                }
                None => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: "demote target has no public ws base configured".to_string(),
                        },
                    )
                    .await;
                }
            }
            return;
        }
```

> `RemoteHandle` carries `endpoint` and `token` as `pub` fields (see `crates/remote/src/lib.rs:70-74`), so `h.token.clone()` is valid. `state.remotes.lock().unwrap().remove(...)` drops the removed `RemoteHandle`, whose `Drop` aborts the disposal task — exactly the dispose semantics. The lock is held only for the synchronous `remove`, never across an await.

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build -p otto-engine`
Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/serve.rs
git commit -m "feat(serve): wire microvm DemoteToLocal to pull back and dispose the VM"
```

---

### Task 5: Replace the unsupported-demote serve test with the no-prior-promote error test

The old `handover_microvm_demote_is_unsupported` asserts the now-removed `"microvm mode"` message. Without the `firecracker` feature a promote cannot succeed, so the live handle never exists — which is exactly the no-prior-promote path the new branch reports.

**Files:**
- Modify: `crates/engine/tests/microvm.rs:261-279` (replace the test)

- [ ] **Step 1: Replace the test**

In `crates/engine/tests/microvm.rs`, replace the whole `handover_microvm_demote_is_unsupported` test (lines 261-279) with:

```rust
#[tokio::test]
async fn handover_microvm_demote_without_prior_promote_errs() {
    // No promote has run, so there is no live microVM handle to pull from: demote errors honestly
    // instead of provisioning anything. (With the firecracker feature a real promote would create
    // the handle; that round-trip is not CI-able, same boundary as the promote-unsupported test.)
    let (src_ws, _w, _d) = start_source_microvm().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let session = next_json(&mut ws).await.unwrap()["session"].as_str().unwrap().to_string();

    let demote = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&demote).unwrap())).await.unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "error" {
            assert!(
                f["message"].as_str().unwrap().contains("no active microvm handover"),
                "{f:?}"
            );
            break;
        }
        assert_ne!(f["type"], "demoted", "demote must not succeed with no prior promote: {f:?}");
    }
}
```

> Match the existing `ws.send(Message::Text(...))` construction in this file verbatim — the surrounding tests (e.g. line 247, 270) use `Message::Text(serde_json::to_string(&...).unwrap())` without `.into()`. Copy that exact shape.

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test -p otto-engine --test microvm handover_microvm_demote_without_prior_promote_errs`
Expected: PASS — the source replies `error` with `"no active microvm handover"`.

- [ ] **Step 3: Run the whole microvm integration file**

Run: `cargo test -p otto-engine --test microvm`
Expected: PASS — `microvm_demote_pull_then_dispose`, the seam round-trip/teardown tests, `handover_microvm_promote_is_unsupported_without_feature`, and the new demote-error test all green.

- [ ] **Step 4: Commit**

```bash
git add crates/engine/tests/microvm.rs
git commit -m "test(engine): microvm demote without prior promote errs honestly"
```

---

### Task 6: Docs + full-suite verification

**Files:**
- Modify: `docs/ARCHITECTURE.md`, `CLAUDE.md`, `docs/superpowers/specs/2026-06-23-microvm-demote-design.md`

- [ ] **Step 1: Update `CLAUDE.md`**

In the remote-axis paragraph (search for `demote-from-microvm is the` — it currently reads "demote-from-microvm is the remaining follow-up"), update it to record demote-from-microvm shipped: a client on a `--promote-microvm` source issues `DemoteToLocal`; the source pulls the session's current bundle off the running microVM via the shared `export_bundle` (`POST /export`), restores it locally with `accept_demotion` (overwriting its own copy via `restore_over`), and disposes the VM by dropping the live handle. Also extend the `remote` crate-table row to mention `export_bundle` (the shared `/export` pull, used by `VpsTarget::export` and microVM demote).

- [ ] **Step 2: Update `docs/ARCHITECTURE.md`**

Find the remote-axis section (search for `microvm` / `demote`). Record that demote-from-microvm is shipped and mirrors vps demote, with two differences: the receiver endpoint comes from the live `RemoteHandle` (not static config), and a successful demote disposes the ephemeral VM. Note that without the `firecracker` feature the serve-level happy path is not CI-able (the same boundary as microVM promote).

- [ ] **Step 3: Mark the design spec shipped**

At the top of `docs/superpowers/specs/2026-06-23-microvm-demote-design.md`, change the **Status** line to:

```markdown
**Status:** Shipped 2026-06-23 (plan: docs/superpowers/plans/2026-06-23-microvm-demote.md).
```

- [ ] **Step 4: Full-suite verification (determinism invariant)**

Run: `cargo test --workspace`
Expected: PASS — `--promote-microvm` stays opt-in; the default offline suite is unchanged.

Run: `cargo fmt --all && cargo clippy --workspace --all-targets`
Expected: clean — no warnings introduced (including no dead `VpsTarget::client` field, per Task 1 Step 2).

- [ ] **Step 5: Commit**

```bash
git add docs/ARCHITECTURE.md CLAUDE.md docs/superpowers/specs/2026-06-23-microvm-demote-design.md
git commit -m "docs: record demote-from-microvm shipped"
```

---

## Spec coverage check

| Spec requirement | Task |
|---|---|
| Extract shared `export_bundle` pull; `VpsTarget::export` delegates | 1 |
| Re-export `export_bundle` from `otto_engine` | 2 |
| microVM demote sources endpoint+token from the live promote handle | 4 |
| No prior promote ⟹ honest error, nothing changed | 4, 5 |
| Pull failure ⟹ error, VM left running | 4 (branch returns before remove/dispose) |
| Restore via `accept_demotion` (sensitive floor + `restore_over`) | 4 |
| Dispose VM on success only (drop handle) | 4, 3 (dispose proven at seam) |
| Reply `Demoted{endpoint: public_ws_base}`; `None` ⟹ misconfig error | 4 |
| Seam happy-path pull + dispose (CI-able) | 3 |
| `VpsTarget::export` regression unaffected | 1 (delegation), 6 (full suite) |
| serve-wiring error path without firecracker | 5 |
| Determinism suite untouched | 6 |
| Full firecracker serve-level round-trip un-CI-able (documented boundary) | 5, 6 (docs) |

**Intentionally not implemented (spec non-goals):** re-attaching to a microVM after `S` restarts; changes to vps/loopback demote beyond the `export_bundle` extraction; making `accept_demotion` transactional; a static microVM endpoint.
