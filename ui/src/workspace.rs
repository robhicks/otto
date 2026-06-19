//! Browser-only `fetch` client for the engine's bearer-authed `POST /workspace` RPC.
//! `gloo-net` targets wasm32 (it wraps `fetch`), so this module is verified by the wasm
//! build and manual testing — the pure routing/decoding logic lives in `url.rs`/`tree.rs`.

use std::path::PathBuf;

use gloo_net::http::Request;
use otto_protocol::{WorkspaceRequest, WorkspaceResponse};

/// Send one `WorkspaceRequest` to `{http_base}/workspace` with the bearer token.
/// Maps transport failures, non-2xx, and `WorkspaceResponse::Error` to `Err(String)`.
async fn rpc(
    http_base: &str,
    token: &str,
    req: &WorkspaceRequest,
) -> Result<WorkspaceResponse, String> {
    let url = format!("{}/workspace", http_base.trim_end_matches('/'));
    let resp = Request::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .json(req)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("workspace rpc failed: HTTP {}", resp.status()));
    }
    let parsed: WorkspaceResponse = resp.json().await.map_err(|e| e.to_string())?;
    if let WorkspaceResponse::Error { message } = &parsed {
        return Err(message.clone());
    }
    Ok(parsed)
}

/// List every file in the served workspace.
pub async fn list_files(http_base: &str, token: &str) -> Result<Vec<PathBuf>, String> {
    match rpc(
        http_base,
        token,
        &WorkspaceRequest::List {
            glob: "**/*".to_string(),
        },
    )
    .await?
    {
        WorkspaceResponse::List { paths } => Ok(paths),
        other => Err(format!("unexpected response to List: {other:?}")),
    }
}

/// Read one file's bytes.
pub async fn read_file(http_base: &str, token: &str, path: PathBuf) -> Result<Vec<u8>, String> {
    match rpc(http_base, token, &WorkspaceRequest::Read { path }).await? {
        WorkspaceResponse::Read { bytes } => Ok(bytes),
        other => Err(format!("unexpected response to Read: {other:?}")),
    }
}
