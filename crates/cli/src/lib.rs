//! otto's interactive command-line client.
//!
//! The REPL speaks the `Command`/`ServerMessage` protocol through `ClientTransport` and never
//! reaches into the engine directly. English-only: `ui-dioxus`'s i18n boundary (interface copy
//! translated through a compile-time catalog) does not extend to this crate — the CLI has no
//! catalog, no `t`/`tf`, no locale, by design, not by omission.

pub mod embedded;
pub mod render;
pub mod transport;

pub use transport::{ClientTransport, FakeTransport};
