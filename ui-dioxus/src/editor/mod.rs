use std::path::PathBuf;

use dioxus::prelude::*;

use crate::i18n::{t, use_locale, Msg};
// Both highlight backends need `language_for_path`; neither is compiled when the crate is built
// with no target feature (`cargo build --no-default-features`, the pure `editor::tokens`/`net::`
// seam check), so the import is gated to match or it warns as unused there.
#[cfg(any(feature = "desktop", feature = "web"))]
use crate::net::tree::language_for_path;
use crate::net::tree::FileBody;

pub mod dirty;
pub mod tokens;

#[cfg(feature = "desktop")]
mod highlight_native;

// The web highlighter is a dependency-free lexer, so unlike `highlight_native` there is no dep to
// feature-gate. It is still gated on `web` so the desktop binary doesn't carry code only the web
// arm below calls — but `test` is OR'd in deliberately, so its tests run under the crate's ordinary
// `cargo test --features desktop` rather than needing a wasm test runner.
#[cfg(any(feature = "web", test))]
mod highlight_web;

pub use dirty::DirtyState;

/// The controlled-buffer editor: a transparent `textarea` overlaid on a syntax-highlighted `pre`.
/// `open` is the currently-open file (path + classified body); `seed` is the initial document text
/// set once at open time (the diff-first non-goal means no VSCode-scale features here — no
/// persistence, no completion); `dirty` is the unsaved-buffer flag app.rs clears on every open and
/// this component latches on the first keystroke, rendered as a "●" beside the path (see
/// `dirty::DirtyState` for the semantics, which mirror `ui/src/components/editor_pane.rs`).
/// No persistence in this slice.
#[component]
pub fn Editor(
    open: Signal<Option<(PathBuf, FileBody)>>,
    seed: Signal<String>,
    mut dirty: Signal<DirtyState>,
) -> Element {
    // Dioxus hooks are POSITIONAL: this persistent (never-remounted) `Editor` instance must call
    // the same hooks in the same order/count on EVERY render, regardless of `open`. So both the
    // buffer signal and the re-seed effect are declared UNCONDITIONALLY at the top of the body,
    // BEFORE any early-return or match branch — mirroring the correct pattern `FileTreeNode` uses.
    // (Nesting them inside the `FileBody::Text` arm would make hook count vary 0-vs-2 across
    // renders, an immediate downcast panic the moment any hook is added around the match — which
    // Task 10's styled-span editor will do.)
    //
    // `use_locale()` is a hook too, and the notices it feeds sit on BOTH sides of the `open` early
    // return below — so it is hoisted here for exactly the same reason.
    let locale = use_locale();
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

    // Not a hook — a plain TRACKED read, which is what subscribes this component to `dirty` so the
    // marker actually re-renders when `oninput` latches the flag (a `.peek()`/write-guard access
    // would never subscribe). Hoisted above the early return so the subscription is unconditional.
    let dirty_marker = dirty().marker();

    let Some((path, body)) = open.read().clone() else {
        return rsx! { div { class: "editor-empty", {t(locale, Msg::EditorNoFileOpen)} } };
    };
    match body {
        FileBody::Binary => {
            rsx! { div { class: "editor-notice", {t(locale, Msg::EditorBinary)} } }
        }
        FileBody::TooLarge => {
            rsx! { div { class: "editor-notice", {t(locale, Msg::EditorTooLarge)} } }
        }
        FileBody::Text(_) => {
            // Span source is a plain function call, not a hook — safe to call here inside the
            // `Text` arm (unlike `buf`/the re-seed effect above, which must stay hoisted and
            // unconditional; see the comment at the top of this component). BOTH targets highlight
            // now, over the same five languages and emitting the same `tok-*` classes, so one
            // `style.css` serves both. They differ only in fidelity: desktop classifies with native
            // tree-sitter, while web uses a dependency-free lexer because tree-sitter's C grammars
            // cannot build for `wasm32-unknown-unknown` and every wasm-capable alternative measured
            // at +1.5 MB or worse against a sub-800 KB bundle. See
            // docs/superpowers/specs/2026-07-24-ui-dioxus-web-highlighting-design.md.
            #[cfg(feature = "desktop")]
            let spans = {
                let lang = language_for_path(&path);
                highlight_native::highlight(&buf.read(), lang)
            };
            #[cfg(feature = "web")]
            let spans = {
                let lang = language_for_path(&path);
                highlight_web::highlight(&buf.read(), lang)
            };
            // Neither target feature enabled (e.g. `cargo test --no-default-features` for the
            // pure `editor::tokens`/`net::` seams): the crate still needs to type-check, so fall
            // back to the plain baseline rather than fail to compile.
            #[cfg(not(any(feature = "web", feature = "desktop")))]
            let spans = tokens::plain_spans(&buf.read());
            rsx! {
                div { class: "editor",
                    div { class: "editor-path", "{path.display()}{dirty_marker}" }
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
                            oninput: move |e| {
                                buf.set(e.value());
                                // Latch the unsaved marker — unconditionally, and without
                                // comparing against `seed`, which is exactly what makes an
                                // edit-then-revert stay dirty (see `dirty::DirtyState`). Note the
                                // re-seed effect above writes `buf` directly and never fires
                                // `oninput`, so opening a file can't mark it dirty.
                                dirty.write().mark_edited();
                            },
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
