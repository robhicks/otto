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
    env_tags
        .iter()
        .find_map(|t| Locale::from_tag(t))
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
}
