//! The one place `cfg(feature)` splits web vs desktop. The reactivity spine sees only these
//! target-agnostic types; each target supplies the impl.
use std::path::PathBuf;

use otto_protocol::{Command, ServerMessage};

/// An inbound socket event, delivered over the receiver `connect` returns.
pub enum SocketEvent {
    Message(Result<ServerMessage, String>),
    Closed,
    Errored,
}

/// The outbound half of a live socket: serialize + send a `Command`. Boxed so the spine holds a
/// trait object, never a concrete socket type.
pub trait Sink {
    fn send(&self, cmd: &Command) -> Result<(), String>;

    /// Tear the socket down explicitly. Dropping the sink is NOT enough on web — the inbound
    /// `web_sys` closures are `.forget()`'d, so the real `WebSocket` outlives the `Rc` and keeps
    /// delivering events. Every reconnect/disconnect path must call this on the old sink before
    /// installing a new one, so a stale socket can't push events into the new connection's state
    /// (mirrors `ui/src/app.rs`'s handler-detach + `close()` at the top of `connect`/`disconnect`).
    fn close(&self);
}

#[cfg(feature = "desktop")]
mod desktop;
#[cfg(feature = "web")]
mod web;

/// Test-only observation seam. `connect` records every target URL here before dispatching to the
/// real transport, so a test can assert that a *call site* — not merely a helper in isolation —
/// actually reached the transport. This exists because the runtime spike's one real bug was a
/// launch-params parser that was unit-tested and correct but had **no web call site**: the parser
/// passed, autoconnect silently did nothing, and it shipped. Asserting on parser output can never
/// catch that; asserting that `connect` was reached can.
///
/// Compiled only under `cfg(test)`, so the shipped `web`/`desktop` builds contain neither the
/// recorder nor the `record` call — runtime behavior is unchanged.
#[cfg(test)]
pub mod connect_probe {
    use std::cell::RefCell;

    thread_local! {
        static ATTEMPTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record(ws_url: &str) {
        ATTEMPTS.with(|a| a.borrow_mut().push(ws_url.to_string()));
    }

    /// Every URL passed to `connect` since the last `reset`, in call order.
    pub fn attempts() -> Vec<String> {
        ATTEMPTS.with(|a| a.borrow().clone())
    }

    /// Clear the record. Call at the start of each test — wasm tests share one thread.
    pub fn reset() {
        ATTEMPTS.with(|a| a.borrow_mut().clear());
    }
}

/// Open a socket to `ws_url`. Returns the outbound sink and a stream of inbound events.
pub fn connect(
    ws_url: &str,
) -> Result<
    (
        Box<dyn Sink>,
        futures_channel::mpsc::UnboundedReceiver<SocketEvent>,
    ),
    String,
> {
    #[cfg(test)]
    connect_probe::record(ws_url);

    #[cfg(feature = "web")]
    {
        web::connect_impl(ws_url)
    }
    #[cfg(feature = "desktop")]
    {
        desktop::connect_impl(ws_url)
    }
    // Neither target feature enabled (e.g. `cargo test --no-default-features` for the pure
    // `net::` seam): the crate still needs to type-check, so fail closed at the call site
    // rather than fail to compile.
    #[cfg(not(any(feature = "web", feature = "desktop")))]
    {
        let _ = ws_url;
        Err(
            "no transport feature enabled (build with --features web or --features desktop)"
                .to_string(),
        )
    }
}

/// List every file in the served workspace (`POST /workspace` `List`).
pub async fn list_files(http_base: &str, token: &str) -> Result<Vec<PathBuf>, String> {
    #[cfg(feature = "web")]
    {
        web::list_files_impl(http_base, token).await
    }
    #[cfg(feature = "desktop")]
    {
        desktop::list_files_impl(http_base, token).await
    }
    #[cfg(not(any(feature = "web", feature = "desktop")))]
    {
        let _ = (http_base, token);
        Err(
            "no transport feature enabled (build with --features web or --features desktop)"
                .to_string(),
        )
    }
}

/// Read one file's bytes (`POST /workspace` `Read`).
pub async fn read_file(http_base: &str, token: &str, path: PathBuf) -> Result<Vec<u8>, String> {
    #[cfg(feature = "web")]
    {
        web::read_file_impl(http_base, token, path).await
    }
    #[cfg(feature = "desktop")]
    {
        desktop::read_file_impl(http_base, token, path).await
    }
    #[cfg(not(any(feature = "web", feature = "desktop")))]
    {
        let _ = (http_base, token, path);
        Err(
            "no transport feature enabled (build with --features web or --features desktop)"
                .to_string(),
        )
    }
}
