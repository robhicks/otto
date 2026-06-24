//! `IndexedRetriever`: the `Retriever` impl backed by the persistent inverted index. On each
//! `search` it refreshes the index (stat-incremental), then ranks every walked file by
//! `5*path_hits + content_score + 8*symbol_name_hits`. The PATH weighting (5×) matches the
//! lexical ContextFinder, but content scores come from token-equality postings in the index
//! (not substring counts), so results can differ from the lexical fallback for
//! substring-of-token matches (e.g. "auth" inside "authenticate"). Content covers ALL files
//! with no read budget.

use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::{Candidate, Retriever};

use crate::index::Index;
use crate::tokenize::query_terms;

/// Max symbol names attached to a single candidate (for the select prompt).
const SYMBOLS_PER_FILE: usize = 5;

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
        // refresh returns the walked entries — reuse them for path scoring (single walk).
        let entries = self.index.refresh(&self.root).await?;
        let terms = query_terms(goal);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

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
        IndexedRetriever::open(root.to_path_buf(), root.join("idx.sqlite"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn ranks_path_hit_above_content_hit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "login.rs", b"fn x() {}"); // path hit
        seed(root, "util.rs", b"login"); // content hit
        let r = retriever(root).await;
        let files: Vec<_> = r
            .search("login", 8)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert_eq!(files.first(), Some(&PathBuf::from("login.rs")));
    }

    #[tokio::test]
    async fn content_only_match_beyond_old_read_budget_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 300 noise files (> the old 200 read budget) that sort before the real match.
        for i in 0..300 {
            seed(
                root,
                &format!("noise/f{i:04}.txt"),
                b"nothing relevant here",
            );
        }
        seed(root, "zzz_only.txt", b"login logic lives here"); // content-only, sorts last
        let r = retriever(root).await;
        let files: Vec<_> = r
            .search("login", 8)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
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
        let files: Vec<_> = r
            .search("login", 8)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert!(
            !files.iter().any(|p| p.to_string_lossy().contains(".env")),
            "{files:?}"
        );
        assert!(files.contains(&PathBuf::from("real.rs")));
    }

    #[tokio::test]
    async fn empty_workspace_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let r = retriever(dir.path()).await;
        assert!(r.search("login", 8).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn large_file_is_still_a_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A >1 MiB file whose name matches the goal — previously excluded by the size cap.
        let big = vec![b'x'; 1_100_000];
        seed(root, "login_huge.rs", &big);
        let r = retriever(root).await;
        let files: Vec<_> = r
            .search("login", 8)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert!(
            files.contains(&PathBuf::from("login_huge.rs")),
            "large file still a candidate: {files:?}"
        );
    }

    #[tokio::test]
    async fn definition_outranks_mention_and_lists_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        // auth.rs DEFINES `fn login` (name hit, no "login" in the path).
        seed(root, "auth.rs", b"fn login() {}\n");
        // mentions.rs only mentions login in a comment-ish body.
        seed(
            root,
            "mentions.rs",
            b"fn handle() {\n    let login = 1;\n}\n",
        );
        let r = IndexedRetriever::open(root.to_path_buf(), dbdir.path().join("idx.sqlite"))
            .await
            .unwrap();
        let cands = r.search("login", 8).await.unwrap();
        assert_eq!(
            cands.first().map(|c| c.path.clone()),
            Some(PathBuf::from("auth.rs"))
        );
        let auth = cands
            .iter()
            .find(|c| c.path == std::path::Path::new("auth.rs"))
            .unwrap();
        assert!(
            auth.symbols.contains(&"login".to_string()),
            "{:?}",
            auth.symbols
        );
    }

    #[tokio::test]
    async fn unsupported_language_content_match_still_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dbdir = tempfile::tempdir().unwrap();
        // No chunks for .md, but whole-file content still indexes "login" (no-regression).
        seed(root, "notes.md", b"login instructions live here");
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
        assert!(files.contains(&PathBuf::from("notes.md")), "{files:?}");
    }
}
