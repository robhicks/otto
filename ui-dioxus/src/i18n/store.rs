//! Persistence for the picked locale — the crate's first and deliberately narrowest settings
//! surface: two functions, no trait, no settings struct.
//!
//! **Load-side silence is correct and stays.** An absent stored value is indistinguishable from
//! "the user never chose", and both mean the same thing: run detection. There is nothing to
//! report.
//!
//! **Store-side silence is not.** A store failure is a broken promise from a deliberate user
//! action — they picked a language and it will not be there next launch. `localStorage` is absent
//! or throws in some private-browsing and sandboxed-iframe configurations, and the config dir may
//! be unwritable. `store_persisted_locale` therefore returns `bool`, so a caller can at least tell
//! success from failure and log it. That is the whole point: the product decision to stay quiet in
//! the UI is fine — the failure being *unrepresentable* was not.
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

/// Persist the tag. Returns whether it was actually stored — `false` covers every failure
/// (no window, `local_storage()` throwing, absent storage, `set_item` throwing on a quota or
/// a sandboxed origin).
#[cfg(feature = "web")]
#[must_use]
pub fn store_persisted_locale(tag: &str) -> bool {
    match web_sys::window().map(|w| w.local_storage()) {
        Some(Ok(Some(s))) => s.set_item(STORAGE_KEY, tag).is_ok(),
        _ => false,
    }
}

// `not(feature = "web")` is load-bearing here and on `store_persisted_locale`: these two names have
// one definition per cfg arm, so a `--features web,desktop` build would otherwise define each twice.
// The private `load_locale_from`/`store_locale_in` below need no such guard — they have no web
// counterpart to collide with, which is why their cfg is the plain one.
//
// This is a CRATE-WIDE convention, not a local quirk of this file: `resolve.rs`'s `env_locale_tags`
// carries the identical `all(feature = "desktop", not(feature = "web"))` guard for the identical
// reason (one name, one definition per target). Change the rule in one place and the other breaks
// the same way.
#[cfg(all(feature = "desktop", not(feature = "web")))]
pub fn load_persisted_locale() -> Option<String> {
    load_locale_from(&dirs::config_dir()?)
}

/// Persist the tag. Returns whether it was actually stored — `false` covers no config dir, an
/// undirectory-able config root, and a failed write.
#[cfg(all(feature = "desktop", not(feature = "web")))]
#[must_use]
pub fn store_persisted_locale(tag: &str) -> bool {
    dirs::config_dir().is_some_and(|dir| store_locale_in(&dir, tag))
}

/// The longest string this file may hold. A BCP-47 tag is a handful of bytes (`zh-Hans` is the
/// longest we write); 16 leaves room for a trailing newline and a future tag without letting the
/// read grow unbounded.
#[cfg(feature = "desktop")]
const MAX_LOCALE_FILE_BYTES: u64 = 16;

/// Read the tag from `<config_root>/otto/ui-locale`. Split out from `load_persisted_locale` so the
/// file IO is testable against a `tempfile` dir instead of the real OS config dir.
///
/// The read is BOUNDED to `MAX_LOCALE_FILE_BYTES`, not a `read_to_string`. This path is under the
/// user's own config dir, but it runs on the UI render thread at startup, and nothing guarantees
/// what is at that path — a multi-gigabyte file (or a FIFO, which never reaches EOF) would OOM or
/// hang the UI before it drew a frame. A locale tag has a tiny known upper bound, so taking only
/// that much costs nothing and removes the failure mode entirely. Anything longer is over-read by
/// design: it is truncated garbage, `from_tag` rejects it, and resolution falls through to
/// detection.
#[cfg(feature = "desktop")]
pub(crate) fn load_locale_from(config_root: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(config_root.join("otto").join("ui-locale")).ok()?;
    let mut buf = String::new();
    f.take(MAX_LOCALE_FILE_BYTES)
        .read_to_string(&mut buf)
        .ok()?;
    let v = buf.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Write the tag to `<config_root>/otto/ui-locale`. Returns whether the write landed.
///
/// The filename is a fixed literal and `tag` is only ever a `Locale::tag()` value, so there is no
/// user-controlled path component and no traversal surface. The OS config dir is used deliberately
/// rather than the workspace root — the engine indexes and edits that tree.
#[cfg(feature = "desktop")]
#[must_use]
pub(crate) fn store_locale_in(config_root: &std::path::Path, tag: &str) -> bool {
    let dir = config_root.join("otto");
    std::fs::create_dir_all(&dir).is_ok() && std::fs::write(dir.join("ui-locale"), tag).is_ok()
}

#[cfg(not(any(feature = "web", feature = "desktop")))]
pub fn load_persisted_locale() -> Option<String> {
    None
}

/// No storage backend in the seam-check build, so a store never succeeds — and says so, rather
/// than claiming a persistence this build cannot provide.
#[cfg(not(any(feature = "web", feature = "desktop")))]
#[must_use]
pub fn store_persisted_locale(_tag: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    // Every test below is desktop-gated (the web arm's `localStorage` needs a browser), so on a
    // web-only test build this module is empty and an unconditional import would be unused.
    #[cfg(feature = "desktop")]
    use super::*;

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_store_round_trips_through_a_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(store_locale_in(dir.path(), "de"), "store reported failure");
        assert_eq!(load_locale_from(dir.path()).as_deref(), Some("de"));
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_store_round_trips_every_shipped_tag_within_the_read_bound() {
        // The bounded read must not truncate any tag we actually write — including the longest.
        let dir = tempfile::tempdir().unwrap();
        for loc in crate::i18n::Locale::ALL {
            assert!(store_locale_in(dir.path(), loc.tag()));
            assert_eq!(load_locale_from(dir.path()).as_deref(), Some(loc.tag()));
        }
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_load_is_bounded_and_never_reads_a_huge_file_whole() {
        // A giant file at that path must not be pulled into memory on the UI render thread. It is
        // over-read to the cap, trimmed, and then rejected downstream by `from_tag` — never a hang
        // or an OOM.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("otto")).unwrap();
        let huge = "x".repeat(4 * 1024 * 1024);
        std::fs::write(dir.path().join("otto").join("ui-locale"), &huge).unwrap();

        let got = load_locale_from(dir.path()).unwrap();
        assert!(
            got.len() <= MAX_LOCALE_FILE_BYTES as usize,
            "read was not bounded: {} bytes",
            got.len()
        );
        assert_eq!(
            super::super::resolve::resolve_locale(Some(&got), &[]),
            crate::i18n::Locale::En,
            "truncated garbage must fall through to detection"
        );
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
        assert!(store_locale_in(dir.path(), "not-a-locale\n"));
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
    fn desktop_store_reports_failure_on_an_unwritable_path_instead_of_panicking() {
        // A file (not a dir) as the config root: `create_dir_all` fails. The write must not take
        // the UI down over a language preference — but it must also not claim to have succeeded,
        // which is the whole reason this returns `bool`.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(
            !store_locale_in(&blocker, "de"),
            "failure reported as success"
        );
        assert_eq!(load_locale_from(&blocker), None);
    }
}
