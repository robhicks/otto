mod app;
mod components;
mod editor;
mod net;
mod transport;

use app::App;

fn main() {
    dioxus::launch(App);
}
