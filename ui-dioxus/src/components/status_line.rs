use dioxus::prelude::*;
use otto_protocol::CapabilitiesManifest;

use crate::components::LanguagePicker;
use crate::i18n::use_locale;
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
                span { class: "status-seq", "seq {seq}" }
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
