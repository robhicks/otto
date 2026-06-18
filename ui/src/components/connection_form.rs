use leptos::prelude::*;

use crate::view_model::ConnState;

/// Engine URL + token inputs; Connect when disconnected, Disconnect otherwise.
/// Inputs are disabled while not disconnected so the URL/token can't change mid-session.
#[component]
pub fn ConnectionForm(
    url: RwSignal<String>,
    token: RwSignal<String>,
    conn: RwSignal<ConnState>,
    on_connect: Callback<()>,
    on_disconnect: Callback<()>,
) -> impl IntoView {
    let disconnected = move || matches!(conn.get(), ConnState::Disconnected);

    view! {
        <div class="conn-form">
            <input
                class="url-input"
                type="text"
                placeholder="ws://127.0.0.1:8787"
                prop:value=move || url.get()
                on:input=move |e| url.set(event_target_value(&e))
                disabled=move || !disconnected()
            />
            <input
                class="token-input"
                type="password"
                placeholder="token"
                prop:value=move || token.get()
                on:input=move |e| token.set(event_target_value(&e))
                disabled=move || !disconnected()
            />
            {move || {
                if disconnected() {
                    view! { <button on:click=move |_| on_connect.run(())>"Connect"</button> }
                        .into_any()
                } else {
                    view! { <button on:click=move |_| on_disconnect.run(())>"Disconnect"</button> }
                        .into_any()
                }
            }}
        </div>
    }
}
