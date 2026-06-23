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
            sqlx::query("DROP TABLE IF EXISTS postings")
                .execute(&self.pool)
                .await?;
            sqlx::query("DROP TABLE IF EXISTS files")
                .execute(&self.pool)
                .await?;
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
            let Ok(content) = std::fs::read_to_string(root.join(&entry.path)) else {
                continue;
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
            tx.commit().await?;
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
            .map(|r| {
                (
                    r.get::<String, _>("path"),
                    r.get::<i64, _>("score").max(0) as u64,
                )
            })
            .collect())
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
}
