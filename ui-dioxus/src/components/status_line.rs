use dioxus::prelude::*;
use otto_protocol::CapabilitiesManifest;

use crate::net::view_model::{capability_segments, short_session, status_label, ConnState};

/// Status strip: the transport half (connection state · session · seq) plus the engine/LLM/
/// sandbox capability segments. Capability segments render only while connected with a
/// manifest present; degraded segments carry the `cap-degraded` class so lost capability is
/// visible (offline-deterministic LLM, absent sandbox).
#[component]
pub fn StatusLine(
    conn: Signal<ConnState>,
    last_seq: Signal<Option<u64>>,
    capabilities: Signal<Option<CapabilitiesManifest>>,
) -> Element {
    let c = conn.read();
    let seq = (*last_seq.read())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "—".into());
    rsx! {
        div { class: "status-line",
            span { class: "status-conn", "{status_label(&c)}" }
            if let ConnState::Connected { session } = &*c {
                span { class: "status-session", "{short_session(session)}" }
                span { class: "status-seq", "seq {seq}" }
                // Only render the capability strip when connected AND a manifest is present.
                if let Some(m) = capabilities.read().as_ref() {
                    for seg in capability_segments(m) {
                        span {
                            class: if seg.degraded { "cap cap-degraded" } else { "cap" },
                            "{seg.label}: {seg.value}"
                        }
                    }
                }
            }
        }
    }
}
