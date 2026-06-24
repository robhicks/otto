# Git-History Recency Boost Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a bounded, per-search `git log`-derived recency boost to `IndexedRetriever` scoring so that, among files that already match the goal, recently-changed ones rank higher — a recall-safe precision re-ranker.

**Architecture:** A new self-contained `git_history` module shells to a read-only, query-independent `git log -n 200 --name-only` and maps each file's most-recent-commit rank (HEAD = 0) to a bounded tier boost (+1..+4). `IndexedRetriever::search` fetches the boost map (via `spawn_blocking`) and adds the boost *inside* the existing `base > 0` candidate guard, so only already-matching files are affected and the no-recall-regression invariant holds.

**Tech Stack:** Rust (edition 2024), `std::process::Command` for git, `tokio::task::spawn_blocking`, `sqlx`/sqlite (unchanged), `tempfile` + real `git` for tests (already a test-time dependency of this workspace, per `crates/mcp-git`).

**Design spec:** `docs/superpowers/specs/2026-06-24-retrieval-git-history-boost-design.md`

---

## File Structure

- **Create:** `crates/retrieval/src/git_history.rs` — the only module that shells to git. Exposes `recency_boosts(root) -> HashMap<String, u64>`; private `tier()` and `parse_log()` helpers. Self-contained, query-independent.
- **Modify:** `crates/retrieval/src/lib.rs` — register `mod git_history;`.
- **Modify:** `crates/retrieval/src/retriever.rs` — fetch the boost map and add it inside the `base > 0` guard in `IndexedRetriever::search`; add git-backed ranking tests.

No `Cargo.toml` change: the workspace `tokio` already enables `rt-multi-thread` (so `spawn_blocking` is available in the lib build), and `std::process::Command` needs no dependency. No `engine-core`, `agents`, or `engine` change; no schema change / `FORMAT_VERSION` bump.

---

## Task 1: The `git_history` reader module

**Files:**
- Create: `crates/retrieval/src/git_history.rs`
- Modify: `crates/retrieval/src/lib.rs` (add `mod git_history;`)
- Test: `crates/retrieval/src/git_history.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Create the module skeleton with the real `tier`/`parse_log` and a stub `recency_boosts`, and register it**

Create `crates/retrieval/src/git_history.rs`:

```rust
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
pub fn recency_boosts(_root: &Path) -> HashMap<String, u64> {
    HashMap::new() // stub — replaced in Step 3
}
```

Add to `crates/retrieval/src/lib.rs`, alongside the existing `mod` lines (keep alphabetical with the others):

```rust
mod chunk;
mod git_history;
mod index;
mod retriever;
mod tokenize;
mod walk;
```

- [ ] **Step 2: Write the failing tests**

Append to `crates/retrieval/src/git_history.rs`:

```rust
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
        assert!(b.get("never.txt").is_none());
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
        assert!(b.get("absent.txt").is_none());
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
```

- [ ] **Step 3: Run the tests to verify the git-backed ones fail**

Run: `cargo test -p otto-retrieval git_history -- --nocapture`
Expected: `tier_boundaries` and `parse_log_first_seen_rank_and_tiers` PASS; `recency_boosts_recent_present_unrelated_absent` and `recency_boosts_most_recent_touch_wins` FAIL (stub returns an empty map, so the `Some(&4)` assertions fail); `recency_boosts_non_repo_is_empty` PASS (stub is already empty).

- [ ] **Step 4: Implement `recency_boosts`**

Replace the stub `recency_boosts` in `crates/retrieval/src/git_history.rs` with:

```rust
/// Relative-path-string -> recency boost for every file touched within the recent window. Empty
/// when `root` is not a git repository, `git` is unavailable, or the log is empty.
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p otto-retrieval git_history`
Expected: all five tests PASS.

- [ ] **Step 6: Format, lint, commit**

Run:
```bash
cargo fmt --all
cargo clippy -p otto-retrieval --all-targets
```
Expected: clean (no warnings introduced).

```bash
git add crates/retrieval/src/git_history.rs crates/retrieval/src/lib.rs
git commit -m "feat(retrieval): git-history recency-boost reader (git_history module)"
```

---

## Task 2: Apply the recency boost in `IndexedRetriever` ranking

**Files:**
- Modify: `crates/retrieval/src/retriever.rs` (the `search` method + the module doc comment)
- Test: `crates/retrieval/src/retriever.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add these three tests inside the existing `#[cfg(test)] mod tests` block at the bottom of `crates/retrieval/src/retriever.rs` (the module already has `use super::*;`, `use std::io::Write;`, `use std::path::Path;`, and a `seed` helper). Add the git helpers once and the three tests:

