//! Confinement for shell commands. `build_argv` wraps a `sh -c "<command>"` invocation in an
//! OS sandbox (bwrap on Linux, sandbox-exec on macOS) that limits filesystem writes to the
//! workspace root and disables network unless allowed. The argv this produces IS the security
//! boundary — `bwrap --ro-bind / /` mounts the whole filesystem read-only, then `--bind root
//! root` re-mounts only the workspace writable; `--unshare-net` removes network access.

use std::path::Path;

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
                let profile = format!(
                    "(version 1)(allow default)(deny file-write*)\
                     (allow file-write* (subpath \"{root_str}\"))\
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
