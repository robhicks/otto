# Tree-sitter Symbol Chunking (retrieval slice 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add tree-sitter symbol chunking to the `retrieval` crate so the indexed retriever boosts files that *define* a symbol named after a goal term and lists matched symbol names — sharpening ranking and the ContextFinder's LLM-select prompt — while keeping the candidate return shape file-level.

**Architecture:** A new self-contained `chunk` module parses Rust/JS/TS/Python/Go files into symbol chunks via tree-sitter. The sqlite index gains `chunks`/`chunk_postings`/`chunk_names` tables (FORMAT_VERSION bump triggers a clean rebuild). `IndexedRetriever::search` adds `8·symbol_name_hits` on top of slice 1's `5·path + whole_file_content` score (a strictly additive, no-recall-regression change) and populates a new `Candidate.symbols` field. `ContextFinder` lists those names in its select prompt; the Coder and orchestrator are untouched.

**Tech Stack:** Rust (edition 2024), `tree-sitter` 0.24+ with the rust/javascript/typescript/python/go grammar crates, `sqlx` 0.8 (sqlite), `tokio`, `tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-06-24-retrieval-tree-sitter-chunking-design.md`

---

## File structure

- `crates/engine-core/src/retrieval.rs` (Modify) — add `symbols: Vec<String>` to `Candidate`; fix the one test literal.
- `crates/retrieval/Cargo.toml` (Modify) — add `tree-sitter` + 5 grammar deps.
- `crates/retrieval/src/chunk.rs` (Create) — `SymbolKind`, `SymbolChunk`, `chunk_file`. The only tree-sitter-aware module.
- `crates/retrieval/src/tokenize.rs` (Modify) — add `name_tokens` (case-splitting tokenizer for symbol names).
- `crates/retrieval/src/index.rs` (Modify) — new tables + FORMAT_VERSION bump; chunk-aware refresh; `symbol_name_hits` + `matched_symbols` queries.
- `crates/retrieval/src/retriever.rs` (Modify) — name boost into the score; populate `Candidate.symbols`.
- `crates/retrieval/src/lib.rs` (Modify) — declare `mod chunk;`.
- `crates/agents/src/context_finder.rs` (Modify) — thread symbols into `select_prompt`; update test literal; add tests.
- `CLAUDE.md`, `docs/ARCHITECTURE.md` (Modify) — record the slice.

---

### Task 1: Add `symbols` to `Candidate` (engine-core)

**Files:**
- Modify: `crates/engine-core/src/retrieval.rs`

- [ ] **Step 1: Add the field**

In `crates/engine-core/src/retrieval.rs`, replace the `Candidate` struct with:

```rust
/// A scored candidate file for a goal. Higher `score` is more relevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub score: u64,
    /// Matched symbol names for this file (best-first, capped), for display in the
    /// ContextFinder's select prompt. Empty for the lexical fallback and for files matched
    /// only by whole-file content (no symbol hit).
    pub symbols: Vec<String>,
}
```

- [ ] **Step 2: Fix the existing test literal**

In the same file's `#[cfg(test)] mod tests`, update `candidate_holds_path_and_score`:

```rust
    #[test]
    fn candidate_holds_path_and_score() {
        let c = Candidate {
            path: PathBuf::from("src/main.rs"),
            score: 7,
            symbols: Vec::new(),
        };
        assert_eq!(c.path, PathBuf::from("src/main.rs"));
        assert_eq!(c.score, 7);
        assert!(c.symbols.is_empty());
    }
```

- [ ] **Step 3: Run the test (engine-core compiles; downstream crates will break until later tasks)**

Run: `cargo test -p otto-engine-core retrieval::`
Expected: PASS.

Note: `cargo build --workspace` will now FAIL in `retrieval` (`retriever.rs`) and `agents` (`context_finder.rs` test) because their `Candidate { path, score }` literals lack the new field. That is expected — Tasks 7 and 8 fix them. Build those crates individually as their tasks land.

- [ ] **Step 4: Commit**

```bash
git add crates/engine-core/src/retrieval.rs
git commit -m "feat(engine-core): add Candidate.symbols for matched symbol names"
```

---

### Task 2: tree-sitter deps + the `chunk` module

**Files:**
- Modify: `crates/retrieval/Cargo.toml`
- Create: `crates/retrieval/src/chunk.rs`
- Modify: `crates/retrieval/src/lib.rs`

- [ ] **Step 1: Add the dependencies**

From the repo root, add the runtime + grammar crates to `otto-retrieval` (this fetches the current mutually-compatible versions):

```bash
cargo add --package otto-retrieval tree-sitter tree-sitter-rust tree-sitter-javascript tree-sitter-typescript tree-sitter-python tree-sitter-go
```

Expected: `crates/retrieval/Cargo.toml` `[dependencies]` now lists those six crates. Versions may differ from the example below; that is fine. The only API requirements the rest of this task relies on are: each grammar exposes a `LANGUAGE` constant (TypeScript exposes `LANGUAGE_TYPESCRIPT` and `LANGUAGE_TSX`) and `tree_sitter::Language` implements `From<LanguageFn>`. Example resulting block:

```toml
tree-sitter = "0.24"
tree-sitter-rust = "0.23"
tree-sitter-javascript = "0.23"
tree-sitter-typescript = "0.23"
tree-sitter-python = "0.23"
tree-sitter-go = "0.23"
```