```rust
    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
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

    fn init_repo(root: &Path) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "commit.gpgsign", "false"]);
    }

    fn commit_path(root: &Path, rel: &str, msg: &str) {
        git(root, &["add", rel]);
        git(root, &["commit", "-q", "-m", msg]);
    }

    #[tokio::test]
    async fn recent_file_outranks_equally_scored_older() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap(); // keep the index DB out of the git repo
        init_repo(root);

        // Both files have an identical base score: a single content hit on "login", no path hit.
        seed(root, "older.rs", b"login");
        commit_path(root, "older.rs", "older"); // oldest commit
        for i in 0..6 {
            let rel = format!("filler{i}.txt");
            seed(root, &rel, b"filler");
            commit_path(root, &rel, "filler");
        }
        seed(root, "newer.rs", b"login");
        commit_path(root, "newer.rs", "newer"); // newest commit -> rank 0

        let r = IndexedRetriever::open(root.to_path_buf(), dbdir.path().join("idx.sqlite"))
            .await
            .unwrap();
        let cands = r.search("login", 8).await.unwrap();
        // Equal base (content=1 each); newer.rs gets tier 4 (rank 0), older.rs tier 3 (rank 7).
        assert_eq!(
            cands.first().map(|c| c.path.clone()),
            Some(PathBuf::from("newer.rs")),
            "recent file should win the tie: {cands:?}"
        );
    }

    #[tokio::test]
    async fn recent_but_unmatched_file_is_not_surfaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        init_repo(root);

        seed(root, "match.rs", b"login");
        commit_path(root, "match.rs", "match"); // older, but matches "login"
        seed(root, "recent.rs", b"totally unrelated content");
        commit_path(root, "recent.rs", "recent"); // newest, but no "login" anywhere

        let r = IndexedRetriever::open(root.to_path_buf(), dbdir.path().join("idx.sqlite"))
            .await
            .unwrap();
        let files: Vec<_> = r
            .search("login", 8)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert!(files.contains(&PathBuf::from("match.rs")));
        assert!(
            !files.contains(&PathBuf::from("recent.rs")),
            "git recency must not surface an unmatched file: {files:?}"
        );
    }

    #[tokio::test]
    async fn committed_sensitive_file_never_appears() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        init_repo(root);

        // .env is committed to git (so it's in `git log`) but excluded by the walk — the boost is a
        // lookup keyed on walked entries, so it can never be surfaced.
        seed(root, ".env", b"login=secret");
        git(root, &["add", "-f", ".env"]); // -f: .env would otherwise need no ignore, but be explicit
        git(root, &["commit", "-q", "-m", "secret"]);
        seed(root, "real.rs", b"login");
        commit_path(root, "real.rs", "real");

        let r = IndexedRetriever::open(root.to_path_buf(), dbdir.path().join("idx.sqlite"))
            .await
            .unwrap();
        let files: Vec<_> = r
            .search("login", 8)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert!(
            !files.iter().any(|p| p.to_string_lossy().contains(".env")),
            "sensitive file must never appear even when committed: {files:?}"
        );
        assert!(files.contains(&PathBuf::from("real.rs")));
    }
```

- [ ] **Step 2: Run the tests to verify the tie-break test fails**

Run: `cargo test -p otto-retrieval retriever::tests::recent_file_outranks_equally_scored_older`
Expected: FAIL — without the boost, `older.rs` and `newer.rs` tie on score (both base 1) and the deterministic `path asc` tiebreak orders `newer.rs` after `older.rs`, so `cands.first()` is `older.rs`, not `newer.rs`.

(`recent_but_unmatched_file_is_not_surfaced` and `committed_sensitive_file_never_appears` already pass against the current code — they assert behavior the boost must preserve. Confirm they pass now so a regression in Step 3 is caught.)

Run: `cargo test -p otto-retrieval retriever::tests::recent_but_unmatched_file_is_not_surfaced retriever::tests::committed_sensitive_file_never_appears`
Expected: both PASS.

- [ ] **Step 3: Apply the boost inside the `base > 0` guard**

