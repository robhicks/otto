mod app;
mod components;
mod url;
mod view_model;
mod ws;

use app::App;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
