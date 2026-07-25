//! Regression: every long-lived process otto spawns must die when its parent is hard-killed.
//!
//! ## The invariant these tests pin
//!
//! otto's child processes survive no destructor. `otto serve` can be `SIGKILL`ed outright — and on
//! desktop it routinely is: `ui-dioxus`'s `PR_SET_PDEATHSIG` guard kills the sidecar with `SIGKILL`
//! when the app window closes. `SIGKILL` runs no `Drop`, so **none** of the usual cleanup fires:
//! not `tokio`'s `kill_on_drop`, not rmcp's `ChildWithCleanup::drop` (which is weaker still — it
//! defers the kill to a `tokio::spawn`ed task that a dying process will never poll).
//!
//! What reaps the top of the tree is **stdio pipe EOF**. Every long-lived child otto spawns is a
//! stdio server holding a pipe to its parent; when the parent dies by any means the pipe closes,
//! the child reads EOF, and its serve loop exits. That cascades: `otto serve` → `mcp-lsp` →
//! `rust-analyzer` all collapse in order.
//!
//! Below the first hop the two guards overlap: `mcp-lsp` ends its own EOF-driven shutdown by
//! *returning* from `main`, which does run destructors, so its `kill_on_drop` handle would also fire
//! there. These tests deliberately assert the **observable** (the whole chain is gone) rather than
//! attributing it to one mechanism — the point is that the tree collapses even at the top, where
//! `SIGKILL` provably rules every destructor out.
//!
//! That property is **load-bearing but implicit** — it is a consequence of choosing stdio
//! transports, not something any code asserts. rmcp also offers HTTP/SSE transports, and a server
//! reached that way (or one that simply ignores stdin EOF) holds no pipe to notice, so it would
//! orphan silently on every hard kill with nothing in the build to flag it. These tests convert
//! that implicit property into a failing test.
//!
//! ## Why an integration test and not `#[cfg(test)] mod tests`
//!
//! The scenario needs a process that is killed *without unwinding*, which cannot be expressed
//! in-process: the assertions have to outlive the thing under test. So a parent test re-executes
//! **this same test binary** into a subprocess role (the pattern
//! `ui-dioxus/src/desktop_boot.rs`'s pdeathsig tests established), lets it build a real child, then
//! `SIGKILL`s it and watches `/proc`.
//!
//! Linux-only: the whole file is `/proc`-based, and the desktop guard it backstops is itself
//! Linux-only.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// libtest paths of the two re-exec entry points. These are plain function names because an
/// integration test binary has no module prefix. `entry_point_names_resolve` pins them to real
/// tests, so a rename cannot silently turn every scenario below into a vacuous pass.
const HOLDER_TEST: &str = "teardown_holder_process";
const SLEEPER_TEST: &str = "teardown_sleeper_process";

/// Role switches. A re-executed copy runs a subprocess body only when its variable is set; under a
/// normal `cargo test` both entry points see nothing and return immediately.
const MODE_ENV: &str = "OTTO_TEST_TEARDOWN_MODE";
const SLEEPER_ENV: &str = "OTTO_TEST_TEARDOWN_SLEEPER";
/// Path of the MCP server binary the holder should connect to (built by the parent via escargot, so
/// the subprocess never pays for a second build).
const BIN_ENV: &str = "OTTO_TEST_TEARDOWN_BIN";
/// Workspace root the holder passes to that server.
const ROOT_ENV: &str = "OTTO_TEST_TEARDOWN_ROOT";

/// Printed by the holder once its child is live and the parent may kill it.
const READY_MARKER: &str = "OTTO_TEST_TEARDOWN_READY";

