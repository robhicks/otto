//! Browser transport: `web_sys::WebSocket` for the socket, `gloo-net` (fetch) for the
//! `/workspace` RPC. Ports `ui/src/ws.rs` + `ui/src/workspace.rs` onto the target-agnostic
//! `Sink`/`SocketEvent` seam — the web_sys closures forward into the channel instead of
//! calling caller-supplied callbacks directly.

use std::path::PathBuf;

use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use gloo_net::http::Request;
use otto_protocol::{Command, ServerMessage, WorkspaceRequest, WorkspaceResponse};
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use web_sys::{CloseEvent, ErrorEvent, MessageEvent, WebSocket};

use super::{SeamError, Sink, SocketEvent};

struct WebSink(WebSocket);
impl Sink for WebSink {
    fn send(&self, cmd: &Command) -> Result<(), SeamError> {
        let json = serde_json::to_string(cmd).map_err(|e| SeamError::new(e.to_string()))?;
        // Redacted for the same reason as `connect_impl` below. This particular JsValue is not
        // known to quote the connection URL, but every string leaving this seam reaches the visible
        // event log, so the whole seam is redacted rather than the one site we happened to audit.
        self.0
            .send_with_str(&json)
            .map_err(|e| SeamError::new(format!("{e:?}")))
    }

    fn close(&self) {
        // Detach the (`.forget()`'d) handlers FIRST, so the socket's async `onclose` can't fire
        // late and emit a stale `Closed` into the channel, then close the underlying socket.
        // Mirrors `ui/src/app.rs:60-66`.
        self.0.set_onmessage(None);
        self.0.set_onclose(None);
        self.0.set_onerror(None);
        let _ = self.0.close();
    }
}

pub fn connect_impl(
    ws_url: &str,
) -> Result<(Box<dyn Sink>, UnboundedReceiver<SocketEvent>), SeamError> {
    let (tx, rx) = unbounded::<SocketEvent>();
    // `ws_url` no longer carries the bearer token (`build_ws_url` emits none; spec §6.4, slice 2
    // moves credentials to post-upgrade frames), but the redaction stays on the seam regardless:
    // a rejected URL still comes back as a `SyntaxError` that QUOTES THE URL IN FULL, and any
    // future call site or browser API echo can hand this constructor a credential-bearing string.
    // This `SeamError` is a transport diagnostic, so `app.rs` routes it to
    // `client_error_row(ClientText::Passthrough(..))` and it renders in the event log — the
    // surface most likely to be copied into a bug report.
    //
    // The redaction is NOT applied here: `SeamError::new` does it for every diagnostic on the
    // seam, so this site cannot forget and neither can the next one. Host/path/session/last_seq
    // survive; only the secret goes.
    let ws = WebSocket::new(ws_url).map_err(|e| SeamError::new(format!("{e:?}")))?;

    let tx_msg: UnboundedSender<SocketEvent> = tx.clone();
    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Some(txt) = e.data().as_string() {
            let parsed = serde_json::from_str::<ServerMessage>(&txt)
                .map_err(|err| SeamError::new(err.to_string()));
            let _ = tx_msg.unbounded_send(SocketEvent::Message(parsed));
        }
    });
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    let tx_close = tx.clone();
    let onclose = Closure::<dyn FnMut(CloseEvent)>::new(move |_e: CloseEvent| {
        let _ = tx_close.unbounded_send(SocketEvent::Closed);
    });
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    onclose.forget();

    let tx_err = tx.clone();
    let onerror = Closure::<dyn FnMut(ErrorEvent)>::new(move |_e: ErrorEvent| {
        let _ = tx_err.unbounded_send(SocketEvent::Errored);
    });
    ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    Ok((Box::new(WebSink(ws)), rx))
}

async fn rpc(
    http_base: &str,
    token: &str,
    req: &WorkspaceRequest,
) -> Result<WorkspaceResponse, SeamError> {
    let url = format!("{}/workspace", http_base.trim_end_matches('/'));
    let resp = Request::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .json(req)
        .map_err(|e| SeamError::new(e.to_string()))?
        .send()
        .await
        .map_err(|e| SeamError::new(e.to_string()))?;
    if !resp.ok() {
        return Err(SeamError::new(format!(
            "workspace rpc failed: HTTP {}",
            resp.status()
        )));
    }
    let parsed: WorkspaceResponse = resp
        .json()
        .await
        .map_err(|e| SeamError::new(e.to_string()))?;
    if let WorkspaceResponse::Error { message } = &parsed {
        return Err(SeamError::new(message.clone()));
    }
    Ok(parsed)
}

pub async fn list_files_impl(http_base: &str, token: &str) -> Result<Vec<PathBuf>, SeamError> {
    match rpc(
        http_base,
        token,
        &WorkspaceRequest::List {
            glob: "**/*".into(),
        },
    )
    .await?
    {
        WorkspaceResponse::List { paths } => Ok(paths),
        other => Err(SeamError::new(format!(
            "unexpected response to List: {other:?}"
        ))),
    }
}

pub async fn read_file_impl(
    http_base: &str,
    token: &str,
    path: PathBuf,
) -> Result<Vec<u8>, SeamError> {
    match rpc(http_base, token, &WorkspaceRequest::Read { path }).await? {
        WorkspaceResponse::Read { bytes } => Ok(bytes),
        other => Err(SeamError::new(format!(
            "unexpected response to Read: {other:?}"
        ))),
    }
}
