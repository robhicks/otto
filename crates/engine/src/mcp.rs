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

/// Core connect: spawn `command`, list tools, map each server tool name to a gate name via `map`.
async fn connect_mapped(
    command: tokio::process::Command,
    map: impl Fn(&str) -> String,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let transport = TokioChildProcess::new(command)?;
    let service = Arc::new(().serve(transport).await?);

    let tools = service.peer().list_all_tools().await?;

    let mcp_tools: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| {
            let server_name = t.name.to_string();
            let gate_name = map(&server_name);
            Arc::new(McpTool {
                service: Arc::clone(&service),
                server_name,
                gate_name,
            }) as Arc<dyn Tool>
        })
        .collect();

    Ok((McpConnection { service }, mcp_tools))
}

/// Spawn `command` as an MCP server, initialise the connection, list its tools, and return a
/// `McpConnection` (keeps the child alive) plus one `Arc<dyn Tool>` per advertised tool.
pub async fn connect(
    command: tokio::process::Command,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    connect_mapped(command, to_gate_name).await
}

/// Namespaced gate name for a plugin-bundled MCP tool. Distinct from otto's `fs.*`/`bash`/`git.*`
/// so a plugin server can never shadow or be confused with a built-in tool by the gate.
fn plugin_gate_name(namespace: &str, server_key: &str, tool: &str) -> String {
    format!("plugin__{namespace}__{server_key}__{tool}")
}

/// Spawn a plugin-bundled MCP server from its spec and register each advertised tool under a
/// namespaced gate name. The spec's `command`/`args`/`env`/`cwd` are already
/// `${CLAUDE_PLUGIN_ROOT}`-expanded by discovery.
///
/// Security note: the registered tools route through the gate like any other, but the gate's
/// sensitive-path floor only inspects the standard `path`/`paths`/`glob` argument shapes (same as
/// otto's own `git.*`/`grep` tools). A third-party plugin tool that takes a file path under a
/// non-standard key would not have that path floor-checked — vetting a plugin's tool schemas is part
/// of deciding whether to add it to `enabledPlugins` (the trust gate for plugins at all).
pub async fn connect_plugin_server(
    spec: &otto_extensions::PluginMcpServer,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let mut command = tokio::process::Command::new(&spec.command);
    command.args(&spec.args);
    for (k, v) in &spec.env {
        command.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let ns = spec.namespace.clone();
    let key = spec.server_key.clone();
    connect_mapped(command, move |tool| plugin_gate_name(&ns, &key, tool)).await
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

/// Convenience: build the `mcp-git <root>` command and connect.
pub async fn connect_git(
    bin: &str,
    root: &Path,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let mut command = tokio::process::Command::new(bin);
    command.arg(root);
    connect(command).await
}

/// Convenience: build the `mcp-bash <root>` command and connect.
pub async fn connect_bash(
    bin: &str,
    root: &Path,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let mut command = tokio::process::Command::new(bin);
    command.arg(root);
    connect(command).await
}

/// Convenience: build the `mcp-lsp <root>` command and connect.
pub async fn connect_lsp(
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
        assert!(
            connect_grep("definitely-not-a-real-binary-xyz", Path::new("."))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn connect_git_with_bogus_binary_errors() {
        assert!(
            connect_git("definitely-not-a-real-binary-xyz", Path::new("."))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn connect_bash_with_bogus_binary_errors() {
        assert!(
            connect_bash("definitely-not-a-real-binary-xyz", Path::new("."))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn connect_lsp_with_bogus_binary_errors() {
        assert!(
            connect_lsp("definitely-not-a-real-binary-xyz", Path::new("."))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn connect_lsp_surfaces_a_pre_handshake_exit_as_err() {
        // `false` spawns successfully then exits nonzero immediately — before speaking MCP,
        // exactly as `mcp-lsp` does when its PATH availability gate finds no language server.
        // `connect` must surface this as an Err (so no lsp tools get registered), not hang.
        assert!(connect_lsp("false", Path::new(".")).await.is_err());
    }

    #[test]
    fn plugin_gate_name_is_namespaced() {
        assert_eq!(
            super::plugin_gate_name("foo", "my-server", "search"),
            "plugin__foo__my-server__search"
        );
    }

    #[tokio::test]
    async fn connect_plugin_server_with_bogus_command_errors() {
        use otto_extensions::PluginMcpServer;
        let spec = PluginMcpServer {
            namespace: "foo".into(),
            server_key: "s".into(),
            command: "definitely-not-a-real-binary-xyz".into(),
            args: vec![],
            env: Default::default(),
            cwd: None,
        };
        assert!(connect_plugin_server(&spec).await.is_err());
    }
}
