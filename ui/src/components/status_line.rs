use leptos::prelude::*;
use otto_protocol::CapabilitiesManifest;

use crate::view_model::{
    capability_segments, cost_estimate, format_meter, short_session, status_label, ConnState,
};

/// Status strip: the transport half (connection state · session · seq) plus the engine/LLM/
/// sandbox capability segments. Capability segments render only while connected with a
/// manifest; degraded segments carry the `cap-degraded` class so lost capability is visible.
#[component]
pub fn StatusLine(
    conn: RwSignal<ConnState>,
    last_seq: RwSignal<Option<u64>>,
    capabilities: RwSignal<Option<CapabilitiesManifest>>,
    meter: RwSignal<Option<(u64, u64)>>,
) -> impl IntoView {
    view! {
        <div class="status">
            {move || {
                let c = conn.get();
                let sess = match &c {
                    ConnState::Connected { session } => short_session(session),
                    _ => "-".to_string(),
                };
                let seq = last_seq.get().map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());
                format!("status: {} · {} · seq {}", status_label(&c), sess, seq)
            }}
            {move || {
                // Only while connected AND a manifest is present — never show a stale one.
                let connected = matches!(conn.get(), ConnState::Connected { .. });
                capabilities.get().filter(|_| connected).map(|m| {
                    let segs = capability_segments(&m);
                    view! {
                        <span class="cap-group">
                            <span class="cap-sep">" | "</span>
                            {segs
                                .into_iter()
                                .enumerate()
                                .map(|(i, s)| {
                                    let cls = if s.degraded { "cap cap-degraded" } else { "cap" };
                                    let text = format!("{}: {}", s.label, s.value);
                                    let sep = if i == 0 { "" } else { " · " };
                                    view! { <span class="cap-sep">{sep}</span><span class=cls>{text}</span> }
                                })
                                .collect_view()}
                        </span>
                    }
                })
            }}
            {move || {
                let connected = matches!(conn.get(), ConnState::Connected { .. });
                meter.get().filter(|_| connected).map(|(i, o)| {
                    let remote = capabilities.get().map(|m| m.remote_llm).unwrap_or(false);
                    let text = match cost_estimate(i, o, remote) {
                        Some(c) => format!("{} · ~${:.4}", format_meter(i, o), c),
                        None => format_meter(i, o),
                    };
                    view! { <span class="cap-sep">" | "</span><span class="meter">{text}</span> }
                })
            }}
        </div>
    }
}
