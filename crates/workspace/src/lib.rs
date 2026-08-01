//! `LocalWorkspace`: edits a real on-disk folder in place, with path containment.

mod remote;
pub use remote::RemoteWorkspace;

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::{Edit, WorkspaceSnapshot};

/// A workspace rooted at a real directory on disk. All paths are resolved relative
/// to `root` and may never escape it.
pub struct LocalWorkspace {
    root: PathBuf,
}

impl LocalWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Materialize a snapshot into this workspace, writing each file through the gated
    /// `apply_edit` path (so path containment is enforced). UTF-8 only for v1: a non-UTF-8
    /// file errors rather than corrupting (raw-bytes restore is a future refinement).
    /// Non-atomic: if an error is returned, files processed before the failure are already
    /// written and are not rolled back.
    pub async fn restore(&self, snapshot: &WorkspaceSnapshot) -> anyhow::Result<()> {
        for (path, bytes) in &snapshot.files {
            // The ingress mirror of `snapshot`'s floor. `apply_edit` enforces containment only,
            // and `LoopbackTarget::provision` calls this directly on a caller-supplied bundle
            // without the `validate_workspace_edits` pass the network paths get. Unreachable
            // today (the only loopback bundle comes from `promote`, which now filters on the way
            // out) — but relying on that is delegation to another control, which is the exact
            // pattern this seam's last leak came from.
            if path.to_str().is_none_or(otto_protocol::is_sensitive) {
                continue;
            }
            let new_contents = String::from_utf8(bytes.clone()).map_err(|_| {
                anyhow::anyhow!("restore: non-UTF-8 contents for {}", path.display())
            })?;
            self.apply_edit(&Edit {
                path: path.clone(),
                new_contents,
            })
            .await?;
        }
        Ok(())
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
impl WorkspaceRead for LocalWorkspace {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        let full = self.contain(path)?;
        Ok(tokio::fs::read(full).await?)
    }

    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>> {
        // Shallow mode (the default `*`): list the root's immediate entries, unchanged.
        if !glob.contains("**") {
            let mut entries = tokio::fs::read_dir(&self.root).await?;
            let mut out = Vec::new();
            while let Some(entry) = entries.next_entry().await? {
                if let Ok(rel) = entry.path().strip_prefix(&self.root) {
                    out.push(rel.to_path_buf());
                }
            }
            out.sort();
            return Ok(out);
        }

        // Recursive mode (`**`): walk the subtree, returning files only. Skips a fixed set of
        // ignored directories (build/VCS/dependency dirs and any dotfile/dotdir). NOTE: the
        // dotfile skip is NOT the sensitive-path floor and must not be mistaken for it — the
        // floor's markers match as substrings, so `id_rsa` and `production.env` have no leading
        // dot and are returned by this walk. `snapshot` applies the floor itself for exactly
        // that reason. Does not follow symlinks, and caps the number of files to bound cost.
        // Output is sorted for determinism.
        const MAX_ENTRIES: usize = 5000;
        fn ignored(name: &str) -> bool {
            name == ".git" || name == "target" || name == "node_modules" || name.starts_with('.')
        }
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
                continue; // skip directories we cannot read rather than failing the whole walk
            };
            loop {
                // A transient per-entry error (e.g. a file vanishing mid-walk) skips that entry
                // or ends this directory rather than failing the whole walk — retrieval should
                // degrade, not blank out, on a flaky filesystem.
                let entry = match entries.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(_) => break,
                };
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if file_type.is_symlink() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if ignored(&name) {
                    continue;
                }
                let path = entry.path();
                if file_type.is_dir() {
                    stack.push(path);
                } else if file_type.is_file() {
                    if let Ok(rel) = path.strip_prefix(&self.root) {
                        out.push(rel.to_path_buf());
                        if out.len() >= MAX_ENTRIES {
                            out.sort();
                            return Ok(out);
                        }
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

#[async_trait]
impl Workspace for LocalWorkspace {
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
        let full = self.contain(&edit.path)?;
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full, edit.new_contents.as_bytes()).await?;
        Ok(edit.new_contents.len() as u64)
    }

    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot> {
        let paths = self.list("**").await?;
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            // The floor, applied at the seam every caller shares. `list`'s dotfile skip is NOT
            // equivalent: the markers match as substrings, so `id_rsa` and `production.env`
            // pass it. A snapshot is the one `Workspace` operation that reads whole file
            // *contents* for shipment off-machine — `otto_remote::promote` puts one straight
            // into a `PromoteBundle` — so the floor is re-asserted here rather than in any one
            // caller, which would leave the next caller to reintroduce the gap.
            //
            // `to_str()`, never `to_string_lossy()`: `validate_workspace_edits` warns in as many
            // words that U+FFFD substitution could let a non-UTF-8 path slip a marker past the
            // check. A non-UTF-8 path is skipped rather than shipped — which also stops one such
            // filename from making a whole bundle unusable, since the receiver refuses them.
            let Some(path_str) = path.to_str() else {
                continue;
            };
            if otto_protocol::is_sensitive(path_str) {
                continue;
            }
            let bytes = self.read(&path).await?;
            files.push((path, bytes));
        }
        Ok(WorkspaceSnapshot { files })
    }
}

