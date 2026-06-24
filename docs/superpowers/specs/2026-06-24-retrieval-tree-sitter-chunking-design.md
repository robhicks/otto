# otto Retrieval Slice 2 Design — Tree-sitter Symbol Chunking (symbol-aware ranking + enriched select)

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-24

## Goal

Advance the **retrieval axis** with its second slice. Slice 1 (`2026-06-23-retrieval-inverted-index-design.md`)
shipped a persistent, stat-incremental sqlite inverted index that scores **whole-file** token
counts for every indexed file. That closed the read-budget and re-read-every-turn gaps, but
ranking is still coarse: a 2,000-line file with one relevant function scores by tokens scattered
across the whole file, and an incidental mention of a term in a comment counts the same as a file
that *defines* a symbol named after it.

This slice parses supported-language files into **symbol chunks** (functions, methods,
structs/classes/enums/traits/types) and layers two precision signals on top of slice 1:

1. A **symbol-name definition boost** — a file that *defines* `fn login` / `class Login` for the
   goal "fix login" is the strongest signal in code search and should outrank an incidental
   mention.
2. **Symbol-enriched select** — the ContextFinder's LLM-select prompt lists the matched symbol
   names per candidate file, giving the model better signal than path + score alone.

The candidate return shape stays **file-level** (`Vec<PathBuf>` to the Coder). The Coder reads and
rewrites whole files today, so spans have no consumer; returning spans is deferred to a later
slice when a consumer exists.

## Decisions (locked during brainstorming)

1. **Scope = symbol-enriched select** (not "return spans to the Coder"). tree-sitter sharpens
   ranking and enriches the LLM-select prompt; the orchestrator, wire types for context flow, and
   the Coder are untouched. The only `engine-core` change is an additive field on `Candidate`.
2. **Languages = Rust + JS/TS/Python/Go.** Five grammars bundled up front (Rust, JavaScript,
   TypeScript incl. TSX, Python, Go). Unsupported extensions and parse failures fall back to the
   slice-1 whole-file indexing already shipped.
3. **No-recall-regression invariant.** Chunking only *adds* signal. We keep slice 1's whole-file
   postings for **every** file, so any file that scored > 0 before scores ≥ that now. Chunk data
   layers precision on top; it never removes a candidate. Slice-1 ranking tests stay green.
4. **Schema migration via `FORMAT_VERSION` bump.** The existing version-mismatch → drop + rebuild
   path migrates an upgraded index cleanly; no hand-written migration.
5. **Symbol-name tokenization splits case boundaries** (`loginHandler` → `login`, `handler`).
   Query-side tokenization is unchanged, so index/query parity is preserved; richer name tokens
   only ever create more matchable tokens, never fewer.

## Non-goals (explicitly out of scope — later slices)

- **Returning symbol spans to the Coder** (changing `AgentOutput::Context` / `AgentRequest::Code`
  to carry spans and reworking the Coder to use them). Deferred until a consumer exists.
- **Best-chunk concentration bonus** (rewarding a file with all terms in one symbol over scattered
  hits). The name boost delivers the headline precision win; the concentration bonus is a possible
  follow-up.
- **Dropping redundant whole-file postings for fully-chunked files** (a storage optimization). We
  keep both to guarantee the no-recall-regression invariant for free.
- git-history ranking signal, vector/embedding index (later / v2 per `ARCHITECTURE.md`).
- Configurable language set / chunk weights, retrieval over `RemoteWorkspace` (a remote engine
  builds its own local index after promote/restore).

## Architecture

### Data flow (unchanged control flow; richer candidates)