In `crates/retrieval/src/retriever.rs`, in `IndexedRetriever::search`, after the `let mut matched = self.index.matched_symbols(...)` line and before the `let mut scored ...` block, add the boost fetch:

```rust
        // Git-history recency boost (empty off-git). Run the bounded `git log` off the async
        // executor; a join failure degrades to no boost (graceful no-op).
        let root = self.root.clone();
        let git_boost = tokio::task::spawn_blocking(move || crate::git_history::recency_boosts(&root))
            .await
            .unwrap_or_default();
```

Then change the scoring closure so the boost is added **inside** the `base > 0` guard. Replace this existing block:

```rust
                let key = e.path.to_string_lossy().into_owned();
                let score = 5 * path_hits
                    + content.get(&key).copied().unwrap_or(0)
                    + 8 * name_hits.get(&key).copied().unwrap_or(0);
                (score > 0).then(|| {
                    let symbols = matched.remove(&key).unwrap_or_default();
                    Candidate {
                        path: e.path,
                        score,
                        symbols,
                    }
                })
```

with:

```rust
                let key = e.path.to_string_lossy().into_owned();
                let base = 5 * path_hits
                    + content.get(&key).copied().unwrap_or(0)
                    + 8 * name_hits.get(&key).copied().unwrap_or(0);
                // Recency is a precision re-ranker, not a recall source: the boost is added ONLY for
                // files that already matched (base > 0), so a recent-but-unmatched file is never
                // constructed and the no-recall-regression invariant holds (boost >= 0).
                (base > 0).then(|| {
                    let score = base + git_boost.get(&key).copied().unwrap_or(0);
                    let symbols = matched.remove(&key).unwrap_or_default();
                    Candidate {
                        path: e.path,
                        score,
                        symbols,
                    }
                })
```

Update the module doc comment at the top of `crates/retrieval/src/retriever.rs` to mention the new term. Replace the first sentence:

```rust
//! `IndexedRetriever`: the `Retriever` impl backed by the persistent inverted index. On each
//! `search` it refreshes the index (stat-incremental), then ranks every walked file by
//! `5*path_hits + content_score + 8*symbol_name_hits`. The PATH weighting (5×) matches the
```

with:

```rust
//! `IndexedRetriever`: the `Retriever` impl backed by the persistent inverted index. On each
//! `search` it refreshes the index (stat-incremental), then ranks every walked file by
//! `5*path_hits + content_score + 8*symbol_name_hits`, plus a bounded git-history recency boost
//! added only to files that already match (a precision re-ranker, never a recall source). The PATH
//! weighting (5×) matches the
```

- [ ] **Step 4: Run the new and existing tests to verify all pass**

Run: `cargo test -p otto-retrieval`
Expected: all tests PASS — the new tie-break test now puts `newer.rs` first; `recent_but_unmatched_file_is_not_surfaced` and `committed_sensitive_file_never_appears` still pass; every prior retriever/index/walk/chunk/tokenize test (which runs in non-git tempdirs, so the boost map is empty) is unchanged and green.

- [ ] **Step 5: Run the full workspace suite + lint to confirm no regression**

Run:
```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```
Expected: workspace tests green (engine-core offline determinism suite untouched — it runs the retriever-free path); clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/retrieval/src/retriever.rs
git commit -m "feat(retrieval): apply git recency boost in IndexedRetriever ranking"
```

---

## Spec coverage check

- Recency-rank tiers, bounded window (200), commit-rank semantics → Task 1 (`tier`, `parse_log`, `recency_boosts`).
- Compute per-search, no schema/`FORMAT_VERSION` change → Task 2 (boost fetched in `search`; no `index.rs` change).
- Re-ranker only / no-recall-regression (boost inside `base > 0`) → Task 2 Step 3 + `recent_but_unmatched_file_is_not_surfaced`.
- Query-independent / no injection surface (fixed argv, `-C`, goal never passed) → Task 1 Step 4.
- Graceful no-op off-git → Task 1 `recency_boosts_non_repo_is_empty` + the `_ => HashMap::new()` arm; existing non-git tests stay green.
- Sensitive paths can't leak → Task 2 `committed_sensitive_file_never_appears`.
- Deterministic by commit order (no fixed dates) → all git tests commit in a fixed order.
- Off the async executor → `spawn_blocking` in Task 2 Step 3.
```
