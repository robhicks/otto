use dioxus::prelude::*;

use crate::i18n::use_locale;
use crate::net::view_model::{render_row, LogRow};

#[component]
pub fn EventLog(rows: Signal<Vec<LogRow>>) -> Element {
    // A tracked read of the locale signal — this is what re-renders every already-received row
    // when the picker changes language, without the rows themselves being rebuilt.
    let locale = use_locale();
    rsx! {
        div { class: "log",
            for r in rows.read().iter() {
                div { class: "row {r.class}", "{render_row(locale, &r.msg)}" }
            }
        }
    }
}