```
IndexedRetriever.search(goal, limit)
  ├─ refresh(root)                      # stat-incremental; chunked files also (re)chunked
  ├─ whole-file content scores          # slice 1, unchanged → recall floor
  ├─ symbol-name hits per file          # NEW: query terms matching a symbol name
  ├─ matched symbols per file           # NEW: name-hit symbols first, then body-hit symbols
  └─ score = 5·path + whole_file_content + 8·name_hits
        └─► Vec<Candidate { path, score, symbols }>   (symbols best-first, capped)

ContextFinder:
  candidates ─► select_prompt lists "- {path} (score {s}) [symbols: a, b]" when symbols present
            ─► LLM rank/select (+ hallucination filter + lexical-top fallback) — shared, unchanged
            ─► Context { files: Vec<PathBuf> }    (Coder untouched)
```

### Dependency flow (inward, no cycles)

| Crate | Change |
|---|---|
| `engine-core` | `Candidate` gains an additive `symbols: Vec<String>` field. No new dep. Must not depend on `retrieval`. |
| `retrieval` | New `chunk` module; `tokenize`, `index`, `retriever` modified. Adds `tree-sitter` + 5 grammar crates. |
| `agents` | `ContextFinder::select_prompt` lists symbol names when present. No new crate dep (the field arrives via `Candidate`). |
| `engine` | No code change. On upgrade, the `FORMAT_VERSION` bump rebuilds existing indexes lazily on next `search`. |

### Component A — the chunker (`crates/retrieval/src/chunk.rs`, new)

Self-contained; the only module that touches tree-sitter.

```rust
pub enum SymbolKind { Function, Method, Type, Class, Other }

pub struct SymbolChunk {
    pub symbol: String,      // the defined name, e.g. "login_handler"
    pub kind: SymbolKind,
    pub start_line: usize,   // 1-based, for display/debug
    pub end_line: usize,
    pub text: String,        // the node's source span (for body tokenization)
}

/// Parse `content` into symbol chunks using the grammar selected by `path`'s extension.
/// Returns `None` for unsupported extensions, parse failure, or a file with no extractable
/// symbols — the caller then uses slice-1 whole-file indexing.
pub fn chunk_file(path: &Path, content: &str) -> Option<Vec<SymbolChunk>>;
```

**Language selection (by extension):** `rs`→rust; `js`/`jsx`/`mjs`/`cjs`→javascript;
`ts`→typescript; `tsx`→tsx; `py`→python; `go`→go. Anything else → `None`.

**Capture (per-language tree-sitter `.scm` query):** named definitions only —
- Rust: `function_item`, `impl_item` methods, `struct_item`, `enum_item`, `trait_item`,
  `type_item`.
- JS/TS: `function_declaration`, `method_definition`, `class_declaration`; TS adds
  `interface_declaration`, `type_alias_declaration`.
- Python: `function_definition`, `class_definition`.
- Go: `function_declaration`, `method_declaration`, `type_declaration`.

Each captured node yields a `SymbolChunk` with its name node's text, line span, and node source.
Determinism: tree-sitter parsing is a pure, deterministic function of input — same content always
yields the same chunks.

### Component B — tokenization (`crates/retrieval/src/tokenize.rs`, modify)

Add a name tokenizer used **only** for symbol names:

```rust
/// Tokens from a symbol name: split on non-alphanumeric AND case boundaries, lowercase,
/// length >= 3, de-duplicated. `loginHandler` -> ["login","handler"]; `LoginV2` -> ["login"].
pub fn name_tokens(symbol: &str) -> Vec<String>;
```

`index_tokens` (chunk-body and whole-file content) and `query_terms` (goal) are **unchanged**, so
index/query parity for content matching holds exactly as in slice 1. The richer name split only
adds matchable name tokens; a plain query term `login` still matches the name token `login`
extracted from `loginHandler`.

### Component C — index schema + refresh (`crates/retrieval/src/index.rs`, modify)

Bump `FORMAT_VERSION` (e.g. `"1"` → `"2"`); the existing mismatch path drops and recreates all
tables, rebuilding the index on the next `search` after an upgrade. New tables alongside the
unchanged `files` and `postings`:

