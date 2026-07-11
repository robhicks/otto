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
}

#[cfg(feature = "desktop")]
mod desktop;
#[cfg(feature = "web")]
mod web;

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
