use leptos::html::Div;
use leptos::prelude::*;

use crate::view_model::LogRow;

/// Scrolling list of received rows, newest at the bottom, auto-scrolled on append.
#[component]
pub fn EventLog(rows: RwSignal<Vec<LogRow>>) -> impl IntoView {
    let container: NodeRef<Div> = NodeRef::new();

    // After each change to `rows`, pin the scroll position to the bottom.
    Effect::new(move |_| {
        rows.track();
        if let Some(el) = container.get() {
            el.set_scroll_top(el.scroll_height());
        }
    });

    view! {
        <div class="log" node_ref=container>
            {move || {
                rows.get()
                    .into_iter()
                    .map(|r| view! { <div class=format!("row {}", r.class)>{r.text}</div> })
                    .collect_view()
            }}
        </div>
    }
}
