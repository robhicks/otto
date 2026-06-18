//! Thin wrapper over `web_sys::WebSocket`. Browser-only; verified by compiling for wasm
//! and by manual browser testing (the pure routing/seq logic lives in `url.rs`).

use otto_protocol::{Command, ServerMessage};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{CloseEvent, ErrorEvent, MessageEvent, WebSocket};

/// Open a WebSocket to `url`, wiring callbacks:
/// - `on_msg`: each text frame parsed as `ServerMessage` (or `Err(detail)` on a parse failure).
/// - `on_close`: the socket closed.
/// - `on_error`: a socket error (e.g. a rejected handshake).
///
/// The event closures are `forget()`-leaked for the socket's lifetime. Each `open_ws` leaks
/// three small closures; acceptable for sub-project A's connect/reconnect cadence.
pub fn open_ws(
    url: &str,
    on_msg: impl Fn(Result<ServerMessage, String>) + 'static,
    on_close: impl Fn() + 'static,
    on_error: impl Fn() + 'static,
) -> Result<WebSocket, String> {
    let ws = WebSocket::new(url).map_err(|e| format!("{e:?}"))?;

    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Some(txt) = e.data().as_string() {
            let parsed = serde_json::from_str::<ServerMessage>(&txt).map_err(|err| err.to_string());
            on_msg(parsed);
        }
    });
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    let onclose = Closure::<dyn FnMut(CloseEvent)>::new(move |_e: CloseEvent| on_close());
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    onclose.forget();

    let onerror = Closure::<dyn FnMut(ErrorEvent)>::new(move |_e: ErrorEvent| on_error());
    ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    Ok(ws)
}

/// Serialize a `Command` to JSON and send it as a text frame.
pub fn send_command(ws: &WebSocket, cmd: &Command) -> Result<(), String> {
    let json = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
    ws.send_with_str(&json).map_err(|e| format!("{e:?}"))
}
