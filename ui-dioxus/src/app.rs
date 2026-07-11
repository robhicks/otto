use dioxus::prelude::*;

#[component]
pub fn App() -> Element {
    rsx! {
        div { class: "app",
            h1 { "otto — Dioxus client" }
        }
    }
}
