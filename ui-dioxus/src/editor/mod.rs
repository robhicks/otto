use std::path::PathBuf;

use dioxus::prelude::*;

use crate::net::tree::FileBody;

/// The unhighlighted (plain-`textarea`) controlled-buffer editor. `open` is the currently-open
/// file (path + classified body); `seed` is the initial document text set once at open time (a
/// later part swaps this for a styled-span highlighted render — the diff-first non-goal means no
/// VSCode-scale features here). No persistence in this slice.
#[component]
pub fn Editor(open: Signal<Option<(PathBuf, FileBody)>>, seed: Signal<String>) -> Element {
    let Some((path, body)) = open.read().clone() else {
        return rsx! { div { class: "editor-empty", "No file open" } };
    };
    match body {
        FileBody::Binary => rsx! { div { class: "editor-notice", "binary file — not editable" } },
        FileBody::TooLarge => rsx! { div { class: "editor-notice", "file too large to edit" } },
        FileBody::Text(_) => {
            let mut buf = use_signal(|| seed.read().clone());
            // Re-seed when a different file opens. `seed()` is a tracked read, so this fires
            // whenever app.rs writes a new seed on open_path — mirrors the app.rs auto-load
            // effect's tracked-read requirement (a write-guard read would never subscribe).
            use_effect(move || buf.set(seed.read().clone()));
            rsx! {
                div { class: "editor",
                    div { class: "editor-path", "{path.display()}" }
                    textarea {
                        class: "editor-area",
                        value: "{buf}",
                        oninput: move |e| buf.set(e.value()),
                    }
                }
            }
        }
    }
}
