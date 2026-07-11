use dioxus::prelude::*;

use crate::net::view_model::ConnState;

#[component]
pub fn PromptBar(
    conn: Signal<ConnState>,
    paused: Signal<bool>,
    on_send: EventHandler<String>,
    on_abort: EventHandler<()>,
    on_pause: EventHandler<()>,
    on_resume: EventHandler<()>,
) -> Element {
    let mut text = use_signal(String::new);
    let connected = matches!(*conn.read(), ConnState::Connected { .. });
    let is_paused = *paused.read();
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
                onclick: move |_| if is_paused { on_resume.call(()) } else { on_pause.call(()) },
                if is_paused { "Resume" } else { "Pause" }
            }
            button {
                disabled: !connected,
                onclick: move |_| on_abort.call(()),
                "Abort"
            }
        }
    }
}
