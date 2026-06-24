# otto Retrieval Slice 3 Design — Git-History Recency Boost (active-work precision re-ranker)

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-24

## Goal

Advance the **retrieval axis** with its third slice. Slice 1
(`2026-06-23-retrieval-inverted-index-design.md`) shipped a persistent, stat-incremental sqlite
inverted index scoring whole-file token counts; slice 2
(`2026-06-24-retrieval-tree-sitter-chunking-design.md`) layered a symbol-name definition boost and
symbol-enriched select on top. Ranking is now driven entirely by the *static text* of the
workspace — path, content, and symbol names.

This slice adds the first **history** signal: among files that already match the goal, the ones the
team is *actively working on* are more likely to be relevant. A `git log`-derived **recency boost**
nudges recently-changed matching files upward. It is a **precision re-ranker, not a recall source**:
git recency alone never surfaces a file that didn't already match by path/content/name.

## Decisions (locked during brainstorming)

1. **Signal = recency-rank tiers.** Rank each file by the *most recent* commit (within a bounded
   recent-commit window) that touched it — HEAD = rank 0, older = larger rank. Map the rank to a
   small bounded tier boost. Recency-rank (commit order) rather than absolute timestamps normalizes
   by repo velocity and is trivially deterministic in tests. (Churn and co-change signals are
   explicit non-goals — see below.)
2. **Compute per-search, bounded.** One bounded `git log -n WINDOW --name-only` per `search`. No new
   index table, no `FORMAT_VERSION` bump. `git log` over a bounded window is cheap and O(window);
   persisted/HEAD-keyed caching is deferred as an explicit non-goal.
3. **Re-ranker only (recall floor preserved).** The boost is added **inside** the existing
   `base > 0` candidate guard, so only files that already matched receive it. The no-recall-
   regression invariant holds exactly: the boost is `>= 0`, so it never removes a candidate and
   never promotes a `0`-score file into the result set.
4. **Query-independent → no injection surface.** The goal/query is **never** passed to git. The argv
   is fixed and rooted via `-C <root>`, mirroring `mcp-git`'s hardening posture. The boost map is a
   pure function of repository history, independent of the search terms.
5. **Graceful no-op off-git.** A non-repo directory, an absent `git` binary, or an empty log yields
   an empty boost map → scoring is byte-identical to slice 2. Every existing retriever test (which
   runs against non-git tempdirs) stays green untouched.

## Non-goals (explicitly out of scope — later slices)

- **Churn signal** (count of recent commits touching a file). A weaker proxy for relevance to *this*
  goal, and frequently-changed central files over-rank. Possible follow-up.
- **Co-change signal** (files that historically changed together with the matched files). Powerful
  but two-phase and recall-affecting (it would *add* files); deferred until warranted.
- **Persisted / HEAD-keyed caching** of the git signal. `git log` over a bounded window is cheap;
  amortization is premature.
- **Absolute-time decay** (boost by wall-clock age rather than commit rank). Rejected for this slice
  in favor of velocity-normalized commit-rank tiers.
- **Configurable window / tier weights**, retrieval over `RemoteWorkspace` (a remote engine builds
  its own local index and runs git against its own checkout after promote/restore).

## Architecture

### Data flow (unchanged control flow; one additive score term)

```
IndexedRetriever.search(goal, limit)
  ├─ refresh(root)                      # slices 1-2, unchanged (stat-incremental index)
  ├─ whole-file content scores          # slice 1, unchanged → recall floor
  ├─ symbol-name hits + matched symbols  # slice 2, unchanged
  ├─ recency_boosts(root)               # NEW: rel-path -> bounded recency tier (empty off-git)
  └─ base = 5·path + whole_file_content + 8·name_hits
        score = base + recency_boost(file)        # boost applied ONLY when base > 0
        └─► Vec<Candidate { path, score, symbols }>   (ranking unchanged: score desc, path asc)
```

### Dependency flow (inward, no cycles)

| Crate | Change |
|---|---|
| `engine-core` | **None.** `Candidate` is unchanged. |
| `retrieval` | New `git_history` module (shells to `git`); `retriever` modified to add the boost term. No new crate dependency (uses `std::process::Command`). |
| `agents` | **None.** The boost only changes a candidate's numeric score; the symbol-enriched select prompt is unchanged. |
| `engine` | **None.** No schema change, so no rebuild on upgrade. |

### Component A — the git-history reader (`crates/retrieval/src/git_history.rs`, new)

Self-contained; the only module that touches git. Query-independent and read-only.

