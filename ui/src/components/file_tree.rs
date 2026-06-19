use std::path::PathBuf;

use leptos::prelude::*;

use crate::tree::TreeNode;

/// The workspace file tree. `nodes` is the current tree; clicking a file invokes `on_open`
/// with its full path. Directories toggle expand/collapse locally.
#[component]
pub fn FileTree(nodes: Signal<Vec<TreeNode>>, on_open: Callback<PathBuf>) -> impl IntoView {
    view! {
        <div class="file-tree">
            {move || {
                let items = nodes.get();
                if items.is_empty() {
                    view! { <div class="tree-empty">"no files"</div> }.into_any()
                } else {
                    items
                        .into_iter()
                        .map(|n| view! { <TreeNodeView node=n on_open=on_open /> }.into_any())
                        .collect_view()
                        .into_any()
                }
            }}
        </div>
    }
}

#[component]
fn TreeNodeView(node: TreeNode, on_open: Callback<PathBuf>) -> impl IntoView {
    if node.is_dir {
        let expanded = RwSignal::new(false);
        let name = node.name.clone();
        let children = node.children.clone();
        let toggle = move || expanded.update(|e| *e = !*e);
        view! {
            <div class="tree-dir">
                <div
                    class="tree-row tree-dir-row"
                    role="button"
                    tabindex=0
                    attr:aria-expanded=move || if expanded.get() { "true" } else { "false" }
                    on:click=move |_| toggle()
                    on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                        if ev.key() == "Enter" || ev.key() == " " {
                            ev.prevent_default();
                            toggle();
                        }
                    }
                >
                    {move || if expanded.get() { "▾ " } else { "▸ " }}
                    {name.clone()}
                </div>
                <Show when=move || expanded.get() fallback=|| ()>
                    <div class="tree-children">
                        {children
                            .clone()
                            .into_iter()
                            .map(|c| view! { <TreeNodeView node=c on_open=on_open /> }.into_any())
                            .collect_view()}
                    </div>
                </Show>
            </div>
        }
        .into_any()
    } else {
        let path = node.path.clone();
        let path2 = path.clone();
        let name = node.name.clone();
        view! {
            <div
                class="tree-row tree-file-row"
                role="button"
                tabindex=0
                on:click=move |_| on_open.run(path.clone())
                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                    if ev.key() == "Enter" || ev.key() == " " {
                        ev.prevent_default();
                        on_open.run(path2.clone());
                    }
                }
            >
                {name}
            </div>
        }
        .into_any()
    }
}
