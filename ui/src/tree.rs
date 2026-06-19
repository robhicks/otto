//! Pure, browser-free workspace-view helpers: build a nested tree from the flat path list the
//! `/workspace` `List` RPC returns, pick an editor language from a path, and decode file bytes
//! into editable text / a binary marker. Unit-tested on the host.

use std::path::{Path, PathBuf};

/// One node in the rendered file tree. Files have an empty `children`; directories may have
/// any number. `path` is the full workspace-relative path (used as the `Read` key for files
/// and as a stable list key for both).
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub children: Vec<TreeNode>,
}

/// Build a sorted nested tree from a flat list of file paths. Directories sort before files;
/// within a kind, lexicographically by segment.
pub fn build_tree(paths: &[PathBuf]) -> Vec<TreeNode> {
    let mut roots: Vec<TreeNode> = Vec::new();
    for path in paths {
        let comps: Vec<String> = path
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        insert_components(&mut roots, &comps, PathBuf::new());
    }
    sort_nodes(&mut roots);
    roots
}

fn insert_components(level: &mut Vec<TreeNode>, comps: &[String], prefix: PathBuf) {
    let Some((head, rest)) = comps.split_first() else {
        return;
    };
    let here = prefix.join(head);
    let is_dir = !rest.is_empty();
    let idx = match level.iter().position(|n| n.name == *head) {
        Some(i) => i,
        None => {
            level.push(TreeNode {
                name: head.clone(),
                path: here.clone(),
                is_dir,
                children: Vec::new(),
            });
            level.len() - 1
        }
    };
    if is_dir {
        level[idx].is_dir = true;
        insert_components(&mut level[idx].children, rest, here);
    }
}

fn sort_nodes(nodes: &mut [TreeNode]) {
    nodes.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    for n in nodes.iter_mut() {
        sort_nodes(&mut n.children);
    }
}

/// The largest file we mount in the editor; bigger files show a notice instead.
pub const MAX_EDITABLE_BYTES: usize = 512 * 1024;

/// A file's body as the UI treats it.
#[derive(Clone, Debug, PartialEq)]
pub enum FileBody {
    /// Valid UTF-8 text, ready to edit.
    Text(String),
    /// Not valid UTF-8 — not editable here.
    Binary,
    /// Over `MAX_EDITABLE_BYTES` — not mounted.
    TooLarge,
}

/// Editor language id for a path, by extension. Returns a stable lowercase id (mapped to a
/// `kode_leptos::Language` in the editor component); unknown extensions get `"text"`.
pub fn language_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("toml") => "toml",
        Some("json") => "json",
        Some("md") | Some("markdown") => "markdown",
        Some("js") | Some("mjs") | Some("cjs") => "javascript",
        Some("ts") => "typescript",
        Some("py") => "python",
        Some("html") | Some("htm") => "html",
        Some("css") => "css",
        Some("sh") | Some("bash") => "bash",
        Some("sql") => "sql",
        Some("yaml") | Some("yml") => "yaml",
        _ => "text",
    }
}

/// Classify raw file bytes for the editor: size cap first, then UTF-8 validity.
pub fn decode_or_binary(bytes: &[u8]) -> FileBody {
    if bytes.len() > MAX_EDITABLE_BYTES {
        return FileBody::TooLarge;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => FileBody::Text(s.to_string()),
        Err(_) => FileBody::Binary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn build_tree_nests_and_sorts_dirs_before_files() {
        let tree = build_tree(&[p("src/main.rs"), p("README.md"), p("src/app.rs")]);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "src");
        assert!(tree[0].is_dir);
        assert_eq!(tree[1].name, "README.md");
        assert!(!tree[1].is_dir);
        let kids: Vec<&str> = tree[0].children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(kids, vec!["app.rs", "main.rs"]);
        assert_eq!(tree[0].children[0].path, p("src/app.rs"));
    }

    #[test]
    fn build_tree_merges_shared_dirs() {
        let tree = build_tree(&[p("a/b/x.rs"), p("a/b/y.rs"), p("a/c.rs")]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "a");
        assert_eq!(tree[0].children[0].name, "b");
        assert!(tree[0].children[0].is_dir);
        assert_eq!(tree[0].children[0].children.len(), 2);
        assert_eq!(tree[0].children[1].name, "c.rs");
    }

    #[test]
    fn language_for_path_maps_known_extensions() {
        assert_eq!(language_for_path(Path::new("src/main.rs")), "rust");
        assert_eq!(language_for_path(Path::new("Cargo.toml")), "toml");
        assert_eq!(language_for_path(Path::new("a/b.json")), "json");
        assert_eq!(language_for_path(Path::new("notes.md")), "markdown");
        assert_eq!(language_for_path(Path::new("LICENSE")), "text");
    }

    #[test]
    fn decode_or_binary_classifies_bytes() {
        assert_eq!(decode_or_binary(b"hello"), FileBody::Text("hello".into()));
        assert_eq!(decode_or_binary(&[0xff, 0xfe, 0x00]), FileBody::Binary);
        let big = vec![b'a'; MAX_EDITABLE_BYTES + 1];
        assert_eq!(decode_or_binary(&big), FileBody::TooLarge);
    }
}
