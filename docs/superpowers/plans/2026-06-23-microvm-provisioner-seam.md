# microvm RemoteTarget via a Provisioner seam — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `Provisioner` seam and a provisioner-generic `MicrovmTarget` that composes it with the existing restore-push, plus a feature-gated `FirecrackerProvisioner` and an `UnsupportedProvisioner` that becomes the single machine-provisioning boundary.

**Architecture:** A `microvm` target is `VpsTarget`'s bundle-push plus the one step vps assumed away — booting the serve. That step becomes the `Provisioner` trait ("boot a reachable `otto serve --accept-promotions`; disposal rides the returned task"). `MicrovmTarget = Provisioner + push_promote_bundle`. The seam is proven end-to-end in CI against an in-process serve; Firecracker is real code behind a default-off `firecracker` cargo feature whose host-side pure logic is unit-tested.

**Tech Stack:** Rust (edition 2024), `async-trait`, `tokio`, `reqwest` (already deps of `otto-remote`), `serde_json`, `anyhow`. Tests use `tokio::test`, `tempfile`, `tokio-tungstenite`.

**Spec:** `docs/superpowers/specs/2026-06-23-microvm-provisioner-seam-design.md`

---

## File structure

- `crates/remote/src/lib.rs` — `Provisioner` trait + `ProvisionedMachine`; `MicrovmTarget`; `UnsupportedProvisioner` (replacing `UnsupportedTarget`); `push_promote_bundle`/`http_base`/`build_promote_client` shared helpers; `MicrovmConfig`; `PromoteMode::Microvm`. Declares `mod firecracker` under the feature.
- `crates/remote/src/firecracker.rs` (new, `#[cfg(feature = "firecracker")]`) — `FirecrackerProvisioner`, pure builders (`fc_config_json`, `guest_cmdline`, `readiness_url`, `validate_prereqs`), `FirecrackerGuard` (Drop cleanup), and gated unit tests.
- `crates/remote/Cargo.toml` — `firecracker` feature.
- `crates/engine/Cargo.toml` — `firecracker` feature forwarding to `otto-remote/firecracker`.
- `crates/engine/src/lib.rs` — re-export updates (drop `UnsupportedTarget`, add the new public types).
- `crates/engine/src/serve.rs` — `handle_handover` `Microvm` arm.
- `crates/engine/src/main.rs` — `--promote-microvm` flag + `OTTO_FC_*` → `MicrovmConfig` + mutual exclusion.
- `crates/engine/tests/microvm.rs` (new) — `TestServeProvisioner`, seam round-trip, teardown, unsupported, handover dispatch.
- `docs/ARCHITECTURE.md`, `CLAUDE.md` — record the seam, the retired `UnsupportedTarget`, the new boundary.

The current `RemoteTarget`/`RemoteHandle`/`promote`/`VpsTarget` live in `crates/remote/src/lib.rs`; `LoopbackTarget` stays in `crates/engine/src/loopback.rs`.

---

## Task 1: Extract the shared restore-push helper; refactor `VpsTarget`

Behavior-preserving refactor. The existing `vps_promote.rs` tests are the safety net.

**Files:**
- Modify: `crates/remote/src/lib.rs`

- [ ] **Step 1: Add the shared helpers above `pub struct VpsTarget`**

Insert these free functions in `crates/remote/src/lib.rs` immediately before the `VpsTarget` struct definition (currently near line 127):

```rust
/// The reqwest client used for promote/export RPCs (30s timeout, rustls). Built per call — these
/// RPCs are rare (a handover), so caching buys nothing.
fn build_promote_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("build reqwest client")
}

/// Map a `ws://`/`wss://` endpoint to its HTTP base for the promote/export POSTs
/// (`ws→http`, `wss→https`); an unrecognized scheme passes through verbatim.
fn http_base(endpoint: &str) -> String {
    if let Some(rest) = endpoint.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = endpoint.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        endpoint.to_string()
    }
}

/// POST a serialized `PromoteBundle` to `{http_base(endpoint)}/promote` with `Bearer token`. On a
/// non-2xx, bail with the receiver's status + body (operator diagnostics). Shared by `VpsTarget`
/// and `MicrovmTarget` so both use the identical gated restore-push.
pub(crate) async fn push_promote_bundle(
    endpoint: &str,
    token: &str,
    bundle: &PromoteBundle,
) -> anyhow::Result<()> {
    let url = format!("{}/promote", http_base(endpoint));
    let resp = build_promote_client()
        .post(&url)
        .bearer_auth(token)
        .json(bundle)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("promote rejected by remote: HTTP {status}: {body}");
    }
    Ok(())
}
```

- [ ] **Step 2: Refactor `VpsTarget` to use the helpers**

Replace the `VpsTarget` struct + its `impl` block (the `new`, `http_base` method, `export`, and the `RemoteTarget` impl) so that:

- The `client` field is kept (used by `export`) but built via `build_promote_client()`.
- The inherent `http_base(&self)` method is **removed** (callers use the free `http_base(&self.endpoint)`).
- `export` uses `http_base(&self.endpoint)`.
- `RemoteTarget::provision` delegates to the new helper.

```rust
pub struct VpsTarget {
    /// `ws://host:port` or `wss://host:port` — what the client reconnects to after promote.
    endpoint: String,
    token: String,
    client: reqwest::Client,
}

impl VpsTarget {
    /// `endpoint` is the receiver's `ws://`/`wss://` base; `token` is its bearer (reused from the
    /// source, by design — source and receiver share a trust domain in v1).
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            client: build_promote_client(),
        }
    }

    /// Pull a session's `PromoteBundle` back from the receiver (the demote primitive). POSTs the
    /// session id to `/export`; surfaces the receiver's status + body on a non-2xx, symmetric with
    /// how `provision` reports a rejected push.
    pub async fn export(&self, session: SessionId) -> anyhow::Result<PromoteBundle> {
        let url = format!("{}/export", http_base(&self.endpoint));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
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
}

#[async_trait]
impl RemoteTarget for VpsTarget {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle> {
        push_promote_bundle(&self.endpoint, &self.token, bundle).await?;
        Ok(RemoteHandle::new(self.endpoint.clone(), self.token.clone()))
    }

