mod components;
mod url;
mod view_model;
mod ws;

use leptos::prelude::*;

#[component]
fn App() -> impl IntoView {
    view! { <div class="app">"otto"</div> }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
