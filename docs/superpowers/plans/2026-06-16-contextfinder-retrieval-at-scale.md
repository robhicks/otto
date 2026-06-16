# ContextFinder Retrieval-at-Scale Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound the ContextFinder's per-turn read cost — instead of reading every workspace file for lexical scoring, drop non-text files by extension and read contents only for the top-N files by path score.

**Architecture:** Keep the existing hybrid lexical→LLM design and the `score_file`/`Context` seam unchanged. In the lexical phase, path-score every non-binary file for free (no read), sort by path score, read `fs.read` content only for the top `READ_BUDGET` (200), and score the rest path-only. Worst-case reads drop from ~5000×64 KB ≈ 320 MB to ≤ 200×64 KB ≈ 13 MB per turn; small repos (< budget) are unaffected.

**Tech Stack:** Rust (edition 2024), async-trait, anyhow, serde_json, tempfile (dev). Providers: `LocalProvider` + `ScriptedProvider` (tests).

---

## Context for the implementer (read once)

- Single code file: `crates/agents/src/context_finder.rs` (logic + tests). Plus `docs/ARCHITECTURE.md`.
- The ContextFinder is an `Agent`. Its lexical phase currently reads **every** file via `fs.read` (the cost being fixed). The fix changes only *which* files get read; `fn score_file(path, content: Option<&str>, kws)` is reused unchanged — it already produces a path-only score when `content` is `None` and the full `5·path + 1·content` score when content is `Some`.
- `fs.list` (called with `{"glob":"**"}`) returns `{"paths":[..]}`; `fs.read` returns `{"content":"<utf8>"}` and errors on non-UTF8. Tools are reached via `ctx.tools().call(name, json_args)`.
- Determinism is a test invariant: the default offline (`LocalProvider`) path must stay reproducible. Sorting must be total (`score desc, path asc`); a `HashSet` may be used for membership only, never iterated into output.
- Existing test helpers in the `#[cfg(test)] mod tests` block: `seed(ws, path, contents)`, `registry(ws_path)` (registers `fs.list` + `fs.read`), `find(router, ws_path, goal) -> Vec<PathBuf>`. The 6 existing tests use small repos (< budget) so they keep their current behavior.
- Conventions: branch `feat/contextfinder-read-budget`; never detach HEAD; `git add`+`commit` only (no `--amend`); `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt` clean; TDD; **no AI/Claude self-attribution anywhere**.

## Execution setup (before Task 1)

```bash
git checkout main && git checkout -b feat/contextfinder-read-budget
```

---

## File Structure

```
crates/agents/src/context_finder.rs   # MODIFY: READ_BUDGET const + is_skippable() + bounded-read lexical phase; tests
docs/ARCHITECTURE.md                   # MODIFY: note the bounded-read retrieval
```

---

## Task 1: Bounded-read lexical phase

**Files:**
- Modify: `crates/agents/src/context_finder.rs`

- [ ] **Step 1: Add the new tests**

Add these to the existing `#[cfg(test)] mod tests` block in `crates/agents/src/context_finder.rs` (the helpers `seed`, `registry`, `find`, and the imports they need already exist there). They reference `is_skippable` and `READ_BUDGET`, which don't exist yet, so this won't compile until Step 3.