/// How long a correct EOF cascade may take. Teardown is driven by pipe close, so the real latency
/// is milliseconds; this is a generous ceiling that keeps the test off the timing knife-edge on a
/// loaded machine.
const DEATH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a child must stay alive for a *survival* assertion to count. It only has to outlast the
/// moment a cascade would have killed it, so this is already ample margin.
const SURVIVAL_WINDOW: Duration = Duration::from_secs(3);
/// Ceiling on waiting for the holder's ready line. Generous because the `mcp-lsp` scenario waits on
/// a cold `rust-analyzer` start behind it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
/// Self-cap on every parked subprocess, so a crashed or interrupted run cannot strand one.
const PARK: Duration = Duration::from_secs(120);

// ----------------------------------------------------------------- re-exec entry points

/// Entry point for the holder: connect to a real MCP server through the **production** connect path,
/// announce readiness, then park holding the connection so nothing can drop it.
///
/// `#[ignore]` because this is a subprocess role, not a test — it asserts nothing, and a normal run
/// must never execute it (if `MODE_ENV` ever leaked into a developer's shell this would spawn a
/// server and sleep). The spawner opts in with `--ignored`.
#[test]
#[ignore = "subprocess role, driven by the tests below via --exact --ignored"]
fn teardown_holder_process() {
    let Some(mode) = std::env::var_os(MODE_ENV) else {
        return;
    };
    // The control role spawns no MCP server and needs no runtime, so it branches before the setup
    // the real scenarios require.
    if mode == *"control" {
        hold_control_child();
        return;
    }
    let bin = std::env::var(BIN_ENV).expect("holder needs a server binary path");
    let root = PathBuf::from(std::env::var(ROOT_ENV).expect("holder needs a workspace root"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build holder runtime");

    rt.block_on(async move {
        // `_conn` is held for the rest of the process: the child must be reaped by the EOF cascade
        // alone, never by a drop that happens to run first.
        let _conn = match mode.to_string_lossy().as_ref() {
            "mcp-fs" => {
                let (conn, _tools) = otto_engine::mcp_connect_fs(&bin, &root)
                    .await
                    .expect("connect to mcp-fs");
                conn
            }
            "mcp-lsp" => {
                let (conn, tools) = otto_engine::mcp_connect_lsp(&bin, &root)
                    .await
                    .expect("connect to mcp-lsp");
                // Drive one real call so mcp-lsp *lazily* spawns its language server — without this
                // the grandchild under test would not exist and the scenario would be vacuous.
                let diag = tools
                    .iter()
                    .find(|t| t.name() == "lsp.diagnostics")
                    .expect("mcp-lsp advertises lsp.diagnostics");
                diag.call(serde_json::json!({ "path": "src/main.rs" }))
                    .await
                    .expect("lsp.diagnostics call");
                conn
            }
            other => panic!("unknown {MODE_ENV}: {other}"),
        };

        announce_ready();
        tokio::time::sleep(PARK).await;
    });
}

/// Entry point for the control child: a process that holds a pipe to its parent but **never reads
/// it**, so pipe close tells it nothing. This is the shape a non-stdio (or EOF-ignoring) server
/// would have, and it is what the death assertions must be able to catch.
///
/// `#[ignore]`d for the same reason as the holder.
#[test]
#[ignore = "subprocess role, driven by the tests below via --exact --ignored"]
fn teardown_sleeper_process() {
    if std::env::var_os(SLEEPER_ENV).is_none() {
        return;
    }
    std::thread::sleep(PARK);
}

/// Control-mode holder body: spawn the never-reading sleeper, announce, park.
///
/// Deliberately plain `std::process::Command` with no `kill_on_drop` and no parent-death guard, so
/// the sleeper's survival is attributable to exactly one thing — that nothing told it to die.
fn hold_control_child() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .arg(SLEEPER_TEST)
        .arg("--exact")
        // The entry point is `#[ignore]`d so a normal run can't execute it; opt in here.
        .arg("--ignored")
        .arg("--test-threads=1")
        .env(SLEEPER_ENV, "1")
        .env_remove(MODE_ENV)
        // Piped-but-unread: the child *has* the pipe, it just never looks at it.
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn control sleeper");
    announce_ready();
    std::thread::sleep(PARK);
    // Only reached if the test never killed us (a wedged or interrupted run): tidy up rather than
    // leave the sleeper to its own self-cap. In the scenario proper we are SIGKILLed mid-`sleep`, so
    // neither line runs — which is the whole point, since the sleeper must survive us.
    let _ = child.kill();
    let _ = child.wait();
}

/// Print the ready line and flush it. `--nocapture` is passed by every spawner, so this reaches the
/// parent's piped stdout immediately rather than sitting in libtest's capture buffer.
fn announce_ready() {
    println!("{READY_MARKER}");
    std::io::Write::flush(&mut std::io::stdout()).expect("flush ready marker");
}

// ----------------------------------------------------------------- /proc plumbing

/// True while `pid` names a process that has not yet died.
///
/// A plain `kill(pid, 0)` is not enough: a child killed by the cascade lingers as a **zombie** until
/// whatever inherited it reaps it, and `kill(pid, 0)` succeeds for zombies — so the naive probe
/// would report a correctly-killed process as alive and fail the test depending on who the reaper
/// is. Reading the state field of `/proc/<pid>/stat` distinguishes "still running" from "dead, not
/// yet reaped".
///
/// Only a genuinely absent `/proc` entry counts as dead. Any other read error is reported as
/// *live*, because "dead" is the answer that makes the death assertions pass — an unrelated I/O
/// failure must not be able to manufacture a green run.
fn process_is_live(pid: i32) -> bool {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    // `comm` (field 2) is parenthesised and may itself contain spaces and parens, so the state
    // character is the first token after the LAST ')'.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return true;
    };
    // 'X'/'x' are both "dead"; modern kernels only emit 'X', but 2.6.33–3.13 emitted 'x'.
    !matches!(rest.trim_start().chars().next(), Some('Z' | 'X' | 'x'))
}

