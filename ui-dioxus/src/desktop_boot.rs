//! Desktop bootstrap: fold the Tauri `desktop/` wrapper's job (pick workspace → launch a local
//! `otto serve` sidecar → wait for it to bind → auto-connect) into the one Dioxus crate. Fixed
//! port 8787, generated token. This whole module is desktop-only — it is `mod`-gated behind
//! `#[cfg(feature = "desktop")]` in `main.rs`, so it never compiles into (or is referenced by)
//! the web build.
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};

use crate::net::url::LaunchParams;

/// Upper bound on how long `boot()` waits for the sidecar's readiness line before connecting
/// anyway. This is a *safety cap*, not the expected wait: `otto serve` prints its readiness line
/// as soon as it binds (normally in well under a second), which breaks the wait loop immediately —
/// so in the common case the effective wait is short. The cap only bites in the pathological case
/// (the line never arrives, e.g. the sidecar wedged), where it stops `boot()` from hanging the app
/// forever and lets `do_connect` try (and fail visibly) rather than blocking.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Fallback grace period used only when the sidecar's stderr could not be piped (so there is no
/// readiness line to watch). Matches the original fixed-sleep shortcut this task started with.
const FALLBACK_GRACE: Duration = Duration::from_millis(400);

/// The result of a boot attempt. Distinguishes the three outcomes so the caller can react
/// differently: a user cancel is silent (fall back to the manual form), a spawn failure is
/// surfaced to the UI (the sidecar binary is missing/misconfigured — the user needs to know why
/// auto-connect didn't happen), and success carries the live child + connect params.
pub enum BootOutcome {
    /// The user dismissed the folder picker. No sidecar was spawned; fall back to the manual form.
    Cancelled,
    /// `otto serve` failed to spawn (e.g. `otto` not on `PATH` and `OTTO_BIN` unset/wrong). The
    /// carried message is suitable for a `client_error_row` so the UI explains the fallback.
    SpawnFailed(String),
    /// The sidecar spawned and (best-effort) signalled readiness. The `Child` is `kill_on_drop`,
    /// so storing it in a component signal keeps it alive for the app's lifetime and kills it when
    /// that signal's value is dropped.
    Ready(Child, LaunchParams),
}

/// Pick a workspace folder, spawn `otto serve` there, wait for it to bind, and return the live
/// child + connect params.
///
/// Uses `rfd::AsyncFileDialog` rather than the blocking `rfd::FileDialog` — the blocking dialog
/// parks the calling OS thread until the user responds, which would stall the async executor
/// (and, transitively, every other `use_future`/`spawn` task in the app) if called directly from
/// one. The async dialog cooperates with the executor instead, so `boot()` is itself `async` and
/// must be driven from a `spawn`/`use_future`, never called synchronously.
///
/// Readiness is detected the same way the shipped Tauri wrapper does it
/// (`desktop/src-tauri/src/launch.rs`): the sidecar's stderr is piped and read line-by-line until
/// otto serve's own readiness line appears (`is_ready_line`), rather than blindly sleeping — so
/// this reproduction is functionally equivalent to Tauri's, not a weaker fixed-delay shortcut.
pub async fn boot() -> BootOutcome {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .set_title("Choose a workspace folder")
        .pick_folder()
        .await
    else {
        return BootOutcome::Cancelled;
    };
    let root = handle.path().to_path_buf();
    let token = uuid::Uuid::new_v4().to_string();
    // `otto` must be on PATH (or point OTTO_BIN at it); mirrors desktop/'s sidecar contract. Fixed
    // port 8787 to match the LaunchParams the web path also uses. `kill_on_drop(true)` kills the
    // sidecar when the stored `Child` is dropped — but Drop only runs if the app *unwinds* on close.
    // Phase 0 Gate E proved a non-unwinding teardown (SIGKILL / `exit` / the `dx serve` dev
    // supervisor) skips Drop and orphans the sidecar, so `install_pdeathsig` below adds a
    // kernel-level guard that does not depend on Drop running. The two are complementary:
    // `kill_on_drop` handles the graceful in-app disconnect, `PR_SET_PDEATHSIG` handles hard exits.
    let bin = std::env::var("OTTO_BIN").unwrap_or_else(|_| "otto".into());
    let mut command = Command::new(&bin);
    command
        .arg("serve")
        .arg("--port")
        .arg("8787")
        .arg("--root")
        .arg(&root)
        .env("OTTO_TOKEN", &token)
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    install_pdeathsig(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Surface spawn failure both to the terminal/log and (via the returned variant) to the
            // UI — otherwise a misconfigured `OTTO_BIN` / missing `otto` would silently fall
            // through to the manual form with no explanation.
            let msg = format!("failed to launch `{bin} serve` sidecar: {e}");
            eprintln!("desktop_boot: {msg}");
            return BootOutcome::SpawnFailed(msg);
        }
    };

    wait_for_ready(&mut child).await;

    BootOutcome::Ready(
        child,
        LaunchParams {
            ws: "ws://127.0.0.1:8787".into(),
            token,
        },
    )
}

