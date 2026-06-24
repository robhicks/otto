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
        let c = Candidate {
            path: PathBuf::from("src/main.rs"),
            score: 7,
        };
        assert_eq!(c.path, PathBuf::from("src/main.rs"));
        assert_eq!(c.score, 7);
    }
}
