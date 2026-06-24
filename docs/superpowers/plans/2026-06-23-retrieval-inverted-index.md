# Persistent Inverted Index (retrieval crate + Retriever seam) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Open the retrieval axis by extracting a `retrieval` crate behind a `Retriever` seam and shipping its first impl — a persistent, stat-incremental sqlite inverted index that scores content for every indexed file (no read budget) and amortizes reads across turns/process invocations.

**Architecture:** A `Retriever` trait + `Candidate` type land in `engine-core`. A new `crates/retrieval` implements `IndexedRetriever` over a standalone sqlite DB in the OS cache dir, keyed by canonical workspace root. `AgentCtx` gains an optional `retriever`; `ContextFinder` uses it to produce candidates (falling back to today's inline lexical pipeline when absent), then runs the existing LLM rank/select. The engine builds and threads the retriever for `otto run` and `otto serve`. Fail-soft: any index failure logs a warning and degrades to the lexical path. Determinism is preserved (the engine-core offline suite runs the retriever-free path).

**Tech Stack:** Rust (edition 2024), `sqlx` 0.8 (sqlite, runtime-tokio), `async-trait`, `tokio`, `dirs`, `tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-06-23-retrieval-inverted-index-design.md`

---

## File structure

- `crates/engine-core/src/retrieval.rs` (Create) — `Candidate` type + `Retriever` trait.
- `crates/engine-core/src/lib.rs` (Modify) — declare module, re-export.
- `crates/engine-core/src/traits.rs` (Modify) — `AgentCtx` optional retriever field + builder + accessor.
- `crates/engine-core/src/orchestrator.rs` (Modify) — `Orchestrator.retriever` field; build `AgentCtx` with it; update test literals.
- `crates/retrieval/Cargo.toml` (Create), `crates/retrieval/src/lib.rs` (Create) — crate root + re-exports.
- `crates/retrieval/src/tokenize.rs` (Create) — `index_tokens`, `query_terms`.
- `crates/retrieval/src/walk.rs` (Create) — workspace walk + exclusions.
- `crates/retrieval/src/index.rs` (Create) — sqlite `Index`: open/refresh/content_scores.
- `crates/retrieval/src/retriever.rs` (Create) — `IndexedRetriever` impl of `Retriever`.
- `Cargo.toml` (Modify) — add `crates/retrieval` to workspace members.
- `crates/agents/src/context_finder.rs` (Modify) — use `ctx.retriever()` when present.
- `crates/engine/src/service.rs` (Modify) — `EngineService` retriever field + builder; thread into `Orchestrator`.
- `crates/engine/src/lib.rs` (Modify) — `build_retriever` helper; `run_goal` retriever param.
- `crates/engine/src/main.rs` (Modify) — wire retriever into `cmd_run` and `serve`.
- `crates/engine/Cargo.toml` (Modify) — add `dirs` + `otto-retrieval` deps.
- `CLAUDE.md`, `docs/ARCHITECTURE.md` (Modify) — record the shipped crate.

---

### Task 1: `Retriever` seam + `Candidate` in engine-core

**Files:**
- Create: `crates/engine-core/src/retrieval.rs`
- Modify: `crates/engine-core/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to a new file `crates/engine-core/src/retrieval.rs`:

```rust
//! The retrieval seam: a `Retriever` produces ranked file candidates for a goal. The
//! orchestrator holds only the trait object; concrete impls (e.g. the indexed retriever) live
//! in the `retrieval` crate. File-level candidates keep the Coder's input shape unchanged.

use std::path::PathBuf;

/// A scored candidate file for a goal. Higher `score` is more relevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub score: u64,
}

#[async_trait::async_trait]
pub trait Retriever: Send + Sync {
    /// Ranked candidates for `goal`, best first, already capped at `limit`.
    async fn search(&self, goal: &str, limit: usize) -> anyhow::Result<Vec<Candidate>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_holds_path_and_score() {
        let c = Candidate { path: PathBuf::from("src/main.rs"), score: 7 };
        assert_eq!(c.path, PathBuf::from("src/main.rs"));
        assert_eq!(c.score, 7);
    }
}
```

- [ ] **Step 2: Wire the module and run the test to verify it fails to compile (module not declared)**

Add to `crates/engine-core/src/lib.rs` after `pub mod registry;`:

```rust
pub mod retrieval;
```

And add to the re-export block (after the `pub use registry::AgentRegistry;` line):

```rust
pub use retrieval::{Candidate, Retriever};
```

Run: `cargo test -p otto-engine-core retrieval::`
Expected: PASS (the module now compiles and the unit test passes).

- [ ] **Step 3: Commit**

```bash
git add crates/engine-core/src/retrieval.rs crates/engine-core/src/lib.rs
git commit -m "feat(engine-core): add Retriever seam + Candidate type"
```

---

### Task 2: Scaffold the `retrieval` crate

**Files:**
- Create: `crates/retrieval/Cargo.toml`
- Create: `crates/retrieval/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Create the crate manifest**

Create `crates/retrieval/Cargo.toml`:

```toml
[package]
name = "otto-retrieval"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-engine-core = { path = "../engine-core" }
async-trait.workspace = true
anyhow.workspace = true
tokio = { workspace = true }
sqlx = { version = "0.8", default-features = false, features = ["runtime-tokio", "sqlite"] }

[dev-dependencies]
tempfile.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "fs"] }
```

- [ ] **Step 2: Create the crate root with module declarations**

Create `crates/retrieval/src/lib.rs`:

```rust
//! otto retrieval: a persistent, stat-incremental inverted index behind the `Retriever` seam.
//! The index lives in a standalone sqlite DB (owned here, separate from the session store) and
//! scores content for every indexed file, removing the ContextFinder's per-turn read budget.

mod index;
mod retriever;
mod tokenize;
mod walk;

pub use retriever::IndexedRetriever;
```