- [ ] **Step 2: Write the failing tests + the chunker**

Create `crates/retrieval/src/chunk.rs`:

```rust
//! Tree-sitter symbol chunking: parse a supported-language file into its named definitions
//! (functions, methods, types, classes) with their source spans. The ONLY tree-sitter-aware
//! module. `chunk_file` returns `None` for unsupported extensions or a file with no extractable
//! symbols — the caller then falls back to slice-1 whole-file indexing. Parsing is deterministic,
//! so chunks are a pure function of the file's content.

use std::path::Path;

use tree_sitter::{Language, Node, Parser};

/// The kind of a defined symbol. Coarse on purpose — used only for index metadata/debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Type,
    Class,
}

impl SymbolKind {
    /// Stable lowercase tag stored in the `chunks.kind` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Type => "type",
            SymbolKind::Class => "class",
        }
    }
}

/// One extracted symbol: its name, kind, 1-based line span, and source text (for body tokens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolChunk {
    pub symbol: String,
    pub kind: SymbolKind,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

/// Select a tree-sitter grammar by the path's file extension. `None` => unsupported language.
fn language_for(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    let lang: Language = match ext.as_str() {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "py" => tree_sitter_python::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        _ => return None,
    };
    Some(lang)
}

/// Map a tree-sitter node kind to a `SymbolKind`, across all supported grammars. `None` => not a
/// definition we capture. Node kinds are consistent enough across grammars to share one table.
fn symbol_kind(node_kind: &str) -> Option<SymbolKind> {
    Some(match node_kind {
        // functions (rust/js/go/python) + rust trait-method signatures
        "function_item" | "function_declaration" | "function_definition"
        | "function_signature_item" => SymbolKind::Function,
        // methods (js/ts/go)
        "method_definition" | "method_declaration" => SymbolKind::Method,
        // types (rust struct/enum/trait/type-alias; go type_spec; ts interface/type-alias)
        "struct_item" | "enum_item" | "trait_item" | "type_item" | "type_spec"
        | "type_declaration" | "type_alias_declaration" | "interface_declaration" => {
            SymbolKind::Type
        }
        // classes (js/ts/python)
        "class_declaration" | "class_definition" => SymbolKind::Class,
        _ => return None,
    })
}

/// Recursively collect named definitions. Recursion is required so methods nested in `impl`/class
/// bodies are captured. A node with a captured kind but no `name` field (e.g. Go `type_declaration`
/// wraps a `type_spec` that carries the name) is skipped here and picked up via its child.
fn collect(node: Node, src: &[u8], out: &mut Vec<SymbolChunk>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(kind) = symbol_kind(child.kind()) {
            if let Some(name) = child.child_by_field_name("name") {
                if let Ok(symbol) = name.utf8_text(src) {
                    out.push(SymbolChunk {
                        symbol: symbol.to_string(),
                        kind,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        text: child.utf8_text(src).unwrap_or_default().to_string(),
                    });
                }
            }
        }
        collect(child, src, out);
    }
}

/// Parse `content` (using the grammar chosen by `path`'s extension) into symbol chunks. `None` for
/// unsupported extensions or a file with no extractable symbols.
pub fn chunk_file(path: &Path, content: &str) -> Option<Vec<SymbolChunk>> {
    let lang = language_for(path)?;
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(content, None)?;
    let mut out = Vec::new();
    collect(tree.root_node(), content.as_bytes(), &mut out);
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn names(path: &str, src: &str) -> Vec<String> {
        chunk_file(&PathBuf::from(path), src)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.symbol)
            .collect()
    }

    #[test]
    fn rust_extracts_fn_struct_and_method() {
        let n = names(
            "src/auth.rs",
            "fn login() {}\nstruct Session {}\nimpl Session { fn renew(&self) {} }\n",
        );
        assert!(n.contains(&"login".to_string()), "{n:?}");
        assert!(n.contains(&"Session".to_string()), "{n:?}");
        assert!(n.contains(&"renew".to_string()), "{n:?}");
    }

    #[test]
    fn javascript_extracts_function_and_class() {
        let n = names("app.js", "function loginUser() {}\nclass Auth { handle() {} }\n");
        assert!(n.contains(&"loginUser".to_string()), "{n:?}");
        assert!(n.contains(&"Auth".to_string()), "{n:?}");
        assert!(n.contains(&"handle".to_string()), "{n:?}");
    }

    #[test]
    fn typescript_extracts_interface_and_function() {
        let n = names("api.ts", "interface User { id: number }\nfunction getUser(): User { return null as any }\n");
        assert!(n.contains(&"User".to_string()), "{n:?}");
        assert!(n.contains(&"getUser".to_string()), "{n:?}");
    }

    #[test]
    fn python_extracts_def_and_class() {
        let n = names("svc.py", "def login():\n    pass\nclass Session:\n    def renew(self):\n        pass\n");
        assert!(n.contains(&"login".to_string()), "{n:?}");
        assert!(n.contains(&"Session".to_string()), "{n:?}");
        assert!(n.contains(&"renew".to_string()), "{n:?}");
    }

    #[test]
    fn go_extracts_func_and_type() {
        let n = names("svc.go", "package main\nfunc Login() {}\ntype Session struct {}\n");
        assert!(n.contains(&"Login".to_string()), "{n:?}");
        assert!(n.contains(&"Session".to_string()), "{n:?}");
    }

    #[test]
    fn unsupported_extension_returns_none() {
        assert!(chunk_file(&PathBuf::from("notes.md"), "# login\nfn login").is_none());
        assert!(chunk_file(&PathBuf::from("Makefile"), "all:\n\tcargo build").is_none());
    }

    #[test]
    fn symbol_less_file_returns_none() {
        // A Rust file with only a const has no captured definitions -> whole-file fallback.
        assert!(chunk_file(&PathBuf::from("c.rs"), "const MAX: i32 = 5;\n").is_none());
    }

    #[test]
    fn span_lines_are_one_based() {
        let chunks = chunk_file(&PathBuf::from("a.rs"), "\nfn second_line() {}\n").unwrap();
        let c = chunks.iter().find(|c| c.symbol == "second_line").unwrap();
        assert_eq!(c.start_line, 2);
    }
}
```

