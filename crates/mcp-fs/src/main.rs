//! `mcp-fs <root>` — an MCP stdio server exposing path-contained fs.read/fs.write/fs.list over a
//! `LocalWorkspace` rooted at <root>. The engine spawns this and registers its tools behind the gate.

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::Edit;
use otto_workspace::LocalWorkspace;
use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Plain, rmcp-independent core (exact per spec)
// ---------------------------------------------------------------------------

/// The server struct wrapping a path-contained `LocalWorkspace`.
#[derive(Clone)]
pub struct FsServer {
    ws: Arc<LocalWorkspace>,
}

impl FsServer {
    pub fn new(root: PathBuf) -> Self {
        Self {
            ws: Arc::new(LocalWorkspace::new(root)),
        }
    }

    pub async fn do_read(&self, path: String) -> anyhow::Result<String> {
        let bytes = self.ws.read(std::path::Path::new(&path)).await?;
        Ok(String::from_utf8(bytes)?)
    }

    pub async fn do_write(&self, path: String, contents: String) -> anyhow::Result<u64> {
        self.ws
            .apply_edit(&Edit {
                path: PathBuf::from(path),
                new_contents: contents,
            })
            .await
    }

    pub async fn do_list(&self, glob: Option<String>) -> anyhow::Result<Vec<String>> {
        let paths = self.ws.list(glob.as_deref().unwrap_or("*")).await?;
        Ok(paths
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Arg structs for rmcp parameter deserialization
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadArgs {
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WriteArgs {
    path: String,
    contents: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListArgs {
    glob: Option<String>,
}

// ---------------------------------------------------------------------------
// rmcp tool wrappers (thin shims over the do_* methods)
// ---------------------------------------------------------------------------

fn to_err(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

#[tool_router(server_handler)]
impl FsServer {
    #[tool(name = "fs.read", description = "Read a file from the workspace")]
    async fn read(
        &self,
        Parameters(ReadArgs { path }): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let content = self.do_read(path).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "content": content }),
        ))
    }

    #[tool(name = "fs.write", description = "Write a file to the workspace")]
    async fn write(
        &self,
        Parameters(WriteArgs { path, contents }): Parameters<WriteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let n = self.do_write(path, contents).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "bytes_written": n }),
        ))
    }

    #[tool(name = "fs.list", description = "List files in the workspace")]
    async fn list(
        &self,
        Parameters(ListArgs { glob }): Parameters<ListArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let paths = self.do_list(glob).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "paths": paths }),
        ))
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-fs <root>"))?;
    let server = FsServer::new(root);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests (rmcp-independent — test the do_* methods directly)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let server = FsServer::new(dir.path().to_path_buf());
        let w = server.do_write("a.txt".into(), "hi".into()).await.unwrap();
        assert_eq!(w, 2); // bytes_written
        let c = server.do_read("a.txt".into()).await.unwrap();
        assert_eq!(c, "hi"); // content
    }

    #[tokio::test]
    async fn list_returns_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let server = FsServer::new(dir.path().to_path_buf());
        server.do_write("a.txt".into(), "x".into()).await.unwrap();
        let paths = server.do_list(Some("**".into())).await.unwrap();
        assert!(paths.contains(&"a.txt".to_string()));
    }

    #[tokio::test]
    async fn read_rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let server = FsServer::new(dir.path().to_path_buf());
        assert!(server.do_read("../escape.txt".into()).await.is_err());
    }
}
