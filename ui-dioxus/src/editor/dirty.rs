//! Unsaved-buffer ("dirty") state for the editor: a pure value type plus the tests that pin both
//! its semantics and the rendered marker.
//!
//! Kept out of `editor/mod.rs` so the semantics are testable as plain values, and so the concurrent
//! highlighting work in that file stays conflict-free.

/// Whether the open file's local buffer has unsaved edits.
///
/// The semantics are ported verbatim from the shipped Leptos editor
/// (`ui/src/components/editor_pane.rs` + `ui/src/app.rs`), not re-invented:
///
/// * **Clean on open.** `ui/`'s `open_path` calls `editor_dirty.set(false)` on *every* successful
///   open, including binary/oversize files that never mount a buffer.
/// * **Latched on the first edit.** `ui/` wires `on_change = |_text: String| dirty.set(true)` —
///   the callback discards the new text entirely, so any keystroke latches the flag.
/// * **Reverting an edit does NOT clear it.** Because `on_change` never compares the buffer
///   against the seed, typing a character and deleting it again leaves the marker on. That is the
///   incumbent's behavior and this port keeps it. A content-comparing "truly modified" check would
///   be a deliberate behavior change, and buffer persistence — the feature that would make the
///   distinction observable — is out of scope here exactly as it is in `ui/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyState {
    dirty: bool,
}

impl DirtyState {
    /// The state a freshly-opened file starts in: no unsaved edits.
    pub fn clean() -> Self {
        Self { dirty: false }
    }

    /// True once the buffer has been edited since the file was opened.
    pub fn is_dirty(self) -> bool {
        self.dirty
    }

    /// Latch the flag on an edit. Idempotent, and never un-latched by a subsequent edit that
    /// happens to restore the original text (see the type docs).
    pub fn mark_edited(&mut self) {
        self.dirty = true;
    }

    /// The marker appended to the open file's path label — `" ●"` when dirty, empty when clean.
    /// Matches `ui/`'s `if dirty.get() { " ●" } else { "" }` byte for byte.
    pub fn marker(self) -> &'static str {
        if self.is_dirty() {
            " ●"
        } else {
            ""
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_on_open() {
        let state = DirtyState::clean();
        assert!(!state.is_dirty());
        assert_eq!(state.marker(), "");
    }

    #[test]
    fn dirty_after_edit() {
        let mut state = DirtyState::clean();
        state.mark_edited();
        assert!(state.is_dirty());
        assert_eq!(state.marker(), " ●");
    }

    #[test]
    fn revert_to_original_content_stays_dirty() {
        // Matching `ui/`: `on_change` ignores the text, so an edit-then-undo back to the file's
        // original content leaves the buffer marked unsaved. Every edit is just another latch.
        let mut state = DirtyState::clean();
        state.mark_edited(); // typed a character
        state.mark_edited(); // deleted it again — buffer now equals the seed
        assert!(state.is_dirty());
        assert_eq!(state.marker(), " ●");
    }

    #[test]
    fn clean_is_the_only_way_back_from_dirty() {
        // `mark_edited` has no inverse — the ONLY un-latch is app.rs replacing the whole value
        // with `DirtyState::clean()` from the file-open flow (`app.rs`'s `open_path`).
        let mut state = DirtyState::clean();
        state.mark_edited();
        assert_eq!(DirtyState::clean(), DirtyState::clean());
        assert_ne!(state, DirtyState::clean());
    }
}

/// Headless render tests for the wiring — the value type above can't prove the marker actually
/// reaches the DOM, which is the regression this slice fixes (the Dioxus editor rendered no marker
/// at all). Mounts the real `Editor` in a `VirtualDom` with no renderer and asserts on the
/// server-rendered markup.
#[cfg(test)]
mod render_tests {
    use super::DirtyState;
    use crate::editor::Editor;
    use crate::net::tree::FileBody;
    use dioxus::prelude::*;
    use std::path::PathBuf;

    const PATH: &str = "src/lib.rs";
    const BODY: &str = "fn main() {}";

    /// Owns the signals `Editor` takes as props — they must be created inside a reactive scope,
    /// so the test can't build them directly.
    #[component]
    fn Harness(dirty: DirtyState) -> Element {
        let open = use_signal(|| Some((PathBuf::from(PATH), FileBody::Text(BODY.to_string()))));
        let seed = use_signal(|| BODY.to_string());
        let dirty = use_signal(|| dirty);
        rsx! { Editor { open, seed, dirty } }
    }

    fn render(dirty: DirtyState) -> String {
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { dirty });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn clean_buffer_renders_the_path_with_no_marker() {
        let html = render(DirtyState::clean());
        assert!(html.contains(PATH), "path label missing: {html}");
        assert!(
            !html.contains('●'),
            "clean buffer must not show a marker: {html}"
        );
    }

    #[test]
    fn dirty_buffer_renders_the_marker_after_the_path() {
        let mut state = DirtyState::clean();
        state.mark_edited();
        let html = render(state);
        // The marker is appended into the existing `.editor-path` text node, exactly as `ui/`
        // appends it to its `.editor-header` — so path and marker land adjacent, not in a
        // separate element.
        assert!(
            html.contains(&format!("{PATH} ●")),
            "expected `{PATH} ●` in: {html}"
        );
    }
}
