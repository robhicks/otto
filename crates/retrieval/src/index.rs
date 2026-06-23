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
