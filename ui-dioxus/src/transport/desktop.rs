//! Native desktop transport: `tokio-tungstenite` for the socket, `reqwest` for the
//! `/workspace` RPC. A tokio task owns the tungstenite read/write loop; inbound frames
//! forward into the channel, outbound `Command`s arrive over an mpsc the `Sink` writes to.

use std::path::PathBuf;

use futures_channel::mpsc::{unbounded, UnboundedReceiver};
use futures_util::{SinkExt, StreamExt};
use otto_protocol::{Command, ServerMessage, WorkspaceRequest, WorkspaceResponse};
use tokio_tungstenite::tungstenite::Message;

use super::{SeamError, Sink, SocketEvent};

// The outbound sender lives behind a `RefCell<Option<..>>` so `close()` can take it out and drop
// it explicitly (an `UnboundedSender` closes its channel only when the last sender drops; the sink
// holds the only one). Single-threaded use, matching the `!Send` `Rc<dyn Sink>` the spine stores.
struct DesktopSink(std::cell::RefCell<Option<tokio::sync::mpsc::UnboundedSender<String>>>);
impl Sink for DesktopSink {
    fn send(&self, cmd: &Command) -> Result<(), SeamError> {
        let json = serde_json::to_string(cmd).map_err(|e| SeamError::new(e.to_string()))?;
        match self.0.borrow().as_ref() {
            Some(tx) => tx.send(json).map_err(|e| SeamError::new(e.to_string())),
            None => Err(SeamError::new("socket closed")),
        }
    }

    fn close(&self) {
        // Drop the outbound sender: the writer loop's `out_rx.recv()` then returns `None`, ending
        // the loop, which `.abort()`s the reader task — an explicit close, not incidental `Drop`.
        self.0.borrow_mut().take();
    }
}

pub fn connect_impl(
    ws_url: &str,
) -> Result<(Box<dyn Sink>, UnboundedReceiver<SocketEvent>), SeamError> {
    let (inbound_tx, inbound_rx) = unbounded::<SocketEvent>();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let url = ws_url.to_string();

    // The desktop build runs on a tokio runtime (dioxus-desktop provides one); spawn the socket
    // loop onto it. All errors surface as SocketEvent::Errored/Closed, matching the web path.
    // CAVEAT (verify at Task 4 manual drive): if `connect()` is ever called before
    // dioxus-desktop's ambient tokio runtime is entered, this `tokio::spawn` will panic
    // ("there is no reactor running"). Compile-correct for this task; if that turns out to be
    // the case, the minimal fix is a dedicated runtime thread owning its own `Runtime`.
    tokio::spawn(async move {
        let (stream, _resp) = match tokio_tungstenite::connect_async(&url).await {
            Ok(ok) => ok,
            Err(_) => {
                let _ = inbound_tx.unbounded_send(SocketEvent::Errored);
                return;
            }
        };
        let (mut write, mut read) = stream.split();
        let inbound_reader = inbound_tx.clone();
        // Reader task: forward each text frame as a parsed ServerMessage.
        let reader = tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(txt)) => {
                        let parsed = serde_json::from_str::<ServerMessage>(&txt)
                            .map_err(|e| SeamError::new(e.to_string()));
                        let _ = inbound_reader.unbounded_send(SocketEvent::Message(parsed));
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        let _ = inbound_reader.unbounded_send(SocketEvent::Closed);
                        break;
                    }
                    _ => {}
                }
            }
        });
        // Writer loop: drain outbound commands until the sink is dropped.
        while let Some(json) = out_rx.recv().await {
            if write.send(Message::Text(json.into())).await.is_err() {
                let _ = inbound_tx.unbounded_send(SocketEvent::Errored);
                break;
            }
        }
        reader.abort();
    });

    Ok((
        Box::new(DesktopSink(std::cell::RefCell::new(Some(out_tx)))),
        inbound_rx,
    ))
}

async fn rpc(
    http_base: &str,
    token: &str,
    req: &WorkspaceRequest,
) -> Result<WorkspaceResponse, SeamError> {
    let url = format!("{}/workspace", http_base.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(req)
        .send()
        .await
        .map_err(|e| SeamError::new(e.to_string()))?;
    if !resp.status().is_success() {
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
