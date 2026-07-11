use dioxus::prelude::*;

use crate::net::view_model::ConnState;

#[component]
pub fn PromptBar(
    conn: Signal<ConnState>,
    on_send: EventHandler<String>,
    on_abort: EventHandler<()>,
) -> Element {
    let mut text = use_signal(String::new);
    let connected = matches!(*conn.read(), ConnState::Connected { .. });
    rsx! {
        div { class: "prompt-bar",
            input {
                value: "{text}",
                disabled: !connected,
                oninput: move |e| text.set(e.value()),
            }
            button {
                disabled: !connected,
                onclick: move |_| {
                    let t = text.read().clone();
                    if !t.trim().is_empty() {
                        on_send.call(t);
                        text.set(String::new());
                    }
                },
                "Send"
            }
            button {
                disabled: !connected,
                onclick: move |_| on_abort.call(()),
                "Abort"
            }
        }
    }
}
