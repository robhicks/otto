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
use otto_persistence::{SessionState, SessionStore};
use otto_protocol::SessionId;
use otto_workspace::LocalWorkspace;

use crate::service::EngineService;

/// A captured session ready to move to another engine: persisted session state + workspace files.
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
        anyhow::bail!("real VPS provisioning requires external infrastructure; not available in-tree")
    }
    async fn teardown(&self, _handle: RemoteHandle) -> anyhow::Result<()> {
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
