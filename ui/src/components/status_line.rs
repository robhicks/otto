use leptos::prelude::*;

use crate::view_model::{short_session, status_label, ConnState};

/// Connection state + short session id + last seq. The seam sub-project B extends into the
/// capabilities status strip.
#[component]
pub fn StatusLine(conn: RwSignal<ConnState>, last_seq: RwSignal<Option<u64>>) -> impl IntoView {
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
        </div>
    }
}