Note: `index`, `retriever`, `tokenize`, `walk` are created in later tasks. To compile this task standalone, create empty placeholder files now:

```bash
mkdir -p crates/retrieval/src
: > crates/retrieval/src/index.rs
: > crates/retrieval/src/retriever.rs
: > crates/retrieval/src/tokenize.rs
: > crates/retrieval/src/walk.rs
```

Then temporarily comment out the `pub use` and the `mod` lines for files that are still empty by replacing `lib.rs` body with just the module docs and:

```rust
mod tokenize;
```

(We'll add the other modules as their tasks land. Keep `lib.rs` referencing only modules that have content.)

- [ ] **Step 3: Add the crate to the workspace**

In the root `Cargo.toml`, add to `members` (after `"crates/persistence",`):

```toml
    "crates/retrieval",
```

- [ ] **Step 4: Verify it builds**

Put a minimal item in `crates/retrieval/src/tokenize.rs` so the crate is non-empty:

```rust
// tokenization helpers land in Task 3
```

Run: `cargo build -p otto-retrieval`
Expected: builds clean (a crate with one empty module).

- [ ] **Step 5: Commit**

```bash
git add crates/retrieval/Cargo.toml crates/retrieval/src/lib.rs crates/retrieval/src/tokenize.rs Cargo.toml
git commit -m "chore(retrieval): scaffold crate + add to workspace"
```

---

### Task 3: Tokenization (index + query parity)

**Files:**
- Modify: `crates/retrieval/src/tokenize.rs`
- Test: same file (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Replace `crates/retrieval/src/tokenize.rs` with:

```rust
//! Tokenization for the index and the query, kept in lock-step so indexed tokens and goal
//! keywords align. Rules mirror `ContextFinder::keywords`: split on non-alphanumeric, lowercase,
//! keep tokens of length >= 3. The query side additionally drops stopwords and de-duplicates.

use std::collections::HashMap;
use std::collections::HashSet;

/// Per-file content scanned for indexing (chars). Mirrors the ContextFinder content-scan cap.
pub const CONTENT_SCAN_CHARS: usize = 65_536;

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "add", "fix", "make", "use", "into", "from",
    "you",
];

/// Index tokens: token -> occurrence count over the first `CONTENT_SCAN_CHARS` chars. No
/// stopword filtering (the query never asks for stopwords, so they cost nothing in the index).
pub fn index_tokens(content: &str) -> HashMap<String, i64> {
    let mut counts: HashMap<String, i64> = HashMap::new();
    for tok in content.chars().take(CONTENT_SCAN_CHARS).collect::<String>()
        .split(|c: char| !c.is_alphanumeric())
    {
        let t = tok.to_lowercase();
        if t.len() >= 3 {
            *counts.entry(t).or_insert(0) += 1;
        }
    }
    counts
}

/// Query terms: lowercased alphanumeric tokens of length >= 3, minus stopwords, de-duplicated.
/// Matches `ContextFinder::keywords` exactly so query terms hit the same indexed tokens.
pub fn query_terms(goal: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tok in goal.split(|c: char| !c.is_alphanumeric()) {
        let t = tok.to_lowercase();
        if t.len() >= 3 && !STOPWORDS.contains(&t.as_str()) && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_tokens_counts_and_filters_short() {
        let m = index_tokens("login Login io fn login_handler");
        assert_eq!(m.get("login"), Some(&2)); // 'login' twice (the split drops the '_')
        assert_eq!(m.get("handler"), Some(&1));
        assert!(!m.contains_key("io")); // length < 3 dropped
        assert!(!m.contains_key("fn")); // length < 3 dropped
    }

    #[test]
    fn query_terms_drops_stopwords_and_dedupes() {
        let t = query_terms("Fix the login Login flow at io");
        assert!(t.contains(&"login".to_string()));
        assert!(t.contains(&"flow".to_string()));
        assert!(!t.contains(&"fix".to_string())); // stopword
        assert!(!t.contains(&"the".to_string())); // stopword
        assert!(!t.contains(&"io".to_string())); // length < 3
        assert_eq!(t.iter().filter(|k| *k == "login").count(), 1); // deduped
    }
}
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test -p otto-retrieval tokenize::`
Expected: PASS (2 tests).

- [ ] **Step 3: Commit**

```bash
git add crates/retrieval/src/tokenize.rs
git commit -m "feat(retrieval): tokenization with index/query parity"
```

---

### Task 4: Workspace walk + exclusions

**Files:**
- Create/replace: `crates/retrieval/src/walk.rs`
- Modify: `crates/retrieval/src/lib.rs` (add `mod walk;`)

- [ ] **Step 1: Write the failing test**

Write `crates/retrieval/src/walk.rs`:

```rust
//! Recursive workspace walk for indexing. Mirrors the `fs.list` recursive walk and the
//! permission gate's sensitive-path floor: skips `.git`/`target`/`node_modules` and ANY
//! dot-prefixed component (covers `.env`/`.ssh`/`.aws`), skips binary/lockfile names, does not
//! follow symlinks, caps enumeration, and skips oversized files. Returns relative paths with
//! their stat (mtime nanos, size) for stat-based staleness.

use std::path::{Path, PathBuf};

/// Max files enumerated per walk (bounds cost on huge trees).
pub const ENUMERATE_CAP: usize = 5000;
/// Files larger than this are skipped during indexing (closes the size-skipping gap).
pub const MAX_FILE_BYTES: u64 = 1_048_576; // 1 MiB

/// One enumerated file: workspace-relative path + stat used for staleness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
    pub path: PathBuf, // relative to root
    pub mtime_ns: i64,
    pub size: i64,
}

/// True if a path component should be pruned from the walk.
fn excluded_component(name: &str) -> bool {
    name == ".git"
        || name == "target"
        || name == "node_modules"
        || name.starts_with('.') // .env / .ssh / .aws and any dotfile/dir
}

/// True if a leaf file name is a binary/lockfile to skip (mirrors ContextFinder::is_skippable).
fn skippable_file(name: &str) -> bool {
    const SKIP_EXTS: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "tiff", "mp3", "mp4", "mov", "avi",
        "wav", "ogg", "flac", "webm", "zip", "gz", "tgz", "tar", "xz", "zst", "bz2", "7z", "rar",
        "exe", "dll", "so", "dylib", "o", "a", "bin", "wasm", "class", "pyc", "pyo", "obj", "pdf",
        "ttf", "otf", "woff", "woff2",
    ];
    const SKIP_NAMES: &[&str] = &[
        "Cargo.lock", "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "poetry.lock",
        "Pipfile.lock",
    ];
    if SKIP_NAMES.contains(&name) {
        return true;
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => SKIP_EXTS.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
}

/// Walk `root` recursively, returning indexable entries (sorted by path for determinism).
pub fn walk(root: &Path) -> Vec<WalkEntry> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            if out.len() >= ENUMERATE_CAP {
                out.sort_by(|a, b| a.path.cmp(&b.path));
                return out;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Use symlink_metadata so we never follow symlinks (and never index their targets).
            let Ok(meta) = entry.metadata() else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if !excluded_component(&name) {
                    stack.push(entry.path());
                }
                continue;
            }
            if !meta.is_file() || excluded_component(&name) || skippable_file(&name) {
                continue;
            }
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root).map(Path::to_path_buf) else { continue };
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            out.push(WalkEntry { path: rel, mtime_ns, size: meta.len() as i64 });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn seed(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn walk_includes_source_excludes_sensitive_binary_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "src/main.rs", b"fn main() {}");
        seed(root, ".env", b"SECRET=1");
        seed(root, ".git/config", b"[core]");
        seed(root, "node_modules/x/index.js", b"x");
        seed(root, "logo.png", b"\x89PNG");
        seed(root, "big.txt", &vec![b'a'; (MAX_FILE_BYTES + 1) as usize]);

        let paths: Vec<_> = walk(root).into_iter().map(|e| e.path).collect();
        assert!(paths.contains(&PathBuf::from("src/main.rs")));
        assert!(!paths.contains(&PathBuf::from(".env")), "secrets excluded: {paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
        assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
        assert!(!paths.contains(&PathBuf::from("logo.png")), "binaries excluded");
        assert!(!paths.contains(&PathBuf::from("big.txt")), "oversized excluded");
    }
}
```

- [ ] **Step 2: Declare the module**

In `crates/retrieval/src/lib.rs`, add `mod walk;` to the module list (keep alongside `mod tokenize;`).

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p otto-retrieval walk::`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/retrieval/src/walk.rs crates/retrieval/src/lib.rs
git commit -m "feat(retrieval): workspace walk with gate-floor exclusions"
```

---

### Task 5: The sqlite `Index` — open + schema

**Files:**
- Create: `crates/retrieval/src/index.rs`
- Modify: `crates/retrieval/src/lib.rs` (add `mod index;`)

- [ ] **Step 1: Write the failing test**

Create `crates/retrieval/src/index.rs`:

```rust
//! The standalone sqlite inverted index: a `files` table (path + stat for staleness) and a
//! `postings` table (token -> file counts). Owned entirely by the retrieval crate, separate from
//! the session store. `CREATE TABLE IF NOT EXISTS` keeps it migration-free; a `meta` format
//! version triggers a full rebuild on mismatch so a schema change never reads a stale layout.

use std::path::Path;

use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// Bump when the schema changes; a mismatch drops and recreates the index.
const FORMAT_VERSION: &str = "1";

pub struct Index {
    pool: SqlitePool,
}

impl Index {
    /// Open (creating if missing) the index DB at `db_path`.
    pub async fn open(db_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new().connect_with(opts).await?;
        let index = Self { pool };
        index.init_schema().await?;
        Ok(index)
    }

    async fn init_schema(&self) -> anyhow::Result<()> {
        sqlx::query("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .execute(&self.pool)
            .await?;
        // Rebuild if the stored format version is absent or stale.
        let stored: Option<String> = sqlx::query("SELECT value FROM meta WHERE key = 'format'")
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.get::<String, _>("value"));
        if stored.as_deref() != Some(FORMAT_VERSION) {
            sqlx::query("DROP TABLE IF EXISTS postings").execute(&self.pool).await?;
            sqlx::query("DROP TABLE IF EXISTS files").execute(&self.pool).await?;
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS files (
                file_id INTEGER PRIMARY KEY,
                path TEXT UNIQUE NOT NULL,
                mtime_ns INTEGER NOT NULL,
                size INTEGER NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS postings (
                token TEXT NOT NULL,
                file_id INTEGER NOT NULL,
                count INTEGER NOT NULL,
                PRIMARY KEY (token, file_id)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS postings_token ON postings(token)")
            .execute(&self.pool)
            .await?;
        sqlx::query("INSERT OR REPLACE INTO meta (key, value) VALUES ('format', ?1)")
            .bind(FORMAT_VERSION)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_creates_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        let idx = Index::open(&db).await.unwrap();
        drop(idx);
        // Reopen the same file: no error, schema already present.
        let _idx = Index::open(&db).await.unwrap();
        assert!(db.exists());
    }
}
```

- [ ] **Step 2: Declare the module**

In `crates/retrieval/src/lib.rs`, add `mod index;`.

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p otto-retrieval index::`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/retrieval/src/index.rs crates/retrieval/src/lib.rs
git commit -m "feat(retrieval): sqlite index schema + open"
```

---

### Task 6: Index refresh (stat-incremental) + content scores

**Files:**
- Modify: `crates/retrieval/src/index.rs`

- [ ] **Step 1: Write the failing test**

Add these methods to `impl Index` in `crates/retrieval/src/index.rs` (above the `#[cfg(test)]` block):

```rust
    /// Bring the index in line with the on-disk workspace: re-tokenize new/changed files, drop
    /// rows for vanished files. `root` is the workspace root; entries come from `walk::walk`.
    pub async fn refresh(&self, root: &Path) -> anyhow::Result<()> {
        use std::collections::HashMap;

        let entries = crate::walk::walk(root);
        let present: HashMap<String, &crate::walk::WalkEntry> = entries
            .iter()
            .map(|e| (e.path.to_string_lossy().into_owned(), e))
            .collect();

        // Existing index state: path -> (file_id, mtime_ns, size).
        let mut existing: HashMap<String, (i64, i64, i64)> = HashMap::new();
        for row in sqlx::query("SELECT file_id, path, mtime_ns, size FROM files")
            .fetch_all(&self.pool)
            .await?
        {
            existing.insert(
                row.get::<String, _>("path"),
                (row.get("file_id"), row.get("mtime_ns"), row.get("size")),
            );
        }

        // Delete rows for files no longer present (handles deletions/renames).
        for (path, (file_id, _, _)) in &existing {
            if !present.contains_key(path) {
                sqlx::query("DELETE FROM postings WHERE file_id = ?1")
                    .bind(file_id)
                    .execute(&self.pool)
                    .await?;
                sqlx::query("DELETE FROM files WHERE file_id = ?1")
                    .bind(file_id)
                    .execute(&self.pool)
                    .await?;
            }
        }

        // Re-index new or changed files (stat compare); skip unchanged.
        for (path, entry) in &present {
            if let Some((_, mtime, size)) = existing.get(path) {
                if *mtime == entry.mtime_ns && *size == entry.size {
                    continue; // unchanged — the amortization win
                }
            }
            let Ok(content) = std::fs::read_to_string(root.join(&entry.path)) else { continue };
            let tokens = crate::tokenize::index_tokens(&content);

            // Upsert the files row and get its id.
            sqlx::query(
                "INSERT INTO files (path, mtime_ns, size) VALUES (?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET mtime_ns = ?2, size = ?3",
            )
            .bind(path)
            .bind(entry.mtime_ns)
            .bind(entry.size)
            .execute(&self.pool)
            .await?;
            let file_id: i64 = sqlx::query("SELECT file_id FROM files WHERE path = ?1")
                .bind(path)
                .fetch_one(&self.pool)
                .await?
                .get("file_id");

            // Replace this file's postings.
            sqlx::query("DELETE FROM postings WHERE file_id = ?1")
                .bind(file_id)
                .execute(&self.pool)
                .await?;
            for (token, count) in tokens {
                sqlx::query("INSERT INTO postings (token, file_id, count) VALUES (?1, ?2, ?3)")
                    .bind(token)
                    .bind(file_id)
                    .bind(count)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Content score per file: SUM(count) over `terms`, across ALL indexed files (no read
    /// budget). Returns (relative-path-string, content_score).
    pub async fn content_scores(&self, terms: &[String]) -> anyhow::Result<Vec<(String, u64)>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = terms.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT f.path AS path, SUM(p.count) AS score
             FROM postings p JOIN files f ON f.file_id = p.file_id
             WHERE p.token IN ({placeholders})
             GROUP BY p.file_id",
        );
        let mut q = sqlx::query(&sql);
        for t in terms {
            q = q.bind(t);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("path"), r.get::<i64, _>("score").max(0) as u64))
            .collect())
    }
```

Add tests at the bottom of the `mod tests` block:

```rust
    use std::io::Write;

    fn seed(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[tokio::test]
    async fn refresh_indexes_content_and_scores() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "a.rs", b"login login handler");
        seed(root, "b.rs", b"unrelated prose");
        let idx = Index::open(root.join("idx.sqlite")).await.unwrap();
        idx.refresh(root).await.unwrap();

        let scores = idx.content_scores(&["login".to_string()]).await.unwrap();
        let a = scores.iter().find(|(p, _)| p == "a.rs").map(|(_, s)| *s);
        assert_eq!(a, Some(2));
        assert!(!scores.iter().any(|(p, _)| p == "b.rs"));
    }

    #[tokio::test]
    async fn refresh_updates_on_edit_and_drops_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "a.rs", b"login");
        seed(root, "gone.rs", b"login");
        let idx = Index::open(root.join("idx.sqlite")).await.unwrap();
        idx.refresh(root).await.unwrap();

        // Edit a.rs to add another hit; delete gone.rs.
        seed(root, "a.rs", b"login login login");
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        idx.refresh(root).await.unwrap();

        let scores = idx.content_scores(&["login".to_string()]).await.unwrap();
        assert_eq!(scores.iter().find(|(p, _)| p == "a.rs").map(|(_, s)| *s), Some(3));
        assert!(!scores.iter().any(|(p, _)| p == "gone.rs"), "deleted file dropped: {scores:?}");
    }
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p otto-retrieval index::`
Expected: PASS (open + the two refresh tests).

- [ ] **Step 3: Commit**

```bash
git add crates/retrieval/src/index.rs
git commit -m "feat(retrieval): stat-incremental refresh + content scoring"
```

---

### Task 7: `IndexedRetriever` — combine path + content scoring

**Files:**
- Create: `crates/retrieval/src/retriever.rs`
- Modify: `crates/retrieval/src/lib.rs` (add `mod retriever;` + the `pub use`)

- [ ] **Step 1: Write the failing test**

Create `crates/retrieval/src/retriever.rs`:

```rust
//! `IndexedRetriever`: the `Retriever` impl backed by the persistent inverted index. On each
//! `search` it refreshes the index (stat-incremental), then ranks every walked file by
//! `5*path_hits + content_score` — the same weighting the lexical ContextFinder uses, but with
//! content scores drawn from the index for ALL files (no read budget).

use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::{Candidate, Retriever};

use crate::index::Index;
use crate::tokenize::query_terms;
use crate::walk::walk;

pub struct IndexedRetriever {
    root: PathBuf,
    index: Index,
}

impl IndexedRetriever {
    /// Open the retriever for `root`, backed by the index DB at `db_path`.
    pub async fn open(root: PathBuf, db_path: PathBuf) -> anyhow::Result<Self> {
        let index = Index::open(db_path).await?;
        Ok(Self { root, index })
    }
}

#[async_trait]
impl Retriever for IndexedRetriever {
    async fn search(&self, goal: &str, limit: usize) -> anyhow::Result<Vec<Candidate>> {
        self.index.refresh(&self.root).await?;
        let terms = query_terms(goal);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        // Content scores from the index (all files), keyed by relative path string.
        let content: std::collections::HashMap<String, u64> =
            self.index.content_scores(&terms).await?.into_iter().collect();

        // Path scores computed live over the current walk (free). Combine 5*path + content.
        let mut scored: Vec<Candidate> = walk(&self.root)
            .into_iter()
            .filter_map(|e| {
                let path_str = e.path.to_string_lossy().to_lowercase();
                let path_hits: u64 = terms
                    .iter()
                    .map(|t| path_str.matches(t.as_str()).count() as u64)
                    .sum();
                let score = 5 * path_hits + content.get(&e.path.to_string_lossy().into_owned()).copied().unwrap_or(0);
                (score > 0).then_some(Candidate { path: e.path, score })
            })
            .collect();

        // Rank by score desc, path asc (deterministic); cap at limit.
        scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        scored.truncate(limit);
        Ok(scored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    fn seed(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(bytes).unwrap();
    }

    async fn retriever(root: &Path) -> IndexedRetriever {
        IndexedRetriever::open(root.to_path_buf(), root.join("idx.sqlite")).await.unwrap()
    }

    #[tokio::test]
    async fn ranks_path_hit_above_content_hit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "login.rs", b"fn x() {}"); // path hit
        seed(root, "util.rs", b"login"); // content hit
        let r = retriever(root).await;
        let files: Vec<_> = r.search("login", 8).await.unwrap().into_iter().map(|c| c.path).collect();
        assert_eq!(files.first(), Some(&PathBuf::from("login.rs")));
    }

    #[tokio::test]
    async fn content_only_match_beyond_old_read_budget_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 300 noise files (> the old 200 read budget) that sort before the real match.
        for i in 0..300 {
            seed(root, &format!("noise/f{i:04}.txt"), b"nothing relevant here");
        }
        seed(root, "zzz_only.txt", b"login logic lives here"); // content-only, sorts last
        let r = retriever(root).await;
        let files: Vec<_> = r.search("login", 8).await.unwrap().into_iter().map(|c| c.path).collect();
        assert!(
            files.contains(&PathBuf::from("zzz_only.txt")),
            "content-only match beyond the old budget is now found: {files:?}",
        );
    }

    #[tokio::test]
    async fn sensitive_paths_never_appear() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, ".env", b"login=secret");
        seed(root, "real.rs", b"login");
        let r = retriever(root).await;
        let files: Vec<_> = r.search("login", 8).await.unwrap().into_iter().map(|c| c.path).collect();
        assert!(!files.iter().any(|p| p.to_string_lossy().contains(".env")), "{files:?}");
        assert!(files.contains(&PathBuf::from("real.rs")));
    }

    #[tokio::test]
    async fn empty_workspace_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let r = retriever(dir.path()).await;
        assert!(r.search("login", 8).await.unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Wire the module**

Replace `crates/retrieval/src/lib.rs` module/re-export section so it reads:

```rust
mod index;
mod retriever;
mod tokenize;
mod walk;

pub use retriever::IndexedRetriever;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p otto-retrieval`
Expected: PASS (all retrieval tests, including the headline content-beyond-budget test).

- [ ] **Step 4: Commit**

```bash
git add crates/retrieval/src/retriever.rs crates/retrieval/src/lib.rs
git commit -m "feat(retrieval): IndexedRetriever combining path + indexed content scores"
```

---

### Task 8: `AgentCtx` optional retriever

**Files:**
- Modify: `crates/engine-core/src/traits.rs`

- [ ] **Step 1: Write the failing test**

In `crates/engine-core/src/traits.rs`, add a test module at the end of the file (or extend an existing one):

```rust
#[cfg(test)]
mod agentctx_retriever_tests {
    use super::*;
    use crate::retrieval::{Candidate, Retriever};

    struct FixedRetriever;
    #[async_trait]
    impl Retriever for FixedRetriever {
        async fn search(&self, _goal: &str, _limit: usize) -> anyhow::Result<Vec<Candidate>> {
            Ok(vec![Candidate { path: std::path::PathBuf::from("hit.rs"), score: 1 }])
        }
    }

    // Minimal stand-ins for the other AgentCtx deps. We only need them to construct the ctx.
    // (Reuse the crate's existing test doubles if present; otherwise these compile-only stubs.)
    #[tokio::test]
    async fn retriever_defaults_none_and_can_be_set() {
        // Build the smallest possible router/workspace/tools to construct an AgentCtx.
        // If the crate already has test doubles for these, prefer them; this test asserts only
        // the retriever accessor behavior.
        let r = FixedRetriever;
        // Construct ctx via a helper that the existing tests use, then:
        // assert ctx.retriever().is_none() before with_retriever, is_some() after.
        // (See note in Step 3 — adapt to the crate's existing AgentCtx test scaffolding.)
        let _ = &r; // placeholder to keep the stub referenced
    }
}
```

NOTE: `AgentCtx` needs concrete `&dyn Router`, `&dyn WorkspaceRead`, `&ToolRegistry` to construct, which are verbose to stub here. Prefer asserting the accessor in the `agents` crate test in Task 10/11 where those doubles already exist. So instead of the stubbed test above, make THIS task's verification a focused accessor unit by adding a `retriever`-free assertion: confirm `AgentCtx::new(...).retriever().is_none()` using whatever lightweight doubles the file already imports. If none exist, skip the test here and rely on Task 11's ContextFinder test (which constructs a full ctx) — but still implement the accessor below.

- [ ] **Step 2: Implement the field, builder, and accessor**

In `crates/engine-core/src/traits.rs`, modify `AgentCtx`:

```rust
pub struct AgentCtx<'a> {
    router: &'a dyn Router,
    workspace: &'a dyn WorkspaceRead,
    tools: &'a ToolRegistry,
    retriever: Option<&'a dyn crate::retrieval::Retriever>,
}

impl<'a> AgentCtx<'a> {
    pub fn new(
        router: &'a dyn Router,
        workspace: &'a dyn WorkspaceRead,
        tools: &'a ToolRegistry,
    ) -> Self {
        Self {
            router,
            workspace,
            tools,
            retriever: None,
        }
    }

    /// Attach a retriever (the indexed candidate source). Absent → agents use their fallback.
    pub fn with_retriever(mut self, retriever: &'a dyn crate::retrieval::Retriever) -> Self {
        self.retriever = Some(retriever);
        self
    }

    // ... existing router()/workspace()/tools() accessors unchanged ...

    /// The retriever, if one is wired. `None` keeps the deterministic offline fallback path.
    pub fn retriever(&self) -> Option<&dyn crate::retrieval::Retriever> {
        self.retriever
    }
}
```

(Keep the existing `router`, `workspace`, `tools` accessor methods exactly as they are.)

- [ ] **Step 3: Verify it compiles and existing tests pass**

If you kept a runnable test in Step 1, make it assert `.retriever().is_none()` / `.is_some()` using the crate's existing doubles; otherwise remove the placeholder test body and rely on later tasks. Then run:

Run: `cargo test -p otto-engine-core`
Expected: PASS (all engine-core tests; the new field defaults to `None` so nothing else changes).

- [ ] **Step 4: Commit**

```bash
git add crates/engine-core/src/traits.rs
git commit -m "feat(engine-core): AgentCtx optional retriever (builder + accessor)"
```

---

### Task 9: Thread retriever through the `Orchestrator`

**Files:**
- Modify: `crates/engine-core/src/orchestrator.rs`

- [ ] **Step 1: Add the field and use it when building `AgentCtx`**

In `crates/engine-core/src/orchestrator.rs`, add a field to the `Orchestrator` struct (after `pub tools: &'a ToolRegistry,`):

```rust
    /// Optional candidate source for the ContextFinder. `None` → the lexical fallback path.
    pub retriever: Option<&'a dyn crate::retrieval::Retriever>,
```

Change the `AgentCtx` construction in `run_turn` (currently `let ctx = AgentCtx::new(self.router, self.workspace, self.tools);`) to:

```rust
        let ctx = {
            let base = AgentCtx::new(self.router, self.workspace, self.tools);
            match self.retriever {
                Some(r) => base.with_retriever(r),
                None => base,
            }
        };
```

- [ ] **Step 2: Update every `Orchestrator { ... }` literal in the test module**

Every `Orchestrator { ... }` literal in this file's `#[cfg(test)] mod tests` (11 of them) must add `retriever: None,`. Add the line alongside `tools: &tools,` in each literal. Example:

```rust
        let orch = Orchestrator {
            registry: &registry,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            retriever: None,
            approver: &approver,
            next_id: &next_id,
            meter: &meter,
            pauser: &pauser,
        };
```

(Repeat for all 11 literals — search the file for `Orchestrator {`.)

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p otto-engine-core`
Expected: PASS (orchestrator tests unchanged behaviorally; all literals now include the field).

- [ ] **Step 4: Commit**

```bash
git add crates/engine-core/src/orchestrator.rs
git commit -m "feat(engine-core): Orchestrator threads optional retriever into AgentCtx"
```

---

### Task 10: `ContextFinder` uses the retriever when present

**Files:**
- Modify: `crates/agents/src/context_finder.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/agents/src/context_finder.rs`:

```rust
    use otto_engine_core::{Candidate, Retriever};

    struct StubRetriever(Vec<Candidate>);
    #[async_trait]
    impl Retriever for StubRetriever {
        async fn search(&self, _goal: &str, _limit: usize) -> anyhow::Result<Vec<Candidate>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn uses_retriever_candidates_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "from_retriever.rs", "login").await;
        let tools = registry(dir.path());
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let retriever = StubRetriever(vec![Candidate {
            path: PathBuf::from("from_retriever.rs"),
            score: 99,
        }]);
        let ctx = AgentCtx::new(&router, &ws, &tools).with_retriever(&retriever);
        let out = ContextFinder
            .run(AgentRequest::FindContext { goal: "login".into() }, &ctx)
            .await
            .unwrap();
        let AgentOutput::Context { files } = out else { panic!("expected Context") };
        assert_eq!(files, vec![PathBuf::from("from_retriever.rs")]);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-agents context_finder::tests::uses_retriever_candidates_when_present`
Expected: FAIL (ContextFinder ignores the retriever; it currently walks via fs.list and would still find the file, but the test pins the retriever path — it fails to compile until `Candidate`/`with_retriever` are used, or fails the assertion if the lexical path diverges). Confirm a red bar before proceeding.

- [ ] **Step 3: Restructure `run` to source candidates from the retriever when present**

In `crates/agents/src/context_finder.rs`, in `impl Agent for ContextFinder`, replace the body from the `// Enumerate the workspace recursively.` comment through the `scored.truncate(CANDIDATE_LIMIT);` line with a branch that produces `scored` either from the retriever or the existing inline lexical pipeline. Concretely:

```rust
        // Produce the scored candidate set: from the retriever when wired, else the inline
        // lexical pipeline (the deterministic offline fallback).
        let scored: Vec<(String, u64)> = if let Some(retriever) = ctx.retriever() {
            retriever
                .search(&goal, CANDIDATE_LIMIT)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|c| (c.path.to_string_lossy().into_owned(), c.score))
                .collect()
        } else {
            // ---- existing inline lexical pipeline (unchanged) ----
            let files: Vec<String> = match ctx.tools().call("fs.list", json!({ "glob": "**" })).await {
                Ok(Value::Object(map)) => map
                    .get("paths")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let kws = keywords(&goal);
            let mut by_path: Vec<(String, u64)> = files
                .into_iter()
                .filter(|p| !is_skippable(p))
                .map(|p| {
                    let path_score = score_file(&p, None, &kws);
                    (p, path_score)
                })
                .collect();
            by_path.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let read_set: HashSet<String> =
                by_path.iter().take(READ_BUDGET).map(|(p, _)| p.clone()).collect();
            let mut scored: Vec<(String, u64)> = Vec::new();
            for (path, path_score) in &by_path {
                let score = if read_set.contains(path) {
                    let content = match ctx.tools().call("fs.read", json!({ "path": path })).await {
                        Ok(Value::Object(map)) => {
                            map.get("content").and_then(Value::as_str).map(str::to_string)
                        }
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
            scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            scored.truncate(CANDIDATE_LIMIT);
            scored
        };
```

Everything from `if scored.is_empty() { ... }` through the end of `run` (the LLM rank/select + hallucination filter + `lexical_top` fallback) stays exactly as-is and is now shared by both paths.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-agents context_finder::`
Expected: PASS — the new retriever test passes AND every existing ContextFinder test (which constructs `AgentCtx` without a retriever) still passes via the unchanged lexical branch.

- [ ] **Step 5: Commit**

```bash
git add crates/agents/src/context_finder.rs
git commit -m "feat(agents): ContextFinder sources candidates from the retriever when present"
```

---

### Task 11: `EngineService` retriever field + thread into the turn

**Files:**
- Modify: `crates/engine/src/service.rs`

- [ ] **Step 1: Add the field, builder, and import**

In `crates/engine/src/service.rs`, add to the `use otto_engine_core::{...}` line the `Retriever` import (so it reads e.g. `use otto_engine_core::{AgentRegistry, Orchestrator, Retriever, Router, TokenMeter, TurnOutcome};`).

Add a field to `EngineService`:

```rust
    retriever: Option<Arc<dyn Retriever>>,
```

In `EngineService::new`, initialize it to `None` (the field is set via the builder below):

```rust
            tools,
            retriever: None,
            turn_lock: tokio::sync::Mutex::new(()),
```

Add a builder method to `impl EngineService` (near `new`):

```rust
    /// Attach a retriever (the indexed candidate source). `None` keeps the lexical fallback.
    pub fn with_retriever(mut self, retriever: Option<Arc<dyn Retriever>>) -> Self {
        self.retriever = retriever;
        self
    }
```

- [ ] **Step 2: Thread it into the spawned turn**

In `run_prompt_with_controls`, inside the `let handle = { ... }` block, add a clone alongside the other `Arc::clone`s:

```rust
            let retriever = self.retriever.clone();
```

Then in the `Orchestrator { ... }` literal, add the field (after `tools: &tools,`):

```rust
                    retriever: retriever.as_deref(),
```

(`Option<Arc<dyn Retriever>>::as_deref()` yields `Option<&dyn Retriever>`, matching the orchestrator field.)

- [ ] **Step 3: Verify the engine still builds and tests pass**

Run: `cargo test -p otto-engine`
Expected: PASS — all existing service tests pass (every `EngineService::new` call site now has a `retriever: None` field via `new`, and none of them set a retriever, so behavior is unchanged).

- [ ] **Step 4: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "feat(engine): EngineService carries an optional retriever into the turn"
```

---

### Task 12: Build + wire the retriever for `run` and `serve`

**Files:**
- Modify: `crates/engine/Cargo.toml`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/main.rs`

- [ ] **Step 1: Add dependencies**

In `crates/engine/Cargo.toml`, under `[dependencies]`, add:

```toml
otto-retrieval = { path = "../retrieval" }
dirs = "5"
```

- [ ] **Step 2: Add the `build_retriever` helper**

In `crates/engine/src/lib.rs`, add (near `build_router`):

```rust
/// Build the persistent retriever for `root`, or `None` (logged) on any failure — retrieval is
/// an optimization, never a gate, so a missing cache dir or open error degrades to the lexical
/// fallback. The index DB lives in the OS cache dir, keyed by the canonical root so each repo
/// gets its own, reused across `otto run` invocations and serve restarts.
pub async fn build_retriever(
    root: &std::path::Path,
) -> Option<Arc<dyn otto_engine_core::Retriever>> {
    use std::hash::{Hash, Hasher};

    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Some(cache) = dirs::cache_dir() else {
        eprintln!("warning: no OS cache dir; retrieval index disabled (lexical fallback)");
        return None;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let db = cache
        .join("otto")
        .join("index")
        .join(format!("{:016x}.sqlite", hasher.finish()));
    if let Some(parent) = db.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("warning: cannot create retrieval cache dir ({e}); lexical fallback");
            return None;
        }
    }
    match otto_retrieval::IndexedRetriever::open(canonical, db).await {
        Ok(r) => Some(Arc::new(r) as Arc<dyn otto_engine_core::Retriever>),
        Err(e) => {
            eprintln!("warning: retrieval index unavailable ({e}); lexical fallback");
            None
        }
    }
}
```

- [ ] **Step 3: Give `run_goal` a retriever param**

Change `run_goal`'s signature and body in `crates/engine/src/lib.rs`:

```rust
pub async fn run_goal(
    goal: &str,
    store: Arc<dyn SessionStore>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
    retriever: Option<Arc<dyn otto_engine_core::Retriever>>,
) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    )
    .with_retriever(retriever);
    let session = service.create_session(goal, &session_config()).await?;
    let mut sink = CollectingSink::default();
    let outcome = service.run_prompt(session, goal, &mut sink).await?;
    Ok((sink.events, outcome))
}
```

- [ ] **Step 4: Update `run_goal` callers**

Find every `run_goal(` call: `grep -rn "run_goal(" crates/`. Update each to pass the new argument:
- In `crates/engine/src/main.rs` `cmd_run` (line ~184), build and pass the retriever (next step covers this).
- In any test caller, pass `None` as the final argument.

- [ ] **Step 5: Wire `cmd_run` (the `otto run` path)**

In `crates/engine/src/main.rs` `cmd_run`, just before the `run_goal(...)` call, build the retriever and pass it:

```rust
    let retriever = otto_engine::build_retriever(&root).await;
    let (events, outcome) = run_goal(&goal, store, router, orch_workspace, tools, retriever).await?;
```

- [ ] **Step 6: Wire `serve`**

In `crates/engine/src/main.rs` serve setup, change the `EngineService::new(...)` line (≈266) to attach the retriever:

```rust
    let retriever = otto_engine::build_retriever(&root).await;
    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools)
        .with_retriever(retriever);