/// `(pid, ppid, comm)` for every process currently visible in `/proc`.
fn process_table() -> Vec<(i32, i32, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // Split at the last ')' for the same reason as `process_is_live`.
        let Some((head, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let comm = head
            .split_once('(')
            .map(|(_, c)| c.to_string())
            .unwrap_or_default();
        let mut fields = rest.split_whitespace();
        let (_state, ppid) = (fields.next(), fields.next());
        let Some(Ok(ppid)) = ppid.map(str::parse::<i32>) else {
            continue;
        };
        out.push((pid, ppid, comm));
    }
    out
}

/// Every transitive descendant of `root`, so a grandchild (`mcp-lsp` → `rust-analyzer`) is covered,
/// not just direct children.
fn descendants(root: i32) -> Vec<(i32, String)> {
    let table = process_table();
    let mut children: HashMap<i32, Vec<(i32, String)>> = HashMap::new();
    for (pid, ppid, comm) in table {
        children.entry(ppid).or_default().push((pid, comm));
    }
    let mut out = Vec::new();
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        for (child, comm) in children.get(&pid).cloned().unwrap_or_default() {
            queue.push(child);
            out.push((child, comm));
        }
    }
    out
}

/// Assert every tracked child is alive *right now*, immediately before the parent is killed.
///
/// The snapshot that produced `kids` only proves they existed at handshake time. Without this, a
/// child that had already exited on its own — a crashed `mcp-fs`, a `rust-analyzer` that gave up —
/// would sail through the death assertions afterwards and the scenario would confirm nothing. The
/// death check is only meaningful as the second half of a live→dead transition, which is why
/// `ui-dioxus`'s equivalent is likewise two-sided.
fn assert_all_live_before_kill(kids: &[(i32, String)]) {
    for (pid, comm) in kids {
        assert!(
            process_is_live(*pid),
            "{comm} (pid {pid}) was already dead before the parent was killed, so the death \
             assertion that follows would pass without proving anything about teardown"
        );
    }
}

