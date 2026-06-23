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
        let content: std::collections::HashMap<String, u64> = self
            .index
            .content_scores(&terms)
            .await?
            .into_iter()
            .collect();

        // Path scores computed live over the current walk (free). Combine 5*path + content.
        let mut scored: Vec<Candidate> = walk(&self.root)
            .into_iter()
            .filter_map(|e| {
                let path_str = e.path.to_string_lossy().to_lowercase();
                let path_hits: u64 = terms
                    .iter()
                    .map(|t| path_str.matches(t.as_str()).count() as u64)
                    .sum();
                let key = e.path.to_string_lossy().into_owned();
                let score = 5 * path_hits + content.get(&key).copied().unwrap_or(0);
                (score > 0).then_some(Candidate {
                    path: e.path,
                    score,
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
}
