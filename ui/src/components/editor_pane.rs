use std::path::PathBuf;
use std::sync::Arc;

use kode_leptos::{CodeEditor, Language, Theme};
use leptos::prelude::*;

use crate::tree::{language_for_path, FileBody};

/// The editor pane. `open` is the currently-open file (path + classified body); `seed` is the
/// initial document text set once at open time (it drives the editor's `content` and is NOT
/// updated on keystroke, so typing never resets the doc); `dirty` is flipped true on the first
/// edit and shown as a "●" marker. No persistence in this slice.
#[component]
pub fn EditorPane(
    open: Signal<Option<(PathBuf, FileBody)>>,
    seed: Signal<String>,
    dirty: RwSignal<bool>,
) -> impl IntoView {
    view! {
        <div class="editor-pane">
            {move || match open.get() {
                None => view! { <div class="editor-empty">"No file open"</div> }.into_any(),
                Some((_, FileBody::Binary)) => {
                    view! { <div class="editor-notice">"binary file — not editable"</div> }
                        .into_any()
                }
                Some((_, FileBody::TooLarge)) => {
                    view! { <div class="editor-notice">"file too large to edit"</div> }.into_any()
                }
                Some((path, FileBody::Text(_))) => {
                    let lang = kode_language(language_for_path(&path));
                    let header = path.display().to_string();
                    view! {
                        <div class="editor-host">
                            <div class="editor-header">
                                {header}
                                {move || if dirty.get() { " ●" } else { "" }}
                            </div>
                            <CodeEditor
                                language=lang
                                content=seed
                                theme=Theme::default()
                                on_change=Arc::new(move |_text: String| dirty.set(true))
                            />
                        </div>
                    }
                    .into_any()
                }
            }}
        </div>
    }
}

/// Map the stable language id from `language_for_path` to a `kode_leptos::Language`.
/// kode-leptos resolves lowercase grammar names internally (rust/json/markdown/…); an
/// unregistered name renders as plain text, so passing the id through `new_static` is safe.
/// "text" maps to the explicit `Language::PLAIN` constant.
fn kode_language(id: &'static str) -> Language {
    if id == "text" {
        Language::PLAIN
    } else {
        Language::new_static(id)
    }
}