/// Poll until `pid` is gone, up to `timeout`. Returns whether it died.
///
/// The re-check after the loop is deliberate: without it, a final nap that straddles the deadline
/// would report a failure for a process that died microseconds before the check.
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

/// Kills leftover descendants on drop, so a panicking assertion cannot leave a subprocess running
/// out its self-cap.
///
/// Kills, never reaps: these are not this process's children, so they can never be `wait()`ed on
/// here — which is exactly why a pid can be **recycled** once whatever inherited it does the
/// reaping. A liveness check alone does not close that: the pid would still look alive, just as
/// somebody else. So each pid is re-identified against the `comm` recorded when the tree was
/// snapshotted, immediately before signalling. `ui-dioxus/src/desktop_boot.rs`'s `KillOnDrop` learned
/// this the same way (its `is_our_sleeper` check); dropping it here would reintroduce a fixed bug.
///
/// The `pid > 1` floor matters independently: `kill(0, …)` signals this whole process group and
/// `kill(-1, …)` every process the user owns, so an implausible pid must never reach `kill`.
///
/// `comm` is compared against `/proc/<pid>/comm`, the same 15-char-truncated field `process_table`
/// read it from, so the two always agree on truncation.
struct ReapOnDrop(Vec<(i32, String)>);

impl ReapOnDrop {
    /// Track every descendant in `kids`, recording the name each pid had when it was seen.
    fn new(kids: &[(i32, String)]) -> Self {
        Self(kids.to_vec())
    }
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        for (pid, comm) in &self.0 {
            if *pid > 1 && still_named(*pid, comm) {
                // SAFETY: plain `kill(2)`; the pid was just re-identified as the process we
                // recorded, and validated positive so it cannot be a group/broadcast target.
                unsafe { libc::kill(*pid, libc::SIGKILL) };
            }
        }
    }
}

/// True when `pid` is still the process that was recorded under `comm` — i.e. live, and not a
/// recycled pid now belonging to something else.
fn still_named(pid: i32, comm: &str) -> bool {
    if !process_is_live(pid) {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(current) => current.trim_end() == comm,
        // Vanished between the two reads, or unreadable — either way, do not signal it.
        Err(_) => false,
    }
}

// ----------------------------------------------------------------- harness

/// Owns the holder subprocess and kills it on drop.
///
/// `std::process::Child` has no kill-on-drop, and every scenario below asserts its fixture is sound
/// *before* reaching the explicit kill — so without this guard a failing assertion would leave the
/// holder parked for its full self-cap. (Found the hard way: a deliberately-failed run left one
/// running.) `ReapOnDrop` does not cover this, since it only tracks the holder's *descendants*.
struct Holder(Child);

impl Holder {
    fn pid(&self) -> i32 {
        self.0.id() as i32
    }