    async fn teardown(&self, _handle: RemoteHandle) -> anyhow::Result<()> {
        // No-op: VpsTarget does not own the operator's server, so it must never abort it.
        Ok(())
    }
}
```

- [ ] **Step 3: Update the `http_base` unit test to call the free function**

In the `#[cfg(test)] mod tests` block of `crates/remote/src/lib.rs`, replace the `vps_http_base_maps_ws_schemes` test body so it calls the free `http_base`:

```rust
    #[test]
    fn http_base_maps_ws_schemes() {
        // wss → https (the production path; otherwise the promote POST silently downgrades to
        // plaintext), ws → http (loopback), and an unrecognized scheme passes through verbatim.
        assert_eq!(http_base("wss://host:9000"), "https://host:9000");
        assert_eq!(http_base("ws://127.0.0.1:7878"), "http://127.0.0.1:7878");
        assert_eq!(http_base("http://host:1"), "http://host:1");
    }
```

- [ ] **Step 4: Build and run the remote + vps tests**

Run: `cargo test -p otto-remote && cargo test -p otto-engine --test vps_promote`
Expected: PASS. The vps round-trip, teardown-no-op, and gating tests all still pass (the push path is unchanged behavior, now routed through the shared helper).

- [ ] **Step 5: Commit**

```bash
git add crates/remote/src/lib.rs
git commit -m "refactor(remote): extract push_promote_bundle shared by VpsTarget"
```

---

## Task 2: Add the `Provisioner` trait and `ProvisionedMachine`

**Files:**
- Modify: `crates/remote/src/lib.rs`

- [ ] **Step 1: Add the trait + type below the `RemoteTarget` trait**

Insert in `crates/remote/src/lib.rs` directly after the `RemoteTarget` trait definition (after its closing `}`, near line 91):

```rust
/// Boots a reachable `otto serve --accept-promotions` and reports how to reach it. This is the step
/// `VpsTarget` assumed already done; `MicrovmTarget` composes a `Provisioner` with the bundle-push.
#[async_trait]
pub trait Provisioner: Send + Sync {
    /// Boot a reachable serve and return the machine. Disposal rides `ProvisionedMachine::task`:
    /// aborting/dropping it stops the in-process serve or kills the microVM. No separate teardown —
    /// this mirrors `RemoteHandle::with_task` and serve.rs's drop-based handover lifecycle.
    async fn provision(&self) -> anyhow::Result<ProvisionedMachine>;
}

/// A booted, reachable `otto serve`. `endpoint` is its `ws://host:port` base; `token` the bearer it
/// requires; `task` owns disposal (the serve task itself, or a guardian whose `Drop` kills a microVM).
pub struct ProvisionedMachine {
    pub endpoint: String,
    pub token: String,
    pub task: tokio::task::JoinHandle<()>,
}
```

- [ ] **Step 2: Build**

Run: `cargo build -p otto-remote`
Expected: PASS (new public items; `async_trait` and `tokio` are already imported/deps).

- [ ] **Step 3: Commit**

```bash
git add crates/remote/src/lib.rs
git commit -m "feat(remote): add Provisioner seam and ProvisionedMachine"
```

---

## Task 3: Replace `UnsupportedTarget` with `UnsupportedProvisioner`

The machine-provisioning boundary moves down to the provisioner layer.

**Files:**
- Modify: `crates/remote/src/lib.rs`
- Modify: `crates/engine/src/lib.rs:33-36`

- [ ] **Step 1: Remove `UnsupportedTarget`**

In `crates/remote/src/lib.rs`, delete the entire `UnsupportedTarget` struct, its doc comment, and its `RemoteTarget` impl (the block currently at lines ~108-122 beginning with `/// A \`RemoteTarget\` that refuses to provision:` and ending at the `impl RemoteTarget for UnsupportedTarget { ... }` closing brace).

- [ ] **Step 2: Add `UnsupportedProvisioner` in its place**

Insert where `UnsupportedTarget` was:

```rust
/// A `Provisioner` that refuses to provision: a real microVM needs a hypervisor + kernel/rootfs and
/// cannot run in-tree or in CI. This is the single machine-provisioning boundary (it replaced
/// `UnsupportedTarget`). `MicrovmTarget` over it surfaces this error honestly.
pub struct UnsupportedProvisioner;

#[async_trait]
impl Provisioner for UnsupportedProvisioner {
    async fn provision(&self) -> anyhow::Result<ProvisionedMachine> {
        anyhow::bail!(
            "real microVM provisioning requires a hypervisor + kernel/rootfs; not available \
             in-tree (build with --features firecracker and supply OTTO_FC_*)"
        )
    }
}
```

- [ ] **Step 3: Replace the `UnsupportedTarget` unit test**

In the `#[cfg(test)] mod tests` block of `crates/remote/src/lib.rs`, replace the `unsupported_target_refuses_to_provision` test with:

```rust
    #[tokio::test]
    async fn unsupported_provisioner_refuses() {
        assert!(UnsupportedProvisioner.provision().await.is_err());
    }
```

(The `sample_bundle`-style `SessionState` imports used only by the old test may now be unused — if `SessionStatus` / `WorkspaceSnapshot` imports in the test module go unused, remove them so the crate stays warning-clean. Run the build in Step 5 to confirm.)

- [ ] **Step 4: Drop the `UnsupportedTarget` re-export in the engine**

In `crates/engine/src/lib.rs`, change the `pub use otto_remote::{...}` block (lines ~33-36) to drop `UnsupportedTarget` and add the new public seam types:

```rust
pub use otto_remote::{
    MicrovmConfig, MicrovmTarget, PromoteBundle, PromoteConfig, PromoteMode, ProvisionedMachine,
    Provisioner, RemoteHandle, RemoteTarget, UnsupportedProvisioner, VpsTarget, promote,
};
```

Note: `MicrovmConfig`, `MicrovmTarget`, and `PromoteMode::Microvm` are added in Tasks 4–5; this re-export will not compile until those exist. To keep this task's build green, add **only** `ProvisionedMachine`, `Provisioner`, and `UnsupportedProvisioner` now and remove `UnsupportedTarget`:

