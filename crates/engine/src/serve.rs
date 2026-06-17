//! WebSocket transport for the engine. Maps WS frames to `Command`/event frames over an
//! `EngineService`: bearer-token auth on upgrade, a `Ready { session }` frame on connect,
//! optional `Last-Event-ID` replay (`?last_seq=`), then live streamed events per `SendPrompt`.
//! Binds loopback; TLS and concurrent sessions are out of scope (see the design spec).

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router as AxumRouter;
use axum_server::tls_rustls::RustlsConfig;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use otto_protocol::{Command, Event, SessionId};
use serde::{Deserialize, Serialize};

use crate::service::{EngineService, EventSink};

/// Outbound WS frame. Reuses the core `Event`; `Ready`/`Error` are transport framing.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Ready { session: SessionId },
    Event { event: Event },
    Error { message: String },
}

#[derive(Deserialize, Default)]
struct ConnectParams {
    session: Option<String>,
    last_seq: Option<u64>,
}

/// Shared server state: the engine service and the required bearer token.
struct ServeState {
    service: EngineService,
    token: String,
}

/// Resolve the TLS flag pair: both present -> `Some((cert, key))`; neither -> `None`;
/// exactly one -> error (the flags must be given together).
pub fn resolve_tls_paths(
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
) -> anyhow::Result<Option<(PathBuf, PathBuf)>> {
    match (cert, key) {
        (Some(c), Some(k)) => Ok(Some((c, k))),
        (None, None) => Ok(None),
        _ => anyhow::bail!("--tls-cert and --tls-key must be provided together"),
    }
}

/// Build the axum app. Exposed for tests so they can serve it on an ephemeral port.
pub fn app(service: EngineService, token: String) -> AxumRouter {
    assert!(!token.is_empty(), "serve token must not be empty");
    let state = Arc::new(ServeState { service, token });
    AxumRouter::new()
        .route("/ws", get(ws_handler))
        .with_state(state)
}

/// Serve `app` on a pre-bound listener, over TLS when `tls` is `Some`. Unifies the plaintext
/// and TLS paths on `axum-server` so both run from a `std::net::TcpListener` (testable on a
/// `127.0.0.1:0` ephemeral port). The listener must be in non-blocking mode.
pub async fn run(
    listener: std::net::TcpListener,
    app: AxumRouter,
    tls: Option<RustlsConfig>,
) -> anyhow::Result<()> {
    match tls {
        Some(cfg) => {
            axum_server::from_tcp_rustls(listener, cfg)
                .serve(app.into_make_service())
                .await?
        }
        None => {
            axum_server::from_tcp(listener)
                .serve(app.into_make_service())
                .await?
        }
    }
    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(params): Query<ConnectParams>,
    State(state): State<Arc<ServeState>>,
) -> Response {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if presented != Some(state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, params, state))
}

/// Send one `ServerMessage` as a JSON text frame.
async fn send_msg(socket: &mut WebSocket, msg: &ServerMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    socket.send(Message::Text(json.into())).await?;
    Ok(())
}

/// A sink that writes each event to the socket as a `ServerMessage::Event` frame.
struct WsSink<'a> {
    socket: &'a mut WebSocket,
}

#[async_trait::async_trait]
impl EventSink for WsSink<'_> {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        send_msg(
            self.socket,
            &ServerMessage::Event {
                event: event.clone(),
            },
        )
        .await
    }
}

async fn handle_socket(mut socket: WebSocket, params: ConnectParams, state: Arc<ServeState>) {
    // Resolve the session: reuse `?session=<uuid>` or mint a new one.
    let session = match resolve_session(&params, &state).await {
        Ok(s) => s,
        Err(e) => {
            let _ = send_msg(
                &mut socket,
                &ServerMessage::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };

    if send_msg(&mut socket, &ServerMessage::Ready { session })
        .await
        .is_err()
    {
        return;
    }

    // Reconnect: replay the gap after `last_seq`.
    if let Some(after) = params.last_seq {
        match state
            .service
            .store()
            .replay_since(session, Some(after))
            .await
        {
            Ok(events) => {
                for event in events {
                    if send_msg(&mut socket, &ServerMessage::Event { event })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = send_msg(
                    &mut socket,
                    &ServerMessage::Error {
                        message: e.to_string(),
                    },
                )
                .await;
                return;
            }
        }
    }

    // The loop ends on a clean close, a transport error, or an Abort. A disconnect with no
    // in-flight turn intentionally leaves the session Active so the client can reconnect.
    while let Some(Ok(msg)) = socket.recv().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue, // ignore binary/ping/pong
        };
        let command: Command = match serde_json::from_str(text.as_str()) {
            Ok(c) => c,
            Err(e) => {
                let _ = send_msg(
                    &mut socket,
                    &ServerMessage::Error {
                        message: format!("bad command: {e}"),
                    },
                )
                .await;
                continue;
            }
        };
        match command {
            Command::SendPrompt { text, .. } => {
                let mut sink = WsSink {
                    socket: &mut socket,
                };
                if let Err(e) = state.service.run_prompt(session, &text, &mut sink).await {
                    let _ = send_msg(
                        &mut socket,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                }
            }
            Command::Abort { .. } => {
                let _ = state.service.abort(session).await;
                break;
            }
            Command::CreateSession => {
                // The session is already established on connect; nothing to do.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tls_paths_both_present_is_some() {
        let got = resolve_tls_paths(Some(PathBuf::from("c.pem")), Some(PathBuf::from("k.pem"))).unwrap();
        assert_eq!(got, Some((PathBuf::from("c.pem"), PathBuf::from("k.pem"))));
    }

    #[test]
    fn tls_paths_neither_is_none() {
        assert_eq!(resolve_tls_paths(None, None).unwrap(), None);
    }

    #[test]
    fn tls_paths_only_one_is_error() {
        assert!(resolve_tls_paths(Some(PathBuf::from("c.pem")), None).is_err());
        assert!(resolve_tls_paths(None, Some(PathBuf::from("k.pem"))).is_err());
    }
}

async fn resolve_session(params: &ConnectParams, state: &ServeState) -> anyhow::Result<SessionId> {
    match &params.session {
        Some(s) => {
            let uuid = uuid::Uuid::parse_str(s)?;
            Ok(SessionId(uuid))
        }
        None => {
            state
                .service
                .create_session("(serve/ws)", &serde_json::json!({}))
                .await
        }
    }
}