/// Drop floor-sensitive entries from a snapshot's file list.
///
/// `LocalWorkspace::snapshot` skips these *before* reading, so the bytes are never loaded at all;
/// this is for the case where the list arrives already-populated from elsewhere
/// (`RemoteWorkspace::snapshot`, which receives it over the wire).
pub(crate) fn strip_sensitive_files(mut files: Vec<(PathBuf, Vec<u8>)>) -> Vec<(PathBuf, Vec<u8>)> {
    // `retain` in place rather than filter-and-collect: the element type is unchanged, so there
    // is no reason to allocate a second backing buffer for what is usually a no-op.
    // Fail closed on a non-UTF-8 path: drop it rather than lossy-convert. See the note in
    // `LocalWorkspace::snapshot`.
    files.retain(|(p, _)| p.to_str().is_some_and(|s| !otto_protocol::is_sensitive(s)));
    files
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
    async fn apply_edit_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());

        let edit = Edit {
            path: PathBuf::from("/etc/passwd"),
            new_contents: "nope".to_string(),
        };
        let err = ws.apply_edit(&edit).await.unwrap_err();
        assert!(err.to_string().contains("absolute paths are not allowed"));
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

    #[tokio::test]
    async fn recursive_list_walks_subdirs_and_skips_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        for (p, c) in [
            ("src/lib.rs", "a"),
            ("src/inner/mod.rs", "b"),
            ("target/debug/junk.rs", "c"),
            (".git/config", "d"),
            ("node_modules/x/index.js", "e"),
        ] {
            ws.apply_edit(&Edit {
                path: PathBuf::from(p),
                new_contents: c.to_string(),
            })
            .await
            .unwrap();
        }
        let listing = ws.list("**").await.unwrap();
        assert!(listing.contains(&PathBuf::from("src/lib.rs")));
        assert!(listing.contains(&PathBuf::from("src/inner/mod.rs")));
        assert!(!listing.iter().any(|p| p.starts_with("target")));
        assert!(!listing.iter().any(|p| p.starts_with(".git")));
        assert!(!listing.iter().any(|p| p.starts_with("node_modules")));
        assert!(!listing.contains(&PathBuf::from("src")));
        let mut sorted = listing.clone();
        sorted.sort();
        assert_eq!(listing, sorted);
    }

    #[tokio::test]
    async fn snapshot_captures_listed_files_and_excludes_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        for (p, c) in [
            ("a.txt", "A"),
            ("src/lib.rs", "L"),
            ("src/inner/mod.rs", "M"),
            ("target/junk.rs", "x"),
            (".git/config", "x"),
            ("node_modules/x/i.js", "x"),
        ] {
            ws.apply_edit(&Edit {
                path: PathBuf::from(p),
                new_contents: c.to_string(),
            })
            .await
            .unwrap();
        }

        let snap = ws.snapshot().await.unwrap();
        let paths: Vec<_> = snap.files.iter().map(|(p, _)| p.clone()).collect();
        assert!(paths.contains(&PathBuf::from("a.txt")));
        assert!(paths.contains(&PathBuf::from("src/lib.rs")));
        assert!(paths.contains(&PathBuf::from("src/inner/mod.rs")));
        assert!(!paths.iter().any(|p| p.starts_with("target")));
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
        assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
        // Contents are captured, not just paths.
        let lib = snap
            .files
            .iter()
            .find(|(p, _)| p == &PathBuf::from("src/lib.rs"))
            .unwrap();
        assert_eq!(lib.1, b"L");
    }

    /// The walk skips dotfiles, which is NOT the same as the sensitive-path floor: the markers
    /// match as substrings, so `id_rsa` and `production.env` have no leading dot and sail
    /// through. A snapshot reads whole file *contents* for shipment off-machine (it is what
    /// `otto_remote::promote` puts in a `PromoteBundle`), so the floor has to be re-asserted
    /// here. Measured before the fix: the snapshot contained `id_rsa`, `production.env`, and
    /// `config/local.env`.
    #[tokio::test]
    async fn snapshot_excludes_floor_sensitive_files_that_are_not_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("id_rsa"), b"PRIVATE KEY").unwrap();
        std::fs::write(dir.path().join("production.env"), b"DB_PASSWORD=hunter2").unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/local.env"), b"SECRET=xyz").unwrap();
        std::fs::write(dir.path().join(".env"), b"HIDDEN=1").unwrap();
        std::fs::write(dir.path().join("ok.txt"), b"fine").unwrap();

        let ws = LocalWorkspace::new(dir.path());
        let snap = ws.snapshot().await.unwrap();
        let names: Vec<String> = snap
            .files
            .iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect();

        // Asserted against `is_sensitive` rather than a hardcoded list, so the test tracks the
        // floor automatically if a marker is ever added.
        let leaked: Vec<&String> = names
            .iter()
            .filter(|n| otto_protocol::is_sensitive(n))
            .collect();
        assert!(
            leaked.is_empty(),
            "floor-sensitive files in a snapshot: {leaked:?}"
        );

        // ...and the snapshot is not vacuously empty.
        assert!(
            names.iter().any(|n| n == "ok.txt"),
            "ordinary files must still be captured: {names:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_restore_round_trips_into_fresh_workspace() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = LocalWorkspace::new(src_dir.path());
        for (p, c) in [
            ("a.txt", "A"),
            ("src/lib.rs", "L"),
            ("src/inner/mod.rs", "M"),
        ] {
            src.apply_edit(&Edit {
                path: PathBuf::from(p),
                new_contents: c.to_string(),
            })
            .await
            .unwrap();
        }
        let snap = src.snapshot().await.unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst = LocalWorkspace::new(dst_dir.path());
        dst.restore(&snap).await.unwrap();

        // Re-snapshotting the destination yields the same files + contents.
        let mut original = snap.files.clone();
        original.sort();
        let mut restored = dst.snapshot().await.unwrap().files;
        restored.sort();
        assert_eq!(original, restored);
    }

    #[tokio::test]
    async fn restore_rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let snap = WorkspaceSnapshot {
            files: vec![(PathBuf::from("../escape.txt"), b"x".to_vec())],
        };
        assert!(ws.restore(&snap).await.is_err());
    }

    #[tokio::test]
    async fn restore_rejects_non_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let snap = WorkspaceSnapshot {
            files: vec![(PathBuf::from("bin.dat"), vec![0xff, 0xfe])],
        };
        assert!(ws.restore(&snap).await.is_err());
    }

    #[tokio::test]
    async fn shallow_list_unchanged_for_star_glob() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        ws.apply_edit(&Edit {
            path: PathBuf::from("a.txt"),
            new_contents: "a".to_string(),
        })
        .await
        .unwrap();
        ws.apply_edit(&Edit {
            path: PathBuf::from("sub/b.txt"),
            new_contents: "b".to_string(),
        })
        .await
        .unwrap();
        let listing = ws.list("*").await.unwrap();
        assert!(listing.contains(&PathBuf::from("a.txt")));
        assert!(listing.contains(&PathBuf::from("sub")));
        assert!(!listing.contains(&PathBuf::from("sub/b.txt")));
    }

    #[tokio::test]
    async fn snapshot_of_empty_workspace_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        assert_eq!(ws.snapshot().await.unwrap().files, Vec::new());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_fails_loud_on_unreadable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        ws.apply_edit(&Edit {
            path: PathBuf::from("secret.txt"),
            new_contents: "x".to_string(),
        })
        .await
        .unwrap();
        let p = dir.path().join("secret.txt");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        // If the file is still readable (e.g. running as root), the premise doesn't hold — skip.
        if tokio::fs::read(&p).await.is_ok() {
            return;
        }
        // snapshot reads each listed file; an unreadable one must fail loudly, not be skipped.
        assert!(ws.snapshot().await.is_err());
    }

    /// `RemoteWorkspace::snapshot` receives its file list from a peer. An otto peer already
    /// filters, so this is normally a no-op — but satisfying the seam's contract by *delegation*
    /// means trusting the peer to be an up-to-date otto, and trusting one control to cover
    /// another is exactly what caused this seam's last leak.
    #[test]
    fn strip_sensitive_files_drops_floor_paths_and_keeps_the_rest() {
        let files = vec![
            (PathBuf::from("ok.txt"), b"fine".to_vec()),
            (PathBuf::from("id_rsa"), b"KEY".to_vec()),
            (PathBuf::from("production.env"), b"PW".to_vec()),
            (PathBuf::from("config/local.env"), b"S".to_vec()),
            (PathBuf::from(".env"), b"H".to_vec()),
        ];
        let kept: Vec<String> = strip_sensitive_files(files)
            .into_iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect();
        assert_eq!(kept, vec!["ok.txt".to_string()]);
    }

    /// `restore` is the ingress mirror of `snapshot`, and `apply_edit` enforces containment only.
    /// `LoopbackTarget::provision` calls this directly on a caller-supplied bundle, without the
    /// `validate_workspace_edits` pass the network ingress paths get — so the floor is applied
    /// here too rather than delegated to whoever built the bundle.
    #[tokio::test]
    async fn restore_refuses_to_write_floor_sensitive_entries() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());

        ws.restore(&WorkspaceSnapshot {
            files: vec![
                (PathBuf::from("ok.txt"), b"fine".to_vec()),
                (PathBuf::from("id_rsa"), b"PRIVATE KEY".to_vec()),
                (PathBuf::from("config/local.env"), b"SECRET=xyz".to_vec()),
            ],
        })
        .await
        .unwrap();

        assert!(
            dir.path().join("ok.txt").exists(),
            "ordinary files must land"
        );
        assert!(
            !dir.path().join("id_rsa").exists(),
            "a floor-sensitive entry must not be written"
        );
        assert!(
            !dir.path().join("config/local.env").exists(),
            "a nested floor-sensitive entry must not be written"
        );
    }
}
