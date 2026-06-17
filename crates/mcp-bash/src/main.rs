//! `mcp-bash <root>` — an MCP stdio server exposing a `bash` tool that runs the command in the
//! OS sandbox (always `SandboxPolicy::Os`, never None — fails closed without a backend). The
//! engine registers it as `bash` so the Ask-gate + sandbox-only registration apply unchanged.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use otto_tools::sandbox::{SandboxPolicy, run_sandboxed};
use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Plain, rmcp-independent core
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BashServer {
    root: Arc<PathBuf>,
}

impl BashServer {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(root),
        }
    }

    /// Run `command` in the OS sandbox. `Os` is hardcoded — there is no path to an unsandboxed
    /// run; if no backend exists, `run_sandboxed`/`build_argv` fails closed.
    pub async fn do_bash(&self, command: String, timeout_ms: Option<u64>) -> anyhow::Result<Value> {
        let timeout =
            Duration::from_millis(timeout_ms.unwrap_or(otto_tools::bash::DEFAULT_TIMEOUT_MS));
        run_sandboxed(
            &SandboxPolicy::Os { allow_net: false },
            &self.root,
            &command,
            timeout,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Arg struct for rmcp parameter deserialization
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct BashArgs {
    command: String,
    timeout_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// rmcp tool wrapper
// ---------------------------------------------------------------------------

fn to_err(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

#[tool_router(server_handler)]
impl BashServer {
    #[tool(name = "bash", description = "Run a shell command in the OS sandbox")]
    async fn bash(
        &self,
        Parameters(BashArgs {
            command,
            timeout_ms,
        }): Parameters<BashArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let out = self.do_bash(command, timeout_ms).await.map_err(to_err)?;
        Ok(CallToolResult::structured(out))
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
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-bash <root>"))?;
    let server = BashServer::new(root);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_echo_in_sandbox() {
        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping: no OS sandbox backend (bwrap/sandbox-exec)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let server = BashServer::new(dir.path().to_path_buf());
        let out = server.do_bash("echo hi".to_string(), None).await.unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("hi"));
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
    }
}
