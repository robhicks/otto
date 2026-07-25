mod app;
mod components;
#[cfg(feature = "desktop")]
mod desktop_boot;
mod editor;
mod net;
mod transport;
/// Wasm integration test for the web mount→parse→connect path (runs under
/// `cargo test --target wasm32-unknown-unknown --features web`). Kept in its own module so the
/// browser-only test harness never enters the desktop build.
#[cfg(all(test, feature = "web"))]
mod web_mount_test;

use app::App;

fn main() {
    dioxus::launch(App);
}
