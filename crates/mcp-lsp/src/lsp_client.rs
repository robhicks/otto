//! Minimal LSP-over-stdio client: JSON-RPC framing, request/response dispatch, and a
//! generation-tracked diagnostics cache. Generic over AsyncRead/AsyncWrite so it can be driven
//! by a real child process or, in tests, an in-memory duplex pipe.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Write one `Content-Length`-framed JSON-RPC message.
pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, value: &Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one `Content-Length`-framed JSON-RPC message.
pub async fn read_message<R: AsyncBufRead + Unpin>(r: &mut R) -> anyhow::Result<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("stream closed while reading LSP headers");
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = Some(v.trim().parse()?);
        }
    }
    let len = content_length.ok_or_else(|| anyhow::anyhow!("missing Content-Length header"))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;

/// A minimal LSP client: JSON-RPC request/response over a `Content-Length`-framed stdio pipe.
pub struct LspClient {
    writer: Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
}

impl LspClient {
    /// Spawn the background reader loop over `reader` and hold `writer` for outgoing messages.
    pub fn spawn<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                let msg = match read_message(&mut reader).await {
                    Ok(m) => m,
                    Err(_) => break, // stream closed / server process exited
                };
                let has_id = msg.get("id").and_then(Value::as_i64);
                let method = msg.get("method").and_then(Value::as_str);
                if let (Some(id), None) = (has_id, method) {
                    // A response to one of our requests (no "method" field). Server-initiated
                    // requests (e.g. `client/registerCapability`) have both an id and a method —
                    // we don't answer those in v1; see the design doc's known-limitations note.
                    if let Some(tx) = reader_pending.lock().await.remove(&id) {
                        let result = msg.get("result").cloned().unwrap_or(Value::Null);
                        let _ = tx.send(result);
                    }
                }
            }
        });

        Self {
            writer: Mutex::new(Box::new(writer)),
            next_id: AtomicI64::new(1),
            pending,
        }
    }

    async fn write(&self, value: &Value) -> anyhow::Result<()> {
        let mut w = self.writer.lock().await;
        write_message(&mut *w, value).await
    }

    /// Send a JSON-RPC request and await its matching response, or time out.
    pub async fn request(&self, method: &str, params: Value, wait: Duration) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match timeout(wait, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => anyhow::bail!("LSP client dropped while awaiting `{method}`"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                anyhow::bail!("timed out waiting for `{method}` response")
            }
        }
    }

    /// Send a fire-and-forget JSON-RPC notification.
    pub async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn write_then_read_round_trips_a_value() {
        let (mut a, b) = tokio::io::duplex(1024);
        let mut b = BufReader::new(b);
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        write_message(&mut a, &msg).await.unwrap();
        let got = read_message(&mut b).await.unwrap();
        assert_eq!(got, msg);
    }

    use std::sync::Arc;
    use std::time::Duration;

    fn duplex_client() -> (LspClient, tokio::io::DuplexStream) {
        let (client_end, server_end) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_end);
        (LspClient::spawn(BufReader::new(cr), cw), server_end)
    }

    #[tokio::test]
    async fn request_resolves_to_the_matching_response() {
        let (client, server_end) = duplex_client();
        let (sr, mut sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);

        tokio::spawn(async move {
            let req = read_message(&mut sr).await.unwrap();
            let id = req["id"].clone();
            write_message(&mut sw, &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}))
                .await
                .unwrap();
        });

        let result = client.request("ping", serde_json::json!({}), Duration::from_secs(2)).await.unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn concurrent_requests_resolve_to_their_own_responses() {
        let (client, server_end) = duplex_client();
        let (sr, mut sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);

        tokio::spawn(async move {
            // Reply out of order: second request first.
            let req1 = read_message(&mut sr).await.unwrap();
            let req2 = read_message(&mut sr).await.unwrap();
            write_message(&mut sw, &serde_json::json!({"jsonrpc": "2.0", "id": req2["id"], "result": "two"}))
                .await
                .unwrap();
            write_message(&mut sw, &serde_json::json!({"jsonrpc": "2.0", "id": req1["id"], "result": "one"}))
                .await
                .unwrap();
        });

        let client = Arc::new(client);
        let c1 = Arc::clone(&client);
        let c2 = Arc::clone(&client);
        let (r1, r2) = tokio::join!(
            c1.request("m1", serde_json::json!({}), Duration::from_secs(2)),
            c2.request("m2", serde_json::json!({}), Duration::from_secs(2)),
        );
        assert_eq!(r1.unwrap(), serde_json::json!("one"));
        assert_eq!(r2.unwrap(), serde_json::json!("two"));
    }

    #[tokio::test]
    async fn request_times_out_when_no_response_arrives() {
        let (client, _server_end) = duplex_client();
        let err = client
            .request("ping", serde_json::json!({}), Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }
}
