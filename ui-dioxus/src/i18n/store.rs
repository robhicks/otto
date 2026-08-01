//! Persistence for the picked locale — the crate's first and deliberately narrowest settings
//! surface: two functions, no trait, no settings struct.
//!
//! Every failure path is swallowed. `localStorage` is absent or throws in some private-browsing
//! and sandboxed-iframe configurations, and the config dir may be unwritable; a language
//! preference is not worth an error surface, and the UI simply runs detection-only.
//!
//! Validation is NOT done here — `resolve_locale` parses the returned string via
//! `Locale::from_tag`, so a garbage or hostile stored value can at worst select a shipped locale.

#[cfg(feature = "web")]
const STORAGE_KEY: &str = "otto.ui.locale";

#[cfg(feature = "web")]
pub fn load_persisted_locale() -> Option<String> {
    let s = web_sys::window()?.local_storage().ok()??;
    let v = s.get_item(STORAGE_KEY).ok()??;
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
}

#[cfg(feature = "web")]
pub fn store_persisted_locale(tag: &str) {
    if let Some(Ok(Some(s))) = web_sys::window().map(|w| w.local_storage()) {
        let _ = s.set_item(STORAGE_KEY, tag);
    }
}

// `not(feature = "web")` is load-bearing here and on `store_persisted_locale`: these two names have
// one definition per cfg arm, so a `--features web,desktop` build would otherwise define each twice.
// The private `load_locale_from`/`store_locale_in` below need no such guard — they have no web
// counterpart to collide with, which is why their cfg is the plain one.
#[cfg(all(feature = "desktop", not(feature = "web")))]
pub fn load_persisted_locale() -> Option<String> {
    load_locale_from(&dirs::config_dir()?)
}

#[cfg(all(feature = "desktop", not(feature = "web")))]
pub fn store_persisted_locale(tag: &str) {
    if let Some(dir) = dirs::config_dir() {
        store_locale_in(&dir, tag);
    }
}

/// Read the tag from `<config_root>/otto/ui-locale`. Split out from `load_persisted_locale` so the
/// file IO is testable against a `tempfile` dir instead of the real OS config dir.
#[cfg(feature = "desktop")]
pub(crate) fn load_locale_from(config_root: &std::path::Path) -> Option<String> {
    let v = std::fs::read_to_string(config_root.join("otto").join("ui-locale")).ok()?;
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Write the tag to `<config_root>/otto/ui-locale`, best-effort.
///
/// The filename is a fixed literal and `tag` is only ever a `Locale::tag()` value, so there is no
/// user-controlled path component and no traversal surface. The OS config dir is used deliberately
/// rather than the workspace root — the engine indexes and edits that tree.
#[cfg(feature = "desktop")]
pub(crate) fn store_locale_in(config_root: &std::path::Path, tag: &str) {
    let dir = config_root.join("otto");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join("ui-locale"), tag);
    }
}

#[cfg(not(any(feature = "web", feature = "desktop")))]
pub fn load_persisted_locale() -> Option<String> {
    None
}

#[cfg(not(any(feature = "web", feature = "desktop")))]
pub fn store_persisted_locale(_tag: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_store_round_trips_through_a_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        store_locale_in(dir.path(), "de");
        assert_eq!(load_locale_from(dir.path()).as_deref(), Some("de"));
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_load_returns_none_for_an_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_locale_from(dir.path()), None);
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_load_trims_and_returns_garbage_for_the_caller_to_reject() {
        // `store`/`load` are dumb string IO; validation is `resolve_locale`'s job (it parses via
        // `Locale::from_tag`), so garbage here must surface rather than be silently swallowed.
        let dir = tempfile::tempdir().unwrap();
        store_locale_in(dir.path(), "not-a-locale\n");
        assert_eq!(
            load_locale_from(dir.path()).as_deref(),
            Some("not-a-locale")
        );
        assert_eq!(
            super::super::resolve::resolve_locale(load_locale_from(dir.path()).as_deref(), &[]),
            crate::i18n::Locale::En
        );
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_store_is_best_effort_and_never_panics_on_an_unwritable_path() {
        // A file (not a dir) as the config root: `create_dir_all` fails, and the write must be
        // swallowed rather than taking the UI down over a language preference.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"x").unwrap();
        store_locale_in(&blocker, "de"); // must not panic
        assert_eq!(load_locale_from(&blocker), None);
    }
}
