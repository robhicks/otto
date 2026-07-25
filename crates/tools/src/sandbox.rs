//! Confinement for shell commands. `build_argv` wraps a `sh -c "<command>"` invocation in an
//! OS sandbox (bwrap on Linux, sandbox-exec on macOS) that limits filesystem writes to the
//! workspace root and disables network unless allowed. The argv this produces IS the security
//! boundary — `bwrap --ro-bind / /` mounts the ENTIRE filesystem read-only (so `/tmp`, `$HOME`,
//! etc. are not writable), then `--bind root root` re-mounts only the workspace writable;
//! `--unshare-net` removes network access and `--unshare-pid/--ipc/--new-session` isolate
//! process, IPC, and session namespaces.
//!
//! ## Teardown is NOT at parity between the two backends
//!
//! Confinement is equivalent; *reaping* is not, and the difference is structural rather than an
//! oversight to be tidied up later.
//!
//! - **Linux is covered on every path.** `--unshare-pid` makes the child pid 1 of a new namespace,
//!   so killing it collapses the whole subtree, and `--die-with-parent` arms `PR_SET_PDEATHSIG` on
//!   `bwrap` itself, so the sandbox dies even when otto is `SIGKILL`ed and no destructor runs.
//! - **macOS is covered on the ordinary paths only.** There is no PID namespace, so
//!   `lead_own_process_group` + `sweep_process_group` supply the equivalent by hand: the child leads
//!   its own group and the group is swept after the command finishes or times out.
//! - **macOS is NOT covered when otto is hard-killed.** The sweep is code *we* run, and a `SIGKILL`
//!   runs none of it. macOS has no `PR_SET_PDEATHSIG` equivalent, and the alternatives (a watchdog
//!   process or thread per sandboxed command) buy a narrow case at a cost in moving parts. So a
//!   `SIGKILL`ed otto on macOS can strand a running sandboxed command and its descendants.
//!
//! Do not "simplify" the macOS process-group handling into parity with Linux by deleting it: the two
//! backends need different mechanisms to reach the same place, and the macOS one is the weaker of
//! the two even with it. Note also that the macOS path is exercised by nobody's test run — the suite
//! is realistically Linux-only and the repo has no CI — which is why `macos_argv` is factored out as
//! a pure function and asserted from any host.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

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