```rust
/// Recent commits scanned per search. Bounds cost to O(window · files-per-commit) and defines the
/// recency tiers below.
const WINDOW: usize = 200;

/// Map a file's most-recent-commit rank (0 = HEAD) to a bounded recency boost. Tiers are small so a
/// recency signal re-ranks among already-relevant files without dominating a symbol-name hit (8) or
/// a path hit (5).
fn tier(rank: usize) -> u64 {
    match rank {
        0..=4   => 4,
        5..=19  => 3,
        20..=49 => 2,
        _       => 1,   // touched within the window but older
    }
}

/// Relative-path-string -> recency boost, for every file touched within the recent window.
/// Returns an empty map when `root` is not a git repository, `git` is unavailable, or the log is
/// empty — a graceful no-op that leaves slice-2 scoring unchanged.
pub fn recency_boosts(root: &Path) -> HashMap<String, u64>;
```

**Command:**
```
git -C <root> -c core.quotePath=false log -n 200 --name-only --pretty=format:%x00
```
- `-C <root>` roots the operation; the workspace root is the only external input — the goal is never
  passed, so there is no agent-input argv-injection surface.
- `-c core.quotePath=false` keeps non-ASCII paths un-escaped so they string-match the walk's
  relative paths (plain forward-slash paths on the Unix targets the sandbox supports).
- `--pretty=format:%x00` prints a single NUL byte as each commit's header line, giving an
  unambiguous commit boundary (file paths never contain NUL). `--name-only` lists changed paths.

**Parsing:** iterate stdout lines, maintaining a 0-based `rank` incremented at each NUL boundary;
every other non-empty line is a changed path recorded at the *current* rank **iff not already seen**
(first occurrence = most recent commit = smallest rank). Then `tier(rank)` each path. Renamed files
appear under their new path (`--name-only`), which is fine since only walked paths are ever looked
up.

**Execution:** run the subprocess via `tokio::task::spawn_blocking` so the bounded `git log` wait
never stalls the async executor. Any spawn/exit/parse failure → empty map (never fatal; retrieval
remains an optimization that degrades to the slice-2 path).

### Component B — `IndexedRetriever::search` (`crates/retrieval/src/retriever.rs`, modify)

```
base  = 5 · path_hits + whole_file_content_score + 8 · symbol_name_hits   (slices 1-2, unchanged)
score = base + recency_boost(file)        // recency_boost added ONLY inside the `base > 0` guard
```

- `recency_boosts(&self.root)` is fetched once per search (empty off-git).
- The boost is looked up by the same relative-path-string key used for content/name scores and added
  **inside** the existing `(base > 0).then(...)` candidate construction — so a recent-but-unmatched
  file (base `0`) is never constructed, and a previously-matching file's score can only rise.
- Ranking is unchanged: score desc, then path asc (deterministic); truncate to `limit`.

The no-recall-regression invariant holds: `recency_boost >= 0`, so `score_slice3 >= score_slice2`
for every candidate and no file that previously had score > 0 drops out.

## Determinism & error handling

- **Deterministic.** For a fixed repository state, `git log` output is deterministic; tests fix the
  state by committing files in a known *order* (recency is ranked by commit order, so no fixed dates
  or `GIT_*_DATE` env are needed). `mtime`/`size` and git rank only affect *ranking*, never recall.
  The engine-core offline determinism suite runs the retriever-free path and is untouched.
- **Best-effort, never a gate.** Missing `git`, a non-repo workspace, or a malformed log all yield
  an empty boost map → identical to slice 2. Index/retrieval failure as a whole still degrades to the
  ContextFinder's inline lexical pipeline.
- **Security unchanged.** The boost is a lookup keyed on the walked entries; the walk's sensitive-
  path floor (`.git`/`.env`/`.ssh`/`.aws`, dot-prefixed components) already excluded secrets, so a
  committed `.env` is never in the candidate set and can never be boosted into results. No new
  filesystem reads are introduced (git reads its own object store; the candidate set is unchanged).
  The query is never forwarded to git. Reaffirmed by test.

## Testing

All tests offline and tempfile-backed; the git tests build real repositories with an isolated local
identity (the `git init -q` + `config user.name/email` + `commit.gpgsign false` helper already used
by the `mcp-git` test suite — `git` is an established test-time dependency of this workspace):

- **`git_history`:** a repo with three sequential commits touching different files — the file in the
  newest commit gets a higher tier than one only in the oldest; a never-committed file is absent from
  the map; a non-repo tempdir → empty map; the query is irrelevant to the output (history-only).
- **`retriever`:** two files with **equal base score** (e.g. each a single content hit on the goal
  term), one touched in a more recent commit → it ranks first; a recent-but-non-matching file does
  **not** appear in results; a committed sensitive file (`.env`) never appears; a slice-2 ranking
  expectation (e.g. a path hit outranks a content-only hit) still holds.
- **workspace suite:** `cargo test --workspace` green; engine-core offline determinism suite
  untouched.

## What this unblocks

With a history signal wired into the score as a bounded, recall-safe additive term, later slices
slot in without touching the orchestrator or the file-level return contract: a churn signal and a
co-change signal over the same `git log` substrate, HEAD-keyed caching if the per-search cost ever
matters, and absolute-time decay as an alternative recency curve.