```rust
pub use otto_remote::{
    PromoteBundle, PromoteConfig, PromoteMode, ProvisionedMachine, Provisioner, RemoteHandle,
    RemoteTarget, UnsupportedProvisioner, VpsTarget, promote,
};
```

(Task 5 extends this with `MicrovmConfig`/`MicrovmTarget`.)

- [ ] **Step 5: Build the workspace and run remote tests**

Run: `cargo build --workspace && cargo test -p otto-remote`
Expected: PASS. No remaining references to `UnsupportedTarget` anywhere (it was only declared/impl'd/tested in `remote` and re-exported once in `engine`).

- [ ] **Step 6: Commit**

```bash
git add crates/remote/src/lib.rs crates/engine/src/lib.rs
git commit -m "feat(remote): replace UnsupportedTarget with UnsupportedProvisioner"
```

---

## Task 4: Add `MicrovmTarget`

**Files:**
- Modify: `crates/remote/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/remote/src/lib.rs`:

```rust
    #[tokio::test]
    async fn microvm_target_over_unsupported_provisioner_errs() {
        let bundle = PromoteBundle {
            session: SessionState {
                id: SessionId::new(),
                goal: "g".to_string(),
                status: SessionStatus::Active,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot { files: vec![] },
        };
        let target = MicrovmTarget::new(std::sync::Arc::new(UnsupportedProvisioner));
        assert!(target.provision(&bundle).await.is_err());
    }
```

If the test module's `use` lines no longer import `SessionState`/`SessionStatus`/`WorkspaceSnapshot` (removed in Task 3 Step 3), re-add them to the test module:

```rust
    use otto_engine_core::types::WorkspaceSnapshot;
    use otto_persistence::{SessionState, SessionStatus};
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-remote microvm_target_over_unsupported_provisioner_errs`
Expected: FAIL — `MicrovmTarget` is not defined.

- [ ] **Step 3: Implement `MicrovmTarget`**

Insert in `crates/remote/src/lib.rs` after the `UnsupportedProvisioner` impl:

```rust
use std::sync::Arc;

/// A `RemoteTarget` that provisions an ephemeral machine, then pushes the bundle to it. It is
/// provisioner-generic: `FirecrackerProvisioner` (v2) or an in-process test serve both fit. Disposal
/// rides the provisioned machine's task, so the returned `RemoteHandle::with_task` aborts the serve
/// (or kills the microVM) on drop — matching serve.rs's existing handover lifecycle.
pub struct MicrovmTarget {
    provisioner: Arc<dyn Provisioner>,
}

impl MicrovmTarget {
    pub fn new(provisioner: Arc<dyn Provisioner>) -> Self {
        Self { provisioner }
    }
}

#[async_trait]
impl RemoteTarget for MicrovmTarget {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle> {
        // Boot the machine. If the push below fails, `machine` is dropped here, aborting its task —
        // so a microVM that booted but rejected the bundle is killed, never leaked.
        let machine = self.provisioner.provision().await?;
        push_promote_bundle(&machine.endpoint, &machine.token, bundle).await?;
        Ok(RemoteHandle::with_task(
            machine.endpoint,
            machine.token,
            machine.task,
        ))
    }

    async fn teardown(&self, mut handle: RemoteHandle) -> anyhow::Result<()> {
        handle.abort();
        Ok(())
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-remote microvm_target_over_unsupported_provisioner_errs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/remote/src/lib.rs
git commit -m "feat(remote): add provisioner-generic MicrovmTarget"
```

---

## Task 5: Add `MicrovmConfig` and `PromoteMode::Microvm`

**Files:**
- Modify: `crates/remote/src/lib.rs`
- Modify: `crates/engine/src/lib.rs:33-36`

- [ ] **Step 1: Add `MicrovmConfig`**

Insert in `crates/remote/src/lib.rs` near the top-level types (after the `PromoteBundle` definition is fine). It must be `Clone` (it is cloned out of `PromoteMode` when building the provisioner) and carry only plain data — no env reads here:

```rust
/// Firecracker microVM parameters, read from `OTTO_FC_*` at the CLI edge (never in this crate) and
/// carried as plain data in `PromoteMode::Microvm`. Always compiled; only `FirecrackerProvisioner`
/// (behind the `firecracker` feature) consumes it.
#[derive(Clone)]
pub struct MicrovmConfig {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub fc_bin: PathBuf,
    pub tap: String,
    pub guest_ip: String,
    pub port: u16,
    pub vcpus: u32,
    pub mem_mib: u32,
    pub boot_timeout: std::time::Duration,
}
```

`PathBuf` is already imported at the top of the file (`use std::path::PathBuf;`).

- [ ] **Step 2: Add the `Microvm` variant to `PromoteMode`**

In the `pub enum PromoteMode` definition, add a third variant:

```rust
    /// Provision an ephemeral microVM (Firecracker), restore the bundle into it, then dispose it on
    /// drop. `config` is read from `OTTO_FC_*` by the CLI; without the `firecracker` feature the
    /// handover builds `UnsupportedProvisioner` and refuses honestly.
    Microvm { config: MicrovmConfig },
```

- [ ] **Step 3: Extend the engine re-export**

In `crates/engine/src/lib.rs`, update the `pub use otto_remote::{...}` block to add `MicrovmConfig` and `MicrovmTarget`:

```rust
pub use otto_remote::{
    MicrovmConfig, MicrovmTarget, PromoteBundle, PromoteConfig, PromoteMode, ProvisionedMachine,
    Provisioner, RemoteHandle, RemoteTarget, UnsupportedProvisioner, VpsTarget, promote,
};
```

- [ ] **Step 4: Build the workspace**

Run: `cargo build --workspace`
Expected: FAIL — `handle_handover` in `serve.rs` matches on `PromoteMode` and is now non-exhaustive (missing the `Microvm` arm). This is expected; Task 7 adds the arm. To confirm the new types themselves compile, run instead:

Run: `cargo build -p otto-remote`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/remote/src/lib.rs crates/engine/src/lib.rs
git commit -m "feat(remote): add MicrovmConfig and PromoteMode::Microvm"
```

---

## Task 6: Add the `firecracker` feature and `FirecrackerProvisioner`

Real host-side orchestration behind a default-off feature. Pure builders are unit-tested; the actual VM boot is external.

**Files:**
- Modify: `crates/remote/Cargo.toml`
- Modify: `crates/engine/Cargo.toml`
- Modify: `crates/remote/src/lib.rs`
- Create: `crates/remote/src/firecracker.rs`

- [ ] **Step 1: Declare the features**

In `crates/remote/Cargo.toml`, add a `[features]` section after `[dependencies]`:

```toml
[features]
# Compiles the real Firecracker microVM provisioner. Off by default: default builds and CI never
# compile hypervisor-specific code. Needs no extra deps (uses std::process + the existing reqwest).
firecracker = []
```

In `crates/engine/Cargo.toml`, add (or extend) a `[features]` section that forwards the flag:

```toml
[features]
firecracker = ["otto-remote/firecracker"]
```

- [ ] **Step 2: Declare the module in `lib.rs`**

In `crates/remote/src/lib.rs`, add near the top (after the module doc comment / `use` lines):

```rust
#[cfg(feature = "firecracker")]
mod firecracker;
#[cfg(feature = "firecracker")]
pub use firecracker::FirecrackerProvisioner;
```

- [ ] **Step 3: Write the pure-logic helpers and provisioner**

Create `crates/remote/src/firecracker.rs`:

```rust
//! `FirecrackerProvisioner` — boots an ephemeral microVM running `otto serve --accept-promotions`,
//! returns a `ProvisionedMachine` whose guardian task kills the VM on drop. Behind the `firecracker`
//! cargo feature: default builds never compile it. The actual boot needs an operator-supplied kernel
//! + rootfs and a host hypervisor, so it cannot run in CI; the pure builders below are unit-tested.

use std::process::Child;
use std::time::Instant;

use async_trait::async_trait;

use crate::{MicrovmConfig, ProvisionedMachine, Provisioner};

/// Build the Firecracker machine-config JSON (consumed via `--config-file`): a boot-source (kernel +
/// guest cmdline), a single root drive (the rootfs), one network interface bound to the host tap, and
/// the machine sizing. `boot_args` carries the guest contract (`otto.token`/`otto.port`/`otto.root`).
fn fc_config_json(config: &MicrovmConfig, token: &str) -> serde_json::Value {
    serde_json::json!({
        "boot-source": {
            "kernel_image_path": config.kernel.to_string_lossy(),
            "boot_args": guest_cmdline(config, token),
        },
        "drives": [{
            "drive_id": "rootfs",
            "path_on_host": config.rootfs.to_string_lossy(),
            "is_root_device": true,
            "is_read_only": false,
        }],
        "network-interfaces": [{
            "iface_id": "eth0",
            "host_dev_name": config.tap,
            "guest_mac": "AA:FC:00:00:00:01",
        }],
        "machine-config": {
            "vcpu_count": config.vcpus,
            "mem_size_mib": config.mem_mib,
        },
    })
}

/// The guest kernel cmdline: a minimal console plus the otto contract the rootfs init reads to launch
/// `otto serve --accept-promotions`. The token rides the cmdline (single-tenant ephemeral guest; same
/// trust domain as the source — `/proc/cmdline` exposure inside the guest is acceptable in v1).
fn guest_cmdline(config: &MicrovmConfig, token: &str) -> String {
    format!(
        "console=ttyS0 reboot=k panic=1 pci=off \
         otto.token={token} otto.port={port} otto.root=/workspace",
        port = config.port,
    )
}

/// The URL the host polls to detect the guest serve is up. Every real route is gated, so any HTTP
/// response (401/404 included) means "serve is listening".
fn readiness_url(config: &MicrovmConfig) -> String {
    format!("http://{}:{}/", config.guest_ip, config.port)
}

/// Fail fast if the operator-supplied prerequisites are missing, before spawning anything.
fn validate_prereqs(config: &MicrovmConfig) -> anyhow::Result<()> {
    for (label, path) in [
        ("firecracker binary", &config.fc_bin),
        ("kernel image", &config.kernel),
        ("rootfs image", &config.rootfs),
    ] {
        if !path.exists() {
            anyhow::bail!("microVM prerequisite missing: {label} not found at {}", path.display());
        }
    }
    Ok(())
}

/// Owns the running VM + its scratch dir; `Drop` (sync) kills the child and removes the dir, so
/// aborting the guardian task disposes the machine. It does not delete the tap (operator-created).
struct FirecrackerGuard {
    child: Child,
    jail_dir: std::path::PathBuf,
}

impl Drop for FirecrackerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.jail_dir);
    }
}

