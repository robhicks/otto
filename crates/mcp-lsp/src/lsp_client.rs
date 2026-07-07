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
}
