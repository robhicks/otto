//! Parent-death guard for spawned child processes.
//!
//! ## Why this exists
//!
//! A `Drop` guard is not a teardown guarantee. `otto serve` can be `SIGKILL`ed — on desktop it
//! routinely is, by `ui-dioxus`'s own `PR_SET_PDEATHSIG` guard when the app window closes — and
//! `SIGKILL` runs no destructor, so a `Drop` impl that kills a child never executes.
//!
//! Most of otto's long-lived children are stdio servers, which is why they survive that: the parent
//! dying closes their stdin pipe, they read EOF and exit, entirely independent of `Drop`
//! (`crates/engine/tests/mcp_child_teardown.rs` pins this). A hypervisor is the exception. Firecracker
//! is spawned with no stdio pipe it monitors and has no reason to notice its parent died, so it holds
//! the *only* case in the tree where a `Drop` guard is genuinely load-bearing — and therefore the only
//! one a hard kill can leak. An orphaned microVM keeps its vCPUs and guest memory.
//!
//! `PR_SET_PDEATHSIG` closes it at the kernel level: the child is signalled when its parent dies for
//! any reason, whether or not anything got to run.
//!
//! Linux-only. Firecracker requires KVM so it is Linux-only regardless; on other targets the guard
//! is a no-op and the caller falls back to its `Drop` guard alone.

/// Install the parent-death guard on `command`, then spawn it. The single spawn path for any child
/// that cannot detect its parent's death by itself.
///
/// `dead_code` is allowed because the only production caller is behind the default-off `firecracker`
/// feature. Keeping the module itself unconditional is the point: a guard that compiles only under a
/// feature flag nobody builds is a guard that rots silently. The tests below exercise it on every
/// default `cargo test`.
#[cfg_attr(not(feature = "firecracker"), allow(dead_code))]
pub(crate) fn spawn_guarded(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    install_pdeathsig(command);
    command.spawn()
}

/// Attach a kernel-level parent-death guard so the child cannot outlive this process, independent of
/// whether teardown runs destructors.
///
/// `PR_SET_PDEATHSIG` is set in the child between fork and exec — so it is armed before the target
/// binary ever runs — and tells the kernel to `SIGKILL` it the moment its parent dies.
///
/// The `getppid() != parent_pid` check closes the race where the parent already died in the window
/// between fork and the `prctl` call, in which case the signal would never be delivered. It is
/// deliberately not the naive `getppid() == 1`: under a child subreaper (systemd `--user`, a
/// container init like tini) an orphan reparents to the *subreaper*, not to pid 1, so a `== 1` test
/// would miss exactly the environments where this matters most. Any ppid other than our own means
/// the parent is gone.
#[cfg(target_os = "linux")]
#[cfg_attr(not(feature = "firecracker"), allow(dead_code))]
fn install_pdeathsig(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;

    // Captured before fork so the child can tell whether its parent is still us.
    let parent_pid = std::process::id();
    // SAFETY: the closure runs in the forked child before exec and calls only async-signal-safe
    // syscalls (`prctl`, `getppid`, `raise`). It captures a single copied integer, allocates
    // nothing, and takes no locks — the three things that make a `pre_exec` closure unsound.
    //
    // `io::Error::last_os_error()` on the failure path only wraps errno; it does not allocate.
    //
    // Note: PR_SET_PDEATHSIG arms against death of the *thread* that forked, not the process. The
    // caller spawns from a tokio worker thread that lives for the process's lifetime, so thread
    // death and process death coincide. Spawning from an ephemeral thread (e.g. inside
    // `spawn_blocking`) would break that assumption and silently kill the child early.
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
            if libc::getppid() != parent_pid as libc::pid_t {
                libc::raise(libc::SIGKILL);
            }
            Ok(())
        });
    }
}

