use std::path::PathBuf;

use dioxus::prelude::*;

use crate::net::tree::TreeNode;

/// The workspace file tree. `nodes` is the current (flat-built, already-sorted) tree; clicking a
/// file invokes `on_open` with its full path. Directories toggle expand/collapse locally (each
/// node owns its own `expanded` signal — mirrors `ui/src/components/file_tree.rs`'s per-node
/// `RwSignal`, just Dioxus-flavored).
#[component]
pub fn FileTree(nodes: Vec<TreeNode>, on_open: EventHandler<PathBuf>) -> Element {
    rsx! {
        ul { class: "file-tree",
            for node in nodes {
                // Keyed by the node's unique workspace path so Dioxus diffs this list by identity,
                // not position: a Refresh that reorders/adds/removes nodes then keeps each
                // FileTreeNode's local `expanded` state attached to the right node.
                FileTreeNode { key: "{node.path.display()}", node, on_open }
            }
        }
    }
}

#[component]
fn FileTreeNode(node: TreeNode, on_open: EventHandler<PathBuf>) -> Element {
    let mut expanded = use_signal(|| true);
    if node.is_dir {
        rsx! {
            li {
                span {
                    class: "tree-row tree-dir-row",
                    onclick: move |_| expanded.toggle(),
                    if *expanded.read() { "▾ " } else { "▸ " }
                    "{node.name}"
                }
                if *expanded.read() {
                    ul {
                        for child in node.children.clone() {
                            FileTreeNode { key: "{child.path.display()}", node: child, on_open }
                        }
                    }
                }
            }
        }
    } else {
        let path = node.path.clone();
        rsx! {
            li {
                span {
                    class: "tree-row tree-file-row",
                    onclick: move |_| on_open.call(path.clone()),
                    "{node.name}"
                }
            }
        }
    }
}
