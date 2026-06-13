//! `BashTool`: runs a shell command confined by a `SandboxPolicy`, with a timeout.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use serde_json::{Value, json};

use crate::sandbox::{SandboxPolicy, build_argv};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// `bash` — args `{ "command": "<sh>", "timeout_ms": <n>? }` →
/// `{ "stdout": "...", "stderr": "...", "exit_code": <i32|null> }`.
pub struct BashTool {
    root: PathBuf,
    policy: SandboxPolicy,
}

impl BashTool {
    pub fn new(root: PathBuf, policy: SandboxPolicy) -> Self {
        Self { root, policy }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("bash requires a string 'command' arg"))?;
        let timeout = Duration::from_millis(
            args.get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
        );

        let (program, argv) = build_argv(&self.policy, &self.root, command)?;

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(argv)
            .current_dir(&self.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn()?;
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            // On timeout the wait_with_output future is dropped, and kill_on_drop kills the child.
            Err(_) => anyhow::bail!("bash command timed out after {} ms", timeout.as_millis()),
            Ok(result) => result?,
        };

        Ok(json!({
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "exit_code": output.status.code(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsandboxed() -> BashTool {
        let dir = tempfile::tempdir().unwrap();
        // Own a real directory path for the tool; the OS cleans /tmp later.
        let root = dir.keep();
        BashTool::new(root, SandboxPolicy::None)
    }

    #[tokio::test]
    async fn runs_echo_and_captures_stdout() {
        let tool = unsandboxed();
        let out = tool.call(json!({"command": "echo hello"})).await.unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
    }

    #[tokio::test]
    async fn captures_nonzero_exit_code() {
        let tool = unsandboxed();
        let out = tool.call(json!({"command": "exit 3"})).await.unwrap();
        assert_eq!(out["exit_code"].as_i64().unwrap(), 3);
    }

    #[tokio::test]
    async fn missing_command_arg_errors() {
        let tool = unsandboxed();
        let err = tool.call(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("requires a string 'command'"));
    }

    #[tokio::test]
    async fn times_out_long_command() {
        let tool = unsandboxed();
        let err = tool
            .call(json!({"command": "sleep 5", "timeout_ms": 100}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn sandboxed_runs_when_backend_available() {
        if !crate::sandbox::os_sandbox_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(
            dir.path().to_path_buf(),
            SandboxPolicy::Os { allow_net: false },
        );
        let out = tool
            .call(json!({"command": "echo sandboxed"}))
            .await
            .unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("sandboxed"));
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
    }
}
