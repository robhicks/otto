//! In-process filesystem tools over a path-contained workspace. The read tools (`fs.read`,
//! `fs.list`) need only the read-only `WorkspaceRead` view; `fs.write` holds the full `Workspace`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::Edit;
use serde_json::{Value, json};

/// `fs.read` — args `{ "path": "<rel>" }` → `{ "content": "<utf8>" }`.
pub struct FsReadTool {
    workspace: Arc<dyn WorkspaceRead>,
}

impl FsReadTool {
    pub fn new(workspace: Arc<dyn WorkspaceRead>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs.read"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fs.read requires a string 'path' arg"))?;
        let bytes = self.workspace.read(Path::new(path)).await?;
        let content = String::from_utf8(bytes)?;
        Ok(json!({ "content": content }))
    }
}

/// `fs.write` — args `{ "path": "<rel>", "contents": "<utf8>" }` → `{ "bytes_written": <n> }`.
pub struct FsWriteTool {
    workspace: Arc<dyn Workspace>,
}

impl FsWriteTool {
    pub fn new(workspace: Arc<dyn Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "fs.write"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fs.write requires a string 'path' arg"))?;
        let contents = args
            .get("contents")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fs.write requires a string 'contents' arg"))?;
        let edit = Edit {
            path: PathBuf::from(path),
            new_contents: contents.to_string(),
        };
        let bytes_written = self.workspace.apply_edit(&edit).await?;
        Ok(json!({ "bytes_written": bytes_written }))
    }
}

/// `fs.list` — args `{ "glob": "<pat>" }` (optional, defaults "*") → `{ "paths": ["<rel>", ...] }`.
pub struct FsListTool {
    workspace: Arc<dyn WorkspaceRead>,
}

impl FsListTool {
    pub fn new(workspace: Arc<dyn WorkspaceRead>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for FsListTool {
    fn name(&self) -> &str {
        "fs.list"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let glob = args.get("glob").and_then(Value::as_str).unwrap_or("*");
        let paths = self.workspace.list(glob).await?;
        let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        Ok(json!({ "paths": paths }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_workspace::LocalWorkspace;

    fn ws() -> (tempfile::TempDir, Arc<dyn Workspace>) {
        let dir = tempfile::tempdir().unwrap();
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        (dir, ws)
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let (_dir, ws) = ws();
        let write = FsWriteTool::new(Arc::clone(&ws));
        let out = write
            .call(json!({"path": "a.txt", "contents": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, json!({ "bytes_written": 5 }));

        let read_ws: Arc<dyn WorkspaceRead> = ws.clone();
        let read = FsReadTool::new(read_ws);
        let out = read.call(json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(out, json!({ "content": "hello" }));
    }

    #[tokio::test]
    async fn list_returns_written_files() {
        let (_dir, ws) = ws();
        FsWriteTool::new(Arc::clone(&ws))
            .call(json!({"path": "a.txt", "contents": "x"}))
            .await
            .unwrap();
        let list_ws: Arc<dyn WorkspaceRead> = ws.clone();
        let out = FsListTool::new(list_ws).call(json!({})).await.unwrap();
        assert_eq!(out, json!({ "paths": ["a.txt"] }));
    }

    #[tokio::test]
    async fn read_missing_path_arg_errors() {
        let (_dir, ws) = ws();
        let err = FsReadTool::new(ws).call(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("requires a string 'path'"));
    }

    #[tokio::test]
    async fn write_rejects_escape_via_workspace_containment() {
        let (_dir, ws) = ws();
        let err = FsWriteTool::new(ws)
            .call(json!({"path": "../escape.txt", "contents": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("escapes workspace root"));
    }
}
