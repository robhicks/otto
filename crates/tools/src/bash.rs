//! `BashTool`: runs a shell command confined by a `SandboxPolicy`, with a timeout.
//! The spawned command runs with a CLEARED environment, then a curated minimal env that
//! also makes the Rust toolchain usable (`PATH` includes the host's `~/.cargo/bin`;
//! `CARGO_HOME`/`RUSTUP_HOME` point at the host toolchain) — non-secret locations only, so
//! it grants no new read access beyond the already-read-only host FS. `HOME` and `TMPDIR` are
//! set last to the workspace root: `HOME` so host credentials in env are not exposed, and
//! `TMPDIR` because the OS sandbox mounts the whole host FS read-only except the workspace, so
//! the default `/tmp` is not writable. Tools that create temp files there (notably `cc`/`lld`
//! when `cargo` builds a registry dependency, whose source dir — the linker's cwd — is on the
//! read-only mount) must be pointed at the writable workspace root, or they fail with
//! "Read-only file system".

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use serde_json::{Value, json};

use crate::sandbox::{SandboxPolicy, build_argv};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The curated environment for a sandboxed command. The host environment is cleared (no
/// credential leakage), then a minimal env is set that also makes the Rust toolchain usable:
/// `PATH` includes the host's `~/.cargo/bin`, and `CARGO_HOME`/`RUSTUP_HOME` point at the host
/// toolchain. These are non-secret locations; the host filesystem is already read-only-readable
/// inside the sandbox, so this grants no new read access — it only makes `cargo`/`rustc` runnable.
/// `HOME` is set separately to the workspace root by the caller.
fn curated_env() -> Vec<(String, String)> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_else(|_| format!("{host_home}/.cargo"));
    let rustup_home =
        std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{host_home}/.rustup"));
    let path =
        format!("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:{cargo_home}/bin");
    vec![
        ("PATH".to_string(), path),
        ("TERM".to_string(), "dumb".to_string()),
        ("CARGO_HOME".to_string(), cargo_home),
        ("RUSTUP_HOME".to_string(), rustup_home),
    ]
}

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
        cmd.args(argv).current_dir(&self.root).env_clear();
        for (key, val) in curated_env() {
            cmd.env(key, val);
        }
        // HOME and TMPDIR are set last so they always point at the writable workspace root and
        // cannot be overridden by curated_env. TMPDIR is required because the sandbox's default
        // /tmp is read-only (see the module doc).
        cmd.env("HOME", &self.root)
            .env("TMPDIR", &self.root)
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
    async fn clears_host_environment() {
        // `cargo test` sets CARGO_MANIFEST_DIR in the host env; it must NOT leak into the
        // sandboxed command after env_clear().
        let tool = unsandboxed();
        let out = tool
            .call(json!({"command": "echo \"manifest=${CARGO_MANIFEST_DIR:-CLEARED}\""}))
            .await
            .unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("manifest=CLEARED"));
    }

    #[test]
    fn curated_env_exposes_the_rust_toolchain() {
        let env: std::collections::HashMap<String, String> = curated_env().into_iter().collect();
        let path = env.get("PATH").expect("PATH set");
        assert!(
            path.contains("/.cargo/bin"),
            "PATH must include the cargo bin dir: {path}"
        );
        assert!(
            path.contains("/usr/bin"),
            "PATH must keep system dirs: {path}"
        );
        assert!(
            env.get("CARGO_HOME")
                .expect("CARGO_HOME set")
                .ends_with(".cargo")
        );
        assert!(
            env.get("RUSTUP_HOME")
                .expect("RUSTUP_HOME set")
                .ends_with(".rustup")
        );
        assert_eq!(env.get("TERM").map(String::as_str), Some("dumb"));
        // HOME is intentionally NOT in curated_env (the caller sets it to the workspace root).
        assert!(!env.contains_key("HOME"));
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

    #[tokio::test]
    async fn tmpdir_points_at_the_workspace_root() {
        // The command's TMPDIR must be the workspace root so toolchain temp files land in the
        // one writable location (the sandbox mounts everything else read-only).
        let tool = unsandboxed();
        let out = tool
            .call(json!({"command": "echo \"tmp=$TMPDIR\""}))
            .await
            .unwrap();
        let stdout = out["stdout"].as_str().unwrap();
        let root = tool.root.to_string_lossy();
        assert!(
            stdout.contains(&format!("tmp={root}")),
            "TMPDIR must be the workspace root ({root}): {stdout}"
        );
    }

    #[tokio::test]
    async fn sandbox_tmpdir_is_writable() {
        // Regression guard for the read-only-/tmp bug: inside the sandbox the whole host FS is
        // read-only except the workspace, so a writable TMPDIR is what lets `cargo`'s linker
        // create its temp files. Without the TMPDIR fix this write fails ("Read-only file
        // system") and the real Verifier's `cargo check` spuriously fails on any project with a
        // registry dependency.
        if !crate::sandbox::os_sandbox_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(
            dir.path().to_path_buf(),
            SandboxPolicy::Os { allow_net: false },
        );
        let out = tool
            .call(json!({
                "command": "test -n \"$TMPDIR\" && echo probe > \"$TMPDIR/otto-probe\" && echo WROTE"
            }))
            .await
            .unwrap();
        assert_eq!(
            out["exit_code"].as_i64().unwrap(),
            0,
            "writing into $TMPDIR inside the sandbox must succeed; stderr={}",
            out["stderr"].as_str().unwrap_or("")
        );
        assert!(out["stdout"].as_str().unwrap().contains("WROTE"));
    }
}