```sql
CREATE TABLE chunks        (chunk_id INTEGER PRIMARY KEY,
                            file_id INTEGER NOT NULL,
                            symbol TEXT NOT NULL, kind TEXT NOT NULL,
                            start_line INTEGER NOT NULL, end_line INTEGER NOT NULL);
CREATE INDEX chunks_file   ON chunks(file_id);
CREATE TABLE chunk_postings(token TEXT NOT NULL, chunk_id INTEGER NOT NULL,
                            count INTEGER NOT NULL, PRIMARY KEY (token, chunk_id));
CREATE INDEX chunk_postings_token ON chunk_postings(token);
CREATE TABLE chunk_names   (token TEXT NOT NULL, chunk_id INTEGER NOT NULL,
                            PRIMARY KEY (token, chunk_id));
CREATE INDEX chunk_names_token ON chunk_names(token);
```

- `chunks`: one row per extracted symbol.
- `chunk_postings`: chunk-**body** tokens (via `index_tokens` over `SymbolChunk.text`) — powers the
  "which symbols matched by body" portion of the display list.
- `chunk_names`: symbol-**name** tokens (via `name_tokens`) — powers the score boost and the
  primary portion of the display list.

**Refresh** (extends slice-1 stat-incremental refresh):

1. For a new/changed file (stat compare unchanged), after writing whole-file `postings` exactly as
   today, call `chunk_file`:
   - `Some(chunks)`: delete this file's `chunks`/`chunk_postings`/`chunk_names`, then insert the new
     chunks and their body + name tokens.
   - `None`: ensure no stale chunk rows remain for this file (whole-file behavior only — identical
     to slice 1).
2. For a deleted/renamed file (gone from the walk): delete its `files` + `postings` **and** its
   `chunks`/`chunk_postings`/`chunk_names` (cascade by `file_id`).
3. Unchanged files: skipped (the amortization win), as in slice 1.

Whole-file postings are retained for **every** file — chunk tables are purely additive. Content
inside a function body is therefore present in both `postings` (whole-file) and `chunk_postings`
(per chunk), but the numeric score draws content only from whole-file `postings`, so there is no
double counting (see Component E). `chunk_postings` is consumed only for the display list.

New query methods:

```rust
/// Per file: how many distinct query terms hit a symbol NAME in that file.
async fn symbol_name_hits(&self, terms: &[String]) -> Result<HashMap<String /*path*/, u64>>;

/// Per file: matched symbol names, name-hit symbols first then body-hit symbols,
/// de-duplicated, capped per file.
async fn matched_symbols(&self, terms: &[String]) -> Result<HashMap<String /*path*/, Vec<String>>>;
```

### Component D — `Candidate` (`crates/engine-core/src/retrieval.rs`, modify)

```rust
pub struct Candidate {
    pub path: PathBuf,
    pub score: u64,
    pub symbols: Vec<String>,   // NEW: matched symbol names, best-first, capped (~5). Empty for
                                // the lexical fallback and for whole-file-only candidates.
}
```

Additive field. Existing `Candidate { path, score }` literals (retrieval + agents tests) gain
`symbols: Vec::new()` (or `vec![...]` where a test asserts enrichment).

### Component E — `IndexedRetriever::search` (`crates/retrieval/src/retriever.rs`, modify)

```
score(file) = 5 · path_hits                       (slice 1, unchanged)
            + whole_file_content_score            (slice 1, unchanged — the recall floor)
            + 8 · symbol_name_hits(file)          (NEW; bounded by #query terms)
```

- `symbol_name_hits(file)` = number of distinct query terms matching at least one symbol name in
  the file (bounded by `terms.len()`, so a file of many same-named tiny functions cannot dominate).
- Weight 8 places a name/definition hit above a path hit (5) — defining a symbol is a stronger
  signal than the term appearing in the path.
- `Candidate.symbols` = `matched_symbols(file)`, capped (~5), name-hit symbols first.
- Ranking unchanged: score desc, then path asc (deterministic); truncate to `limit`.

The no-recall-regression invariant holds: every term is `≥ 0`, so `score_slice2 ≥ score_slice1`
for every file; no file that previously had score > 0 drops to 0.