/// Boots ephemeral Firecracker microVMs. `token` is the bearer the guest serve requires (shared from
/// the source, as loopback/vps do).
pub struct FirecrackerProvisioner {
    config: MicrovmConfig,
    token: String,
}

impl FirecrackerProvisioner {
    pub fn new(config: MicrovmConfig, token: impl Into<String>) -> Self {
        Self { config, token: token.into() }
    }
}

#[async_trait]
impl Provisioner for FirecrackerProvisioner {
    async fn provision(&self) -> anyhow::Result<ProvisionedMachine> {
        validate_prereqs(&self.config)?;

        // Per-machine scratch dir + config file.
        let jail_dir = std::env::temp_dir().join(format!("otto-fc-{}", self.config.guest_ip));
        std::fs::create_dir_all(&jail_dir)?;
        let cfg_path = jail_dir.join("vm-config.json");
        let cfg = fc_config_json(&self.config, &self.token);
        std::fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg)?)?;

        // Spawn firecracker with the config file.
        let child = std::process::Command::new(&self.config.fc_bin)
            .arg("--no-api")
            .arg("--config-file")
            .arg(&cfg_path)
            .current_dir(&jail_dir)
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn firecracker: {e}"))?;

        let guard = FirecrackerGuard { child, jail_dir };

        // Poll until the guest serve answers (any HTTP status) or the boot timeout elapses.
        let url = readiness_url(&self.config);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?;
        let deadline = Instant::now() + self.config.boot_timeout;
        loop {
            if client.get(&url).send().await.is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                // `guard` drops here → VM killed, scratch removed. Nothing leaks on timeout.
                anyhow::bail!("microVM did not become reachable within boot timeout");
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // Guardian task: owns the guard, parks until aborted; abort → guard Drop → VM disposed.
        let task = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });

        Ok(ProvisionedMachine {
            endpoint: format!("ws://{}:{}", self.config.guest_ip, self.config.port),
            token: self.token.clone(),
            task,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_config() -> MicrovmConfig {
        MicrovmConfig {
            kernel: PathBuf::from("/img/vmlinux"),
            rootfs: PathBuf::from("/img/rootfs.ext4"),
            fc_bin: PathBuf::from("/usr/bin/firecracker"),
            tap: "fc-tap0".to_string(),
            guest_ip: "172.16.0.2".to_string(),
            port: 7878,
            vcpus: 2,
            mem_mib: 1024,
            boot_timeout: std::time::Duration::from_secs(10),
        }
    }

    #[test]
    fn config_json_has_boot_drive_network_and_machine() {
        let v = fc_config_json(&sample_config(), "tok");
        assert_eq!(v["boot-source"]["kernel_image_path"], "/img/vmlinux");
        assert_eq!(v["drives"][0]["is_root_device"], true);
        assert_eq!(v["drives"][0]["path_on_host"], "/img/rootfs.ext4");
        assert_eq!(v["network-interfaces"][0]["host_dev_name"], "fc-tap0");
        assert_eq!(v["machine-config"]["vcpu_count"], 2);
        assert_eq!(v["machine-config"]["mem_size_mib"], 1024);
    }

    #[test]
    fn guest_cmdline_carries_the_otto_contract() {
        let line = guest_cmdline(&sample_config(), "secret-tok");
        assert!(line.contains("otto.token=secret-tok"), "{line}");
        assert!(line.contains("otto.port=7878"), "{line}");
        assert!(line.contains("otto.root=/workspace"), "{line}");
    }

    #[test]
    fn readiness_url_is_guest_ip_and_port() {
        assert_eq!(readiness_url(&sample_config()), "http://172.16.0.2:7878/");
    }

    #[test]
    fn validate_prereqs_errors_when_a_path_is_missing() {
        // sample_config points at non-existent paths → error mentioning the first missing prereq.
        let err = validate_prereqs(&sample_config()).unwrap_err().to_string();
        assert!(err.contains("firecracker binary"), "{err}");
    }

    #[test]
    fn validate_prereqs_ok_when_all_present() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let config = MicrovmConfig {
            fc_bin: mk("firecracker"),
            kernel: mk("vmlinux"),
            rootfs: mk("rootfs.ext4"),
            ..sample_config()
        };
        assert!(validate_prereqs(&config).is_ok());
    }
}
```

- [ ] **Step 4: Add `tempfile` as a dev-dependency of `otto-remote` (for the prereq test)**

In `crates/remote/Cargo.toml`, add a `[dev-dependencies]` section if absent:

```toml
[dev-dependencies]
tempfile = { workspace = true }
```

(`tempfile` is already a workspace dependency used by other crates' tests.)

- [ ] **Step 5: Run the gated unit tests**

Run: `cargo test -p otto-remote --features firecracker firecracker::`
Expected: PASS — all five pure-logic tests pass. No VM is started.

- [ ] **Step 6: Confirm the default build is unchanged**

Run: `cargo build -p otto-remote && cargo build --workspace`
Expected: `otto-remote` PASSES with no firecracker code compiled; `--workspace` still FAILS only on the non-exhaustive `handle_handover` match (added in Task 7) — confirm the error is exactly that and not a firecracker reference.

- [ ] **Step 7: Commit**

```bash
git add crates/remote/Cargo.toml crates/remote/src/lib.rs crates/remote/src/firecracker.rs crates/engine/Cargo.toml
git commit -m "feat(remote): add feature-gated FirecrackerProvisioner with unit-tested host logic"
```

---

## Task 7: Wire the `Microvm` arm in `handle_handover`

**Files:**
- Modify: `crates/engine/src/serve.rs:603-722` (the `handle_handover` fn and its `match &cfg.mode` block)

- [ ] **Step 1: Add the demote refusal for microvm mode**

In `handle_handover`, the early `if !to_remote { ... }` block currently handles only `PromoteMode::Vps`. After that vps demote block (before the idempotency-cache lookup), add a microvm demote refusal:

```rust
    if !to_remote {
        if let otto_remote::PromoteMode::Microvm { .. } = &cfg.mode {
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
    }
```

- [ ] **Step 2: Add the `Microvm` arm to the target-building match**

In the `let target: Box<dyn otto_remote::RemoteTarget> = match &cfg.mode { ... }` block (currently the `Loopback` and `Vps` arms near line 692), add a `Microvm` arm. Build a `FirecrackerProvisioner` under the feature, else `UnsupportedProvisioner`:

```rust
                    otto_remote::PromoteMode::Microvm { config } => {
                        #[cfg(feature = "firecracker")]
                        let provisioner: std::sync::Arc<dyn otto_remote::Provisioner> =
                            std::sync::Arc::new(otto_remote::FirecrackerProvisioner::new(
                                config.clone(),
                                cfg.token.clone(),
                            ));
                        #[cfg(not(feature = "firecracker"))]
                        let provisioner: std::sync::Arc<dyn otto_remote::Provisioner> = {
                            let _ = config; // unused without the firecracker feature
                            std::sync::Arc::new(otto_remote::UnsupportedProvisioner)
                        };
                        Box::new(otto_engine::MicrovmTarget::new(provisioner))
                    }
```

Note: reference `MicrovmTarget` via the path already in use in this file for `otto_remote` types — if the file imports them as `otto_remote::X`, use `otto_remote::MicrovmTarget` instead of `otto_engine::MicrovmTarget`. Match the surrounding `Loopback`/`Vps` arms' pathing exactly (they use `otto_remote::VpsTarget` and `LoopbackTarget`). So use `Box::new(otto_remote::MicrovmTarget::new(provisioner))`.

- [ ] **Step 3: Build the workspace (default features)**

Run: `cargo build --workspace`
Expected: PASS — the match is now exhaustive; without the firecracker feature the arm uses `UnsupportedProvisioner`.

- [ ] **Step 4: Build with the firecracker feature**

Run: `cargo build -p otto-engine --features firecracker`
Expected: PASS — the `#[cfg(feature = "firecracker")]` arm compiles against `FirecrackerProvisioner`.

- [ ] **Step 5: Run clippy on the touched crates**

Run: `cargo clippy -p otto-engine -p otto-remote --all-targets`
Expected: no warnings from the new code.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/serve.rs
git commit -m "feat(engine): dispatch PromoteMode::Microvm in handle_handover"
```

---

## Task 8: Wire the `--promote-microvm` CLI flag

**Files:**
- Modify: `crates/engine/src/main.rs:167-257`

- [ ] **Step 1: Add the flag variable and parse arm**

In `cmd_serve`, beside the other promote flags (after `let mut promote_vps: Option<String> = None;` near line 178), add:

```rust
    let mut promote_microvm = false;
```

In the argument `match a.as_str()` block, after the `"--promote-vps"` arm (near line 212), add:

```rust
            "--promote-microvm" => promote_microvm = true,
```

- [ ] **Step 2: Add an `OTTO_FC_*` → `MicrovmConfig` reader**

Add a free function in `crates/engine/src/main.rs` (near the other helpers like `parse_root`):

```rust
/// Read Firecracker microVM parameters from `OTTO_FC_*` env vars. Defaults match common Firecracker
/// quickstart values; required paths have no default. Env-reading lives here (the CLI edge), never
/// in `otto-remote`, mirroring how `build_router` reads its env.
fn microvm_config_from_env() -> otto_engine::MicrovmConfig {
    let req = |k: &str| std::env::var(k).unwrap_or_default();
    let num = |k: &str, d: u32| std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
    otto_engine::MicrovmConfig {
        kernel: PathBuf::from(req("OTTO_FC_KERNEL")),
        rootfs: PathBuf::from(req("OTTO_FC_ROOTFS")),
        fc_bin: PathBuf::from(
            std::env::var("OTTO_FC_BIN").unwrap_or_else(|_| "firecracker".to_string()),
        ),
        tap: std::env::var("OTTO_FC_TAP").unwrap_or_else(|_| "fc-tap0".to_string()),
        guest_ip: std::env::var("OTTO_FC_GUEST_IP").unwrap_or_else(|_| "172.16.0.2".to_string()),
        port: num("OTTO_FC_PORT", 7878) as u16,
        vcpus: num("OTTO_FC_VCPUS", 2),
        mem_mib: num("OTTO_FC_MEM_MIB", 1024),
        boot_timeout: std::time::Duration::from_secs(num("OTTO_FC_BOOT_TIMEOUT_SECS", 30) as u64),
    }
}
```

- [ ] **Step 3: Extend the `promote` selection with mutual exclusion**

Replace the `let promote = match (promote_loopback, promote_vps) { ... }` block (lines ~238-257) so the three modes are mutually exclusive and `--promote-microvm` builds a `PromoteMode::Microvm`:

```rust
    let promote = match (promote_loopback, promote_vps, promote_microvm) {
        (l, v, m) if (l as u8) + (v.is_some() as u8) + (m as u8) > 1 => {
            eprintln!(
                "error: --promote-loopback, --promote-vps, and --promote-microvm are mutually exclusive"
            );
            std::process::exit(2);
        }
        (true, _, _) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            // The dot-prefix is load-bearing: `LocalWorkspace::list` skips dot-directories, so a
            // provisioned engine's restored store/workspace under here is never recursively
            // captured by a later `workspace.snapshot()`. Do not rename without that guarantee.
            mode: otto_engine::PromoteMode::Loopback {
                base_dir: root.join(".otto-remotes"),
            },
        }),
        (_, Some(endpoint), _) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Vps { endpoint },
        }),
        (_, _, true) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Microvm {
                config: microvm_config_from_env(),
            },
        }),
        (false, None, false) => None,
    };