```

(`root` is already in scope here as the `--root` value.)

- [ ] **Step 7: Build the whole workspace and run the engine tests**

Run: `cargo build --workspace`
Expected: builds clean.

Run: `cargo test -p otto-engine`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/engine/Cargo.toml crates/engine/src/lib.rs crates/engine/src/main.rs
git commit -m "feat(engine): build and wire the indexed retriever for run and serve"
```

---

### Task 13: Full verification + docs

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Full offline suite, format, lint**

Run: `cargo test --workspace`
Expected: PASS (the offline determinism suite is untouched; new retrieval + agents + engine tests pass).

Run: `cargo fmt --all`
Expected: no diff after (or it formats the new files).

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings in the new crate (fix any that appear — e.g. needless clones).

- [ ] **Step 2: Smoke-test the `run` path end to end**

Run: `cargo run -p otto-engine -- run "improve logging" --root .`
Expected: a normal turn runs; on first run the index DB is created under the OS cache dir; a second run reuses it. No panic, no error about retrieval (a warning + lexical fallback is acceptable if the cache dir is unavailable in the sandbox).

- [ ] **Step 3: Update `CLAUDE.md`**

In `CLAUDE.md`, add a row to the crate table for `retrieval` and a sentence in the prose noting the retrieval axis now has a real first slice. Suggested table row (place after the `persistence` row):

