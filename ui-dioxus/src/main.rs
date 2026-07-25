mod app;
mod components;
#[cfg(feature = "desktop")]
mod desktop_boot;
mod editor;
mod net;
mod transport;
/// Wasm integration test for the web mount→parse→connect path. Runs only under
/// `cargo test --target wasm32-unknown-unknown --features web`. The `target_arch` gate matches the
/// wasm32-target gate on its dev-dependencies, so a host `cargo test --features web` skips the
/// module rather than failing to resolve `wasm-bindgen-test`/`gloo-timers`.
#[cfg(all(test, feature = "web", target_arch = "wasm32"))]
mod web_mount_test;

use app::App;

fn main() {
    dioxus::launch(App);
}
