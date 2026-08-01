use dioxus::prelude::*;
use otto_protocol::CapabilitiesManifest;

use crate::components::LanguagePicker;
use crate::i18n::{tf, use_locale, Msg};
use crate::net::view_model::{
    capability_segments, cost_estimate, format_meter, short_session, status_label, ConnState,
};

/// Status strip: the transport half (connection state · session · seq) plus the engine/LLM/
/// sandbox capability segments, plus the token/cost meter. Capability and meter segments render
/// only while connected; degraded capability segments carry the `cap-degraded` class so lost
/// capability is visible (offline-deterministic LLM, absent sandbox).
#[component]
pub fn StatusLine(
    conn: Signal<ConnState>,
    last_seq: Signal<Option<u64>>,
    capabilities: Signal<Option<CapabilitiesManifest>>,
    meter: Signal<Option<(u64, u64)>>,
) -> Element {
    let locale = use_locale();
    let c = conn.read();
    let seq = (*last_seq.read())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "—".into());
    rsx! {
        div { class: "status",
            span { class: "status-conn", "{status_label(locale, &c)}" }
            if let ConnState::Connected { session } = &*c {
                span { class: "status-session", "{short_session(session)}" }
                span { class: "status-seq", {tf(locale, Msg::SeqLabel, &[("seq", &seq)])} }
                // Only render the capability strip when connected AND a manifest is present.
                if let Some(m) = capabilities.read().as_ref() {
                    for seg in capability_segments(locale, m) {
                        span {
                            class: if seg.degraded { "cap cap-degraded" } else { "cap" },
                            "{seg.label}: {seg.value}"
                        }
                    }
                }
                // Token/cost meter: tokens always shown once set; the dollar estimate only when
                // a remote (billable) model is configured.
                if let Some((i, o)) = *meter.read() {
                    span { class: "meter", "{format_meter(locale, i, o)}" }
                    if let Some(m) = capabilities.read().as_ref() {
                        if let Some(cost) = cost_estimate(i, o, m.remote_llm) {
                            span { class: "meter-cost", "${cost:.4}" }
                        }
                    }
                }
            }
            // Outside the `Connected` block on purpose: switching language must be reachable
            // before connecting, which is its most likely first use.
            LanguagePicker {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    #[component]
    fn Harness(start: Locale) -> Element {
        use_context_provider(|| Signal::new(start));
        let conn = use_signal(|| ConnState::Connected {
            session: "abcd1234".to_string(),
        });
        let last_seq = use_signal(|| Some(7u64));
        let capabilities = use_signal(|| {
            Some(CapabilitiesManifest {
                engine_remote: false,
                local_llm: true,
                remote_llm: false,
                sandbox: false,
            })
        });
        let meter = use_signal(|| None::<(u64, u64)>);
        rsx! { StatusLine { conn, last_seq, capabilities, meter } }
    }

    fn render(start: Locale) -> String {
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { start });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_seq_label_follows_the_locale() {
        assert!(render(Locale::En).contains("seq 7"));
        assert!(render(Locale::ZhHans).contains("序号 7"));
    }

    #[test]
    fn capability_copy_follows_the_locale() {
        // Wired in Task 3; asserted here so the strip's full copy is covered end to end.
        assert!(
            render(Locale::De).contains("aus"),
            "sandbox value not localized"
        );
    }

    #[test]
    fn status_strip_renders_without_a_provider() {
        // The same provider-less guarantee `editor/dirty.rs`'s render tests rely on.
        #[component]
        fn Bare() -> Element {
            let conn = use_signal(|| ConnState::Disconnected);
            let last_seq = use_signal(|| None::<u64>);
            let capabilities = use_signal(|| None::<CapabilitiesManifest>);
            let meter = use_signal(|| None::<(u64, u64)>);
            rsx! { StatusLine { conn, last_seq, capabilities, meter } }
        }
        let mut dom = VirtualDom::new(Bare);
        dom.rebuild_in_place();
        assert!(dioxus_ssr::render(&dom).contains("disconnected"));
    }
}
