mod app;
mod components;
mod tree;
mod url;
mod view_model;
mod workspace;
mod ws;

use app::App;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