```markdown
| `retrieval` | Persistent inverted index behind the `Retriever` seam (defined in `engine-core`). `IndexedRetriever` keeps a standalone sqlite index (token→file postings) in the OS cache dir keyed by workspace root, refreshed stat-incrementally (mtime+size), and scores content for every indexed file — removing the ContextFinder's per-turn read budget. Mirrors the gate's sensitive-path floor (secrets never indexed). Depends inward on `engine-core`. |
```

Suggested prose (append to the architecture/“what this is” area where the spine is described): a sentence noting the ContextFinder now sources candidates from the indexed `Retriever` when wired, falling back to its inline lexical pipeline (the deterministic offline path) when absent.

- [ ] **Step 4: Update `docs/ARCHITECTURE.md`**

In `docs/ARCHITECTURE.md`, the `retrieval` crate line currently reads:

```
│   ├── retrieval        # Tree-sitter chunking, git-history, grep selection (vector index = v2).
```

Update it to reflect what shipped vs. what's still v2:

```
│   ├── retrieval        # Persistent inverted index behind the Retriever seam (shipped). Tree-sitter chunking, git-history selection, vector index = later/v2.
```

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md
git commit -m "docs: record the retrieval crate (persistent inverted index) shipped"
```

---

## Done criteria

- `crates/retrieval` exists in the workspace, implements `IndexedRetriever` behind the `engine-core` `Retriever` seam, and its tests pass — including the headline test that a content-only file beyond the old 200-file read budget is now found, plus the secrets-excluded test.
- `ContextFinder` uses the retriever when wired and falls back to its inline lexical pipeline (unchanged) when not; every prior ContextFinder test still passes.
- `otto run` and `otto serve` build and wire the retriever; failures degrade to the lexical path with a logged warning (fail-soft, never a gate).
- `cargo test --workspace` is green and the engine-core offline determinism suite is untouched (it runs the retriever-free path).
- `CLAUDE.md` and `ARCHITECTURE.md` record the shipped crate.
