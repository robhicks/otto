use leptos::prelude::*;

use crate::view_model::ConnState;

/// Prompt input with Send / Pause-Resume / Abort. Enabled only while `Connected`. Send clears the input.
#[component]
pub fn PromptBar(
    conn: RwSignal<ConnState>,
    paused: RwSignal<bool>,
    on_send: Callback<String>,
    on_abort: Callback<()>,
    on_pause: Callback<()>,
    on_resume: Callback<()>,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let connected = move || matches!(conn.get(), ConnState::Connected { .. });

    let send = move |_| {
        let t = text.get();
        if !t.trim().is_empty() {
            on_send.run(t);
            text.set(String::new());
        }
    };

    view! {
        <div class="prompt">
            <input
                class="prompt-input"
                type="text"
                placeholder="prompt…"
                prop:value=move || text.get()
                on:input=move |e| text.set(event_target_value(&e))
                disabled=move || !connected()
            />
            <button on:click=send disabled=move || !connected()>"Send"</button>
            <button
                on:click=move |_| if paused.get() { on_resume.run(()) } else { on_pause.run(()) }
                disabled=move || !connected()
            >
                {move || if paused.get() { "Resume" } else { "Pause" }}
            </button>
            <button
                on:click=move |_| on_abort.run(())
                disabled=move || !connected()
            >"Abort"</button>
        </div>
    }
}
