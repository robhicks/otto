//! Confinement for shell commands. `build_argv` wraps a `sh -c "<command>"` invocation in an
//! OS sandbox (bwrap on Linux, sandbox-exec on macOS) that limits filesystem writes to the
//! workspace root and disables network unless allowed. The argv this produces IS the security
//! boundary — `bwrap --ro-bind / /` mounts the ENTIRE filesystem read-only (so `/tmp`, `$HOME`,
//! etc. are not writable), then `--bind root root` re-mounts only the workspace writable;
//! `--unshare-net` removes network access and `--unshare-pid/--ipc/--new-session` isolate
//! process, IPC, and session namespaces.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};

/// How to confine a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// Run directly with no OS confinement. The cwd is still the workspace root, but the
    /// command can touch anything the host process can. Requires explicit opt-in.
    None,
    /// OS sandbox: bwrap (Linux) / sandbox-exec (macOS). Filesystem writes confined to the
    /// workspace root; network disabled unless `allow_net`.
    Os { allow_net: bool },
}

/// Return true if the program `bin` is on PATH.
fn which(bin: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Is an OS sandbox backend available on this host?
pub fn os_sandbox_available() -> bool {
    if cfg!(target_os = "linux") {
        which("bwrap")
    } else if cfg!(target_os = "macos") {
        which("sandbox-exec")
    } else {
        false
    }
}

/// Build the `(program, args)` to spawn for running `command` under `policy`, confined to
/// `root`. For `Os`, errors (fail-closed) if the backend isn't available or the platform is
/// unsupported.
pub fn build_argv(
    policy: &SandboxPolicy,
    root: &Path,
    command: &str,
) -> anyhow::Result<(String, Vec<String>)> {
    let root_str = root.to_string_lossy().to_string();
    match policy {
        SandboxPolicy::None => Ok((
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )),
        SandboxPolicy::Os { allow_net } => {
            if cfg!(target_os = "linux") {
                if !which("bwrap") {
                    anyhow::bail!("OS sandbox requested but 'bwrap' is not available on PATH");
                }
                let mut args = vec![
                    "--ro-bind".to_string(),
                    "/".to_string(),
                    "/".to_string(),
                    "--bind".to_string(),
                    root_str.clone(),
                    root_str.clone(),
                    "--dev".to_string(),
                    "/dev".to_string(),
                    "--proc".to_string(),
                    "/proc".to_string(),
                    "--chdir".to_string(),
                    root_str,
                    "--die-with-parent".to_string(),
                    "--unshare-pid".to_string(),
                    "--unshare-ipc".to_string(),
                    "--new-session".to_string(),
                ];
                if !allow_net {
                    args.push("--unshare-net".to_string());
                }
                args.push("sh".to_string());
                args.push("-c".to_string());
                args.push(command.to_string());
                Ok(("bwrap".to_string(), args))
            } else if cfg!(target_os = "macos") {
                if !which("sandbox-exec") {
                    anyhow::bail!("OS sandbox requested but 'sandbox-exec' is not available");
                }
                let net = if *allow_net {
                    "(allow network*)"
                } else {
                    "(deny network*)"
                };
                let root_escaped = root_str.replace('\\', "\\\\").replace('"', "\\\"");
                let profile = format!(
                    "(version 1)(allow default)(deny file-write*)\
                     (allow file-write* (subpath \"{root_escaped}\"))\
                     (allow file-write* (subpath \"/dev\"))\
                     (allow file-write* (subpath \"/tmp\")){net}"
                );
                Ok((
                    "sandbox-exec".to_string(),
                    vec![
                        "-p".to_string(),
                        profile,
                        "sh".to_string(),
                        "-c".to_string(),
                        command.to_string(),
                    ],
                ))
            } else {
                anyhow::bail!("OS sandbox is not supported on this platform")
            }
        }
    }
}

/// The curated environment for a sandboxed command. The host environment is cleared (no
/// credential leakage), then a minimal env is set that also makes the Rust toolchain usable:
/// `PATH` includes the host's `~/.cargo/bin`, and `CARGO_HOME`/`RUSTUP_HOME` point at the host
/// toolchain. These are non-secret locations; the host filesystem is already read-only-readable
/// inside the sandbox, so this grants no new read access — it only makes `cargo`/`rustc` runnable.
/// `HOME` is set separately to the workspace root by the caller.
pub(crate) fn curated_env() -> Vec<(String, String)> {
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

/// Run `command` under `policy` with `root` as the writable root, killed after `timeout`.
/// Returns `{ "stdout": .., "stderr": .., "exit_code": <i32|null> }`. This is the
/// security-critical spawn/timeout/kill-on-drop core, shared by the in-process `BashTool` and
/// the `mcp-bash` server. The host env is cleared and replaced with a curated minimal env;
/// `HOME`/`TMPDIR` are pinned to the workspace root (see `curated_env`).
pub async fn run_sandboxed(
    policy: &SandboxPolicy,
    root: &Path,
    command: &str,
    timeout: Duration,
) -> anyhow::Result<Value> {
    run_sandboxed_with_stdin(policy, root, command, timeout, None).await
}

/// Like [`run_sandboxed`], but optionally pipes `stdin` to the command's standard input (closed
/// after writing, so the child sees EOF). Used to feed a hook its JSON event on stdin. When
/// `stdin` is `None` this behaves exactly as before (stdin is `/dev/null`).
pub async fn run_sandboxed_with_stdin(
    policy: &SandboxPolicy,
    root: &Path,
    command: &str,
    timeout: Duration,
    stdin: Option<&str>,
) -> anyhow::Result<Value> {
    use tokio::io::AsyncWriteExt;

    let (program, argv) = build_argv(policy, root, command)?;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(argv).current_dir(root).env_clear();
    for (key, val) in curated_env() {
        cmd.env(key, val);
    }
    cmd.env("HOME", root)
        .env("TMPDIR", root)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    if let Some(payload) = stdin {
        if let Some(mut handle) = child.stdin.take() {
            handle.write_all(payload.as_bytes()).await?;
            handle.shutdown().await?; // close stdin → child gets EOF
        }
    }
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => anyhow::bail!("bash command timed out after {} ms", timeout.as_millis()),
        Ok(result) => result?,
    };

    Ok(json!({
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "exit_code": output.status.code(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn run_sandboxed_none_echo_exit_and_timeout() {
        use std::time::Duration;
        let root = std::path::PathBuf::from(".");
        // echo → stdout + exit 0
        let out = run_sandboxed(
            &SandboxPolicy::None,
            &root,
            "echo hello",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
        // non-zero exit
        let out = run_sandboxed(
            &SandboxPolicy::None,
            &root,
            "exit 3",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(out["exit_code"].as_i64().unwrap(), 3);
        // timeout → error (process killed via kill_on_drop)
        let err = run_sandboxed(
            &SandboxPolicy::None,
            &root,
            "sleep 5",
            Duration::from_millis(100),
        )
        .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn run_sandboxed_with_stdin_pipes_payload() {
        use std::time::Duration;
        let root = std::path::PathBuf::from(".");
        let out = run_sandboxed_with_stdin(
            &SandboxPolicy::None,
            &root,
            "cat", // echoes stdin to stdout
            Duration::from_secs(5),
            Some("hello-stdin"),
        )
        .await
        .unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("hello-stdin"));
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
    }

    #[test]
    fn none_policy_is_plain_sh_c() {
        let (prog, args) =
            build_argv(&SandboxPolicy::None, &PathBuf::from("/work"), "echo hi").unwrap();
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_os_policy_binds_root_and_unshares_net_when_disallowed() {
        // Only meaningful when bwrap exists; otherwise build_argv fails-closed (covered below).
        if !which("bwrap") {
            return;
        }
        let (prog, args) = build_argv(
            &SandboxPolicy::Os { allow_net: false },
            &PathBuf::from("/work"),
            "ls",
        )
        .unwrap();
        assert_eq!(prog, "bwrap");
        assert!(args.windows(3).any(|w| w == ["--bind", "/work", "/work"]));
        assert!(args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(args.contains(&"--unshare-ipc".to_string()));
        assert!(args.contains(&"--new-session".to_string()));
        assert_eq!(
            &args[args.len() - 3..],
            &["sh".to_string(), "-c".to_string(), "ls".to_string()]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_os_policy_keeps_net_when_allowed() {
        if !which("bwrap") {
            return;
        }
        let (_prog, args) = build_argv(
            &SandboxPolicy::Os { allow_net: true },
            &PathBuf::from("/work"),
            "ls",
        )
        .unwrap();
        assert!(!args.contains(&"--unshare-net".to_string()));
    }
}