    /// `SIGKILL` the holder and reap it — precisely the signal, and the abruptness, that the
    /// desktop `PR_SET_PDEATHSIG` guard delivers to `otto serve`.
    ///
    /// Asserting the exit was *by `SIGKILL`* is the load-bearing part, not a formality. If the
    /// holder had already died on its own — a panic in the subprocess role exits 101, and a panic
    /// **unwinds**, running the `Drop` impls (rmcp's `ChildWithCleanup`, tokio's `kill_on_drop`)
    /// that this whole file exists to bypass — then its children would die for the wrong reason and
    /// every death assertion below would pass while proving nothing. `ui-dioxus`'s
    /// `sigkill_and_reap` was corrected to this same check for this same reason; `!success()` is
    /// not enough, because exit-101 satisfies that too.
    fn hard_kill(&mut self) {
        self.0.kill().expect("SIGKILL holder");
        let status = self.0.wait().expect("reap holder");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "holder did not die by SIGKILL (status {status:?}) — it exited on its own first, so \
             its destructors ran and any child death below proves nothing about hard-kill teardown"
        );
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        // Already-killed holders make these fail harmlessly; the point is the panicking path.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Re-execute this binary into the holder role and wait for its ready line.
///
/// Returns the live holder plus the descendants it built. `bin`/`root` are unused by the `control`
/// mode, which branches before reading them.
fn start_holder(mode: &str, bin: &str, root: &str) -> (Holder, Vec<(i32, String)>) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut holder = Command::new(exe)
        .arg(HOLDER_TEST)
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(MODE_ENV, mode)
        .env(BIN_ENV, bin)
        .env(ROOT_ENV, root)
        .env_remove(SLEEPER_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn holder");

    // Read the holder's stdout on a worker thread so a holder that dies before printing cannot wedge
    // the test forever — the recv timeout below bounds the wait either way.
    let stdout = holder.stdout.take().expect("holder stdout piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains(READY_MARKER) {
                let _ = tx.send(());
                return;
            }
        }
    });

    let holder = Holder(holder);
    if rx.recv_timeout(HANDSHAKE_TIMEOUT).is_err() {
        // A timed-out holder may already have spawned children (a cold `rust-analyzer` is the slow
        // case this timeout exists for). Reap them explicitly rather than leaving it to the EOF
        // cascade — relying on the property under test to clean up after its own failed setup would
        // be circular, and a stranded `rust-analyzer` can hold hundreds of MB.
        let _reaper = ReapOnDrop::new(&descendants(holder.pid()));
        // `holder` drops here too, killing it.
        panic!("holder ({mode}) never signalled readiness within {HANDSHAKE_TIMEOUT:?}");
    }

    let kids = descendants(holder.pid());
    (holder, kids)
}

/// Build a sibling MCP server binary. Mirrors the escargot pattern the other `mcp_*` tests use, so
/// the test works under `cargo test -p otto-engine` without relying on a workspace-wide build.
fn build_bin(package: &str, bin: &str) -> PathBuf {
    escargot::CargoBuild::new()
        .package(package)
        .bin(bin)
        .run()
        .unwrap_or_else(|e| panic!("build {bin}: {e}"))
        .path()
        .to_path_buf()
}

/// A minimal Rust crate — enough for `mcp-lsp` to route `src/main.rs` to `rust-analyzer`.
fn rust_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
    dir
}

// ----------------------------------------------------------------- tests

/// Guards the two `--exact` filters above. Without this, renaming an entry point would make every
/// spawner match zero tests, and a subprocess that runs nothing exits 0 — turning the scenarios
/// into silent no-ops rather than failures.
#[test]
fn entry_point_names_resolve() {
    let exe = std::env::current_exe().expect("current_exe");
    for name in [HOLDER_TEST, SLEEPER_TEST] {
        let out = Command::new(&exe)
            .arg(name)
            .arg("--exact")
            .arg("--ignored")
            .arg("--list")
            .env_remove(MODE_ENV)
            .env_remove(SLEEPER_ENV)
            .output()
            .expect("list tests");
        let listed = String::from_utf8_lossy(&out.stdout);
        assert!(
            listed.contains(name),
            "entry point `{name}` does not resolve to a real test; the --exact filters in this \
             file would silently match nothing. Listed:\n{listed}"
        );
    }
}

/// An MCP server spawned through the production connect path dies when its parent is hard-killed —
/// with no destructor, no `kill_on_drop`, and no rmcp cleanup task able to run.
#[test]
fn mcp_server_dies_when_parent_is_hard_killed() {
    let bin = build_bin("otto-mcp-fs", "mcp-fs");
    let root = tempfile::tempdir().expect("tempdir");

    let (mut holder, kids) = start_holder(
        "mcp-fs",
        bin.to_str().expect("utf-8 bin path"),
        root.path().to_str().expect("utf-8 root"),
    );
    let _reaper = ReapOnDrop::new(&kids);

    assert!(
        kids.iter().any(|(_, comm)| comm.contains("mcp-fs")),
        "fixture broken: holder has no mcp-fs child, so this scenario would pass vacuously. \
         Descendants: {kids:?}"
    );

    assert_all_live_before_kill(&kids);
    holder.hard_kill();

    for (pid, comm) in &kids {
        assert!(
            wait_until_dead(*pid, DEATH_TIMEOUT),
            "{comm} (pid {pid}) survived its parent's SIGKILL — the stdio-EOF teardown this \
             depends on is broken, so closing the desktop window now orphans MCP servers"
        );
    }
}

