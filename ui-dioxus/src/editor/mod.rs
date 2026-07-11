use std::path::PathBuf;

use dioxus::prelude::*;

#[cfg(feature = "desktop")]
use crate::net::tree::language_for_path;
use crate::net::tree::FileBody;

pub mod tokens;

#[cfg(feature = "desktop")]
mod highlight_native;

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
    // Element ref to the highlight `pre`, captured via `onmounted` below. HOISTED here (positional
    // hooks: every render must call the same hooks in the same order/count regardless of `open`),
    // exactly like `buf`/the re-seed effect. Used by the textarea's `onscroll` to mirror the
    // overlay's scroll offset onto the underlying highlight layer so the two never desync on a file
    // taller/wider than the viewport (the classic textarea-over-pre gotcha). Both `dioxus-web` and
    // `dioxus-desktop` implement `MountedData::scroll`, so this is one cross-target path — no cfg.
    let mut highlight_ref = use_signal(|| None::<MountedEvent>);

    let Some((path, body)) = open.read().clone() else {
        return rsx! { div { class: "editor-empty", "No file open" } };
    };
    match body {
        FileBody::Binary => rsx! { div { class: "editor-notice", "binary file — not editable" } },
        FileBody::TooLarge => rsx! { div { class: "editor-notice", "file too large to edit" } },
        FileBody::Text(_) => {
            // Span source is a plain function call, not a hook — safe to call here inside the
            // `Text` arm (unlike `buf`/the re-seed effect above, which must stay hoisted and
            // unconditional; see the comment at the top of this component). Desktop gets real
            // tree-sitter-classified spans; web keeps `plain_spans` until Task 12 wires web
            // highlighting.
            #[cfg(feature = "desktop")]
            let spans = {
                let lang = language_for_path(&path);
                highlight_native::highlight(&buf.read(), lang)
            };
            #[cfg(feature = "web")]
            let spans = tokens::plain_spans(&buf.read());
            // Neither target feature enabled (e.g. `cargo test --no-default-features` for the
            // pure `editor::tokens`/`net::` seams): the crate still needs to type-check, so fall
            // back to the plain baseline rather than fail to compile.
            #[cfg(not(any(feature = "web", feature = "desktop")))]
            let spans = tokens::plain_spans(&buf.read());
            rsx! {
                div { class: "editor",
                    div { class: "editor-path", "{path.display()}" }
                    div { class: "editor-stack",
                        pre {
                            class: "editor-highlight",
                            onmounted: move |e| highlight_ref.set(Some(e)),
                            for line in spans {
                                div { class: "hl-line",
                                    for sp in line {
                                        span { class: "{sp.class}", "{sp.text}" }
                                    }
                                }
                            }
                        }
                        textarea {
                            class: "editor-overlay",
                            value: "{buf}",
                            oninput: move |e| buf.set(e.value()),
                            // Mirror the overlay textarea's scroll offset onto the highlight `pre`
                            // beneath it. `scroll` is scrollTo-absolute (returns a future, so
                            // spawn it); `ScrollBehavior::Instant` keeps the layers locked frame
                            // for frame with no smooth-scroll lag. x = scroll_left, y = scroll_top.
                            onscroll: move |e| {
                                let (top, left) = (e.scroll_top(), e.scroll_left());
                                if let Some(hl) = highlight_ref() {
                                    spawn(async move {
                                        let _ = hl
                                            .scroll(
                                                dioxus::html::geometry::PixelsVector2D::new(left, top),
                                                ScrollBehavior::Instant,
                                            )
                                            .await;
                                    });
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}