```

- [ ] **Step 4: Build the workspace**

Run: `cargo build --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): add --promote-microvm CLI flag and OTTO_FC_* config"
```

---

## Task 9: End-to-end seam tests against an in-process serve

Proves provision → push → restore without a hypervisor, plus disposal and the unsupported path.

**Files:**
- Create: `crates/engine/tests/microvm.rs`

- [ ] **Step 1: Write the test fixtures + the seam round-trip test (failing)**

Create `crates/engine/tests/microvm.rs`:

```rust
//! microVM Provisioner seam, exercised against an in-process `otto serve --accept-promotions` on an
//! ephemeral loopback port (no hypervisor). Proves the MicrovmTarget composition end to end.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine::{
    EngineService, MicrovmTarget, PromoteBundle, ProvisionedMachine, Provisioner, RemoteTarget,
    UnsupportedProvisioner, build_default_registry, build_tool_registry, serve_app, serve_run,
};
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStatus, SqliteStore};
use otto_protocol::{CapabilitiesManifest, SessionId};
use otto_workspace::LocalWorkspace;

const TOKEN: &str = "microvm-token";

fn caps() -> CapabilitiesManifest {
    CapabilitiesManifest { engine_remote: false, local_llm: false, remote_llm: false, sandbox: false }
}

fn sample_bundle(id: SessionId, files: Vec<(&str, &[u8])>) -> PromoteBundle {
    PromoteBundle {
        session: SessionState {
            id,
            goal: "g".to_string(),
            status: SessionStatus::Active,
            config: serde_json::json!({}),
            events: vec![],
            turns: vec![],
        },
        workspace: WorkspaceSnapshot {
            files: files.into_iter().map(|(p, b)| (PathBuf::from(p), b.to_vec())).collect(),
        },
    }
}

