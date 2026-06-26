//! `SandboxedHookExecutor`: runs `settings.json` hook commands through the shared OS sandbox
//! core (`SandboxPolicy::Os`), piping the hook's JSON event on stdin. It is the engine-side
//! implementation of `otto_extensions::HookExecutor`; the orchestrator never constructs it, so
//! the offline determinism suite is unaffected. Built only when an OS sandbox backend exists.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use otto_extensions::{HookExecutor, HookOutcome};
use otto_tools::{SandboxPolicy, run_sandboxed_with_stdin};

pub struct SandboxedHookExecutor {
    root: PathBuf,
}

impl SandboxedHookExecutor {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl HookExecutor for SandboxedHookExecutor {
    async fn run(
        &self,
        command: &str,
        stdin_json: &str,
        timeout: Duration,
    ) -> anyhow::Result<HookOutcome> {
        let out = run_sandboxed_with_stdin(
            &SandboxPolicy::Os { allow_net: false },
            &self.root,
            command,
            timeout,
            Some(stdin_json),
        )
        .await?;
        Ok(HookOutcome {
            exit_code: out["exit_code"].as_i64().map(|c| c as i32),
            stdout: out["stdout"].as_str().unwrap_or("").to_string(),
            stderr: out["stderr"].as_str().unwrap_or("").to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_stdin_and_reports_exit_code() {
        if !otto_tools::os_sandbox_available() {
            return; // fail-closed: no backend → nothing to test here
        }
        let dir = tempfile::tempdir().unwrap();
        let exec = SandboxedHookExecutor::new(dir.path().to_path_buf());
        // Exit 2 if stdin contains "PreToolUse", else 0 — proves stdin is delivered.
        let out = exec
            .run(
                "grep -q PreToolUse && exit 2 || exit 0",
                r#"{"hook_event_name":"PreToolUse"}"#,
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(2));
    }
}
