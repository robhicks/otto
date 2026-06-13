//! `LocalWorkspace`: edits a real on-disk folder in place, with path containment.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::Edit;

/// A workspace rooted at a real directory on disk. All paths are resolved relative
/// to `root` and may never escape it.
pub struct LocalWorkspace {
    root: PathBuf,
}

impl LocalWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve a workspace-relative path against the root, rejecting any path that
    /// escapes the root via `..` or absolute components.
    fn contain(&self, path: &Path) -> anyhow::Result<PathBuf> {
        if path.as_os_str().is_empty() {
            anyhow::bail!("path must not be empty");
        }
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    anyhow::bail!("path escapes workspace root: {}", path.display())
                }
                Component::Prefix(_) | Component::RootDir => {
                    anyhow::bail!("absolute paths are not allowed: {}", path.display())
                }
                _ => {}
            }
        }
        Ok(self.root.join(path))
    }
}

#[async_trait]
impl Workspace for LocalWorkspace {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        let full = self.contain(path)?;
        Ok(tokio::fs::read(full).await?)
    }

    async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
        // Skeleton: shallow listing of the root, relative paths. Globbing arrives
        // with the retrieval/ContextFinder work in a later plan.
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut out = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if let Ok(rel) = entry.path().strip_prefix(&self.root) {
                out.push(rel.to_path_buf());
            }
        }
        out.sort();
        Ok(out)
    }

    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
        let full = self.contain(&edit.path)?;
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full, edit.new_contents.as_bytes()).await?;
        Ok(edit.new_contents.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_edit_writes_file_and_read_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());

        let edit = Edit {
            path: PathBuf::from("greeting.txt"),
            new_contents: "hello otto".to_string(),
        };
        let written = ws.apply_edit(&edit).await.unwrap();
        assert_eq!(written, 10);

        let bytes = ws.read(Path::new("greeting.txt")).await.unwrap();
        assert_eq!(bytes, b"hello otto");
    }

    #[tokio::test]
    async fn apply_edit_rejects_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());

        let edit = Edit {
            path: PathBuf::from("../escape.txt"),
            new_contents: "nope".to_string(),
        };
        let err = ws.apply_edit(&edit).await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace root"));
    }

    #[tokio::test]
    async fn apply_edit_rejects_empty_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());

        let edit = Edit {
            path: PathBuf::from(""),
            new_contents: "nope".to_string(),
        };
        let err = ws.apply_edit(&edit).await.unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[tokio::test]
    async fn list_returns_relative_entries() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        ws.apply_edit(&Edit {
            path: PathBuf::from("a.txt"),
            new_contents: "a".to_string(),
        })
        .await
        .unwrap();

        let listing = ws.list("*").await.unwrap();
        assert_eq!(listing, vec![PathBuf::from("a.txt")]);
    }
}
