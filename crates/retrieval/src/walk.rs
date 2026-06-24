//! Recursive workspace walk for indexing. Mirrors the `fs.list` recursive walk and the
//! permission gate's sensitive-path floor: skips `.git`/`target`/`node_modules` and ANY
//! dot-prefixed component (covers `.env`/`.ssh`/`.aws`), skips binary/lockfile names, does not
//! follow symlinks, and caps enumeration. Large files are enumerated so they are path-scored;
//! `index.rs` bounds the content read to 1 MiB per file. Returns relative paths with their stat
//! (mtime nanos, size) for stat-based staleness.
//!
//! The sensitive-path floor is now `otto_engine_core::is_sensitive` — the same function the
//! permission gate (`DefaultPermissionGate`) enforces — so the index walk and the gate share one
//! canonical list and cannot drift.

use std::path::{Path, PathBuf};

/// Max files enumerated per walk (bounds cost on huge trees).
pub const ENUMERATE_CAP: usize = 5000;

/// One enumerated file: workspace-relative path + stat used for staleness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
    pub path: PathBuf, // relative to root
    pub mtime_ns: i64,
    pub size: i64,
}

/// True if a path component should be pruned from the walk.
fn excluded_component(name: &str) -> bool {
    name == ".git" || name == "target" || name == "node_modules" || name.starts_with('.') // .env / .ssh / .aws and any dotfile/dir
}

/// True if a leaf file name is a binary/lockfile to skip (mirrors ContextFinder::is_skippable).
fn skippable_file(name: &str) -> bool {
    const SKIP_EXTS: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "tiff", "mp3", "mp4", "mov", "avi",
        "wav", "ogg", "flac", "webm", "zip", "gz", "tgz", "tar", "xz", "zst", "bz2", "7z", "rar",
        "exe", "dll", "so", "dylib", "o", "a", "bin", "wasm", "class", "pyc", "pyo", "obj", "pdf",
        "ttf", "otf", "woff", "woff2",
    ];
    const SKIP_NAMES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "Pipfile.lock",
    ];
    if SKIP_NAMES.contains(&name) {
        return true;
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => SKIP_EXTS.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
}

/// Walk `root` recursively, returning indexable entries (sorted by path for determinism).
pub fn walk(root: &Path) -> Vec<WalkEntry> {
    let mut out: Vec<WalkEntry> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if out.len() >= ENUMERATE_CAP {
                out.sort_by(|a, b| a.path.cmp(&b.path));
                return out;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // `DirEntry::metadata()` does not traverse symlinks (unlike `fs::metadata`), so we
            // see the link itself and skip it below.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if !excluded_component(&name) {
                    stack.push(entry.path());
                }
                continue;
            }
            if !meta.is_file() || excluded_component(&name) || skippable_file(&name) {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root).map(Path::to_path_buf) else {
                continue;
            };
            let rel_str = rel.to_string_lossy();
            if otto_engine_core::is_sensitive(&rel_str) {
                continue;
            }
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            out.push(WalkEntry {
                path: rel,
                mtime_ns,
                size: meta.len() as i64,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn seed(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn walk_includes_source_excludes_sensitive_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "src/main.rs", b"fn main() {}");
        seed(root, ".env", b"SECRET=1");
        seed(root, ".git/config", b"[core]");
        seed(root, "node_modules/x/index.js", b"x");
        seed(root, "logo.png", b"\x89PNG");
        // Large files are now enumerated (content is bounded in index.rs, not here).
        seed(root, "big.txt", &vec![b'a'; 1_100_000]);

        let paths: Vec<_> = walk(root).into_iter().map(|e| e.path).collect();
        assert!(paths.contains(&PathBuf::from("src/main.rs")));
        assert!(
            !paths.contains(&PathBuf::from(".env")),
            "secrets excluded: {paths:?}"
        );
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
        assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
        assert!(
            !paths.contains(&PathBuf::from("logo.png")),
            "binaries excluded"
        );
        assert!(
            paths.contains(&PathBuf::from("big.txt")),
            "large files are now enumerated (path-scored): {paths:?}"
        );
    }

    #[test]
    fn walk_excludes_gate_sensitive_files_without_dot_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed(root, "id_rsa", b"-----BEGIN PRIVATE KEY-----");
        seed(root, "config/production.env", b"DB_PASSWORD=hunter2");
        seed(root, "src/main.rs", b"fn main() {}");

        let paths: Vec<_> = walk(root).into_iter().map(|e| e.path).collect();
        assert!(
            !paths.contains(&PathBuf::from("id_rsa")),
            "ssh key excluded: {paths:?}"
        );
        assert!(
            !paths.contains(&PathBuf::from("config/production.env")),
            "*.env excluded: {paths:?}"
        );
        assert!(paths.contains(&PathBuf::from("src/main.rs")));
    }
}