/// A `Provisioner` that boots an in-process `otto serve --accept-promotions` on `127.0.0.1:0` — the
/// CI stand-in for a real microVM. The serve task IS the disposal handle (abort stops serving).
struct TestServeProvisioner {
    // Tempdirs are retained for the lifetime of the provisioner so the booted serve's store/workspace
    // outlive provisioning; in these tests the provisioner is kept alive by the test body.
    _ws: tempfile::TempDir,
    _db: tempfile::TempDir,
    endpoint: String,
    listener: std::sync::Mutex<Option<std::net::TcpListener>>,
}

impl TestServeProvisioner {
    fn new() -> Self {
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        Self {
            _ws: ws,
            _db: db,
            endpoint: format!("ws://127.0.0.1:{port}"),
            listener: std::sync::Mutex::new(Some(listener)),
        }
    }

    fn http_base(&self) -> String {
        self.endpoint.replace("ws://", "http://")
    }
}

#[async_trait]
impl Provisioner for TestServeProvisioner {
    async fn provision(&self) -> anyhow::Result<ProvisionedMachine> {
        let ws_path = self._ws.path().to_path_buf();
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_path));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_path));
        let tools = Arc::new(build_tool_registry(tools_ws, ws_path.clone()));
        let store: Arc<dyn otto_persistence::SessionStore> =
            Arc::new(SqliteStore::open(self._db.path().join("r.db")).await.unwrap());
        let service = EngineService::new(
            store,
            Arc::new(build_default_registry()),
            Arc::from(otto_engine::build_router()),
            workspace,
            tools,
        );
        // accept_promotions = true so the /promote restore RPC is live.
        let app = serve_app(service, TOKEN.to_string(), caps(), None, true);
        let listener = self.listener.lock().unwrap().take().expect("provision once");
        let task = tokio::spawn(async move {
            serve_run(listener, app, None).await.unwrap();
        });
        Ok(ProvisionedMachine { endpoint: self.endpoint.clone(), token: TOKEN.to_string(), task })
    }
}

