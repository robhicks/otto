use dioxus::prelude::*;

use crate::net::view_model::LogRow;

#[component]
pub fn EventLog(rows: Signal<Vec<LogRow>>) -> Element {
    rsx! {
        div { class: "log",
            for r in rows.read().iter() {
                div { class: "row {r.class}", "{r.text}" }
            }
        }
    }
}
