mod app;
mod net;

use app::App;

fn main() {
    dioxus::launch(App);
}