```rust
    #[test]
    fn is_skippable_filters_binaries_and_lockfiles() {
        assert!(is_skippable("assets/logo.png"));
        assert!(is_skippable("Cargo.lock"));
        assert!(is_skippable("dir/package-lock.json"));
        assert!(!is_skippable("src/main.rs"));
        assert!(!is_skippable("Makefile")); // extensionless is kept
        assert!(!is_skippable("README.md"));
    }

    #[tokio::test]
    async fn extension_filter_skips_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        // A binary file whose name AND content match the goal is still filtered out.
        seed(&ws, "login.png", "login login login").await;
        seed(&ws, "login.rs", "fn x() {}").await;
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "login").await;
        assert!(files.contains(&PathBuf::from("login.rs")));
        assert!(
            !files.contains(&PathBuf::from("login.png")),
            "binary files are filtered out before reading: {files:?}"
        );
    }

    #[tokio::test]
    async fn content_only_match_within_budget_is_found() {
        // A small repo: a keyword present only in a file's CONTENTS (no path hit) is still read.
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "helper.rs", "fn do_login() {}").await; // 'login' in content only
        seed(&ws, "unrelated.rs", "fn nothing() {}").await;
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "login").await;
        assert!(
            files.contains(&PathBuf::from("helper.rs")),
            "content-only match is found in a small repo: {files:?}"
        );
        assert!(!files.contains(&PathBuf::from("unrelated.rs")));
    }

    #[tokio::test]
    async fn content_only_match_beyond_read_budget_is_missed() {
        // Fill the read budget with noise (no path or content hit), then add a content-only match
        // that sorts last (beyond the budget) and a path-hit file (always ranked).
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        for i in 0..READ_BUDGET {
            seed(&ws, &format!("noise/f{i:04}.txt"), "nothing relevant here").await;
        }
        seed(&ws, "zzz_only.txt", "login logic lives here").await; // content-only, sorts last
        seed(&ws, "login_handler.rs", "nothing relevant here").await; // path hit

        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "login").await;
        assert!(
            files.contains(&PathBuf::from("login_handler.rs")),
            "a path-hit file is found regardless of budget: {files:?}"
        );
        assert!(
            !files.contains(&PathBuf::from("zzz_only.txt")),
            "a content-only match beyond the read budget is missed: {files:?}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail (don't compile yet)**

Run: `cargo test -p otto-agents context_finder::`
Expected: FAIL to compile — `is_skippable` and `READ_BUDGET` are not defined.

- [ ] **Step 3: Implement the budget constant, the filter, and the bounded-read phase**

(a) Add the `READ_BUDGET` constant next to the other consts near the top of the file (after `CONTENT_SCAN_CHARS`):

```rust
/// Maximum files whose contents are read per turn; the rest are scored on their path only. This
/// bounds per-turn read cost on large repos — small repos (fewer text files than this) read all.
const READ_BUDGET: usize = 200;
```

(b) Add the `is_skippable` free function just above `fn score_file` (so it sits with the other lexical helpers):

```rust
/// Whether a path is a binary/non-text file to skip before reading — by extension or by a known
/// lockfile name. Keeps the read budget for source files. Extensionless files (e.g. `Makefile`,
/// scripts) are kept.
fn is_skippable(path: &str) -> bool {
    const SKIP_EXTS: &[&str] = &[
        // images / media
        "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "tiff", "mp3", "mp4", "mov", "avi",
        "wav", "ogg", "flac", "webm",
        // archives
        "zip", "gz", "tgz", "tar", "xz", "zst", "bz2", "7z", "rar",
        // binaries / objects
        "exe", "dll", "so", "dylib", "o", "a", "bin", "wasm", "class", "pyc", "pyo", "obj",
        // docs / fonts
        "pdf", "ttf", "otf", "woff", "woff2",
    ];
    const SKIP_NAMES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "Pipfile.lock",
    ];
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if SKIP_NAMES.contains(&name) {
        return true;
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => SKIP_EXTS.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
}
```

(c) In `impl Agent for ContextFinder`'s `run`, replace the current lexical-scoring block. The block to replace is everything from `let kws = keywords(&goal);` through `scored.truncate(CANDIDATE_LIMIT);` — i.e. this existing code:

```rust
        let kws = keywords(&goal);

        // Lexical scoring. Read each file via fs.read; a non-UTF8/unreadable file scores on its
        // path only.
        let mut scored: Vec<(String, u64)> = Vec::new();
        for path in &files {
            let content = match ctx.tools().call("fs.read", json!({ "path": path })).await {
                Ok(Value::Object(map)) => map
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            };
            let score = score_file(path, content.as_deref(), &kws);
            if score > 0 {
                scored.push((path.clone(), score));
            }
        }
        // Rank by score desc, tie-broken by path asc (deterministic), keep the top candidates.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(CANDIDATE_LIMIT);