#[tokio::test]
async fn microvm_target_seam_round_trip() {
    let provisioner = Arc::new(TestServeProvisioner::new());
    let http_base = provisioner.http_base();
    let target = MicrovmTarget::new(provisioner.clone());

    let id = SessionId::new();
    let bundle = sample_bundle(id, vec![("out.txt", b"HELLO")]);
    let handle = target.provision(&bundle).await.unwrap();
    assert_eq!(handle.endpoint, provisioner.endpoint);

    // Prove the restore landed: export the session back off the provisioned serve and check the file.
    let resp = reqwest::Client::new()
        .post(format!("{http_base}/export"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "session": id.0.to_string() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let restored: PromoteBundle = resp.json().await.unwrap();
    assert_eq!(restored.session.id, id);
    assert!(
        restored.workspace.files.iter().any(|(p, b)| p == &PathBuf::from("out.txt") && b == b"HELLO"),
        "restored workspace should contain out.txt: {:?}",
        restored.workspace.files
    );

    // Keep `handle` alive until here so its task (the serve) is not aborted mid-assertion.
    drop(handle);
}
```

- [ ] **Step 2: Run the test to verify it fails (then passes)**

Run: `cargo test -p otto-engine --test microvm microvm_target_seam_round_trip`
Expected: PASS. (Everything it depends on — `MicrovmTarget`, the `/promote` + `/export` RPCs — already exists from Tasks 4 and the shipped vps work, so this passes once the file compiles. If it does not compile, fix imports before moving on.)

- [ ] **Step 3: Add the disposal + unsupported tests**

Append to `crates/engine/tests/microvm.rs`:

```rust
#[tokio::test]
async fn microvm_target_teardown_stops_the_machine() {
    let provisioner = Arc::new(TestServeProvisioner::new());
    let http_base = provisioner.http_base();
    let target = MicrovmTarget::new(provisioner.clone());

    let bundle = sample_bundle(SessionId::new(), vec![]);
    let handle = target.provision(&bundle).await.unwrap();

    // Teardown aborts the serve task → the endpoint stops listening.
    target.teardown(handle).await.unwrap();
    // Give the abort a moment to drop the listener.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let result = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap()
        .post(format!("{http_base}/promote"))
        .bearer_auth(TOKEN)
        .json(&sample_bundle(SessionId::new(), vec![]))
        .send()
        .await;
    assert!(result.is_err(), "serve should be unreachable after teardown");
}

#[tokio::test]
async fn microvm_target_over_unsupported_provisioner_errs() {
    let target = MicrovmTarget::new(Arc::new(UnsupportedProvisioner));
    let bundle = sample_bundle(SessionId::new(), vec![]);
    let err = target.provision(&bundle).await.unwrap_err().to_string();
    assert!(err.contains("microVM provisioning requires"), "{err}");
}
```

- [ ] **Step 4: Run the full microvm test file**

Run: `cargo test -p otto-engine --test microvm`
Expected: PASS — all three tests.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/tests/microvm.rs
git commit -m "test(engine): seam round-trip, teardown, and unsupported microVM target"
```

---

## Task 10: Add the handover-dispatch test and update docs

**Files:**
- Modify: `crates/engine/tests/microvm.rs`
- Modify: `docs/ARCHITECTURE.md:213-226`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add a handover-dispatch test (no firecracker feature → honest refusal)**

Append to `crates/engine/tests/microvm.rs`. This boots a source serve in `Microvm` mode and asserts that, without the `firecracker` feature, `PromoteToRemote` surfaces the `UnsupportedProvisioner` error and `DemoteToLocal` returns the ephemeral-not-supported error:

```rust
use futures_util::SinkExt;
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn authed_ws_request(url: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = url.to_string().into_client_request().unwrap();
    req.headers_mut().insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}

async fn next_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<serde_json::Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(_)) => return None,
        }
    }
}

/// Start a source serve in Microvm promote mode (config paths are dummy: without the firecracker
/// feature the provisioner is Unsupported and never reads them). Returns its ws base.
async fn start_source_microvm() -> (String, tempfile::TempDir, tempfile::TempDir) {
    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, ws_dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(SqliteStore::open(db_dir.path().join("s.db")).await.unwrap());
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        Arc::from(otto_engine::build_router()),
        workspace,
        tools,
    );
    let config = otto_engine::MicrovmConfig {
        kernel: PathBuf::from("/nonexistent/vmlinux"),
        rootfs: PathBuf::from("/nonexistent/rootfs"),
        fc_bin: PathBuf::from("/nonexistent/firecracker"),
        tap: "fc-tap0".to_string(),
        guest_ip: "172.16.0.2".to_string(),
        port: 7878,
        vcpus: 2,
        mem_mib: 1024,
        boot_timeout: std::time::Duration::from_secs(5),
    };
    let promote = Some(otto_engine::PromoteConfig {
        token: TOKEN.to_string(),
        mode: otto_engine::PromoteMode::Microvm { config },
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("ws://127.0.0.1:{port}");
    let app = otto_engine::serve_app_with_base(
        service, TOKEN.to_string(), caps(), promote, false, base.clone(),
    );
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (base, ws_dir, db_dir)
}

#[cfg(not(feature = "firecracker"))]
#[tokio::test]
async fn handover_microvm_promote_is_unsupported_without_feature() {
    let (src_ws, _w, _d) = start_source_microvm().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = next_json(&mut ws).await.unwrap();
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    let promote = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&promote).unwrap())).await.unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "error" {
            assert!(
                f["message"].as_str().unwrap().contains("microVM provisioning requires"),
                "{f:?}"
            );
            break;
        }
        assert_ne!(f["type"], "promoted", "promote must not succeed without firecracker: {f:?}");
    }
}

#[tokio::test]
async fn handover_microvm_demote_is_unsupported() {
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
            assert!(f["message"].as_str().unwrap().contains("microvm mode"), "{f:?}");
            break;
        }
        assert_ne!(f["type"], "demoted", "demote must not succeed in microvm mode: {f:?}");
    }
}
```

- [ ] **Step 2: Run the handover tests**

Run: `cargo test -p otto-engine --test microvm`
Expected: PASS — including the two handover-dispatch tests. (`futures-util` and `tokio-tungstenite` are already dev-deps of `otto-engine`, used by `vps_promote.rs`.)

- [ ] **Step 3: Run the whole default suite to confirm determinism is untouched**

Run: `cargo test --workspace`
Expected: PASS with no env vars set. The new flags are opt-in; firecracker is behind a default-off feature; all default paths stay offline.

- [ ] **Step 4: Update `docs/ARCHITECTURE.md`**

In `docs/ARCHITECTURE.md`, in the `RemoteTarget` section (the paragraph ending near line 224-226 about the `UnsupportedTarget` boundary and `microvm` being v2), replace the closing sentences with:

```markdown
The machine-provisioning step now lives behind a **`Provisioner` seam** (`provision()` boots a
reachable `otto serve --accept-promotions`; disposal rides the returned task). `MicrovmTarget`
composes any `Provisioner` with the shared `push_promote_bundle` restore-push, and the `microvm`
provisioner is **shipped**: `FirecrackerProvisioner` (behind the default-off `firecracker` cargo
feature) boots an ephemeral per-session microVM and restores the bundle into it via the same gated
`POST /promote`. `UnsupportedProvisioner` (which **replaced `UnsupportedTarget`**) is the single
honest boundary — "no hypervisor / kernel / rootfs in-tree." The seam is proven end-to-end in CI
against an in-process serve; the real VM boot needs operator-supplied images and a host hypervisor.
**demote-from-microvm** (pulling an ephemeral session back) is the next follow-up.
```

Also update the crate-tree comment at line 37 (`# RemoteTarget seam + vps (shipped) / microvm (v2).`) to:

```markdown
│   ├── remote           # RemoteTarget + Provisioner seam; vps + microvm (firecracker, feat-gated). LoopbackTarget stays in engine.
```

- [ ] **Step 5: Update `CLAUDE.md`**

In `CLAUDE.md`, find the sentence describing the remote axis state (the paragraph mentioning "the `remote` crate split is **shipped**" and "a `microvm` `RemoteTarget` is still ahead"). Replace "a `microvm` `RemoteTarget` is still ahead" with:

```markdown
the `microvm` axis is **shipped** via a `Provisioner` seam: `MicrovmTarget` composes any
`Provisioner` with the shared restore-push, `FirecrackerProvisioner` (behind the default-off
`firecracker` feature) boots an ephemeral per-session microVM, and `UnsupportedProvisioner`
(which replaced `UnsupportedTarget`) is the single machine-provisioning boundary;
demote-from-microvm is the remaining follow-up
```

Also update the `remote` crate row in the crate table: change its description to mention the `Provisioner` seam, `MicrovmTarget`, `FirecrackerProvisioner` (feature-gated), and `UnsupportedProvisioner` in place of `UnsupportedTarget`.

- [ ] **Step 6: Final verification**

Run: `cargo test --workspace && cargo test -p otto-remote --features firecracker && cargo clippy --workspace --all-targets`
Expected: all PASS, no clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/engine/tests/microvm.rs docs/ARCHITECTURE.md CLAUDE.md
git commit -m "test(engine): microvm handover dispatch; docs: record the Provisioner seam"
```

---

## Coverage check (spec → task)

- Provisioner trait + ProvisionedMachine → Task 2
- MicrovmTarget (provisioner-generic, disposal rides task, no-leak on push failure) → Task 4, tested in Task 9
- Shared `push_promote_bundle` (VpsTarget refactor, behavior-preserving) → Task 1
- UnsupportedProvisioner replaces UnsupportedTarget (single boundary) → Task 3, tested in Tasks 4/9
- FirecrackerProvisioner (feature-gated; config JSON, guest cmdline contract, readiness, guardian Drop, prereq validation) → Task 6
- MicrovmConfig as plain data; OTTO_FC_* read at the CLI edge → Tasks 5, 8
- PromoteMode::Microvm + handle_handover dispatch + demote refusal → Tasks 5, 7
- `--promote-microvm` flag + mutual exclusion → Task 8
- Crux end-to-end seam test against in-process serve → Task 9
- Determinism suite untouched → Task 10 Step 3
- Docs (ARCHITECTURE, CLAUDE) → Task 10
- Non-goals (demote-from-microvm, guest image, real-VM integration test, multi-session, tap creation) → intentionally excluded; honest errors/docs cover them