/// The cascade runs deeper than one level: `mcp-lsp`'s own lazily-spawned language server also dies,
/// even though the intermediate process is itself killed only by EOF (never signalled directly).
///
/// Skipped when `rust-analyzer` is absent — `mcp-lsp` refuses to start with no supported server on
/// PATH, and the repo has no CI, so a developer without it should not see a hard failure.
#[test]
fn language_server_grandchild_dies_when_parent_is_hard_killed() {
    let ra = std::env::var("OTTO_RUST_ANALYZER_BIN").unwrap_or_else(|_| "rust-analyzer".into());
    if which(&ra).is_none() {
        eprintln!("skipping: `{ra}` not on PATH, so mcp-lsp cannot start a language server");
        return;
    }

    let bin = build_bin("otto-mcp-lsp", "mcp-lsp");
    let fixture = rust_fixture();

    let (mut holder, kids) = start_holder(
        "mcp-lsp",
        bin.to_str().expect("utf-8 bin path"),
        fixture.path().to_str().expect("utf-8 root"),
    );
    let _reaper = ReapOnDrop::new(&kids);

    assert!(
        kids.iter().any(|(_, comm)| comm.contains("rust-analyz")),
        "fixture broken: no rust-analyzer grandchild, so the deep cascade is untested. \
         Descendants: {kids:?}"
    );

    assert_all_live_before_kill(&kids);
    holder.hard_kill();

    for (pid, comm) in &kids {
        assert!(
            wait_until_dead(*pid, DEATH_TIMEOUT),
            "{comm} (pid {pid}) survived — the EOF cascade does not reach language servers, so \
             closing the desktop window leaves rust-analyzer running"
        );
    }
}

/// Proves the death assertions above are not vacuous.
///
/// This is the control for the whole file. The scenarios above pass because otto's children happen
/// to watch stdin; if `wait_until_dead` were simply unable to observe survival, they would pass for
/// the wrong reason and keep passing after a real regression. Here a child holds an identical pipe
/// but never reads it — the shape an HTTP/SSE-transport or EOF-ignoring server would have — and the
/// same harness must report it as a **survivor**.
#[test]
fn control_child_ignoring_stdin_eof_survives_hard_kill() {
    // Same harness, same signal as the scenarios above — the only variable is that this child never
    // reads the pipe it holds. `bin`/`root` are unused by the control role.
    let (mut holder, kids) = start_holder("control", "", "");
    let _reaper = ReapOnDrop::new(&kids);
    let sleeper = kids
        .iter()
        .map(|(pid, _)| *pid)
        .next()
        .expect("control holder spawned no child");

    assert_all_live_before_kill(&kids);
    holder.hard_kill();

    // Give it at least as long as a real cascade would have taken to kill it.
    std::thread::sleep(SURVIVAL_WINDOW);
    assert!(
        process_is_live(sleeper),
        "control child died, so this file's death assertions prove nothing: a process that never \
         reads its stdin cannot have been reaped by EOF. Something else in the environment (a \
         process-group kill, a subreaper, a cgroup) is killing subprocesses, and the scenarios \
         above would pass even if the EOF cascade were broken."
    );
}

/// Minimal PATH lookup, so the LSP scenario can skip cleanly rather than fail on a machine with no
/// `rust-analyzer`.
fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(bin);
            candidate.is_file().then_some(candidate)
        })
    })
}
