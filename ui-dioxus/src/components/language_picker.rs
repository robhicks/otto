use dioxus::logger::tracing::warn;
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
                let raw = e.value();
                let Some(next) = Locale::from_tag(&raw) else {
                    // Unreachable through the UI — every `<option>` carries a `Locale::tag()` — so
                    // reaching it means the option values and `from_tag` have diverged. The
                    // rejected value is a locale tag, not user content, so it is safe to log.
                    warn!("language picker ignoring an unparseable locale tag: {raw:?}");
                    return;
                };
                // A provider-less mount is fully inert: with nowhere to publish the choice, the
                // pick cannot take effect, so it must not persist or relabel the document either.
                let Some(mut sig) = sink else {
                    warn!(
                        "language picker has no Signal<Locale> provider; the pick of {:?} is inert",
                        next.tag()
                    );
                    return;
                };
                sig.set(next);
                // Persisted ONLY on an explicit pick, never at startup, so environment detection
                // never becomes accidentally sticky for a user who has not chosen.
                //
                // A failed store is logged, NOT surfaced: the language still switches for this
                // session, and interrupting a user who just picked a language with an error about
                // their browser's storage policy is worse than the lost preference. But it is no
                // longer invisible — see `store.rs` on why store-side silence, unlike load-side
                // silence, was the wrong default.
                if !store_persisted_locale(next.tag()) {
                    warn!(
                        "could not persist the locale choice {:?}; it will not survive a restart",
                        next.tag()
                    );
                }
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
/// content in the right language.
///
/// Implemented on **both** real targets. The desktop target is a wry webview with a genuine
/// `documentElement`, so a screen reader there reads the same `lang` attribute a browser does —
/// treating it as a no-op would have shipped a desktop user who picks 中文 a document still
/// declaring `lang="en"`, which is a `web-sys` feature-gate dressed up as a platform property.
/// Only the mechanism differs: web writes the DOM directly through `web-sys` (already linked, and
/// synchronous), desktop goes through `dioxus::document::eval`, which needs no `web-sys` at all.
///
/// `locale.tag()` is one of a fixed set of `&'static str` literals, so the interpolated script has
/// no injection surface.
#[cfg(feature = "web")]
pub fn set_document_lang(locale: Locale) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = el.set_attribute("lang", locale.tag());
    }
}

#[cfg(all(feature = "desktop", not(feature = "web")))]
pub fn set_document_lang(locale: Locale) {
    // SAFE INTERPOLATION: the only substituted value is `locale.tag()`, which the `locales!` macro
    // fixes to one of five `&'static str` literals ("en"/"de"/"es"/"hi"/"zh-Hans"). No user,
    // server, or workspace input can reach this script, so there is no injection surface. (It is
    // reached only via a `<select>` whose values are those same literals, and `from_tag` has
    // already rejected anything else.)
    //
    // Fire-and-forget: `Eval` is a handle for reading a result back, and the script is dispatched
    // to the webview on creation — the same pattern `dioxus-desktop`'s own `create_meta`/
    // `create_style` use when they drop the handle.
    let _ = dioxus::document::eval(&format!(
        "document.documentElement.setAttribute('lang', '{}');",
        locale.tag()
    ));
}

/// The `--no-default-features` seam check has no document to label.
#[cfg(not(any(feature = "web", feature = "desktop")))]
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

    /// The `<option>` fragment carrying `value="{tag}"`, for asserting on its attributes.
    ///
    /// Splitting on `<option` rather than searching the whole document is what makes `selected`
    /// attributable to ONE option: the rendered markup carries `selected=true` only on the active
    /// one and omits the attribute entirely on the rest, so a whole-document `contains("selected")`
    /// would be true no matter which locale were active.
    fn option_for<'a>(html: &'a str, tag: &str) -> &'a str {
        html.split("<option")
            .find(|s| s.starts_with(&format!(" value=\"{tag}\"")))
            .unwrap_or_else(|| panic!("no {tag} option in: {html}"))
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
        assert!(
            option_for(&html, "de").contains("selected"),
            "de not selected: {html}"
        );
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

    #[test]
    fn set_document_lang_is_safe_to_call_from_a_mounted_component() {
        // The desktop arm dispatches through `dioxus::document::eval`, which needs a live runtime
        // and resolves to a no-op Document when none provides one (as in this SSR harness). This
        // pins that it neither panics nor wedges the render on the desktop test build — the arm
        // that used to be a bare no-op and so could not fail.
        #[component]
        fn Labels() -> Element {
            use_effect(|| set_document_lang(Locale::ZhHans));
            rsx! { div { "ok" } }
        }
        let mut dom = VirtualDom::new(Labels);
        dom.rebuild_in_place();
        assert!(dioxus_ssr::render(&dom).contains("ok"));
    }

    /// Mounts the picker with NO provider. Must render, and render AT `en` — the same guarantee
    /// that keeps `editor/dirty.rs`'s provider-less render tests working.
    ///
    /// Deliberately does NOT assert on an option LABEL. The labels are endonyms, which are
    /// locale-invariant by design, so `contains("English")` holds of every render at every locale —
    /// it would establish only "did not panic" while its message claimed to check the fallback.
    /// The two things that actually discriminate are which option is `selected` and which language
    /// the accessible name is in; both are asserted here.
    #[test]
    fn renders_at_the_en_fallback_without_a_provider() {
        #[component]
        fn Bare() -> Element {
            rsx! { LanguagePicker {} }
        }
        let mut dom = VirtualDom::new(Bare);
        dom.rebuild_in_place();
        // Rendering at all is half the guarantee: a provider-less mount must be inert, not fatal.
        let html = dioxus_ssr::render(&dom);
        assert!(
            option_for(&html, "en").contains("selected"),
            "en is not the selected option: {html}"
        );
        // …and it is the ONLY selected one, so this cannot pass by accident on another locale.
        for loc in Locale::ALL.iter().filter(|l| **l != Locale::En) {
            assert!(
                !option_for(&html, loc.tag()).contains("selected"),
                "{loc:?} is selected without a provider: {html}"
            );
        }
        // The translated part of the picker is its accessible name, so it must be the English one.
        assert!(
            html.contains(&format!(
                "aria-label=\"{}\"",
                t(Locale::En, Msg::LanguageLabel)
            )),
            "the accessible name is not the en one: {html}"
        );
    }
}
