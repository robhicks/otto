use dioxus::prelude::*;

use crate::i18n::{t, use_locale, Msg};
use crate::net::view_model::ConnState;

#[component]
pub fn ConnectionForm(
    url: Signal<String>,
    token: Signal<String>,
    conn: Signal<ConnState>,
    on_connect: EventHandler<()>,
    on_disconnect: EventHandler<()>,
) -> Element {
    let locale = use_locale();
    let connected = matches!(*conn.read(), ConnState::Connected { .. });
    let connecting = matches!(*conn.read(), ConnState::Connecting);
    rsx! {
        div { class: "conn-form",
            input {
                class: "url-input",
                value: "{url}",
                placeholder: "ws://127.0.0.1:8787",
                oninput: move |e| url.set(e.value()),
            }
            input {
                class: "token-input",
                value: "{token}",
                r#type: "password",
                placeholder: "{t(locale, Msg::TokenPlaceholder)}",
                oninput: move |e| token.set(e.value()),
            }
            if connected {
                button { onclick: move |_| on_disconnect.call(()), {t(locale, Msg::Disconnect)} }
            } else if connecting {
                // Disabled while a connect is in flight so a double-click can't re-enter
                // `do_connect` (which would orphan the in-progress socket). Mirrors `ui/`'s
                // disabled-on-Connecting behavior.
                button { disabled: true, {t(locale, Msg::Connecting)} }
            } else {
                button { onclick: move |_| on_connect.call(()), {t(locale, Msg::Connect)} }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    #[component]
    fn Harness(start: Locale) -> Element {
        use_context_provider(|| Signal::new(start));
        let url = use_signal(|| "ws://x".to_string());
        let token = use_signal(String::new);
        let conn = use_signal(|| ConnState::Disconnected);
        rsx! {
            ConnectionForm {
                url, token, conn,
                on_connect: move |_| {},
                on_disconnect: move |_| {},
            }
        }
    }

    fn render(start: Locale) -> String {
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { start });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn connect_button_and_token_placeholder_follow_the_locale() {
        let en = render(Locale::En);
        assert!(en.contains("Connect"), "{en}");
        let de = render(Locale::De);
        assert!(de.contains("Verbinden"), "{de}");
        assert!(
            de.contains("Token"),
            "token placeholder not localized: {de}"
        );
    }

    #[test]
    fn the_url_placeholder_is_never_translated() {
        // An example VALUE, not copy (spec §2) — byte-identical in every language.
        for loc in Locale::ALL {
            assert!(render(loc).contains("ws://127.0.0.1:8787"), "{loc:?}");
        }
    }
}