```

Replace it with:

```rust
        let kws = keywords(&goal);

        // Path-only score every non-binary file (free, no read), dropping binary/non-text files
        // so they neither consume read budget nor appear as context.
        let mut by_path: Vec<(String, u64)> = files
            .into_iter()
            .filter(|p| !is_skippable(p))
            .map(|p| {
                let path_score = score_file(&p, None, &kws);
                (p, path_score)
            })
            .collect();
        // Read content only for the most path-relevant files, bounding per-turn read cost. Sort
        // by path score desc, path asc (deterministic) and take the top READ_BUDGET to read.
        by_path.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let read_set: HashSet<String> =
            by_path.iter().take(READ_BUDGET).map(|(p, _)| p.clone()).collect();

        // Final scoring: read content for the budgeted set (full 5·path + 1·content score),
        // path-only for the rest.
        let mut scored: Vec<(String, u64)> = Vec::new();
        for (path, path_score) in &by_path {
            let score = if read_set.contains(path) {
                let content = match ctx.tools().call("fs.read", json!({ "path": path })).await {
                    Ok(Value::Object(map)) => map
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                };
                score_file(path, content.as_deref(), &kws)
            } else {
                *path_score
            };
            if score > 0 {
                scored.push((path.clone(), score));
            }
        }
        // Rank by score desc, tie-broken by path asc (deterministic), keep the top candidates.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(CANDIDATE_LIMIT);
```

Leave everything after `scored.truncate(CANDIDATE_LIMIT);` (the `if scored.is_empty()` early return, `lexical_top`, the LLM-select stage) exactly as it is. `score_file`, `keywords`, `select_prompt`, and the consts above are unchanged except for the new `READ_BUDGET`.

NOTE: `files` is consumed by `.into_iter()` now (it was iterated by reference before); it is not used afterward, so this is fine.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-agents context_finder::`
Expected: PASS — the 4 new tests (`is_skippable_filters_binaries_and_lockfiles`, `extension_filter_skips_binaries`, `content_only_match_within_budget_is_found`, `content_only_match_beyond_read_budget_is_missed`) plus the 6 existing ones (they use small repos, so all files are still read — identical behavior).

Also run `cargo test -p otto-agents` to confirm nothing else broke.

- [ ] **Step 5: Lint, format, commit**

Run: `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean) and `cargo fmt -p otto-agents`.

```bash
git add crates/agents/src/context_finder.rs
git commit -m "feat(agents): bound ContextFinder reads (ext filter + top-N path-ranked read budget)"
```

---

## Task 2: Docs + final quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the bounded-read retrieval**

In `docs/ARCHITECTURE.md`, find the `### \`Agent\`` subsection paragraph describing the `ContextFinder`'s lexical prefilter (it currently says it scores files by keyword matches and keeps the top candidates). Append (or weave in) a sentence describing the bound:

```markdown
To stay bounded on large repositories, the lexical phase path-scores every file for free, drops
non-text files by extension, and reads file contents only for the top ~200 files by path score
(the rest are scored on their path alone) — so a small repo still reads everything, while a
huge one reads a bounded subset. A file relevant only by content and ranked beyond that budget
may be missed; path-named relevance (weighted higher) is always read.
```

Reconcile any neighboring wording that implies the ContextFinder reads every file. Do not make unrelated edits.

- [ ] **Step 2: Final gate**

Run and capture output:
- `cargo fmt --all -- --check` (clean)
- `cargo clippy --workspace --all-targets -- -D warnings` (clean)
- `cargo test --workspace` — capture the per-crate `test result:` lines + summed total.

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: note the ContextFinder bounded-read retrieval"
```

---

## Done — what this delivers

The ContextFinder no longer reads the whole workspace each turn: it path-scores everything for
free, skips binaries, and reads contents for only the top ~200 path-ranked files — bounding
per-turn read cost (~320 MB → ~13 MB worst case on a 5000-file repo) while leaving small repos
unchanged and always reading the strongest (path-named) candidates.

**Carried forward / deferred:**
- A file relevant only by content, ranked beyond the read budget, is missed (the accepted
  cost/fidelity tradeoff).
- No size-based skipping (would need a tool seam change — `fs.read` `max_bytes` or `fs.list`
  sizes); a single pathologically large text file within the budget is still read whole (then
  scanned to `CONTENT_SCAN_CHARS`).
- No persistent inverted index (needs the future persistence crate).
- `READ_BUDGET` and the extension denylist are constants, not yet configurable.
```