- [ ] **Step 3: Declare the module**

In `crates/retrieval/src/lib.rs`, add `mod chunk;` to the module list (alongside `mod index;` etc.). Do not re-export anything (the chunker is crate-internal).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p otto-retrieval chunk::`
Expected: PASS (all 8 chunk tests). If a grammar fails to resolve at build time, adjust its version per the Step 1 note (the API contract is `LANGUAGE`/`LANGUAGE_TYPESCRIPT`/`LANGUAGE_TSX` + `Language: From<LanguageFn>`).

- [ ] **Step 5: Commit**

```bash
git add crates/retrieval/Cargo.toml crates/retrieval/src/chunk.rs crates/retrieval/src/lib.rs Cargo.lock
git commit -m "feat(retrieval): tree-sitter chunker for rust/js/ts/python/go"
```

---

### Task 3: `name_tokens` (case-splitting symbol-name tokenizer)

**Files:**
- Modify: `crates/retrieval/src/tokenize.rs`

- [ ] **Step 1: Write the failing tests + implementation**

In `crates/retrieval/src/tokenize.rs`, add this function above the `#[cfg(test)]` block (keep `index_tokens` and `query_terms` unchanged so index/query parity for content is preserved):

```rust
/// Split a symbol name into match tokens: break on non-alphanumeric AND on lower/digit->upper case
/// boundaries, lowercase, keep length >= 3, de-duplicate. So `loginHandler` and `login_handler`
/// both yield ["login","handler"], letting a plain goal term like "login" match a symbol name that
/// the content tokenizer (which does not case-split) would store as one token. Names only; the
/// query side stays `query_terms`, so this never breaks parity.
pub fn name_tokens(symbol: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for word in symbol.split(|c: char| !c.is_alphanumeric()) {
        for piece in split_case(word) {
            let t = piece.to_lowercase();
            if t.len() >= 3 && seen.insert(t.clone()) {
                out.push(t);
            }
        }
    }
    out
}

/// Split a single alphanumeric word at lower/digit -> upper transitions (camelCase / PascalCase).
fn split_case(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for c in word.chars() {
        if let Some(p) = prev {
            if c.is_uppercase() && (p.is_lowercase() || p.is_numeric()) {
                parts.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
        prev = Some(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}
```