/// Attach a kernel-level parent-death guard to the sidecar so it cannot outlive this process,
/// independent of whether the app's teardown runs destructors.
///
/// `kill_on_drop(true)` alone is insufficient: it fires only when the stored `Child` is *dropped*,
/// which a non-unwinding teardown (SIGKILL, `std::process::exit`, or the `dx serve` dev supervisor
/// killing the app) skips entirely — the Phase 0 Gate-E failure, where a closed window orphaned the
/// `otto serve` sidecar. `PR_SET_PDEATHSIG` is set in the child between fork and exec (so it is
/// established before `otto serve` ever runs) and tells the kernel to `SIGKILL` the child the moment
/// its parent dies for *any* reason. The `getppid() == 1` check closes the race where the parent
/// already died in the window between fork and the `prctl` call. Only `prctl`/`getppid`/`raise` —
/// all async-signal-safe — run inside the `pre_exec` closure.
///
/// Linux-only: `PR_SET_PDEATHSIG` is Linux-specific. On other targets this is a no-op and the app
/// falls back to `kill_on_drop` alone (macOS desktop teardown is revisited if it ever ships).
#[cfg(target_os = "linux")]
fn install_pdeathsig(command: &mut Command) {
    // SAFETY: the closure runs in the forked child before exec and calls only async-signal-safe
    // syscalls (`prctl`, `getppid`, `raise`); it captures nothing and allocates nothing.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // If the parent already exited before prctl took effect, PR_SET_PDEATHSIG will never
            // fire — reparented to init means ppid == 1 — so self-terminate instead of orphaning.
            if libc::getppid() == 1 {
                libc::raise(libc::SIGKILL);
            }
            Ok(())
        });
    }
}

/// Non-Linux fallback: no `PR_SET_PDEATHSIG` equivalent wired, so `kill_on_drop` is the only guard.
#[cfg(not(target_os = "linux"))]
fn install_pdeathsig(_command: &mut Command) {}

/// Block until the sidecar signals readiness on stderr, or the safety cap elapses. If stderr
/// wasn't piped (shouldn't happen — we always request it), fall back to a short fixed grace
/// period so we still give the port a moment to bind.
async fn wait_for_ready(child: &mut Child) {
    let Some(stderr) = child.stderr.take() else {
        tokio::time::sleep(FALLBACK_GRACE).await;
        return;
    };
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    // `timeout` bounds the wait; the inner loop returns the instant the readiness line is seen, so
    // the common-case wait is just "until the port binds," not the full cap. A closed stream
    // (`Ok(None)`) or read error also ends the loop — we then connect and let `do_connect` surface
    // any real failure, rather than hang.
    let _ = tokio::time::timeout(READY_TIMEOUT, async {
        while let Ok(Some(line)) = lines.next_line().await {
            if is_ready_line(&line) {
                break;
            }
        }
    })
    .await;
}

/// True when `line` (one line of the sidecar's stderr) is otto serve's own readiness message
/// (`crates/engine/src/main.rs`: `eprintln!("otto serve listening on {scheme}://{addr}/ws")`).
/// Ported verbatim from `desktop/src-tauri/src/launch.rs`'s `is_ready_line`, so the Dioxus
/// reproduction watches for the exact same signal the shipped Tauri wrapper does.
fn is_ready_line(line: &str) -> bool {
    line.contains("otto serve listening on")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_line_matches_otto_serves_own_readiness_message() {
        assert!(is_ready_line(
            "otto serve listening on ws://127.0.0.1:8787/ws"
        ));
        assert!(is_ready_line(
            "otto serve listening on wss://127.0.0.1:8787/ws"
        ));
    }

    #[test]
    fn ready_line_rejects_unrelated_output() {
        assert!(!is_ready_line(""));
        assert!(!is_ready_line("warning: something else"));
        assert!(!is_ready_line("otto run finished"));
    }
}
