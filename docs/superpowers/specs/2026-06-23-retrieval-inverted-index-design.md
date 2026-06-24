# otto Retrieval Slice 1 Design — Persistent Inverted Index (+ `retrieval` crate & `Retriever` seam)

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-23

## Goal

Open the **retrieval axis** — the one core axis still running on a placeholder. Today all
retrieval lives inside `crates/agents/src/context_finder.rs`: a per-turn lexical pipeline that
path-scores every file for free, then reads contents for only the top `READ_BUDGET = 200`
path-ranked files. The cost of that bound is documented as a carried-forward gap: **a file
relevant only by content, ranked beyond the read budget, is missed**, and **every turn re-reads
the workspace from scratch**.

This slice closes both gaps by extracting a `retrieval` crate behind a `Retriever` seam and
shipping its first implementation: a **persistent, stat-incremental inverted index** that scores
content for *every* indexed file (no read budget) and amortizes reads across turns and process
invocations. The return shape stays file-level, so nothing ripples into the Coder.

## Decisions (locked during brainstorming)

1. **Lead capability = persistent inverted index** (not tree-sitter chunking or git-history —
   those are later slices). It directly closes the two documented retrieval gaps with no change
   to the candidate return shape.
2. **Extract the `retrieval` crate and the `Retriever` seam now.** The trait lives in
   `engine-core`; the impl lives in `retrieval`; `engine` wires it.
3. **Index location = OS cache dir, keyed by workspace.**
   `dirs::cache_dir()/otto/index/<stable-hash-of-canonical-root>.sqlite` — survives across
   `otto run` invocations and serve restarts, never pollutes the repo, never sits inside the
   gated/sensitive tree.
4. **Substrate = standalone sqlite owned by `retrieval`** (its own schema + connection via
   `sqlx`), keeping the `persistence` crate session-focused.
5. **Staleness = stat-based** (per-file `mtime` + `size`), self-healing against external edits
   (`git pull`, manual edits) with no write-hook.

## Non-goals (explicitly out of scope — later slices)

