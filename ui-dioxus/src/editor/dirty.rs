//! Unsaved-buffer ("dirty") state for the editor, as a pure value type.
//!
//! Kept out of `editor/mod.rs` so the semantics are unit-testable without a rendered component
//! (this crate has no headless Dioxus render harness — every other test here is likewise a pure
//! host-side test).

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
        if self.dirty {
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
        // `Default` must agree with `clean()` — app.rs seeds the signal via `DirtyState::default`.
        assert_eq!(DirtyState::default(), state);
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
    fn opening_another_file_clears_a_dirty_buffer() {
        let mut state = DirtyState::clean();
        state.mark_edited();
        state = DirtyState::clean(); // app.rs resets on every successful open
        assert!(!state.is_dirty());
        assert_eq!(state.marker(), "");
    }
}
