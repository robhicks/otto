//! MCP client adapter: spawn an MCP stdio server, list its tools, and wrap each as a `Tool` so it
//! registers in the `ToolRegistry` behind the permission gate. The engine talks to MCP servers
//! over stdio (never by linking). rmcp API specifics are pinned to the resolved version.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Type alias for the running rmcp client service.
type McpClientService = RunningService<RoleClient, ()>;

/// A live MCP client connection. Optionally retained by the caller, but no longer the sole anchor
/// of the child process — each registered `McpTool` independently holds a strong ref.
pub struct McpConnection {
    #[allow(dead_code)]
    service: Arc<McpClientService>,
}

/// An rmcp-backed `Tool` that forwards calls to an MCP server over stdio.
///
/// Each `McpTool` holds a strong ref to the running MCP client service, so the spawned MCP server
/// stays alive as long as any of its tools are registered (independently of whether the caller
/// retains the `McpConnection`).
struct McpTool {
    /// Strong ref to the running client service — keeps the child process alive.
    service: Arc<McpClientService>,
    /// The tool name as the MCP server knows it (may use underscores).
    server_name: String,
    /// The tool name the `ToolRegistry` / gate sees (dotted: `fs.read` etc.).
    gate_name: String,
}

// ---------------------------------------------------------------------------
// McpTool: Tool impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.gate_name
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let arguments = args.as_object().cloned();
        let mut params = CallToolRequestParams::new(self.server_name.clone());
        if let Some(obj) = arguments {
            params = params.with_arguments(obj);
        }
        let result = self.service.peer().call_tool(params).await?;
        if result.is_error == Some(true) {
            // Extract the first text content as the error message.
            let msg = result
                .content
                .first()
                .and_then(|c| c.as_text())
                .map(|t| t.text.as_str().to_owned())
                .unwrap_or_else(|| "mcp tool error".to_string());
            anyhow::bail!("{}", msg);
        }
        // Prefer structured content; fall back to concatenating text items.
        if let Some(sc) = result.structured_content {
            return Ok(sc);
        }
        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("");
        if text.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

// ---------------------------------------------------------------------------
// Name remapping: server names -> gate names
// ---------------------------------------------------------------------------

/// Map server tool names to the gate names the `ToolRegistry` expects.
///
/// Currently a no-op for mcp-fs (which advertises dotted names natively). Kept as a client-side
/// normalization shim so MCP servers that expose underscore tool names (`fs_read`, ...) still
/// register under the dotted gate names (`fs.read`, ...).
fn to_gate_name(server_name: &str) -> String {
    match server_name {
        "fs_read" => "fs.read".to_string(),
        "fs_write" => "fs.write".to_string(),
        "fs_list" => "fs.list".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Public connect API
// ---------------------------------------------------------------------------

/// Spawn `command` as an MCP server, initialise the connection, list its tools, and return a
/// `McpConnection` (keeps the child alive) plus one `Arc<dyn Tool>` per advertised tool.
pub async fn connect(
    command: tokio::process::Command,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let transport = TokioChildProcess::new(command)?;
    let service = Arc::new(().serve(transport).await?);

    let tools = service.peer().list_all_tools().await?;

    let mcp_tools: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| {
            let server_name = t.name.to_string();
            let gate_name = to_gate_name(&server_name);
            Arc::new(McpTool {
                service: Arc::clone(&service),
                server_name,
                gate_name,
            }) as Arc<dyn Tool>
        })
        .collect();

    Ok((McpConnection { service }, mcp_tools))
}

/// Convenience: build the `mcp-fs <root>` command and connect.
pub async fn connect_fs(
    bin: &str,
    root: &Path,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let mut command = tokio::process::Command::new(bin);
    command.arg(root);
    connect(command).await
}

/// Convenience: build the `mcp-grep <root>` command and connect.
pub async fn connect_grep(
    bin: &str,
    root: &Path,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let mut command = tokio::process::Command::new(bin);
    command.arg(root);
    connect(command).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_fs_with_bogus_binary_errors() {
        let err = connect_fs("definitely-not-a-real-binary-xyz", Path::new(".")).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn connect_grep_with_bogus_binary_errors() {
        assert!(connect_grep("definitely-not-a-real-binary-xyz", Path::new(".")).await.is_err());
    }
}