- Tree-sitter symbol chunking (changes the return shape to spans; next slice).
- git-history ranking signal (recently-/co-changed boost; later).
- Vector / embedding index (v2 per `ARCHITECTURE.md`).
- Making `READ_BUDGET` / the extension denylist configurable (carried-forward gap #4).
- Retrieval over `RemoteWorkspace` — this slice is local-filesystem only. A remote engine builds
  its own local index after a promote/restore.

## Architecture

### Data flow (trait seam added; orchestrator control flow unchanged)

```
engine: build IndexedRetriever(root, cache_db_path)   ─┐  (None on cache/open failure → warn)
                                                        │
orchestrator.run_turn ── Option<&dyn Retriever> ──► AgentCtx.with_retriever(..)
                                                        │
FindContext { goal }                                    │
   └─ ContextFinder:                                    │
        if Some(retriever):  retriever.search(goal, CANDIDATE_LIMIT) ─► ranked candidates
        else (None):         today's inline lexical pipeline (unchanged)
              └─► LLM rank/select + hallucination-filter + lexical-top fallback (shared)
                    └─► Context { files: Vec<PathBuf> }   (ranked, capped at SELECT_LIMIT = 8)
```

`AgentOutput::Context` and `AgentRequest::Code` keep `Vec<PathBuf>`. The Coder is untouched.

### Dependency flow (inward, no cycles)

| Crate | Change |
|---|---|
| `engine-core` | Adds the `Retriever` trait + `Candidate` type. Depends on nothing new. Must not depend on `retrieval`. |
| `retrieval` (new) | Implements `IndexedRetriever`. Depends on `engine-core` (trait + types), `sqlx` (sqlite), `tokio`, `anyhow`, `async-trait`. |
| `agents` | `ContextFinder` reads `ctx.retriever()`; no new crate dep (the trait arrives via `AgentCtx`). |
| `engine` | Depends on `retrieval`; constructs `IndexedRetriever`, threads `Option<&dyn Retriever>` through the orchestrator. Adds `dirs` for the cache path. |

### Component A — the `Retriever` seam (`crates/engine-core`)

```rust
pub struct Candidate {
    pub path: PathBuf,
    pub score: u64,
}

#[async_trait]
pub trait Retriever: Send + Sync {
    /// Ranked candidates for a goal, best first, already capped at `limit`.
    async fn search(&self, goal: &str, limit: usize) -> anyhow::Result<Vec<Candidate>>;
}
```

`Send + Sync` and async, consistent with the other seams; the orchestrator only ever holds the
trait object.

### Component B — `AgentCtx` threading (`crates/engine-core/src/traits.rs`)

`AgentCtx` gains a private `retriever: Option<&'a dyn Retriever>` field, defaulting to `None`:

- `AgentCtx::new(router, workspace, tools)` keeps its current signature (all 8 existing call
  sites compile unchanged; `retriever` defaults to `None`).
- A builder `fn with_retriever(self, r: &'a dyn Retriever) -> Self` sets it.
- An accessor `fn retriever(&self) -> Option<&dyn Retriever>` reads it.

The orchestrator accepts an `Option<&dyn Retriever>` alongside the router/workspace/tools it
already threads, and calls `.with_retriever(..)` when present. The offline engine-core
orchestrator tests construct `AgentCtx` without a retriever → the deterministic fallback path →
those tests are untouched.

### Component C — `ContextFinder` change (`crates/agents/src/context_finder.rs`)

Minimal and fallback-preserving:

- If `ctx.retriever()` is `Some`, call `search(&goal, CANDIDATE_LIMIT)` and feed the returned
  ranked candidates into the **existing** LLM rank/select stage (with its hallucination filter
  and lexical-top fallback). The indexed retriever replaces only *how candidates are produced*.
- If `None`, run today's inline lexical pipeline **unchanged**.

This leaves two lexical scoring paths (the inline `None` fallback and the crate's indexed path).
That duplication is accepted for this slice; extracting today's lexical logic into a
`LexicalRetriever` so `ContextFinder` always goes through the seam is a deferred follow-up, not
required to land the crate or close the gaps.

### Component D — `IndexedRetriever` (`crates/retrieval`)

Constructed with the workspace **root path** and a sqlite **db path**:
`IndexedRetriever::open(root: PathBuf, db_path: PathBuf) -> Result<Self>`.

**Schema (standalone sqlite):**

```sql
CREATE TABLE meta     (key TEXT PRIMARY KEY, value TEXT);            -- schema/format version
CREATE TABLE files    (file_id INTEGER PRIMARY KEY,
                       path TEXT UNIQUE, mtime_ns INTEGER, size INTEGER);
CREATE TABLE postings (token TEXT, file_id INTEGER, count INTEGER,
                       PRIMARY KEY (token, file_id));
CREATE INDEX postings_token ON postings(token);
```

`meta` carries a format version; a version mismatch triggers a full rebuild (drop + recreate),
so schema changes never read a stale layout.

**Tokenization (must match the query side exactly):** split content on non-alphanumeric
characters, lowercase, keep tokens of length ≥ 3 (mirrors `ContextFinder::keywords` so query
keywords and indexed tokens align). Content is scanned up to `CONTENT_SCAN_CHARS` (64 KiB) per
file, matching today's bound. Stopwords are irrelevant to the index (the query never asks for
them).

**Walk + exclusions (mirrors `fs.list` and the permission gate's sensitive-path floor):** a
recursive filesystem walk from `root` that skips `.git`, `target`, `node_modules`, **any
path component beginning with `.`** (covers `.env`/`.ssh`/`.aws` — the security-critical
exclusion), and the binary/lockfile set already encoded in `is_skippable`. It does **not** follow
symlinks and caps enumeration at **5000** entries. **Secrets never enter the index** — covered by
an explicit test. (Bonus, nearly free: files larger than a fixed max size are skipped during
indexing, closing carried-forward gap #2.)

**Stat-based refresh (runs lazily at the start of `search`):**

1. Walk the workspace (exclusions above), collecting `(path, mtime_ns, size)`.
2. For each enumerated file: if new, or if `(mtime_ns, size)` differ from its `files` row,
   read + tokenize and **replace** its `postings`, then upsert its `files` row. If unchanged,
   skip (the amortization win — a warm index with no edits does only stats).
3. For each `files` row whose path is no longer present on disk, delete its `files` + `postings`
   rows (handles deletions and renames).

**Query (`search`):**

1. Refresh (above).
2. `keywords(goal)` → content score per file:
   `SELECT file_id, SUM(count) FROM postings WHERE token IN (…) GROUP BY file_id` — over **every
   indexed file**, no read budget.
3. Path score computed live from the enumerated paths (free), same `5 × path_hits` weighting.
4. Combined score `5·path + 1·content` (unchanged weighting), files with score 0 dropped, ranked
   by score desc then path asc (deterministic), capped at `limit`. Return `Vec<Candidate>`.

### Component E — engine wiring (`crates/engine`)

- Compute the cache path: `dirs::cache_dir()` → `otto/index/<hash>.sqlite`, where `<hash>` is a
  stable hash of the **canonicalized** root path (distinct repos → distinct indexes; same repo →
  reuse). Create parent dirs as needed.
- Construct `IndexedRetriever::open(root, db_path)`. On any failure (no cache dir, open/migrate
  error), **log a warning** and proceed with `None` — fail-soft to the inline lexical path. The
  degradation is logged, never silent.
- Thread the `Option<&dyn Retriever>` into the orchestrator for both `otto run` and `otto serve`.

## Determinism & error handling

- **Determinism is preserved.** Index *content* is a pure function of file contents; `mtime`
  only decides *whether to re-read*, never the resulting postings — so `search` output is
  deterministic given file contents. The engine-core offline suite runs the retriever-free
  (`None`) path and is untouched.
- **Retrieval is an optimization, never a gate.** Index open/refresh failure degrades to the
  inline lexical path with a logged warning. A single unreadable file during refresh is skipped
  (logged at debug), not fatal.

## Testing

All tests are offline and tempfile-backed (no network, no keys):

- **`retrieval` crate:** index build over a seeded workspace; **content-only file beyond the old
  200-read-budget is now found** (inverts today's `content_only_match_beyond_read_budget_is_missed`);
  staleness re-index after an edit (changed file's postings update); deletion cleanup (removed
  file drops out); **sensitive/dot-prefixed paths are excluded from the index**; max-file-size
  skip; tokenization parity with `keywords`.
- **`ContextFinder`:** a test exercising the wired retriever path (candidates sourced from a
  retriever, LLM-select + hallucination filter still applied); every existing ContextFinder test
  stays green (the `None` fallback path is unchanged).
- **`engine`:** the wired path constructs an `IndexedRetriever` against a temp cache dir and runs
  a turn end to end; cache-dir-failure degrades to `None` without error.

## What this unblocks

With the `retrieval` crate and `Retriever` seam in place, later slices slot in as additional
`Retriever` impls or ranking layers without touching the orchestrator or the candidate return
shape: tree-sitter symbol chunking, a git-history recency/co-change boost, and (v2) a vector
index.
