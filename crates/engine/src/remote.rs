//! Promote a session to another engine. `promote()` snapshots a session + its workspace into a
//! `PromoteBundle` and hands it to a `RemoteTarget`. `LoopbackTarget` provisions a real second
//! in-process engine (testable on loopback); `UnsupportedTarget` honestly refuses, marking the
//! boundary where a real VPS provisioner (external infra, manual-only) would go.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::Router;
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStore, SqliteStore};
use otto_protocol::SessionId;
use otto_workspace::LocalWorkspace;

use crate::service::EngineService;
use crate::{build_default_registry, build_router, build_tool_registry};

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
}

/// A captured session ready to move to another engine: persisted session state + workspace files.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PromoteBundle {
    pub session: SessionState,
    pub workspace: WorkspaceSnapshot,
}

/// A reachable, provisioned remote engine. `endpoint` is a `ws://host:port` base; `token` is the
/// bearer token it requires. Impl-private shutdown state tears it down on `teardown`.
pub struct RemoteHandle {
    pub endpoint: String,
    pub token: String,
    shutdown: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        if let Some(task) = self.shutdown.take() {
            task.abort();
        }
    }
}

#[async_trait]
pub trait RemoteTarget: Send + Sync {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle>;
    async fn teardown(&self, handle: RemoteHandle) -> anyhow::Result<()>;
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

/// A `RemoteTarget` that refuses to provision: a real cloud/VPS provisioner needs external
/// infrastructure (a machine, SSH, a deployed engine) and cannot run in-tree or in CI.
pub struct UnsupportedTarget;

#[async_trait]
impl RemoteTarget for UnsupportedTarget {
    async fn provision(&self, _bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle> {
        anyhow::bail!(
            "real VPS provisioning requires external infrastructure; not available in-tree"
        )
    }
    async fn teardown(&self, _handle: RemoteHandle) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Provisions a real second engine in-process: restores the bundle into a fresh sqlite store +
/// workspace under `base_dir` (one subdir per session id) and serves it on `127.0.0.1:0`.
pub struct LoopbackTarget {
    token: String,
    base_dir: PathBuf,
    /// The `engine_remote` capability the provisioned engine reports: `true` for promote
    /// (it's now "remote"), `false` for demote (back to "local").
    engine_remote: bool,
}

impl LoopbackTarget {
    /// `token` is the bearer the provisioned remote requires; `base_dir` is where the restored
    /// store + workspace are written; `engine_remote` is the capability flag it reports.
    pub fn new(token: impl Into<String>, base_dir: PathBuf, engine_remote: bool) -> Self {
        Self {
            token: token.into(),
            base_dir,
            engine_remote,
        }
    }
}

#[async_trait]
impl RemoteTarget for LoopbackTarget {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle> {
        // One isolated directory per provisioned session.
        let dir = self.base_dir.join(bundle.session.id.0.to_string());
        let ws_dir = dir.join("workspace");
        tokio::fs::create_dir_all(&ws_dir).await?;

        // Restore the session into a fresh store.
        let store = SqliteStore::open(dir.join("sessions.db")).await?;
        store.restore(&bundle.session).await?;

        // Restore the workspace files.
        LocalWorkspace::new(&ws_dir)
            .restore(&bundle.workspace)
            .await?;

        // Build the remote engine over the restored state.
        let store: Arc<dyn SessionStore> = Arc::new(store);
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_dir));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_dir));
        let tools = Arc::new(build_tool_registry(tools_ws, ws_dir.clone()));
        let router: Arc<dyn Router> = Arc::from(build_router());
        let service = EngineService::new(
            store,
            Arc::new(build_default_registry()),
            router,
            workspace,
            tools,
        );

        // This provisioned engine reports the configured capability and is itself promote-capable
        // (so the round-trip — demote, re-promote — works), rooted at a nested base dir.
        let capabilities = otto_protocol::CapabilitiesManifest {
            engine_remote: self.engine_remote,
            ..crate::build_capabilities()
        };
        let promote = Some(PromoteConfig {
            token: self.token.clone(),
            mode: PromoteMode::Loopback {
                base_dir: dir.join("promote"),
            },
        });
        let app = crate::serve::app(service, self.token.clone(), capabilities, promote, false);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let task = tokio::spawn(async move {
            let _ = crate::serve::run(listener, app, None).await;
        });

        Ok(RemoteHandle {
            endpoint: format!("ws://127.0.0.1:{port}"),
            token: self.token.clone(),
            shutdown: Some(task),
        })
    }

    async fn teardown(&self, mut handle: RemoteHandle) -> anyhow::Result<()> {
        if let Some(task) = handle.shutdown.take() {
            task.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SessionStatus;

    #[tokio::test]
    async fn unsupported_target_refuses_to_provision() {
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
        assert!(UnsupportedTarget.provision(&bundle).await.is_err());
    }
}
