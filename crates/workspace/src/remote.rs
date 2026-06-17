//! `RemoteWorkspace`: a `Workspace` implemented over the bearer-authed `POST /workspace` RPC
//! of a remote engine. Each trait method is one unary request. The server enforces the
//! permission floor and path containment, so this client is a thin proxy.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::{Edit, WorkspaceSnapshot};
use otto_protocol::{WorkspaceRequest, WorkspaceResponse};

/// A workspace backed by a remote engine's `POST {base_url}/workspace` endpoint.
pub struct RemoteWorkspace {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl RemoteWorkspace {
    /// `base_url` is the engine origin (e.g. `http://127.0.0.1:7878` or `https://host:port`);
    /// `token` is the bearer token the server requires.
    ///
    /// Note: an https base_url uses the system/default root store; connecting to a self-signed
    /// remote is not yet supported (a later refinement).
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("build reqwest client"),
        }
    }

    async fn rpc(&self, req: &WorkspaceRequest) -> anyhow::Result<WorkspaceResponse> {
        let url = format!("{}/workspace", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("workspace rpc failed: HTTP {}", resp.status());
        }
        let parsed: WorkspaceResponse = resp.json().await?;
        if let WorkspaceResponse::Error { message } = &parsed {
            anyhow::bail!("workspace rpc error: {message}");
        }
        Ok(parsed)
    }
}

#[async_trait]
impl WorkspaceRead for RemoteWorkspace {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        match self
            .rpc(&WorkspaceRequest::Read {
                path: path.to_path_buf(),
            })
            .await?
        {
            WorkspaceResponse::Read { bytes } => Ok(bytes),
            other => anyhow::bail!("unexpected response to Read: {other:?}"),
        }
    }

    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>> {
        match self
            .rpc(&WorkspaceRequest::List {
                glob: glob.to_string(),
            })
            .await?
        {
            WorkspaceResponse::List { paths } => Ok(paths),
            other => anyhow::bail!("unexpected response to List: {other:?}"),
        }
    }
}

#[async_trait]
impl Workspace for RemoteWorkspace {
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
        match self
            .rpc(&WorkspaceRequest::ApplyEdit {
                path: edit.path.clone(),
                contents: edit.new_contents.clone(),
            })
            .await?
        {
            WorkspaceResponse::ApplyEdit { bytes_written } => Ok(bytes_written),
            other => anyhow::bail!("unexpected response to ApplyEdit: {other:?}"),
        }
    }

    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot> {
        match self.rpc(&WorkspaceRequest::Snapshot).await? {
            WorkspaceResponse::Snapshot { files } => Ok(WorkspaceSnapshot { files }),
            other => anyhow::bail!("unexpected response to Snapshot: {other:?}"),
        }
    }
}
