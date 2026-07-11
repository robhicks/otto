mod app;
mod components;
#[cfg(feature = "desktop")]
mod desktop_boot;
mod editor;
mod net;
mod transport;

use app::App;

fn main() {
    dioxus::launch(App);
}
