//! Pure, browser-free, framework-agnostic logic ported verbatim from the Leptos `ui/`.
//! This is the shared seam: zero Dioxus/Leptos dependency, host-tested with plain `cargo test`.
pub mod tree;
pub mod url;
pub mod view_model;
