//! `RemoteWorkspace`: a `Workspace` implemented over the bearer-authed `POST /workspace` RPC
//! of a remote engine. Each trait method is one unary request. The server enforces the
//! permission floor and path containment, so this client is a thin proxy — with one deliberate
//! exception: `snapshot` re-applies the sensitive-path floor to what the peer returned, because
//! "the server enforces it" is a statement about a peer this client does not control.

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
    /// `RemoteWorkspace` is **source/client-side only**. It is constructed by a client (or a source
    /// engine acting for a client) to reach a served engine over the `/workspace` RPC. It must never be
    /// constructed on a promoted machine: a remote that can reach yet another machine's workspace, or
    /// that holds a source-valid credential, is a pivot. It has no production construction site on a
    /// promoted machine; keep it that way.
    ///
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
            WorkspaceResponse::Snapshot { files } => {
                // Re-apply the floor to what the peer sent. An otto peer already filters (its
                // `/workspace` handler is gate-filtered, a superset of the floor), so this is
                // normally a no-op — but satisfying the seam's contract by *delegation* means
                // trusting the peer to be an up-to-date otto. That is precisely the shape of
                // assumption that caused this seam's last leak: the walk was believed to cover
                // the floor because it skipped dotfiles, and it did not. Enforce locally.
                Ok(WorkspaceSnapshot {
                    files: crate::strip_sensitive_files(files),
                })
            }
            other => anyhow::bail!("unexpected response to Snapshot: {other:?}"),
        }
    }
}
