//! The standalone sqlite inverted index: a `files` table (path + stat for staleness) and a
//! `postings` table (token -> file counts). Owned entirely by the retrieval crate, separate from
//! the session store. `CREATE TABLE IF NOT EXISTS` keeps it migration-free; a `meta` format
//! version triggers a full rebuild on mismatch so a schema change never reads a stale layout.

use std::path::Path;

use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// Bump when the schema changes; a mismatch drops and recreates the index.
const FORMAT_VERSION: &str = "2";

/// Max bytes read per file for indexing — bounds I/O on large files (only the head is indexed,
/// consistent with the lexical path's content-scan cap) while still letting large files be
/// enumerated and path-scored.
const MAX_READ_BYTES: u64 = 1_048_576; // 1 MiB

/// Read at most `MAX_READ_BYTES` of `path` and lossily decode to text. Lossy decoding means a
/// non-UTF-8 file never errors here, so it still gets a stat row and is not re-read every refresh.
fn read_capped(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(MAX_READ_BYTES).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub struct Index {
    pool: SqlitePool,
}

impl Index {
    /// Open (creating if missing) the index DB at `db_path`.
    pub async fn open(db_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5))
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
            for tbl in [
                "chunk_postings",
                "chunk_names",
                "chunks",
                "postings",
                "files",
            ] {
                sqlx::query(&format!("DROP TABLE IF EXISTS {tbl}"))
                    .execute(&self.pool)
                    .await?;
            }
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
        sqlx::query("INSERT OR REPLACE INTO meta (key, value) VALUES ('format', ?1)")
            .bind(FORMAT_VERSION)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

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

    /// Bring the index in line with the on-disk workspace: re-tokenize new/changed files, drop
    /// rows for vanished files. `root` is the workspace root; entries come from `walk::walk`.
    /// Returns the walked entries so callers can reuse them (avoiding a second walk).
    pub async fn refresh(&self, root: &Path) -> anyhow::Result<Vec<crate::walk::WalkEntry>> {
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

        // Re-index new or changed files (stat compare); skip unchanged.
        for (path, entry) in &present {
            if let Some((_, mtime, size)) = existing.get(path) {
                if *mtime == entry.mtime_ns && *size == entry.size {
                    continue; // unchanged — the amortization win
                }
            }
            // Bounded + lossy: large files index only their head; non-UTF-8 files don't error, so
            // they still get a stat row (no re-read every refresh). A true I/O error skips this
            // file for now and retries next refresh.
            let content = match read_capped(&root.join(&entry.path)) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let tokens = crate::tokenize::index_tokens(&content);

            // Atomic per-file update: the files stat row and this file's postings commit
            // together, so a crash mid-write can't leave an updated stat with incomplete
            // postings (which a later refresh would skip). Torn writes roll back and the
            // next refresh re-indexes — strictly self-healing.
            let mut tx = self.pool.begin().await?;
            sqlx::query(
                "INSERT INTO files (path, mtime_ns, size) VALUES (?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET mtime_ns = ?2, size = ?3",
            )
            .bind(path)
            .bind(entry.mtime_ns)
            .bind(entry.size)
            .execute(&mut *tx)
            .await?;
            let file_id: i64 = sqlx::query("SELECT file_id FROM files WHERE path = ?1")
                .bind(path)
                .fetch_one(&mut *tx)
                .await?
                .get("file_id");
            sqlx::query("DELETE FROM postings WHERE file_id = ?1")
                .bind(file_id)
                .execute(&mut *tx)
                .await?;
            for (token, count) in tokens {
                sqlx::query("INSERT INTO postings (token, file_id, count) VALUES (?1, ?2, ?3)")
                    .bind(token)
                    .bind(file_id)
                    .bind(count)
                    .execute(&mut *tx)
                    .await?;
            }

            // Replace any prior chunks for this file (inlined deletes: &mut *tx is not Copy, so it
            // can't use the delete_chunks helper, which requires a Copy executor like &self.pool).
            sqlx::query("DELETE FROM chunk_postings WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE file_id = ?1)")
                .bind(file_id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM chunk_names WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE file_id = ?1)")
                .bind(file_id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM chunks WHERE file_id = ?1")
                .bind(file_id)
                .execute(&mut *tx)
                .await?;

            // Chunk the file (best-effort). `None` (unsupported language or no symbols) leaves only
            // the whole-file postings above — identical to slice 1.
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

            tx.commit().await?;
        }
        drop(present); // release the borrow on `entries` before returning it
        Ok(entries)
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
            .map(|r| {
                (
                    r.get::<String, _>("path"),
                    r.get::<i64, _>("score").max(0) as u64,
                )
            })
            .collect())
    }

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
        Ok(q.fetch_all(&self.pool)
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

        // Name-hit pass.
        let mut q = sqlx::query(&name_sql);
        for t in terms {
            q = q.bind(t);
        }
        for row in q.fetch_all(&self.pool).await? {
            let path: String = row.get("path");
            let symbol: String = row.get("symbol");
            let v = out.entry(path).or_default();
            if v.len() < cap_per_file && !v.contains(&symbol) {
                v.push(symbol);
            }
        }

        // Body-hit pass.
        let mut q = sqlx::query(&body_sql);
        for t in terms {
            q = q.bind(t);
        }
        for row in q.fetch_all(&self.pool).await? {
            let path: String = row.get("path");
            let symbol: String = row.get("symbol");
            let v = out.entry(path).or_default();
            if v.len() < cap_per_file && !v.contains(&symbol) {
                v.push(symbol);
            }
        }

        Ok(out)
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

    use std::io::Write;

    fn seed(root: &std::path::Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[tokio::test]
    async fn refresh_indexes_content_and_scores() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        seed(root, "a.rs", b"login login handler");
        seed(root, "b.rs", b"unrelated prose");
        let idx = Index::open(dbdir.path().join("idx.sqlite")).await.unwrap();
        idx.refresh(root).await.unwrap();

        let scores = idx.content_scores(&["login".to_string()]).await.unwrap();
        let a = scores.iter().find(|(p, _)| p == "a.rs").map(|(_, s)| *s);
        assert_eq!(a, Some(2));
        assert!(!scores.iter().any(|(p, _)| p == "b.rs"));
    }

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

    #[tokio::test]
    async fn refresh_updates_on_edit_and_drops_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        seed(root, "a.rs", b"login");
        seed(root, "gone.rs", b"login");
        let idx = Index::open(dbdir.path().join("idx.sqlite")).await.unwrap();
        idx.refresh(root).await.unwrap();

        // Edit a.rs to add another hit; delete gone.rs.
        seed(root, "a.rs", b"login login login");
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        idx.refresh(root).await.unwrap();

        let scores = idx.content_scores(&["login".to_string()]).await.unwrap();
        assert_eq!(
            scores.iter().find(|(p, _)| p == "a.rs").map(|(_, s)| *s),
            Some(3)
        );
        assert!(
            !scores.iter().any(|(p, _)| p == "gone.rs"),
            "deleted file dropped: {scores:?}"
        );
    }

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
        let login_names: i64 =
            sqlx::query("SELECT COUNT(*) AS n FROM chunk_names WHERE token='login'")
                .fetch_one(&idx.pool)
                .await
                .unwrap()
                .get("n");
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
            sqlx::query("SELECT COUNT(*) AS n FROM chunks")
                .fetch_one(&idx.pool)
                .await
                .unwrap()
                .get::<i64, _>("n")
                > 0
        );

        std::fs::remove_file(root.join("gone.rs")).unwrap();
        idx.refresh(root).await.unwrap();
        for tbl in ["chunks", "chunk_postings", "chunk_names"] {
            let n: i64 = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {tbl}"))
                .fetch_one(&idx.pool)
                .await
                .unwrap()
                .get("n");
            assert_eq!(n, 0, "{tbl} cascaded on file removal");
        }
    }

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
        assert!(
            !hits.contains_key("util.rs"),
            "util.rs only mentions login in a body"
        );

        let syms = idx
            .matched_symbols(&["login".to_string()], 5)
            .await
            .unwrap();
        assert_eq!(syms.get("auth.rs"), Some(&vec!["login".to_string()]));
        // util.rs matched by body -> its enclosing symbol `helper` is listed.
        assert_eq!(syms.get("util.rs"), Some(&vec!["helper".to_string()]));
    }
}
