//! `LoopbackTarget` — an in-process `RemoteTarget` that boots a real second engine. It lives in
//! `otto-engine` (not `otto-remote`) because it constructs an `EngineService` and serves it; keeping
//! it here makes the crate dependency one-directional (`engine → otto-remote`, never the reverse).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::Router;
use otto_engine_core::traits::Workspace;
use otto_persistence::{SessionStore, SqliteStore};
use otto_remote::{PromoteBundle, PromoteConfig, PromoteMode, RemoteHandle, RemoteTarget};
use otto_workspace::LocalWorkspace;

use crate::service::EngineService;
use crate::{build_default_registry, build_router, build_tool_registry};

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

        Ok(RemoteHandle::with_task(
            format!("ws://127.0.0.1:{port}"),
            self.token.clone(),
            task,
        ))
    }

    async fn teardown(&self, mut handle: RemoteHandle) -> anyhow::Result<()> {
        handle.abort();
        Ok(())
    }
}
