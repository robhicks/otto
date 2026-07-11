use dioxus::prelude::*;

use crate::net::view_model::ConnState;

#[component]
pub fn ConnectionForm(
    url: Signal<String>,
    token: Signal<String>,
    conn: Signal<ConnState>,
    on_connect: EventHandler<()>,
    on_disconnect: EventHandler<()>,
) -> Element {
    let connected = matches!(*conn.read(), ConnState::Connected { .. });
    let connecting = matches!(*conn.read(), ConnState::Connecting);
    rsx! {
        div { class: "connection-form",
            input {
                value: "{url}",
                placeholder: "ws://127.0.0.1:8787",
                oninput: move |e| url.set(e.value()),
            }
            input {
                value: "{token}",
                r#type: "password",
                placeholder: "token",
                oninput: move |e| token.set(e.value()),
            }
            if connected {
                button { onclick: move |_| on_disconnect.call(()), "Disconnect" }
            } else if connecting {
                // Disabled while a connect is in flight so a double-click can't re-enter
                // `do_connect` (which would orphan the in-progress socket). Mirrors `ui/`'s
                // disabled-on-Connecting behavior.
                button { disabled: true, "Connecting…" }
            } else {
                button { onclick: move |_| on_connect.call(()), "Connect" }
            }
        }
    }
}
