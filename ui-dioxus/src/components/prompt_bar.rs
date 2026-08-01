use dioxus::prelude::*;

use crate::i18n::{t, use_locale, Msg};
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
    let locale = use_locale();
    let mut text = use_signal(String::new);
    let connected = matches!(*conn.read(), ConnState::Connected { .. });
    let is_paused = *paused.read();
    rsx! {
        div { class: "prompt",
            input {
                class: "prompt-input",
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
                {t(locale, Msg::Send)}
            }
            button {
                disabled: !connected,
                onclick: move |_| if is_paused { on_resume.call(()) } else { on_pause.call(()) },
                if is_paused { {t(locale, Msg::Resume)} } else { {t(locale, Msg::Pause)} }
            }
            button {
                disabled: !connected,
                onclick: move |_| on_abort.call(()),
                {t(locale, Msg::Abort)}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    #[component]
    fn Harness(start: Locale, paused_start: bool) -> Element {
        use_context_provider(|| Signal::new(start));
        let conn = use_signal(|| ConnState::Connected {
            session: "s".to_string(),
        });
        let paused = use_signal(|| paused_start);
        rsx! {
            PromptBar {
                conn, paused,
                on_send: move |_: String| {},
                on_abort: move |_| {},
                on_pause: move |_| {},
                on_resume: move |_| {},
            }
        }
    }

    fn render(start: Locale, paused_start: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            Harness,
            HarnessProps {
                start,
                paused_start,
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn prompt_buttons_follow_the_locale() {
        let en = render(Locale::En, false);
        assert!(
            en.contains("Send") && en.contains("Pause") && en.contains("Abort"),
            "{en}"
        );
        let es = render(Locale::Es, false);
        assert!(
            es.contains("Enviar") && es.contains("Pausar") && es.contains("Cancelar"),
            "{es}"
        );
    }

    #[test]
    fn the_paused_button_swaps_label_and_stays_localized() {
        assert!(render(Locale::De, true).contains("Fortsetzen"));
    }
}