### Component F — `ContextFinder::select_prompt` (`crates/agents/src/context_finder.rs`, modify)

`select_prompt` formats each candidate as `- {path} (score {s})`, appending
` [symbols: name1, name2]` **only** when that candidate carries symbols. With empty `symbols`
(lexical fallback, or whole-file-only candidates) the prompt is byte-identical to today, so every
existing ContextFinder test stays green. The candidate-set / hallucination filter / lexical-top
fallback are unchanged; output remains `Context { files: Vec<PathBuf> }`.

The `ContextFinder` must thread `Candidate.symbols` from the retriever into the prompt — slice 1
collapses candidates to `(path, score)`, so this slice carries the symbols alongside.

## Dependencies

Added to the `retrieval` crate only:

- `tree-sitter` (the runtime)
- `tree-sitter-rust`, `tree-sitter-javascript`, `tree-sitter-typescript` (provides typescript +
  tsx), `tree-sitter-python`, `tree-sitter-go`

Grammar crate versions must be ABI-compatible with the pinned `tree-sitter` runtime version (the
plan pins exact versions and verifies they build together). These compile C via `cc` — build cost
is contained to the `retrieval` crate and has no runtime-determinism impact.

## Determinism & error handling

- **Determinism preserved.** Chunks, chunk postings, and name tokens are pure functions of file
  content; `mtime`/`size` only decide *whether* to re-parse, never the result. The engine-core
  offline suite runs the retriever-free (`None`) path and is untouched.
- **Chunking is best-effort, never a gate.** A parse failure or unsupported extension returns
  `None` → slice-1 whole-file indexing for that file. A single unreadable/unparsable file is
  skipped, not fatal. Retrieval as a whole remains an optimization: index failure still degrades to
  the ContextFinder's inline lexical pipeline (slice-1 behavior).
- **Security unchanged.** Chunking reads only files the walk already admitted; the sensitive-path
  floor (`.git`/`.env`/`.ssh`/`.aws`, dot-prefixed components, etc.) excludes secrets *before* any
  parse. No new filesystem access is introduced. Reaffirmed by test.

## Testing

All tests offline and tempfile-backed (no network, no keys):

- **`chunk`:** Rust file → extracts `fn`/`struct`/`impl` method/`enum`/`trait`/`type` names + spans;
  JS/TS → functions/classes/methods (+ TS interfaces/type aliases, TSX); Python → `def`/`class`;
  Go → `func`/`method`/`type`. Parse failure → `None`; unsupported extension → `None`; symbol-less
  file → `None`.
- **`tokenize`:** `name_tokens` splits camelCase/PascalCase/snake_case, lowercases, drops < 3,
  de-dupes; `query_terms`/`index_tokens` parity unchanged.
- **`index`:** schema present after `FORMAT_VERSION` bump (old-version DB is rebuilt, not read
  stale); refresh populates `chunks`/`chunk_postings`/`chunk_names` for a chunked file and only
  whole-file `postings` for an unsupported file; edit re-chunks; delete cascades all chunk rows.
- **`retriever`:** a file **defining** `fn login` outranks one that only mentions `login` in a
  comment (name boost); `Candidate.symbols` lists the matched name; an unsupported-language file
  matching only by content is still returned (no-recall regression); a slice-1 ranking expectation
  still holds (whole-file recall floor); sensitive/dot-prefixed paths never appear.
- **`context_finder`:** `select_prompt` includes `[symbols: …]` when candidates carry names and is
  unchanged when they do not; every existing ContextFinder test stays green (empty-symbols path).
- **workspace suite:** `cargo test --workspace` green; engine-core offline determinism suite
  untouched.

## What this unblocks

With symbol chunks in the index and `Candidate` carrying matched symbol names, later slices slot in
without touching the orchestrator or the file-level return contract: a best-chunk concentration
bonus, a git-history recency/co-change boost, returning symbol spans to a future span-aware Coder,
and (v2) a vector index over the same chunk substrate.