Add these tests inside the existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn name_tokens_splits_snake_and_camel() {
        assert_eq!(name_tokens("login_handler"), vec!["login", "handler"]);
        assert_eq!(name_tokens("loginHandler"), vec!["login", "handler"]);
    }

    #[test]
    fn name_tokens_drops_short_and_dedupes() {
        // "V2" -> "v2" (len 2) dropped; repeated "Login" de-duplicated to one.
        assert_eq!(name_tokens("LoginV2"), vec!["login"]);
        assert_eq!(name_tokens("LoginLogin"), vec!["login"]);
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p otto-retrieval tokenize::`
Expected: PASS (existing parity tests + the two new ones).

- [ ] **Step 3: Commit**

```bash
git add crates/retrieval/src/tokenize.rs
git commit -m "feat(retrieval): name_tokens case-splitting tokenizer for symbol names"
```

---

### Task 4: Index schema — chunk tables + FORMAT_VERSION bump

**Files:**
- Modify: `crates/retrieval/src/index.rs`

- [ ] **Step 1: Bump the format version**

In `crates/retrieval/src/index.rs`, change:

```rust
const FORMAT_VERSION: &str = "1";
```

to:

```rust
const FORMAT_VERSION: &str = "2";
```

- [ ] **Step 2: Drop + create the chunk tables in `init_schema`**

In `init_schema`, extend the rebuild-on-mismatch drop block (currently drops `postings` then `files`) to also drop the chunk tables. Replace the `if stored.as_deref() != Some(FORMAT_VERSION) { ... }` block with:

```rust
        if stored.as_deref() != Some(FORMAT_VERSION) {
            for tbl in ["chunk_postings", "chunk_names", "chunks", "postings", "files"] {
                sqlx::query(&format!("DROP TABLE IF EXISTS {tbl}"))
                    .execute(&self.pool)
                    .await?;
            }
        }
```

Then, after the existing `CREATE TABLE IF NOT EXISTS postings (...)` and its `postings_token` index, add the chunk tables (before the `INSERT OR REPLACE INTO meta ...` line):

```rust
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS chunks (
                chunk_id INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL,
                symbol TEXT NOT NULL,
                kind TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS chunk_postings (
                token TEXT NOT NULL,
                chunk_id INTEGER NOT NULL,
                count INTEGER NOT NULL,
                PRIMARY KEY (token, chunk_id)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS chunk_postings_token ON chunk_postings(token)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS chunk_names (
                token TEXT NOT NULL,
                chunk_id INTEGER NOT NULL,
                PRIMARY KEY (token, chunk_id)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS chunk_names_token ON chunk_names(token)")
            .execute(&self.pool)
            .await?;
```

- [ ] **Step 3: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `index.rs`:

```rust
    #[tokio::test]
    async fn schema_has_chunk_tables_after_open() {
        let dir = tempfile::tempdir().unwrap();
        let idx = Index::open(dir.path().join("idx.sqlite")).await.unwrap();
        // The three chunk tables exist (querying an empty table returns 0 rows, not an error).
        for tbl in ["chunks", "chunk_postings", "chunk_names"] {
            let n: i64 = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {tbl}"))
                .fetch_one(&idx.pool)
                .await
                .unwrap()
                .get("n");
            assert_eq!(n, 0, "{tbl} should exist and be empty");
        }
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p otto-retrieval index::`
Expected: PASS (existing index tests — which rebuild cleanly under the new version — plus the new schema test).

- [ ] **Step 5: Commit**

```bash
git add crates/retrieval/src/index.rs
git commit -m "feat(retrieval): chunk index tables + FORMAT_VERSION bump to 2"
```

---

### Task 5: Chunk-aware refresh + cascade delete

**Files:**
- Modify: `crates/retrieval/src/index.rs`

- [ ] **Step 1: Add a chunk-delete helper**

In `impl Index`, add this private helper (above `refresh`). It deletes a file's chunk rows on a transaction or the pool — `E` is any sqlx sqlite executor:

```rust
    /// Delete all chunk rows (chunks + their postings + name tokens) for one file. Used both when
    /// a file vanishes and before re-chunking a changed file.
    async fn delete_chunks<'e, E>(executor: E, file_id: i64) -> anyhow::Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Sqlite> + Copy,
    {
        sqlx::query(
            "DELETE FROM chunk_postings WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE file_id = ?1)",
        )
        .bind(file_id)
        .execute(executor)
        .await?;
        sqlx::query(
            "DELETE FROM chunk_names WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE file_id = ?1)",
        )
        .bind(file_id)
        .execute(executor)
        .await?;
        sqlx::query("DELETE FROM chunks WHERE file_id = ?1")
            .bind(file_id)
            .execute(executor)
            .await?;
        Ok(())
    }
```

- [ ] **Step 2: Cascade chunk deletion for vanished files**

In `refresh`, inside the loop that deletes rows for files no longer present, add a chunk cascade. Replace that loop body so it reads:

```rust
        // Delete rows for files no longer present (handles deletions/renames).
        for (path, (file_id, _, _)) in &existing {
            if !present.contains_key(path) {
                Self::delete_chunks(&self.pool, *file_id).await?;
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
```

- [ ] **Step 3: Write chunk rows for new/changed files**

In `refresh`, inside the per-file re-index transaction, after the existing loop that inserts whole-file `postings` (the `for (token, count) in tokens { ... }` loop) and BEFORE `tx.commit().await?;`, insert the chunk data:

```rust
            // Chunk the file (best-effort). `None` (unsupported language or no symbols) leaves
            // only the whole-file postings above — identical to slice 1. Replace any prior chunks.
            Self::delete_chunks(&mut *tx, file_id).await?;
            if let Some(chunks) = crate::chunk::chunk_file(&entry.path, &content) {
                for ch in chunks {
                    let chunk_id: i64 = sqlx::query(
                        "INSERT INTO chunks (file_id, symbol, kind, start_line, end_line)
                         VALUES (?1, ?2, ?3, ?4, ?5) RETURNING chunk_id",
                    )
                    .bind(file_id)
                    .bind(&ch.symbol)
                    .bind(ch.kind.as_str())
                    .bind(ch.start_line as i64)
                    .bind(ch.end_line as i64)
                    .fetch_one(&mut *tx)
                    .await?
                    .get("chunk_id");
                    for (token, count) in crate::tokenize::index_tokens(&ch.text) {
                        sqlx::query(
                            "INSERT INTO chunk_postings (token, chunk_id, count) VALUES (?1, ?2, ?3)",
                        )
                        .bind(token)
                        .bind(chunk_id)
                        .bind(count)
                        .execute(&mut *tx)
                        .await?;
                    }
                    for token in crate::tokenize::name_tokens(&ch.symbol) {
                        sqlx::query(
                            "INSERT OR IGNORE INTO chunk_names (token, chunk_id) VALUES (?1, ?2)",
                        )
                        .bind(token)
                        .bind(chunk_id)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
            }
```

Note: `delete_chunks(&mut *tx, ...)` requires a `Copy` executor; `&mut *tx` is not `Copy`. Since the chunks for a freshly-upserted/replaced file are removed transactionally here, call the pooled form is wrong inside a tx. To keep the `Copy` bound simple, inline the three deletes here instead of calling the helper. Replace the `Self::delete_chunks(&mut *tx, file_id).await?;` line above with:

```rust
            sqlx::query("DELETE FROM chunk_postings WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE file_id = ?1)")
                .bind(file_id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM chunk_names WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE file_id = ?1)")
                .bind(file_id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM chunks WHERE file_id = ?1")
                .bind(file_id).execute(&mut *tx).await?;
```

And keep `delete_chunks` (the helper from Step 1) for the vanished-file path in Step 2, where `&self.pool` IS `Copy`.

- [ ] **Step 4: Write the failing test**

Add to `index.rs` tests:

```rust
    #[tokio::test]
    async fn refresh_populates_and_updates_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        seed(root, "auth.rs", b"fn login() {}\nstruct Session {}\n");
        seed(root, "notes.md", b"login notes here"); // unsupported -> no chunks
        let idx = Index::open(dbdir.path().join("idx.sqlite")).await.unwrap();
        idx.refresh(root).await.unwrap();

        // auth.rs produced chunk rows; notes.md did not.
        let auth_chunks: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM chunks c JOIN files f ON f.file_id=c.file_id WHERE f.path='auth.rs'",
        )
        .fetch_one(&idx.pool).await.unwrap().get("n");
        assert_eq!(auth_chunks, 2, "fn login + struct Session");
        let md_chunks: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM chunks c JOIN files f ON f.file_id=c.file_id WHERE f.path='notes.md'",
        )
        .fetch_one(&idx.pool).await.unwrap().get("n");
        assert_eq!(md_chunks, 0);

        // Edit auth.rs to rename the function; re-chunk replaces old rows (no "login" name left).
        seed(root, "auth.rs", b"fn logout() {}\nstruct Session {}\n");
        idx.refresh(root).await.unwrap();
        let login_names: i64 = sqlx::query("SELECT COUNT(*) AS n FROM chunk_names WHERE token='login'")
            .fetch_one(&idx.pool).await.unwrap().get("n");
        assert_eq!(login_names, 0, "renamed-away symbol name is gone");
    }

    #[tokio::test]
    async fn refresh_cascades_chunk_deletion_on_file_removal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        seed(root, "gone.rs", b"fn login() {}\n");
        let idx = Index::open(dbdir.path().join("idx.sqlite")).await.unwrap();
        idx.refresh(root).await.unwrap();
        assert!(
            sqlx::query("SELECT COUNT(*) AS n FROM chunks").fetch_one(&idx.pool).await.unwrap().get::<i64, _>("n") > 0
        );

        std::fs::remove_file(root.join("gone.rs")).unwrap();
        idx.refresh(root).await.unwrap();
        for tbl in ["chunks", "chunk_postings", "chunk_names"] {
            let n: i64 = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {tbl}"))
                .fetch_one(&idx.pool).await.unwrap().get("n");
            assert_eq!(n, 0, "{tbl} cascaded on file removal");
        }
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p otto-retrieval index::`
Expected: PASS (all index tests, including the two new chunk tests).

- [ ] **Step 6: Commit**

```bash
git add crates/retrieval/src/index.rs
git commit -m "feat(retrieval): chunk-aware refresh with cascade delete"
```

---

### Task 6: Index queries — `symbol_name_hits` + `matched_symbols`

**Files:**
- Modify: `crates/retrieval/src/index.rs`

- [ ] **Step 1: Add the two query methods**

In `impl Index`, after `content_scores`, add:

```rust
    /// Per file: how many DISTINCT query terms matched at least one symbol NAME in that file.
    /// Drives the name/definition boost. Returns (relative-path-string, distinct-term-hits).
    pub async fn symbol_name_hits(
        &self,
        terms: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, u64>> {
        if terms.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = terms.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT f.path AS path, COUNT(DISTINCT cn.token) AS hits
             FROM chunk_names cn
             JOIN chunks c ON c.chunk_id = cn.chunk_id
             JOIN files f ON f.file_id = c.file_id
             WHERE cn.token IN ({placeholders})
             GROUP BY f.file_id",
        );
        let mut q = sqlx::query(&sql);
        for t in terms {
            q = q.bind(t);
        }
        Ok(q
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("path"),
                    r.get::<i64, _>("hits").max(0) as u64,
                )
            })
            .collect())
    }

    /// Per file: matched symbol names — those whose NAME matched a term first (alphabetical), then
    /// those whose BODY matched (by descending body score), de-duplicated and capped per file.
    pub async fn matched_symbols(
        &self,
        terms: &[String],
        cap_per_file: usize,
    ) -> anyhow::Result<std::collections::HashMap<String, Vec<String>>> {
        use std::collections::HashMap;
        if terms.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = terms.iter().map(|_| "?").collect::<Vec<_>>().join(",");

        // Name-hit symbols (alphabetical, deterministic).
        let name_sql = format!(
            "SELECT DISTINCT f.path AS path, c.symbol AS symbol
             FROM chunk_names cn
             JOIN chunks c ON c.chunk_id = cn.chunk_id
             JOIN files f ON f.file_id = c.file_id
             WHERE cn.token IN ({placeholders})
             ORDER BY f.path, c.symbol",
        );
        // Body-hit symbols (by descending summed body count, then symbol for ties).
        let body_sql = format!(
            "SELECT f.path AS path, c.symbol AS symbol, SUM(cp.count) AS sc
             FROM chunk_postings cp
             JOIN chunks c ON c.chunk_id = cp.chunk_id
             JOIN files f ON f.file_id = c.file_id
             WHERE cp.token IN ({placeholders})
             GROUP BY c.chunk_id
             ORDER BY f.path, sc DESC, c.symbol",
        );

        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        let mut bind_all = |sql: &str| {
            let mut q = sqlx::query(sql);
            for t in terms {
                q = q.bind(t);
            }
            q
        };

        for row in bind_all(&name_sql).fetch_all(&self.pool).await? {
            let path: String = row.get("path");
            let symbol: String = row.get("symbol");
            let v = out.entry(path).or_default();
            if v.len() < cap_per_file && !v.contains(&symbol) {
                v.push(symbol);
            }
        }
        for row in bind_all(&body_sql).fetch_all(&self.pool).await? {
            let path: String = row.get("path");
            let symbol: String = row.get("symbol");
            let v = out.entry(path).or_default();
            if v.len() < cap_per_file && !v.contains(&symbol) {
                v.push(symbol);
            }
        }
        Ok(out)
    }
```

- [ ] **Step 2: Write the failing test**

Add to `index.rs` tests:

```rust
    #[tokio::test]
    async fn symbol_name_hits_and_matched_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        // defines `fn login` (name hit) + body mentions "session"
        seed(root, "auth.rs", b"fn login() {\n    let session = 1;\n}\n");
        // only a body mention of "login" inside `fn helper`
        seed(root, "util.rs", b"fn helper() {\n    let login = 1;\n}\n");
        let idx = Index::open(dbdir.path().join("idx.sqlite")).await.unwrap();
        idx.refresh(root).await.unwrap();

        let hits = idx.symbol_name_hits(&["login".to_string()]).await.unwrap();
        assert_eq!(hits.get("auth.rs"), Some(&1), "auth.rs defines fn login");
        assert!(!hits.contains_key("util.rs"), "util.rs only mentions login in a body");

        let syms = idx.matched_symbols(&["login".to_string()], 5).await.unwrap();
        assert_eq!(syms.get("auth.rs"), Some(&vec!["login".to_string()]));
        // util.rs matched by body -> its enclosing symbol `helper` is listed.
        assert_eq!(syms.get("util.rs"), Some(&vec!["helper".to_string()]));
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-retrieval index::symbol_name_hits_and_matched_symbols`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/retrieval/src/index.rs
git commit -m "feat(retrieval): symbol_name_hits + matched_symbols index queries"
```

---

### Task 7: `IndexedRetriever::search` — name boost + symbols

**Files:**
- Modify: `crates/retrieval/src/retriever.rs`

- [ ] **Step 1: Update `search` to add the name boost and populate symbols**

In `crates/retrieval/src/retriever.rs`, replace the body of `search` (from the `content` map down through `Ok(scored)`) with:

```rust
        // Content scores from the index (all files), keyed by relative path string (slice 1).
        let content: std::collections::HashMap<String, u64> = self
            .index
            .content_scores(&terms)
            .await?
            .into_iter()
            .collect();
        // NEW: symbol-name definition hits (distinct terms per file) and matched symbol names.
        let name_hits = self.index.symbol_name_hits(&terms).await?;
        let mut matched = self.index.matched_symbols(&terms, SYMBOLS_PER_FILE).await?;

        // Path scores computed from the entries returned by refresh (no second walk).
        // Score: 5*path_hits + whole_file_content + 8*name_hits. The first two terms are slice 1
        // unchanged (the recall floor); the name boost is strictly additive, so no file that
        // previously scored > 0 can drop out.
        let mut scored: Vec<Candidate> = entries
            .into_iter()
            .filter_map(|e| {
                let path_str = e.path.to_string_lossy().to_lowercase();
                let path_hits: u64 = terms
                    .iter()
                    .map(|t| path_str.matches(t.as_str()).count() as u64)
                    .sum();
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
            })
            .collect();

        // Rank by score desc, path asc (deterministic); cap at limit.
        scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        scored.truncate(limit);
        Ok(scored)
```

Add the cap constant near the top of the file (after the `use` block):

```rust
/// Max symbol names attached to a single candidate (for the select prompt).
const SYMBOLS_PER_FILE: usize = 5;
```

Also update the module doc comment's score description (line ~3) from `5*path_hits + content_score` to `5*path_hits + content_score + 8*symbol_name_hits`.

- [ ] **Step 2: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `retriever.rs`:

```rust
    #[tokio::test]
    async fn definition_outranks_mention_and_lists_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        // auth.rs DEFINES `fn login` (name hit, no "login" in the path).
        seed_in(root, "auth.rs", b"fn login() {}\n");
        // mentions.rs only mentions login in a comment-ish body.
        seed_in(root, "mentions.rs", b"fn handle() {\n    let login = 1;\n}\n");
        let r = IndexedRetriever::open(root.to_path_buf(), dbdir.path().join("idx.sqlite"))
            .await
            .unwrap();
        let cands = r.search("login", 8).await.unwrap();
        assert_eq!(cands.first().map(|c| c.path.clone()), Some(PathBuf::from("auth.rs")));
        let auth = cands.iter().find(|c| c.path == PathBuf::from("auth.rs")).unwrap();
        assert!(auth.symbols.contains(&"login".to_string()), "{:?}", auth.symbols);
    }

    #[tokio::test]
    async fn unsupported_language_content_match_still_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        // No chunks for .md, but whole-file content still indexes "login" (no-regression).
        seed_in(root, "notes.md", b"login instructions live here");
        let r = IndexedRetriever::open(root.to_path_buf(), dbdir.path().join("idx.sqlite"))
            .await
            .unwrap();
        let files: Vec<_> = r.search("login", 8).await.unwrap().into_iter().map(|c| c.path).collect();
        assert!(files.contains(&PathBuf::from("notes.md")), "{files:?}");
    }
```

The existing tests in this file place the db under `root` via the `retriever`/`seed` helpers. The two new tests use a separate db dir, so add a sibling seed helper next to the existing `seed`:

```rust
    fn seed_in(root: &Path, rel: &str, bytes: &[u8]) {
        seed(root, rel, bytes);
    }
```

(The alias keeps the new tests readable; `seed` already does the work.)

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-retrieval`
Expected: PASS — the new tests plus every slice-1 retriever test (`ranks_path_hit_above_content_hit`, `content_only_match_beyond_old_read_budget_is_found`, `sensitive_paths_never_appear`, `empty_workspace_returns_nothing`, `large_file_is_still_a_candidate`) still green, proving no-recall-regression.

- [ ] **Step 4: Commit**

```bash
git add crates/retrieval/src/retriever.rs
git commit -m "feat(retrieval): symbol-name boost + Candidate.symbols in search"
```

---

### Task 8: `ContextFinder` select-prompt enrichment

**Files:**
- Modify: `crates/agents/src/context_finder.rs`

- [ ] **Step 1: Thread symbols into `select_prompt`**

In `crates/agents/src/context_finder.rs`, change `select_prompt` to take a symbols map and list names when present:

```rust
fn select_prompt(
    goal: &str,
    candidates: &[(String, u64)],
    symbols_by_path: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    let listed = candidates
        .iter()
        .map(|(p, s)| match symbols_by_path.get(p) {
            Some(syms) if !syms.is_empty() => {
                format!("- {p} (score {s}) [symbols: {}]", syms.join(", "))
            }
            _ => format!("- {p} (score {s})"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are otto's context finder. From the candidate files below, choose up to {SELECT_LIMIT} \
         files most relevant to the goal, most relevant first.\n\
         Goal: {goal}\n\
         Candidates:\n{listed}\n\
         Respond ONLY with valid JSON: an object with a string-array field named files, each an \
         exact path copied from the candidates."
    )
}
```

- [ ] **Step 2: Build the symbols map in `run` and pass it through**

In `impl Agent for ContextFinder`, replace the `let scored: Vec<(String, u64)> = match ctx.retriever() { ... };` block with one that also collects symbols:

```rust
        // Produce the scored candidate set (+ matched symbol names) from the retriever when wired,
        // else the deterministic lexical pipeline. A retriever ERROR falls back to the lexical
        // pipeline — retrieval is an optimization, never a gate. The lexical path has no symbols.
        let mut symbols_by_path: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let scored: Vec<(String, u64)> = match ctx.retriever() {
            Some(retriever) => match retriever.search(&goal, CANDIDATE_LIMIT).await {
                Ok(candidates) => candidates
                    .into_iter()
                    .map(|c| {
                        let p = c.path.to_string_lossy().into_owned();
                        if !c.symbols.is_empty() {
                            symbols_by_path.insert(p.clone(), c.symbols);
                        }
                        (p, c.score)
                    })
                    .collect(),
                Err(e) => {
                    eprintln!(
                        "warning: retriever search failed ({e}); falling back to lexical scan"
                    );
                    self.lexical_candidates(&goal, ctx).await
                }
            },
            None => self.lexical_candidates(&goal, ctx).await,
        };
```

Then update the single `select_prompt(&goal, &scored)` call site to:

```rust
                    prompt: select_prompt(&goal, &scored, &symbols_by_path),
```

- [ ] **Step 3: Update the existing test `Candidate` literal**

In the `tests` module, the `StubRetriever` in `retriever_candidates_supersede_the_lexical_scan` builds a `Candidate`; add the field:

```rust
        let retriever = StubRetriever(vec![Candidate {
            path: PathBuf::from("from_retriever.rs"),
            score: 99,
            symbols: Vec::new(),
        }]);
```

- [ ] **Step 4: Write the new tests**

Add to the `tests` module:

```rust
    #[test]
    fn select_prompt_lists_symbols_only_when_present() {
        let cands = vec![("a.rs".to_string(), 10u64), ("b.rs".to_string(), 5u64)];
        let mut syms = std::collections::HashMap::new();
        syms.insert("a.rs".to_string(), vec!["login".to_string(), "logout".to_string()]);
        let p = select_prompt("goal", &cands, &syms);
        assert!(p.contains("- a.rs (score 10) [symbols: login, logout]"), "{p}");
        assert!(p.contains("- b.rs (score 5)"), "{p}");
        assert!(!p.contains("- b.rs (score 5) [symbols"), "{p}");
    }

    #[tokio::test]
    async fn select_prompt_symbols_reach_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "a.rs", "fn x() {}").await;
        seed(&ws, "b.rs", "fn y() {}").await;
        let tools = registry(dir.path());
        // The retriever ranks a.rs first; only a.rs carries a symbol. The scripted provider picks
        // b.rs ONLY if the prompt contained "[symbols: alpha]" — distinguishing it from the
        // lexical-top fallback (which would be [a.rs, b.rs]).
        let retriever = StubRetriever(vec![
            Candidate { path: PathBuf::from("a.rs"), score: 10, symbols: vec!["alpha".to_string()] },
            Candidate { path: PathBuf::from("b.rs"), score: 5, symbols: Vec::new() },
        ]);
        let provider =
            ScriptedProvider::new("{}").on("[symbols: alpha]", r#"{"files": ["b.rs"]}"#);
        let router = SingleProviderRouter::new(Arc::new(provider));
        let ctx = AgentCtx::new(&router, &ws, &tools).with_retriever(&retriever);
        let out = ContextFinder
            .run(AgentRequest::FindContext { goal: "anything".into() }, &ctx)
            .await
            .unwrap();
        let AgentOutput::Context { files } = out else { panic!("expected Context") };
        assert_eq!(files, vec![PathBuf::from("b.rs")], "symbol-enriched prompt drove the pick");
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p otto-agents context_finder::`
Expected: PASS — the two new tests plus every existing ContextFinder test (the retriever-less ones use the lexical path → empty `symbols_by_path` → prompt unchanged).

- [ ] **Step 6: Commit**

```bash
git add crates/agents/src/context_finder.rs
git commit -m "feat(agents): ContextFinder lists matched symbols in the select prompt"
```

---

### Task 9: Full verification + docs

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Full offline suite, format, lint**

Run: `cargo test --workspace`
Expected: PASS (the engine-core offline determinism suite is untouched; new retrieval + agents tests pass).

Run: `cargo fmt --all`
Expected: formats any new files; re-run `cargo fmt --all -- --check` → no diff.

Run: `cargo clippy --workspace --all-targets`
Expected: no new warnings (fix any in the new code — e.g. needless clones, `or_default` suggestions).

- [ ] **Step 2: Smoke-test the `run` path end to end**

Run: `cargo run -p otto-engine -- run "improve login handling" --root .`
Expected: a normal turn runs. The first run rebuilds the index under the OS cache dir at FORMAT_VERSION 2 (an existing v1 index from slice 1 is dropped and rebuilt — the first post-upgrade search re-parses the tree); a second run reuses it. No panic and no error about retrieval (a warning + lexical fallback is acceptable if the cache dir is unavailable).

- [ ] **Step 3: Update `CLAUDE.md`**

In `CLAUDE.md`, replace the `retrieval` crate-table row with one that records chunking:

```markdown
| `retrieval` | Persistent inverted index behind the `Retriever` seam (defined in `engine-core`). `IndexedRetriever` keeps a standalone sqlite index (token→file postings) in the OS cache dir keyed by workspace root, refreshed stat-incrementally (mtime+size) and atomically per file, and scores content for every indexed file — removing the ContextFinder's per-turn read budget. Tree-sitter symbol chunking (Rust/JS/TS/Python/Go) adds a symbol-name *definition* boost and surfaces matched symbol names per candidate; unsupported languages fall back to whole-file indexing. Mirrors the gate's sensitive-path floor (secrets never indexed; the walk is the sole defense since the index reads files directly). Depends inward on `engine-core`. |
```

Also update the prose sentence that describes the ContextFinder sourcing candidates from the indexed `Retriever`: append that the retriever now boosts symbol-name definitions and the select prompt lists matched symbol names.

- [ ] **Step 4: Update `docs/ARCHITECTURE.md`**

In `docs/ARCHITECTURE.md`, the `retrieval` line currently reads:

```
│   ├── retrieval        # Persistent inverted index behind the Retriever seam (shipped). Tree-sitter chunking, git-history selection, vector index = later/v2.
```

Update it to reflect chunking shipped:

```
│   ├── retrieval        # Persistent inverted index + tree-sitter symbol chunking behind the Retriever seam (shipped). git-history selection, vector index = later/v2.
```

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md
git commit -m "docs: record tree-sitter symbol chunking (retrieval slice 2)"
```

---

## Done criteria

- A `chunk` module parses Rust/JS/TS/Python/Go into symbol chunks and returns `None` (→ whole-file fallback) for unsupported extensions or symbol-less files.
- The sqlite index carries `chunks`/`chunk_postings`/`chunk_names`, rebuilt cleanly via the FORMAT_VERSION 2 bump, populated and cascade-deleted by the stat-incremental refresh.
- `IndexedRetriever::search` adds `8·symbol_name_hits` on top of slice 1's score (strictly additive — every slice-1 retriever test still passes) and populates `Candidate.symbols`; a file that *defines* `fn login` outranks one that merely mentions it.
- `ContextFinder` lists matched symbol names in its select prompt when present and is byte-identical when absent; every prior ContextFinder test still passes.
- `cargo test --workspace` is green, `cargo fmt --all -- --check` is clean, `cargo clippy --workspace --all-targets` has no new warnings, and the engine-core offline determinism suite is untouched.
- `CLAUDE.md` and `ARCHITECTURE.md` record the slice.
```
