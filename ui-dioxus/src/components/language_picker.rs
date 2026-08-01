use dioxus::prelude::*;

use crate::i18n::{store_persisted_locale, t, use_locale, Locale, Msg};

/// The language selector: a native `<select>`, one option per shipped locale.
///
/// **Takes no props by design.** It is the only writer of the locale signal, so threading a
/// `Signal<Locale>` prop would force `StatusLine` to grow a prop it does not otherwise need and
/// `App` to thread it. It consumes the context directly instead, and is inert (rather than
/// panicking) when mounted without a provider.
///
/// Option labels are ENDONYMS and are deliberately not catalog entries — a reader currently stuck
/// in a language they cannot read must still be able to find their own. Only the accessible name
/// is translated, because a screen reader announces it in the current language.
///
/// A native `<select>` is keyboard-reachable and screen-reader-labeled by default on both targets
/// (the desktop target is a webview), which is why it beats a custom dropdown here.
#[component]
pub fn LanguagePicker() -> Element {
    let active = use_locale();
    // Not `use_locale()` — the handler needs to WRITE. `try_use_context` is a hook, so this is
    // called unconditionally at the top of the body like every other hook in this crate.
    let sink = try_use_context::<Signal<Locale>>();
    rsx! {
        select {
            class: "lang-picker",
            "aria-label": "{t(active, Msg::LanguageLabel)}",
            onchange: move |e| {
                let Some(next) = Locale::from_tag(&e.value()) else { return };
                // A provider-less mount is fully inert: with nowhere to publish the choice, the
                // pick cannot take effect, so it must not persist or relabel the document either.
                let Some(mut sig) = sink else { return };
                sig.set(next);
                // Persisted ONLY on an explicit pick, never at startup, so environment detection
                // never becomes accidentally sticky for a user who has not chosen.
                store_persisted_locale(next.tag());
                set_document_lang(next);
            },
            for loc in Locale::ALL {
                option {
                    key: "{loc.tag()}",
                    value: "{loc.tag()}",
                    selected: loc == active,
                    "{loc.endonym()}"
                }
            }
        }
    }
}

/// Keep `document.documentElement.lang` in step with the active locale so assistive tech announces
/// content in the right language. Web-only; a no-op on desktop and in the seam-check build.
#[cfg(feature = "web")]
pub fn set_document_lang(locale: Locale) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = el.set_attribute("lang", locale.tag());
    }
}

#[cfg(not(feature = "web"))]
pub fn set_document_lang(_locale: Locale) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    /// Mounts the picker WITH a provider — the ordinary case.
    #[component]
    fn Provided(start: Locale) -> Element {
        use_context_provider(|| Signal::new(start));
        rsx! { LanguagePicker {} }
    }

    fn render_with(start: Locale) -> String {
        let mut dom = VirtualDom::new_with_props(Provided, ProvidedProps { start });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn lists_every_language_endonymically() {
        let html = render_with(Locale::En);
        for loc in Locale::ALL {
            assert!(
                html.contains(loc.endonym()),
                "missing endonym {} in: {html}",
                loc.endonym()
            );
        }
    }

    #[test]
    fn marks_the_active_locale_selected() {
        let html = render_with(Locale::De);
        // The `de` option carries `selected`; find its tag and check.
        let de_opt = html
            .split("<option")
            .find(|s| s.contains("value=\"de\""))
            .unwrap_or_else(|| panic!("no de option in: {html}"));
        assert!(de_opt.contains("selected"), "de not selected: {de_opt}");
    }

    #[test]
    fn carries_an_accessible_label() {
        let html = render_with(Locale::En);
        assert!(html.contains("aria-label"), "no aria-label in: {html}");
        assert!(html.contains("Language"), "label not localized in: {html}");
    }

    #[test]
    fn the_accessible_label_follows_the_active_locale() {
        assert!(render_with(Locale::De).contains("Sprache"));
    }

    /// Mounts the picker with NO provider. Must render (falling back to `en`) rather than panic —
    /// the same guarantee that keeps `editor/dirty.rs`'s provider-less render tests working.
    #[test]
    fn renders_without_a_provider_instead_of_panicking() {
        #[component]
        fn Bare() -> Element {
            rsx! { LanguagePicker {} }
        }
        let mut dom = VirtualDom::new(Bare);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("English"), "expected en fallback in: {html}");
    }
}
