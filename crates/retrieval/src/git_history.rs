//! Git-history recency signal for ranking. Shells to a bounded, read-only `git log` to learn how
//! recently each file was last touched (by commit rank, HEAD = 0), then maps that rank to a small
//! bounded boost. Query-independent — the search goal is never passed to git, so there is no
//! agent-input argv-injection surface. Returns an empty map off-git (non-repo, `git` absent, or an
//! empty log), making the signal a graceful no-op that leaves prior-slice scoring unchanged.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Recent commits scanned per search. Bounds cost to O(window · files-per-commit) and defines the
/// recency tiers below.
const WINDOW: usize = 200;

/// Map a file's most-recent-commit rank (0 = HEAD) to a bounded recency boost. Tiers are small so a
/// recency signal re-ranks among already-relevant files without dominating a symbol-name hit (8) or
/// a path hit (5).
fn tier(rank: usize) -> u64 {
    match rank {
        0..=4 => 4,
        5..=19 => 3,
        20..=49 => 2,
        _ => 1,
    }
}

/// Parse the NUL-delimited `git log --name-only` stream into a path -> boost map. A line equal to a
/// single NUL byte starts the next commit (incrementing the rank); any other non-empty line is a
/// changed path, recorded at the current rank iff not already seen (first occurrence = most recent
/// commit = smallest rank wins).
fn parse_log(stdout: &str) -> HashMap<String, u64> {
    let mut boosts: HashMap<String, u64> = HashMap::new();
    let mut rank: usize = 0;
    let mut started = false;
    for line in stdout.lines() {
        if line == "\u{0}" {
            if started {
                rank += 1;
            }
            started = true;
            continue;
        }
        if line.is_empty() {
            continue;
        }
        boosts.entry(line.to_string()).or_insert_with(|| tier(rank));
    }
    boosts
}

/// Relative-path-string -> recency boost for every file touched within the recent window. Empty
/// when `root` is not a git repository, `git` is unavailable, or the log is empty.
/// Keys are git's forward-slash paths; they string-match the retriever's walk-derived path keys on
/// the Unix targets otto supports (on Windows the OS-separated walk keys would differ, making the
/// boost a silent no-op for nested paths — acceptable, but noted to prevent a future regression).
pub fn recency_boosts(root: &Path) -> HashMap<String, u64> {
    // Fixed argv, rooted via `-C`: the search goal is never passed, so there is no agent-input
    // argv-injection surface. `core.quotePath=false` keeps non-ASCII paths un-escaped so they
    // string-match the walk's plain relative paths. `--pretty=format:%x00` prints a single NUL as
    // each commit's header line — an unambiguous boundary (paths never contain NUL).
    let n = WINDOW.to_string();
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.quotePath=false",
            "log",
            "-n",
            &n,
            "--name-only",
            "--pretty=format:%x00",
        ])
        .output();
    match output {
        Ok(o) if o.status.success() => parse_log(&String::from_utf8_lossy(&o.stdout)),
        _ => HashMap::new(), // non-repo / git absent / git error: graceful no-op
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `git init` + isolated local identity (mirrors the mcp-git test helper).
    fn init_repo(root: &Path) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "commit.gpgsign", "false"]);
    }

    fn commit_file(root: &Path, rel: &str, body: &str, msg: &str) {
        std::fs::write(root.join(rel), body).unwrap();
        git(root, &["add", rel]);
        git(root, &["commit", "-q", "-m", msg]);
    }

    #[test]
    fn tier_boundaries() {
        assert_eq!(tier(0), 4);
        assert_eq!(tier(4), 4);
        assert_eq!(tier(5), 3);
        assert_eq!(tier(19), 3);
        assert_eq!(tier(20), 2);
        assert_eq!(tier(49), 2);
        assert_eq!(tier(50), 1);
        assert_eq!(tier(10_000), 1);
    }

    #[test]
    fn parse_log_first_seen_rank_and_tiers() {
        // rank 0: a.txt, re.txt ; ranks 1..=6: filler.txt ; rank 7: b.txt, re.txt
        let mut s = String::new();
        s.push_str("\u{0}\na.txt\nre.txt\n");
        for _ in 0..6 {
            s.push_str("\u{0}\nfiller.txt\n");
        }
        s.push_str("\u{0}\nb.txt\nre.txt\n");

        let b = parse_log(&s);
        assert_eq!(b.get("a.txt"), Some(&4)); // rank 0
        assert_eq!(b.get("b.txt"), Some(&3)); // rank 7 -> tier 3
        assert_eq!(b.get("re.txt"), Some(&4)); // first seen at rank 0; the rank-7 sighting is ignored
        assert!(!b.contains_key("never.txt"));
    }

    #[test]
    fn recency_boosts_recent_present_unrelated_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        commit_file(root, "old.txt", "a", "c1");
        commit_file(root, "new.txt", "b", "c2");

        let b = recency_boosts(root);
        assert_eq!(b.get("new.txt"), Some(&4)); // most recent commit, rank 0
        assert!(b.contains_key("old.txt")); // touched within the window
        assert!(!b.contains_key("absent.txt"));
    }

    #[test]
    fn recency_boosts_most_recent_touch_wins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        commit_file(root, "f.txt", "v1", "first"); // oldest touch of f.txt
        for i in 0..6 {
            commit_file(root, &format!("filler{i}.txt"), "x", "filler");
        }
        commit_file(root, "f.txt", "v2", "retouch"); // newest commit touches f.txt again

        let b = recency_boosts(root);
        // f.txt's *most recent* commit is rank 0 even though it first appeared 7 commits ago.
        assert_eq!(b.get("f.txt"), Some(&4));
        // filler0 was committed at rank 6 (counting back from HEAD) -> tier 3.
        assert_eq!(b.get("filler0.txt"), Some(&3));
    }

    #[test]
    fn recency_boosts_non_repo_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(recency_boosts(dir.path()).is_empty());
    }
}
