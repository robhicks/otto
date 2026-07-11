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

use super::{Sink, SocketEvent};

struct WebSink(WebSocket);
impl Sink for WebSink {
    fn send(&self, cmd: &Command) -> Result<(), String> {
        let json = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
        self.0.send_with_str(&json).map_err(|e| format!("{e:?}"))
    }
}

pub fn connect_impl(
    ws_url: &str,
) -> Result<(Box<dyn Sink>, UnboundedReceiver<SocketEvent>), String> {
    let (tx, rx) = unbounded::<SocketEvent>();
    let ws = WebSocket::new(ws_url).map_err(|e| format!("{e:?}"))?;

    let tx_msg: UnboundedSender<SocketEvent> = tx.clone();
    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Some(txt) = e.data().as_string() {
            let parsed = serde_json::from_str::<ServerMessage>(&txt).map_err(|err| err.to_string());
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
) -> Result<WorkspaceResponse, String> {
    let url = format!("{}/workspace", http_base.trim_end_matches('/'));
    let resp = Request::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .json(req)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("workspace rpc failed: HTTP {}", resp.status()));
    }
    let parsed: WorkspaceResponse = resp.json().await.map_err(|e| e.to_string())?;
    if let WorkspaceResponse::Error { message } = &parsed {
        return Err(message.clone());
    }
    Ok(parsed)
}

pub async fn list_files_impl(http_base: &str, token: &str) -> Result<Vec<PathBuf>, String> {
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
        other => Err(format!("unexpected response to List: {other:?}")),
    }
}

pub async fn read_file_impl(
    http_base: &str,
    token: &str,
    path: PathBuf,
) -> Result<Vec<u8>, String> {
    match rpc(http_base, token, &WorkspaceRequest::Read { path }).await? {
        WorkspaceResponse::Read { bytes } => Ok(bytes),
        other => Err(format!("unexpected response to Read: {other:?}")),
    }
}
