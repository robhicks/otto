//! The engine-axis remote seam, split out of `otto-engine`. `promote()` snapshots a session + its
//! workspace into a `PromoteBundle` and hands it to a `RemoteTarget`. `VpsTarget` pushes the bundle
//! to an already-running `otto serve --accept-promotions`; `UnsupportedProvisioner` honestly refuses
//! at the provisioner layer, marking the machine-provisioning boundary. The in-process
//! `LoopbackTarget` lives in `otto-engine` (it boots an engine), implementing this crate's
//! `RemoteTarget`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStore};
use otto_protocol::SessionId;

/// Enables session handover on a served engine. `token` is the bearer the target requires (reused
/// from the source, by design); `mode` selects which `RemoteTarget` a handover provisions onto.
/// `ServeState` holds this as `Option`: `Some` ⟺ `--promote-loopback` or `--promote-vps`.
#[derive(Clone)]
pub struct PromoteConfig {
    pub token: String,
    pub mode: PromoteMode,
}

/// Which kind of remote a promote provisions onto.
#[derive(Clone)]
pub enum PromoteMode {
    /// Provision a fresh in-process engine, restoring under `base_dir` (loopback round-trip).
    Loopback { base_dir: PathBuf },
    /// Push to an already-running remote `otto serve` at `endpoint` (`ws://…` / `wss://…`).
    Vps { endpoint: String },
    /// Provision an ephemeral microVM (Firecracker), restore the bundle into it, then dispose it on
    /// drop. `config` is read from `OTTO_FC_*` by the CLI; without the `firecracker` feature the
    /// handover builds `UnsupportedProvisioner` and refuses honestly.
    Microvm { config: MicrovmConfig },
}

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

/// A captured session ready to move to another engine: persisted session state + workspace files.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PromoteBundle {
    pub session: SessionState,
    pub workspace: WorkspaceSnapshot,
}

/// A reachable, provisioned remote engine. `endpoint` is a `ws://host:port` base; `token` is the
/// bearer token it requires. An impl-private `shutdown` task tears down an in-process engine on
/// `teardown`/drop (set via `with_task`); network targets that own no task use `new`.
pub struct RemoteHandle {
    pub endpoint: String,
    pub token: String,
    shutdown: Option<tokio::task::JoinHandle<()>>,
}

impl RemoteHandle {
    /// A handle to a remote this process does not own (e.g. `VpsTarget`): nothing to abort.
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            shutdown: None,
        }
    }

    /// A handle to an in-process provisioned engine (`LoopbackTarget`): `task` is aborted on
    /// `teardown`/drop.
    pub fn with_task(
        endpoint: impl Into<String>,
        token: impl Into<String>,
        task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            shutdown: Some(task),
        }
    }

    /// Abort the backing task if any (idempotent). `Drop` also calls this.
    pub fn abort(&mut self) {
        if let Some(task) = self.shutdown.take() {
            task.abort();
        }
    }
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        self.abort();
    }
}

#[async_trait]
pub trait RemoteTarget: Send + Sync {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle>;
    async fn teardown(&self, handle: RemoteHandle) -> anyhow::Result<()>;
}

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

/// Snapshot `session` and its `workspace` and provision the result onto `target`. Does not stop
/// the source engine — handover (drop local, reconnect remote) is a client concern.
pub async fn promote(
    store: &dyn SessionStore,
    workspace: &dyn Workspace,
    session: SessionId,
    target: &dyn RemoteTarget,
) -> anyhow::Result<RemoteHandle> {
    let bundle = PromoteBundle {
        session: store.snapshot(session).await?,
        workspace: workspace.snapshot().await?,
    };
    target.provision(&bundle).await
}

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
        // Boot the machine, then wrap it in a RemoteHandle immediately. The handle's Drop aborts the
        // task, so if the push below fails the `?` returns, `handle` drops, and the booted machine
        // (in-process serve or microVM) is torn down — never leaked.
        let machine = self.provisioner.provision().await?;
        let handle = RemoteHandle::with_task(machine.endpoint, machine.token, machine.task);
        push_promote_bundle(&handle.endpoint, &handle.token, bundle).await?;
        Ok(handle)
    }

    async fn teardown(&self, _handle: RemoteHandle) -> anyhow::Result<()> {
        Ok(())
    }
}

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

/// A `RemoteTarget` that promotes onto an already-running, operator-managed `otto serve` over the
/// network. Unlike `LoopbackTarget`, it does not create or own the receiver — `teardown` is a
/// no-op so it never aborts the operator's long-lived server.
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

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SessionStatus;

    #[tokio::test]
    async fn unsupported_provisioner_refuses() {
        assert!(UnsupportedProvisioner.provision().await.is_err());
    }

    #[test]
    fn http_base_maps_ws_schemes() {
        // wss → https (the production path; otherwise the promote POST silently downgrades to
        // plaintext), ws → http (loopback), and an unrecognized scheme passes through verbatim.
        assert_eq!(http_base("wss://host:9000"), "https://host:9000");
        assert_eq!(http_base("ws://127.0.0.1:7878"), "http://127.0.0.1:7878");
        assert_eq!(http_base("http://host:1"), "http://host:1");
    }

    #[tokio::test]
    async fn microvm_target_aborts_machine_when_push_fails() {
        // A provisioner that boots a never-ending task pointed at a closed port, so the bundle-push
        // fails. We keep the task's AbortHandle to prove the task is aborted (not detached/leaked)
        // once provision() returns Err.
        struct DeadEndpointProvisioner {
            abort: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
            endpoint: String,
        }

        #[async_trait]
        impl Provisioner for DeadEndpointProvisioner {
            async fn provision(&self) -> anyhow::Result<ProvisionedMachine> {
                let task = tokio::spawn(std::future::pending::<()>());
                *self.abort.lock().unwrap() = Some(task.abort_handle());
                Ok(ProvisionedMachine {
                    endpoint: self.endpoint.clone(),
                    token: "t".to_string(),
                    task,
                })
            }
        }

        // Bind then drop a listener to obtain a definitely-closed local port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let provisioner = std::sync::Arc::new(DeadEndpointProvisioner {
            abort: std::sync::Mutex::new(None),
            endpoint: format!("ws://127.0.0.1:{port}"),
        });
        let target = MicrovmTarget::new(provisioner.clone());
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

        assert!(
            target.provision(&bundle).await.is_err(),
            "push to a closed port must fail"
        );

        let abort = provisioner.abort.lock().unwrap().take().expect("provisioned");
        // Allow the abort to take effect (no `time` feature, so yield instead of sleep). The bound
        // is generous: abort of a `pending()` task resolves in ≤1 scheduler turn on a healthy
        // runtime, so a high cap only guards against a saturated CI runner without ever hanging.
        for _ in 0..2000 {
            if abort.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            abort.is_finished(),
            "machine task must be aborted when push fails (no leak)"
        );
    }

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
        let target = MicrovmTarget::new(Arc::new(UnsupportedProvisioner));
        assert!(target.provision(&bundle).await.is_err());
    }
}