/// Put the sandboxed child in its own process group so its whole subtree can be reaped together.
///
/// ## Why macOS needs this and Linux does not
///
/// On Linux the sandboxed command runs under `bwrap --unshare-pid`, so the child is pid 1 of a fresh
/// PID namespace: when it dies the kernel tears down every process in that namespace. Killing the
/// one child really does kill the whole tree.
///
/// macOS has no such namespace. `sandbox-exec -p … sh -c '…'` applies the profile and then *execs*
/// the shell, so killing that pid kills only the shell — anything it spawned (a backgrounded job, a
/// dev server, a compiler) is a separate process that keeps running. That makes it a leak on the
/// **ordinary timeout path**, not just on a hard kill: every `bash` tool call that times out could
/// strand descendants. Leading its own process group gives `sweep_process_group` a handle on the
/// entire subtree.
///
/// `setpgid(0, 0)` is called in the child between fork and exec, so the group exists before the
/// sandboxed command runs and no descendant can be created outside it.
#[cfg(target_os = "macos")]
fn lead_own_process_group(cmd: &mut tokio::process::Command) {
    // No `std::os::unix::process::CommandExt` import: `pre_exec` here is tokio's own inherent
    // method on `tokio::process::Command`, not the std extension trait (which applies to
    // `std::process::Command`). Importing the trait compiles but warns as unused.
    //
    // SAFETY: runs in the forked child before exec and calls only `setpgid`, which is
    // async-signal-safe. Captures nothing, allocates nothing, takes no locks.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// `SIGKILL` every process still in `pgid`, sweeping descendants the leader left behind.
///
/// ## Why this is safe to call after the leader has been reaped
///
/// POSIX guarantees a process group ID is not reused while any process remains in the group. So in
/// the case this exists for — descendants outlived the shell — the group is still populated, the
/// pgid is therefore still reserved, and the signal cannot reach anything else. If instead nothing
/// survived, the group no longer exists and `kill` fails with `ESRCH`, which is a no-op.
///
/// The residual race is the narrow one where the group is genuinely empty *and* the pid has since
/// been recycled *and* the new owner has made itself a group leader. That is the same exposure every
/// process supervisor accepts (`timeout(1)`, tmux, `subprocess` with `start_new_session`), and it is
/// bounded by calling this immediately after the wait rather than at some later cleanup point.
///
/// `pgid > 1` is checked because `kill(0, …)` signals *our own* process group and `kill(-1, …)`
/// every process the user owns — a malformed pgid must never reach `kill`.
#[cfg(target_os = "macos")]
fn sweep_process_group(pgid: i32) {
    if pgid > 1 {
        // SAFETY: plain `kill(2)` with a validated positive pgid, negated to address the group.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

/// Build the `sandbox-exec` argv confining writes to `root`.
///
/// Deliberately a free function rather than an inline branch: the `cfg!(target_os = "macos")` arm in
/// `build_argv` is a *runtime* condition, so on Linux it compiles but is unreachable — which is why
/// the macOS profile had no test coverage at all while the two `bwrap` assertions did. As a plain
/// function it is assertable from any host (see `macos_argv_*` in the tests), so a change to the
/// confinement profile can't slip through on the platform nobody runs the suite on.
fn macos_argv(root_str: &str, allow_net: bool, command: &str) -> (String, Vec<String>) {
    let net = if allow_net {
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
    (
        "sandbox-exec".to_string(),
        vec![
            "-p".to_string(),
            profile,
            "sh".to_string(),
            "-c".to_string(),
            command.to_string(),
        ],
    )
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
                Ok(macos_argv(&root_str, *allow_net, command))
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

    // macOS only: the child leads its own process group so the whole subtree can be swept below.
    // Linux needs nothing here — `bwrap --unshare-pid` already makes the child a namespace init, so
    // killing it collapses the tree. See `lead_own_process_group`.
    #[cfg(target_os = "macos")]
    lead_own_process_group(&mut cmd);

    let mut child = cmd.spawn()?;

    // Captured before `child` moves into the future below, because the sweep has to happen on the
    // timeout path too — where the future (and the handle) are already gone. The child is its own
    // group leader, so its pid *is* the pgid.
    #[cfg(target_os = "macos")]
    let pgid = child.id().map(|id| id as i32);

    // Wrap the entire write + shutdown + wait in ONE timeout so that a hook that never reads
    // stdin (and stays alive with a large payload filling the pipe buffer) cannot block forever.
    // On timeout the future is dropped → `child` is dropped → killed via kill_on_drop.
    let outcome = tokio::time::timeout(timeout, async move {
        if let Some(payload) = stdin {
            // `Stdio::piped()` is set above whenever `stdin.is_some()`, so the handle is present.
            let mut handle = child
                .stdin
                .take()
                .expect("stdin handle present after Stdio::piped()");
            // A hook that ignores stdin may exit before we finish writing; a BrokenPipe here is
            // not a failure — the child's exit code (from wait_with_output) is what we report.
            if let Err(e) = handle.write_all(payload.as_bytes()).await {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err::<std::process::Output, anyhow::Error>(e.into());
                }
            }
            // EOF is delivered when `handle` is dropped at end of scope; shutdown is best-effort.
            let _ = handle.shutdown().await;
        }
        child.wait_with_output().await.map_err(Into::into)
    })
    .await;

    // Sweep on BOTH paths, before returning either way.
    //
    // The timeout path is the obvious one: `kill_on_drop` has just killed the shell, and anything it
    // spawned would otherwise be left running. But the *success* path needs it just as much — a
    // command that backgrounds a job and exits 0 leaves that job behind exactly the same way, and
    // that case never reaches a timeout at all.
    //
    // Ordering matters: on timeout the future has already been dropped, so the leader is dead and
    // only descendants remain — which is precisely when POSIX still reserves the pgid for us.
    #[cfg(target_os = "macos")]
    if let Some(pgid) = pgid {
        sweep_process_group(pgid);
    }

    let output = outcome.map_err(|_| {
        anyhow::anyhow!("bash command timed out after {} ms", timeout.as_millis())
    })??;

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

    #[tokio::test]
    async fn run_sandboxed_with_stdin_ok_when_command_ignores_stdin() {
        use std::time::Duration;
        let root = std::path::PathBuf::from(".");
        // `true` never reads stdin and exits 0 immediately — must not surface a BrokenPipe error.
        let out = run_sandboxed_with_stdin(
            &SandboxPolicy::None,
            &root,
            "true",
            Duration::from_secs(5),
            Some("payload-the-command-never-reads"),
        )
        .await
        .unwrap();
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
    }

    #[tokio::test]
    async fn run_sandboxed_with_stdin_times_out_when_command_never_reads_large_payload() {
        use std::time::Duration;
        let root = std::path::PathBuf::from(".");
        // A long-lived command that never reads stdin. With a payload larger than the OS pipe
        // buffer (~64 KiB) the stdin write blocks; the overall timeout must still fire rather
        // than hang forever (regression test for the write-outside-timeout bug).
        let big = "x".repeat(256 * 1024);
        let res = run_sandboxed_with_stdin(
            &SandboxPolicy::None,
            &root,
            "sleep 30",
            Duration::from_millis(300),
            Some(&big),
        )
        .await;
        assert!(res.is_err(), "expected a timeout error, got: {res:?}");
    }

    #[test]
    fn none_policy_is_plain_sh_c() {
        let (prog, args) =
            build_argv(&SandboxPolicy::None, &PathBuf::from("/work"), "echo hi").unwrap();
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    // The `macos_argv_*` tests below run on EVERY host, not just macOS. That is the point: the
    // macOS arm of `build_argv` is behind a runtime `cfg!`, so on Linux it compiles but never
    // executes, and it went uncovered while the two `bwrap` cases were asserted. Testing the pure
    // argv builder directly is the only way this confinement profile gets checked at all, given the
    // suite is realistically only ever run on Linux.

    #[test]
    fn macos_argv_confines_writes_to_root_and_denies_net_by_default() {
        let (prog, args) = macos_argv("/work", false, "echo hi");
        assert_eq!(prog, "sandbox-exec");
        assert_eq!(args[0], "-p");
        let profile = &args[1];
        assert!(profile.contains("(deny file-write*)"), "{profile}");
        assert!(
            profile.contains("(allow file-write* (subpath \"/work\"))"),
            "{profile}"
        );
        assert!(profile.contains("(deny network*)"), "{profile}");
        // The command must stay a separate argv element — never interpolated into the profile.
        assert_eq!(
            args[2..],
            ["sh".to_string(), "-c".to_string(), "echo hi".to_string()]
        );
    }

    #[test]
    fn macos_argv_allows_net_only_when_requested() {
        let (_, args) = macos_argv("/work", true, "echo hi");
        assert!(args[1].contains("(allow network*)"), "{}", args[1]);
        assert!(!args[1].contains("(deny network*)"), "{}", args[1]);
    }

    #[test]
    fn macos_argv_escapes_quotes_in_the_root_path() {
        // An unescaped `"` would close the subpath string and let the rest of the path inject
        // profile syntax — i.e. widen the sandbox from a directory name.
        let (_, args) = macos_argv("/work/ev\"il", false, "echo hi");
        assert!(
            args[1].contains(r#"(subpath "/work/ev\"il")"#),
            "quote not escaped: {}",
            args[1]
        );
    }

    #[test]
    fn macos_argv_escapes_backslashes_before_quotes() {
        // Backslash must be escaped first, else `\"` in a path would be rewritten into an escaped
        // quote and the profile would still break out.
        let (_, args) = macos_argv(r"/work/a\b", false, "echo hi");
        assert!(
            args[1].contains(r#"(subpath "/work/a\\b")"#),
            "backslash not escaped: {}",
            args[1]
        );
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
