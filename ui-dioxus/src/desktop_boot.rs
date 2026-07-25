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
    let mut child = match spawn_guarded(&mut command) {
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

/// The **only** way the sidecar is spawned: install the parent-death guard, then spawn.
///
/// Funnelling `install_pdeathsig` + `spawn` through one function makes "the sidecar is always
/// spawned guarded" structural rather than a convention `boot()` has to remember — and gives the
/// regression tests below a call site that exercises exactly what `boot()` does, so dropping the
/// guard from the spawn path fails the suite instead of silently re-opening the Gate-E orphan bug.
fn spawn_guarded(command: &mut Command) -> std::io::Result<Child> {
    install_pdeathsig(command);
    command.spawn()
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
    // Captured before fork so the child can tell whether its parent is still this app. Checking
    // `getppid() != app_pid` (rather than `getppid() == 1`) closes the race under a child-subreaper
    // (systemd --user, `dx serve`, tini): if the parent dies in the fork→prctl window the child
    // reparents to the *subreaper's* pid, not init's 1, so a `== 1` test would miss it — but any
    // ppid other than this app's own pid still means the app is gone.
    let app_pid = std::process::id();
    // SAFETY: the closure runs in the forked child before exec and calls only async-signal-safe
    // syscalls (`prctl`, `getppid`, `raise`); it captures only a copied integer, allocates nothing,
    // and `io::Error::last_os_error()` on the failure path only wraps errno (no heap allocation).
    //
    // Note: PR_SET_PDEATHSIG arms against death of the *thread* that forked, not the process. That
    // is safe here because `dioxus::launch` owns a single long-lived `rt-multi-thread` runtime whose
    // worker threads live for the app's lifetime, so thread-death and process-death coincide; if the
    // spawn ever moved onto an ephemeral (e.g. `spawn_blocking`) thread, this assumption would need
    // revisiting.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // If the parent already exited in the fork→prctl window, PR_SET_PDEATHSIG never fires,
            // so self-terminate instead of orphaning. `!= app_pid` catches reparenting to init (1)
            // *and* to any subreaper.
            if libc::getppid() != app_pid as libc::pid_t {
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

/// Regression coverage for the Phase 0 Gate-E fix: the sidecar must not outlive the app when the
/// app dies **without unwinding**, i.e. when `kill_on_drop`'s `Drop` never runs.
///
/// Gate E was verified exactly once, by an operator closing a window. These tests turn that
/// one-time manual check into an invariant by reproducing the same shape in-process:
///
/// ```text
///   cargo test  ──spawn──▶  helper (this binary, --exact <helper test>)
///                              └──spawn_guarded──▶  grandchild ("the sidecar")
///   cargo test  ──SIGKILL──▶  helper                      (a non-unwinding death: no Drop, no
///                                                          kill_on_drop, no destructors at all)
///   cargo test  ──poll /proc─▶ grandchild must be gone
/// ```
///
/// SIGKILL is what makes this a real test of `PR_SET_PDEATHSIG` rather than of `kill_on_drop`: an
/// uncatchable signal guarantees the helper runs no teardown code, so the *only* thing that can
/// reap the grandchild is the kernel guard. Each death assertion is paired with an unguarded
/// control that spawns the identical grandchild **without** the guard and asserts it survives —
/// without that pairing a vacuous test (e.g. one whose grandchild died on its own) would pass.
///
/// Two scenarios are covered, matching the two halves of `install_pdeathsig`:
/// 1. the ordinary path — `prctl(PR_SET_PDEATHSIG)` armed, parent dies later;
/// 2. the fork→prctl race — parent already dead by the time the child runs `prctl`, where
///    `PR_SET_PDEATHSIG` can never fire and only the `getppid()` self-kill saves us.
///
/// Linux-only (`PR_SET_PDEATHSIG` is Linux-specific, and the liveness probe reads `/proc`), and
/// desktop-only (the whole module is `#[cfg(feature = "desktop")]`), so other targets stay green.
#[cfg(all(test, target_os = "linux"))]
mod pdeathsig_tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Child as StdChild, Command as StdCommand};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// libtest paths of the two re-exec entry points. The subprocesses in these tests are *this
    /// test binary* re-executed with a `--exact` filter — the guard under test is a private
    /// function of a binary crate, so an out-of-process helper can only reach the real code by
    /// being the same binary. `helper_test_names_resolve` pins these strings to real tests.
    const HELPER_TEST: &str = "desktop_boot::pdeathsig_tests::pdeathsig_helper_process";
    const SLEEPER_TEST: &str = "desktop_boot::pdeathsig_tests::pdeathsig_sleeper_process";

    /// Role switches. A re-executed copy runs the helper/sleeper body only when its variable is
    /// set; under a normal `cargo test` both entry points see nothing and return immediately.
    const MODE_ENV: &str = "OTTO_TEST_PDEATHSIG_MODE";
    const SLEEPER_ENV: &str = "OTTO_TEST_PDEATHSIG_SLEEPER";
    /// Path the race helper's `pre_exec` writes the grandchild pid to (see `helper_race`).
    const PIDFILE_ENV: &str = "OTTO_TEST_PDEATHSIG_PIDFILE";

    /// Line the guarded/unguarded helper prints so the test learns the grandchild's pid.
    const PID_MARKER: &str = "OTTO_TEST_GRANDCHILD_PID=";

    /// How long a correct guard may take to reap the grandchild. The kernel signals at parent
    /// death, so the real latency is sub-millisecond; this is a generous ceiling that keeps the
    /// test from being timing-sensitive on a loaded machine.
    const DEATH_TIMEOUT: Duration = Duration::from_secs(10);
    /// How long an *unguarded* grandchild must stay alive for the control to count. It only has to
    /// outlast the moment a guard would have killed it (sub-millisecond), so this is already ~3
    /// orders of magnitude of margin; keeping it small keeps the suite fast.
    const SURVIVAL_WINDOW: Duration = Duration::from_secs(2);
    /// Ceiling on waiting for a helper to report its grandchild's pid (a cold re-exec of this
    /// binary plus process startup).
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
    /// Ceiling on the race helper's in-child wait for its parent to die, as an iteration count of
    /// 2 ms naps. Bounds the fork→prctl window so a wedged test can never leave a spinning child.
    const RACE_SPIN_LIMIT: u32 = 5_000; // ~10s

    // ---------------------------------------------------------------- re-exec entry points

    /// Entry point for the spawned helper process: build a grandchild, report its pid, then park
    /// until the parent test SIGKILLs us. A no-op in a normal test run.
    #[test]
    fn pdeathsig_helper_process() {
        let Some(mode) = std::env::var_os(MODE_ENV) else {
            return;
        };
        // `tokio::process::Command::spawn` needs a runtime context, and `install_pdeathsig` takes a
        // tokio `Command` — the same type `boot()` uses — so the helper drives a small one.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build helper runtime");
        rt.block_on(async {
            match mode.to_string_lossy().as_ref() {
                "guarded" => helper_simple(true),
                "unguarded" => helper_simple(false),
                "race-guarded" => helper_race(true),
                "race-unguarded" => helper_race(false),
                other => panic!("unknown {MODE_ENV}: {other}"),
            }
        });
    }

    /// Entry point for the grandchild — the stand-in for `otto serve`. Parks long enough to outlast
    /// any assertion window, with a self-terminating cap so a crashed test run cannot strand it.
    /// A no-op in a normal test run.
    #[test]
    fn pdeathsig_sleeper_process() {
        if std::env::var_os(SLEEPER_ENV).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_secs(120));
    }

    // ---------------------------------------------------------------- helper bodies

    /// The grandchild command: this binary, re-executed into `pdeathsig_sleeper_process`.
    ///
    /// Deliberately **not** `kill_on_drop`: the whole point is that nothing but the kernel guard
    /// can reap it, so the guarded/unguarded pair differs in exactly one thing.
    fn sleeper_command() -> Command {
        let exe = std::env::current_exe().expect("current_exe");
        let mut command = Command::new(exe);
        command
            .arg(SLEEPER_TEST)
            .arg("--exact")
            .arg("--test-threads=1")
            .env(SLEEPER_ENV, "1")
            .env_remove(MODE_ENV)
            .env_remove(PIDFILE_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// Scenario 1: spawn the grandchild (guarded via the real `spawn_guarded`, or bare), announce
    /// its pid, then park. The parent test kills us once it has the pid.
    fn helper_simple(guarded: bool) {
        let mut command = sleeper_command();
        let child = if guarded {
            spawn_guarded(&mut command)
        } else {
            command.spawn()
        }
        .expect("spawn grandchild");
        let pid = child.id().expect("grandchild pid");
        // `--nocapture` is passed by the spawner, so this reaches our piped stdout immediately.
        println!("{PID_MARKER}{pid}");
        std::io::Write::flush(&mut std::io::stdout()).expect("flush pid marker");
        // Park holding `child`, so it is never dropped and `kill_on_drop` semantics can play no
        // part even in principle. We are SIGKILLed out of this sleep.
        std::thread::sleep(Duration::from_secs(120));
        drop(child);
    }

    /// Scenario 2: the fork→prctl race. A `pre_exec` closure registered **before**
    /// `install_pdeathsig` publishes the child's pid and then blocks until the parent is gone, so
    /// by the time `install_pdeathsig`'s own closure runs the parent is already dead —
    /// `PR_SET_PDEATHSIG` is armed against a corpse and can never fire. Only the `getppid()`
    /// self-kill can stop the child from exec'ing into a permanent orphan.
    fn helper_race(guarded: bool) {
        let pidfile = std::env::var(PIDFILE_ENV).expect("pidfile path");
        let pidfile = std::ffi::CString::new(pidfile).expect("pidfile path has no NUL");
        let my_pid = std::process::id() as libc::pid_t;
        let mut command = sleeper_command();
        // SAFETY: runs in the forked child before exec. Only `open`/`write`/`close`/`getpid`/
        // `getppid`/`nanosleep` — all async-signal-safe — are called, nothing is allocated, and
        // the captured `CString`/pid were built before the fork. Errors are returned as raw errno
        // values so even the failure path allocates nothing.
        unsafe {
            command.pre_exec(move || {
                if !write_pid_raw(pidfile.as_ptr(), libc::getpid()) {
                    return Err(std::io::Error::last_os_error());
                }
                let mut spins = 0u32;
                while libc::getppid() == my_pid {
                    if spins >= RACE_SPIN_LIMIT {
                        return Err(std::io::Error::from_raw_os_error(libc::ETIMEDOUT));
                    }
                    spins += 1;
                    let nap = libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 2_000_000,
                    };
                    libc::nanosleep(&nap, std::ptr::null_mut());
                }
                Ok(())
            });
        }
        // `spawn_guarded` registers the guard's closure second, so it runs second — after the
        // closure above has confirmed the parent is dead. Going through the same spawn helper
        // `boot()` uses keeps this test anchored to the real code path.
        //
        // Blocks inside `fork`+status-pipe until the closure above returns; we are SIGKILLed
        // while parked here, which is precisely the race being reproduced.
        let child = if guarded {
            spawn_guarded(&mut command)
        } else {
            command.spawn()
        };
        std::thread::sleep(Duration::from_secs(120));
        drop(child);
    }

    /// Write `pid` as 4 native-endian bytes to `path`. Async-signal-safe: no allocation, no
    /// formatting, no stdio.
    ///
    /// # Safety
    /// `path` must be a valid NUL-terminated C string that outlives the call.
    unsafe fn write_pid_raw(path: *const libc::c_char, pid: libc::pid_t) -> bool {
        let fd = libc::open(
            path,
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600 as libc::c_int,
        );
        if fd < 0 {
            return false;
        }
        // `pid_t` is `i32` on Linux, so this is exactly the 4 bytes `read_pidfile` expects.
        let bytes = pid.to_ne_bytes();
        let written = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        libc::close(fd);
        written == bytes.len() as isize
    }

    // ---------------------------------------------------------------- test-side plumbing

    /// Kills a pid on drop so a panicking assertion can never strand a 120-second sleeper.
    struct Reaper(i32);
    impl Drop for Reaper {
        fn drop(&mut self) {
            // SAFETY: `kill` on a possibly-dead pid is defined; a stale pid just yields ESRCH.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    /// True while `pid` names a process that has not yet died.
    ///
    /// A plain `kill(pid, 0)` is not enough: an orphaned grandchild that the guard killed lingers
    /// as a **zombie** until whatever inherited it (init, or a `--user` systemd/`dx serve`
    /// subreaper) reaps it, and `kill(pid, 0)` succeeds for zombies. Reading the state field of
    /// `/proc/<pid>/stat` distinguishes "still running" from "dead, not yet reaped", which is what
    /// makes the death assertion reliable regardless of who the reaper is.
    fn process_is_live(pid: i32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // `comm` (field 2) is parenthesised and may itself contain spaces and parens, so the state
        // character is the first token after the LAST ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return false;
        };
        match rest.trim_start().chars().next() {
            Some('Z') | Some('X') | None => false,
            Some(_) => true,
        }
    }

    fn wait_until_dead(pid: i32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !process_is_live(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        !process_is_live(pid)
    }

    /// Re-execute this binary as `pdeathsig_helper_process` in the given mode.
    fn spawn_helper(mode: &str, pidfile: Option<&PathBuf>) -> StdChild {
        let exe = std::env::current_exe().expect("current_exe");
        let mut command = StdCommand::new(exe);
        command
            .arg(HELPER_TEST)
            .arg("--exact")
            // Without `--nocapture` libtest buffers the helper's stdout until the test returns —
            // which it never does — so the pid marker would never reach us.
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(MODE_ENV, mode)
            .env_remove(SLEEPER_ENV)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        match pidfile {
            Some(path) => command.env(PIDFILE_ENV, path),
            None => command.env_remove(PIDFILE_ENV),
        };
        command.spawn().expect("spawn helper")
    }

    /// Read the grandchild pid off the helper's stdout, bounded so a mis-filtered helper (which
    /// would exit having run zero tests) fails loudly instead of hanging the suite.
    fn read_grandchild_pid(helper: &mut StdChild) -> i32 {
        let stdout = helper.stdout.take().expect("helper stdout piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Keep draining after the marker: stopping early could wedge the helper on a full
            // pipe if libtest writes anything more.
            let mut sent = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sent {
                    continue;
                }
                // libtest emits `test <name> ... ` with no trailing newline before handing the
                // test its stdout, so the marker lands mid-line rather than at line start.
                if let Some(at) = line.find(PID_MARKER) {
                    let digits: String = line[at + PID_MARKER.len()..]
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect();
                    let _ = tx.send(digits);
                    sent = true;
                }
            }
        });
        match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(pid) => pid.parse().expect("grandchild pid is an integer"),
            Err(e) => {
                let _ = helper.kill();
                panic!("helper never reported a grandchild pid ({e}); is {HELPER_TEST} still the right test path?");
            }
        }
    }

    /// Kill the helper outright and reap it. SIGKILL is the point of the test: it guarantees the
    /// helper executes no teardown, so `kill_on_drop` cannot be what cleans up the grandchild.
    fn sigkill_and_reap(helper: &mut StdChild) {
        // SAFETY: `helper.id()` is a live child of this process until we wait on it below.
        unsafe { libc::kill(helper.id() as libc::pid_t, libc::SIGKILL) };
        let status = helper.wait().expect("reap helper");
        assert!(
            !status.success(),
            "helper should have died from SIGKILL, not exited normally: {status:?}"
        );
    }

    fn unique_pidfile(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("otto-pdeathsig-{tag}-{}.pid", std::process::id()))
    }

    /// Poll for the race helper's `pre_exec` to publish the grandchild pid.
    fn read_pidfile(path: &PathBuf, helper: &mut StdChild) -> i32 {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(bytes) = std::fs::read(path) {
                if bytes.len() == 4 {
                    return i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = helper.kill();
        panic!("race helper never published a grandchild pid to {path:?}");
    }

    // ---------------------------------------------------------------- the tests

    /// Guards the two `--exact` filters above: if either entry point is renamed or moved, the
    /// subprocess tests would silently run zero tests and hang until their handshake timeout.
    /// This turns that into an instant, obvious failure.
    #[test]
    fn helper_test_names_resolve() {
        let exe = std::env::current_exe().expect("current_exe");
        for name in [HELPER_TEST, SLEEPER_TEST] {
            let out = StdCommand::new(&exe)
                .arg(name)
                .arg("--exact")
                .arg("--list")
                .env_remove(MODE_ENV)
                .env_remove(SLEEPER_ENV)
                .output()
                .expect("list tests");
            let listing = String::from_utf8_lossy(&out.stdout);
            assert!(
                listing.contains(&format!("{name}: test")),
                "`{name}` does not name a real test; --list said:\n{listing}"
            );
        }
    }

    /// THE regression test: a sidecar spawned the way `boot()` spawns it must not survive a
    /// non-unwinding death of its parent. Fails if `install_pdeathsig` ever leaves the spawn path.
    #[test]
    fn guarded_child_dies_when_parent_is_sigkilled() {
        let mut helper = spawn_helper("guarded", None);
        let grandchild = read_grandchild_pid(&mut helper);
        let _reaper = Reaper(grandchild);
        assert!(
            process_is_live(grandchild),
            "grandchild {grandchild} should be running before the parent is killed"
        );

        sigkill_and_reap(&mut helper);

        assert!(
            wait_until_dead(grandchild, DEATH_TIMEOUT),
            "guarded grandchild {grandchild} outlived its SIGKILLed parent by more than {DEATH_TIMEOUT:?} \
             — PR_SET_PDEATHSIG is not on the sidecar spawn path"
        );
    }

    /// Control for the test above. Same helper, same grandchild, guard omitted — the grandchild
    /// survives, which is the original Gate-E orphan bug. Without this, a grandchild that died of
    /// unrelated causes would make the death assertion vacuously true.
    #[test]
    fn unguarded_child_survives_parent_sigkill() {
        let mut helper = spawn_helper("unguarded", None);
        let grandchild = read_grandchild_pid(&mut helper);
        let _reaper = Reaper(grandchild);

        sigkill_and_reap(&mut helper);

        std::thread::sleep(SURVIVAL_WINDOW);
        assert!(
            process_is_live(grandchild),
            "unguarded grandchild {grandchild} died on its own — the death assertion in \
             guarded_child_dies_when_parent_is_sigkilled would be vacuous"
        );
    }

    /// The `getppid()` race guard: when the parent dies inside the fork→prctl window,
    /// `PR_SET_PDEATHSIG` is armed too late to ever fire, so the child must notice its parent is
    /// gone and self-terminate before exec'ing. Fails if that check is dropped, even though the
    /// `prctl` call itself still succeeds.
    #[test]
    fn race_guard_self_kills_when_parent_dies_before_prctl() {
        let pidfile = unique_pidfile("race-guarded");
        let _ = std::fs::remove_file(&pidfile);
        let mut helper = spawn_helper("race-guarded", Some(&pidfile));
        let grandchild = read_pidfile(&pidfile, &mut helper);
        let _reaper = Reaper(grandchild);

        // Kill the parent while the child is still parked in `pre_exec`, before `prctl` runs.
        sigkill_and_reap(&mut helper);

        assert!(
            wait_until_dead(grandchild, DEATH_TIMEOUT),
            "child {grandchild} survived a parent that died inside the fork→prctl window — the \
             getppid() race guard is missing"
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    /// Control for the race test: with no guard installed the child completes `pre_exec`, execs,
    /// and becomes exactly the orphan the guard exists to prevent.
    #[test]
    fn unguarded_race_child_becomes_an_orphan() {
        let pidfile = unique_pidfile("race-unguarded");
        let _ = std::fs::remove_file(&pidfile);
        let mut helper = spawn_helper("race-unguarded", Some(&pidfile));
        let grandchild = read_pidfile(&pidfile, &mut helper);
        let _reaper = Reaper(grandchild);

        sigkill_and_reap(&mut helper);

        std::thread::sleep(SURVIVAL_WINDOW);
        assert!(
            process_is_live(grandchild),
            "unguarded race child {grandchild} died without the guard — the assertion in \
             race_guard_self_kills_when_parent_dies_before_prctl would be vacuous"
        );
        let _ = std::fs::remove_file(&pidfile);
    }
}