/// No-op on targets without `PR_SET_PDEATHSIG`. The caller's `Drop` guard still covers ordinary
/// teardown; only the hard-kill case is uncovered, and it is unreachable in practice because the
/// only caller (Firecracker) requires KVM.
#[cfg(not(target_os = "linux"))]
fn install_pdeathsig(_command: &mut std::process::Command) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// libtest path of the re-exec entry point. `sleeper_name_resolves` pins it to a real test, so a
    /// rename cannot turn the `--exact` filters below into silent no-ops (a subprocess that matches
    /// zero tests exits 0, which would look like a pass).
    const SLEEPER_TEST: &str = "proc_guard::tests::pdeathsig_sleeper_process";
    /// Role switches: a re-executed copy runs a subprocess body only when its variable is set.
    const SLEEPER_ENV: &str = "OTTO_TEST_PROC_GUARD_SLEEPER";
    const HOLDER_ENV: &str = "OTTO_TEST_PROC_GUARD_HOLDER";
    /// Line the holder prints so the test learns the grandchild's pid.
    const PID_MARKER: &str = "OTTO_TEST_PROC_GUARD_PID=";

    /// A correct guard signals at parent death, so real latency is sub-millisecond; this ceiling
    /// just keeps the test off the knife-edge on a loaded machine.
    const DEATH_TIMEOUT: Duration = Duration::from_secs(15);
    /// How long the unguarded control must stay alive to count. It only has to outlast the moment a
    /// guard would have killed it.
    const SURVIVAL_WINDOW: Duration = Duration::from_secs(3);
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
    /// Self-cap on every parked subprocess, so an interrupted run cannot strand one.
    const PARK: Duration = Duration::from_secs(120);

    /// Entry point for the grandchild: park long enough to outlast any assertion window.
    /// `#[ignore]`d because it is a subprocess role, not a test — it asserts nothing, and a normal
    /// run must never execute it.
    #[test]
    #[ignore = "subprocess role, driven by the tests below via --exact --ignored"]
    fn pdeathsig_sleeper_process() {
        if std::env::var_os(SLEEPER_ENV).is_none() {
            return;
        }
        std::thread::sleep(PARK);
    }

    /// Entry point for the holder: spawn the sleeper (guarded or not), report its pid, then park
    /// holding it. The parent test `SIGKILL`s us once it has the pid.
    #[test]
    #[ignore = "subprocess role, driven by the tests below via --exact --ignored"]
    fn pdeathsig_holder_process() {
        let Some(mode) = std::env::var_os(HOLDER_ENV) else {
            return;
        };
        let mut command = sleeper_command();
        let child = if mode == *"guarded" {
            super::spawn_guarded(&mut command)
        } else {
            // The control: identical in every respect except the guard, so a difference in outcome
            // is attributable to the guard and nothing else.
            command.spawn()
        }
        .expect("spawn sleeper");
        println!("{PID_MARKER}{}", child.id());
        std::io::Write::flush(&mut std::io::stdout()).expect("flush pid marker");
        // Park holding `child` so it is never dropped — no `Drop`-based path can play any part.
        std::thread::sleep(PARK);
        drop(child);
    }

    /// The grandchild command: this test binary, re-executed into the sleeper role.
    ///
    /// Deliberately not `kill_on_drop` and not otherwise supervised: the guard must be the only
    /// thing that can reap it, so the guarded/unguarded pair differs in exactly one variable.
    fn sleeper_command() -> Command {
        let exe = std::env::current_exe().expect("current_exe");
        let mut command = Command::new(exe);
        command
            .arg(SLEEPER_TEST)
            .arg("--exact")
            // The entry point is `#[ignore]`d so a normal run can't reach it; opt in here.
            .arg("--ignored")
            .arg("--test-threads=1")
            .env(SLEEPER_ENV, "1")
            .env_remove(HOLDER_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// True while `pid` names a process that has not yet died.
    ///
    /// A plain `kill(pid, 0)` is not enough: a guard-killed orphan lingers as a **zombie** until
    /// whatever inherited it reaps it, and `kill(pid, 0)` succeeds for zombies — so the naive probe
    /// would call a correctly-killed process alive, making the result depend on who the reaper is.
    /// Only a genuinely absent `/proc` entry counts as dead; any other read error reports *live*, so
    /// an unrelated I/O failure can never manufacture a passing death assertion.
    fn process_is_live(pid: i32) -> bool {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        };
        // `comm` (field 2) is parenthesised and may contain spaces and parens, so the state
        // character is the first token after the LAST ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return true;
        };
        // 'X'/'x' are both "dead"; modern kernels emit only 'X', but 2.6.33–3.13 emitted 'x'.
        !matches!(rest.trim_start().chars().next(), Some('Z' | 'X' | 'x'))
    }

    fn wait_until_dead(pid: i32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !process_is_live(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Re-check after the loop: a final nap straddling the deadline must not report a failure for
        // a process that died microseconds earlier.
        !process_is_live(pid)
    }

    /// Kills the grandchild on drop so a panicking assertion cannot leave it running out its
    /// self-cap. Identity is re-checked immediately before signalling: the grandchild is not this
    /// process's child, so it can never be `wait()`ed on here, which means its pid can be recycled
    /// once whatever inherited it does the reaping — and a bare `kill` would then hit a stranger.
    /// The `pid > 1` floor is separate and non-negotiable: `kill(0, …)` signals this whole process
    /// group and `kill(-1, …)` every process the user owns.
    struct KillOnDrop(i32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let is_ours = std::fs::read(format!("/proc/{}/cmdline", self.0))
                .map(|c| String::from_utf8_lossy(&c).contains(SLEEPER_TEST))
                .unwrap_or(false);
            if self.0 > 1 && is_ours {
                // SAFETY: plain `kill(2)`; pid validated positive and just re-identified as ours.
                unsafe { libc::kill(self.0, libc::SIGKILL) };
            }
        }
    }

    /// Re-exec into the holder role and return it plus the grandchild pid it reports.
    fn start_holder(mode: &str) -> (std::process::Child, i32) {
        use std::io::{BufRead, BufReader};

        let exe = std::env::current_exe().expect("current_exe");
        let mut holder = Command::new(exe)
            .arg("proc_guard::tests::pdeathsig_holder_process")
            .arg("--exact")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(HOLDER_ENV, mode)
            .env_remove(SLEEPER_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn holder");

        // Read on a worker thread so a holder that dies before printing cannot wedge the test.
        let stdout = holder.stdout.take().expect("holder stdout piped");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.split_once(PID_MARKER).map(|(_, r)| r.to_string()) {
                    let _ = tx.send(rest.trim().to_string());
                    return;
                }
            }
        });

        match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(pid) => {
                let pid: i32 = pid.parse().expect("grandchild pid parses");
                assert!(pid > 1, "implausible grandchild pid {pid}");
                (holder, pid)
            }
            Err(_) => {
                let _ = holder.kill();
                let _ = holder.wait();
                panic!("holder ({mode}) never reported a grandchild pid");
            }
        }
    }

    /// `SIGKILL` the holder and confirm it died *by that signal*.
    ///
    /// The signal check is load-bearing, not ceremony: a holder that had already panicked exits 101,
    /// and a panic **unwinds**, running the destructors this scenario exists to bypass. Without the
    /// check, that case would look like a pass.
    fn sigkill_and_reap(holder: &mut std::process::Child) {
        use std::os::unix::process::ExitStatusExt as _;
        holder.kill().expect("SIGKILL holder");
        let status = holder.wait().expect("reap holder");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "holder did not die by SIGKILL (status {status:?}); it exited on its own, so its \
             destructors ran and the result below proves nothing"
        );
    }

    /// Guards the `--exact` filters above.
    #[test]
    fn sleeper_name_resolves() {
        let exe = std::env::current_exe().expect("current_exe");
        let out = Command::new(&exe)
            .arg(SLEEPER_TEST)
            .arg("--exact")
            .arg("--ignored")
            .arg("--list")
            .env_remove(SLEEPER_ENV)
            .env_remove(HOLDER_ENV)
            .output()
            .expect("list tests");
        let listed = String::from_utf8_lossy(&out.stdout);
        assert!(
            listed.contains(SLEEPER_TEST),
            "`{SLEEPER_TEST}` does not resolve to a real test, so the --exact filters here would \
             match nothing and every scenario would pass vacuously. Listed:\n{listed}"
        );
    }

    /// The guard kills a child whose parent is hard-killed — the case a `Drop` guard cannot reach.
    #[test]
    fn guarded_child_dies_when_its_parent_is_hard_killed() {
        let (mut holder, pid) = start_holder("guarded");
        let _cleanup = KillOnDrop(pid);
        assert!(
            process_is_live(pid),
            "grandchild was already dead before the parent was killed, so the death assertion \
             below would pass without proving anything"
        );

        sigkill_and_reap(&mut holder);

        assert!(
            wait_until_dead(pid, DEATH_TIMEOUT),
            "grandchild (pid {pid}) survived its parent's SIGKILL — PR_SET_PDEATHSIG is not \
             installed, so a hard-killed otto would orphan the microVM"
        );
    }

    /// The control that makes the test above meaningful.
    ///
    /// Identical setup minus the guard: this child *must* survive. If it did not, something in the
    /// environment (a process-group kill, a subreaper, a cgroup) would be reaping subprocesses on
    /// its own, and the guarded case would pass whether or not the guard worked.
    #[test]
    fn unguarded_child_survives_its_parent_being_hard_killed() {
        let (mut holder, pid) = start_holder("unguarded");
        let _cleanup = KillOnDrop(pid);

        sigkill_and_reap(&mut holder);

        std::thread::sleep(SURVIVAL_WINDOW);
        assert!(
            process_is_live(pid),
            "the unguarded control died too, so this file's death assertion proves nothing about \
             PR_SET_PDEATHSIG — something else is killing orphaned subprocesses here"
        );
    }
}
