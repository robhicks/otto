//! Locale resolution: a pure precedence function plus per-target environment detection.

use super::Locale;

/// Choose the active locale. Precedence: an explicitly persisted choice, then the environment's
/// ordered preferences, then English.
///
/// Pure and browser-free so the precedence rule is host-testable — the per-target detection that
/// produces `env_tags` lives in `env_locale_tags` below.
///
/// An unparseable `persisted` value is treated as absent: a tag written by a future build (or a
/// hostile `localStorage` entry) must degrade to detection, never wedge the UI.
pub fn resolve_locale(persisted: Option<&str>, env_tags: &[String]) -> Locale {
    if let Some(loc) = persisted.and_then(Locale::from_tag) {
        return loc;
    }
    // First PARSEABLE tag, not first tag: the list is ordered by user preference, so a preference
    // for a language we don't ship must not shadow a later one we do.
    //
    // The scan is VARIANT-AWARE for Chinese, and that is load-bearing. Browsers append the base
    // tag after the regional one — a Traditional reader reports
    // `["zh-TW", "zh", "en-US", "en"]`, and POSIX `LANGUAGE` yields `zh_TW:zh:en`. `from_tag`
    // correctly rejects `zh-TW`, but the very next entry is a bare `zh`, which on its own is
    // reasonably Simplified. Composed, the two rules would serve Simplified to exactly the reader
    // the rejection exists to protect. So once a `zh-*` tag has been rejected, a later bare `zh`
    // is understood as that variant's base tag rather than an independent Simplified preference,
    // and the scan continues to the next language. A genuine Simplified user is unaffected:
    // `zh-CN` matches before any rejection can be recorded.
    let mut zh_rejected = false;
    env_tags
        .iter()
        .find_map(|tag| {
            let norm = tag.trim().replace('_', "-").to_ascii_lowercase();
            let is_zh = norm == "zh" || norm.starts_with("zh-");
            match Locale::from_tag(tag) {
                Some(Locale::ZhHans) if zh_rejected => None,
                Some(loc) => Some(loc),
                None => {
                    zh_rejected |= is_zh;
                    None
                }
            }
        })
        .unwrap_or(Locale::En)
}

/// The environment's preferred locales, most-preferred first.
#[cfg(feature = "web")]
pub fn env_locale_tags() -> Vec<String> {
    let Some(win) = web_sys::window() else {
        return Vec::new();
    };
    let nav = win.navigator();
    // `languages` is the ordered preference list; `language` is the single-value fallback.
    let mut out: Vec<String> = nav
        .languages()
        .iter()
        .filter_map(|v| v.as_string())
        .collect();
    if out.is_empty() {
        if let Some(l) = nav.language() {
            out.push(l);
        }
    }
    out
}

// `not(feature = "web")` is load-bearing: `env_locale_tags` has one definition per cfg arm, so a
// `--features web,desktop` build would otherwise define it twice. Same crate-wide convention (and
// same reason) as `store.rs`'s `load_persisted_locale`/`store_persisted_locale`, where it is
// spelled out at length.
#[cfg(all(feature = "desktop", not(feature = "web")))]
pub fn env_locale_tags() -> Vec<String> {
    sys_locale::get_locales().collect()
}

/// Neither target feature (the `--no-default-features` seam check): no environment to read, so
/// resolution yields `En`.
#[cfg(not(any(feature = "web", feature = "desktop")))]
pub fn env_locale_tags() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn persisted_choice_beats_environment_detection() {
        // The AC's named precedence rule.
        assert_eq!(resolve_locale(Some("de"), &tags(&["es"])), Locale::De);
    }

    #[test]
    fn unparseable_persisted_value_falls_through_to_environment() {
        // A stale tag from a future build must not wedge the UI in the wrong language.
        assert_eq!(resolve_locale(Some("xx-YY"), &tags(&["es"])), Locale::Es);
    }

    #[test]
    fn environment_uses_the_first_parseable_tag_not_the_first_tag() {
        // navigator.languages is ordered by user preference: an unshipped first choice must not
        // shadow a shipped second choice.
        assert_eq!(resolve_locale(None, &tags(&["fr", "de"])), Locale::De);
    }

    #[test]
    fn falls_back_to_english_when_nothing_matches() {
        assert_eq!(resolve_locale(None, &tags(&["fr", "pt-BR"])), Locale::En);
        assert_eq!(resolve_locale(None, &[]), Locale::En);
        assert_eq!(resolve_locale(Some("zh-Hant"), &[]), Locale::En);
    }

    #[test]
    fn environment_tags_are_normalized_like_persisted_ones() {
        assert_eq!(resolve_locale(None, &tags(&["zh-CN"])), Locale::ZhHans);
        assert_eq!(resolve_locale(None, &tags(&["en_US"])), Locale::En);
    }

    #[test]
    fn a_traditional_chinese_environment_does_not_resolve_to_simplified() {
        // The real shapes production sees. A browser appends the base tag after the regional one,
        // so the bare `zh` that follows an explicitly-Traditional tag is that tag's base — not an
        // independent Simplified preference. Testing `from_tag` and `resolve_locale` separately
        // hid this: only the composed list reaches users.
        assert_eq!(
            resolve_locale(None, &tags(&["zh-TW", "zh", "en-US", "en"])),
            Locale::En
        );
        assert_eq!(
            resolve_locale(None, &tags(&["zh-Hant-HK", "zh", "en"])),
            Locale::En
        );
        // POSIX `LANGUAGE=zh_TW:zh:en`, underscore-separated.
        assert_eq!(
            resolve_locale(None, &tags(&["zh_TW", "zh", "en"])),
            Locale::En
        );
        // With no other shipped language after it, the fallback is still English, never Simplified.
        assert_eq!(resolve_locale(None, &tags(&["zh-TW", "zh"])), Locale::En);
    }

    #[test]
    fn a_genuine_simplified_environment_still_resolves_to_simplified() {
        // The suppression must be narrow: it triggers only AFTER a `zh-*` rejection.
        assert_eq!(
            resolve_locale(None, &tags(&["zh-CN", "zh", "en"])),
            Locale::ZhHans
        );
        assert_eq!(
            resolve_locale(None, &tags(&["zh-Hans", "zh", "en"])),
            Locale::ZhHans
        );
        // A bare `zh` with no other Chinese signal at all is reasonably Simplified.
        assert_eq!(resolve_locale(None, &tags(&["zh"])), Locale::ZhHans);
        assert_eq!(resolve_locale(None, &tags(&["zh", "en"])), Locale::ZhHans);
    }

    #[test]
    fn a_zh_rejection_does_not_suppress_other_languages() {
        // Only Simplified is suppressed by a prior `zh-*` rejection; an unrelated shipped language
        // later in the list must still win normally.
        assert_eq!(
            resolve_locale(None, &tags(&["zh-TW", "zh", "de"])),
            Locale::De
        );
        // And an explicit persisted choice always beats the whole scan.
        assert_eq!(
            resolve_locale(Some("zh-Hans"), &tags(&["zh-TW", "zh", "en"])),
            Locale::ZhHans
        );
    }
}
