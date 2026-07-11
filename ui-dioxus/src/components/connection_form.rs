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
            } else {
                button { onclick: move |_| on_connect.call(()), "Connect" }
            }
        }
    }
}
