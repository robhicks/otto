//! The wire-protocol client kit for otto's interactive command-line surface.
//!
//! Everything here speaks the `Command`/`ServerMessage` protocol through the `ClientTransport`
//! seam and pure event rendering — no engine, no socket, no terminal. `otto-engine`'s `otto`
//! binary is the concrete consumer: it implements `ClientTransport` over an in-process
//! `EngineService` (`crates/engine/src/embedded.rs`) and drives the REPL loop
//! (`crates/engine/src/repl.rs`), both living there rather than here because this crate stays
//! upstream of the engine (a client-side dependency depending back on the engine that already
//! depends on it is a Cargo package cycle, not just a layering preference). English-only:
//! `ui-dioxus`'s i18n boundary (interface copy translated through a compile-time catalog) does not
//! extend to this crate — the CLI has no catalog, no `t`/`tf`, no locale, by design, not by
//! omission.

pub mod render;
pub mod transport;

pub use transport::{ClientTransport, FakeTransport};
