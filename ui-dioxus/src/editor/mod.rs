use std::path::PathBuf;

use dioxus::prelude::*;

use crate::net::tree::FileBody;

pub mod tokens;

/// The unhighlighted (plain-`textarea`) controlled-buffer editor. `open` is the currently-open
/// file (path + classified body); `seed` is the initial document text set once at open time (a
/// later part swaps this for a styled-span highlighted render — the diff-first non-goal means no
/// VSCode-scale features here). No persistence in this slice.
#[component]
pub fn Editor(open: Signal<Option<(PathBuf, FileBody)>>, seed: Signal<String>) -> Element {
    // Dioxus hooks are POSITIONAL: this persistent (never-remounted) `Editor` instance must call
    // the same hooks in the same order/count on EVERY render, regardless of `open`. So both the
    // buffer signal and the re-seed effect are declared UNCONDITIONALLY at the top of the body,
    // BEFORE any early-return or match branch — mirroring the correct pattern `FileTreeNode` uses.
    // (Nesting them inside the `FileBody::Text` arm would make hook count vary 0-vs-2 across
    // renders, an immediate downcast panic the moment any hook is added around the match — which
    // Task 10's styled-span editor will do.)
    let mut buf = use_signal(|| seed.read().clone());
    // Re-seed when a different file opens. `seed.read()` is a tracked read, so this fires whenever
    // app.rs writes a new seed on open_path — keeping the textarea in sync with the newly-opened
    // file (a write-guard access would never subscribe).
    use_effect(move || buf.set(seed.read().clone()));

    let Some((path, body)) = open.read().clone() else {
        return rsx! { div { class: "editor-empty", "No file open" } };
    };
    match body {
        FileBody::Binary => rsx! { div { class: "editor-notice", "binary file — not editable" } },
        FileBody::TooLarge => rsx! { div { class: "editor-notice", "file too large to edit" } },
        FileBody::Text(_) => {
            // `plain_spans` is a plain function call, not a hook — safe to call here inside the
            // `Text` arm (unlike `buf`/the re-seed effect above, which must stay hoisted and
            // unconditional; see the comment at the top of this component). Tasks 11/12 swap this
            // call for a real tokenizer behind the same `Vec<Vec<Span>>` shape.
            let spans = tokens::plain_spans(&buf.read());
            rsx! {
                div { class: "editor",
                    div { class: "editor-path", "{path.display()}" }
                    div { class: "editor-stack",
                        pre { class: "editor-highlight",
                            for line in spans {
                                div { class: "hl-line",
                                    for sp in line {
                                        span { class: "{sp.class}", "{sp.text}" }
                                    }
                                }
                            }
                        }
                        textarea {
                            class: "editor-area editor-overlay",
                            value: "{buf}",
                            oninput: move |e| buf.set(e.value()),
                        }
                    }
                }
            }
        }
    }
}
