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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;

/// How long a fresh diagnostics entry must sit unchanged before `wait_for_diagnostics` trusts
/// it — see that method's doc comment for why returning on first publish is wrong. Sized
/// empirically: rust-analyzer's gap between the pre-analysis empty publish and the real one was
/// observed to exceed 300ms under load (~0.5-0.6s), so this carries a few-x margin.
const DIAGNOSTICS_QUIESCENCE: Duration = Duration::from_millis(2000);

/// Cached diagnostics for one URI, tagged with the client-side generation they were received at
/// (see `bump_generation`/`wait_for_diagnostics`) and the instant they arrived (so
/// `wait_for_diagnostics` can debounce until the publish stream quiesces).
#[derive(Clone)]
struct DiagEntry {
    generation: u64,
    diagnostics: Vec<lsp_types::Diagnostic>,
    updated_at: tokio::time::Instant,
}

/// A minimal LSP client: JSON-RPC request/response over a `Content-Length`-framed stdio pipe,
/// plus a versioned cache of `textDocument/publishDiagnostics` notifications.
pub struct LspClient {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    next_id: AtomicI64,
    // Each pending request receives the *full* response message so `request` can distinguish
    // a `result` from an `error` member (an LSP error must surface as Err, not a null result).
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    diagnostics: Arc<Mutex<HashMap<String, DiagEntry>>>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
    alive: Arc<AtomicBool>,
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
        let diagnostics: Arc<Mutex<HashMap<String, DiagEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let generations: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(Mutex::new(Box::new(writer)));
        let alive = Arc::new(AtomicBool::new(true));

        let reader_pending = Arc::clone(&pending);
        let reader_diagnostics = Arc::clone(&diagnostics);
        let reader_generations = Arc::clone(&generations);
        let reader_writer = Arc::clone(&writer);
        let reader_alive = Arc::clone(&alive);
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                let msg = match read_message(&mut reader).await {
                    Ok(m) => m,
                    Err(_) => {
                        reader_alive.store(false, Ordering::SeqCst);
                        break;
                    }
                };
                let method = msg.get("method").and_then(Value::as_str);
                if method.is_none() {
                    if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                        if let Some(tx) = reader_pending.lock().await.remove(&id) {
                            let _ = tx.send(msg);
                        }
                    }
                    continue;
                }
                let method = method.unwrap();
                if method == "textDocument/publishDiagnostics" {
                    if let Some(params) = msg.get("params") {
                        if let Ok(p) = serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(
                            params.clone(),
                        ) {
                            let uri = p.uri.as_str().to_string();
                            let generation =
                                *reader_generations.lock().await.get(&uri).unwrap_or(&0);
                            reader_diagnostics.lock().await.insert(
                                uri,
                                DiagEntry {
                                    generation,
                                    diagnostics: p.diagnostics,
                                    updated_at: tokio::time::Instant::now(),
                                },
                            );
                        }
                    }
                    continue;
                }
                // A server->client request (has id + method) we don't handle. Minimal client
                // capabilities normally stop servers from sending these; reply defensively so a
                // capability-mismatched server never blocks on an unanswered request.
                if let Some(id) = msg.get("id") {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": id.clone(),
                        "error": {"code": -32601, "message": "method not supported by otto lsp bridge"},
                    });
                    let mut w = reader_writer.lock().await;
                    if write_message(&mut *w, &reply).await.is_err() {
                        reader_alive.store(false, Ordering::SeqCst);
                    }
                }
            }
        });

        Self {
            writer,
            next_id: AtomicI64::new(1),
            pending,
            diagnostics,
            generations,
            alive,
        }
    }

    async fn write(&self, value: &Value) -> anyhow::Result<()> {
        let mut w = self.writer.lock().await;
        let result = write_message(&mut *w, value).await;
        if result.is_err() {
            self.alive.store(false, Ordering::SeqCst);
        }
        result
    }

    /// False once the server's stream has closed (process exited) or a write to it failed.
    /// Callers evict a dead client and re-spawn on the next call instead of hanging.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Send a JSON-RPC request and await its matching response, or time out. An `error`
    /// response surfaces as `Err` carrying the server's message — never as a null result.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match timeout(wait, rx).await {
            Ok(Ok(msg)) => {
                if let Some(err) = msg.get("error") {
                    let message = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    match err.get("code").and_then(Value::as_i64) {
                        Some(code) => {
                            anyhow::bail!("LSP `{method}` failed: {message} (code {code})")
                        }
                        None => anyhow::bail!("LSP `{method}` failed: {message}"),
                    }
                }
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            }
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

    /// Bump and return the generation counter for `uri` — call right before sending a
    /// `didOpen`/`didChange` so the reader loop attributes the next `publishDiagnostics` to it.
    pub async fn bump_generation(&self, uri: &str) -> u64 {
        let mut g = self.generations.lock().await;
        let next = g.get(uri).copied().unwrap_or(0) + 1;
        g.insert(uri.to_string(), next);
        next
    }

    /// Poll the diagnostics cache for `uri` until an entry at generation >= `min_generation`
    /// arrives **and the publish stream has quiesced** (no newer publish for
    /// `DIAGNOSTICS_QUIESCENCE`), or `wait` elapses. Returns `(diagnostics, timed_out)`.
    ///
    /// The debounce exists because language servers (rust-analyzer observed) publish an empty
    /// pre-analysis diagnostics set right after `didOpen`, then the real diagnostics shortly
    /// after — returning on the first publish would read broken code as clean. The debounce only
    /// *delays* returning a fresh entry; it never converts a fresh result into a timeout: at the
    /// deadline, a fresh (even un-quiesced) entry is returned with `timed_out: false`.
    pub async fn wait_for_diagnostics(
        &self,
        uri: &str,
        min_generation: u64,
        wait: Duration,
    ) -> (Vec<lsp_types::Diagnostic>, bool) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let now = tokio::time::Instant::now();
            // A dead server (pipe closed → `alive` flipped) will never publish again, so treat its
            // death like the deadline rather than polling the full (now up to 60s) budget out: a
            // fresh entry already in the cache is its final answer (returned below with
            // `timed_out: false`); otherwise we fall through to the stale/empty return.
            let terminal = now >= deadline || !self.alive.load(Ordering::SeqCst);
            if let Some(entry) = self.diagnostics.lock().await.get(uri) {
                if entry.generation >= min_generation
                    && (terminal || now >= entry.updated_at + DIAGNOSTICS_QUIESCENCE)
                {
                    return (entry.diagnostics.clone(), false);
                }
            }
            if terminal {
                let stale = self
                    .diagnostics
                    .lock()
                    .await
                    .get(uri)
                    .map(|e| e.diagnostics.clone())
                    .unwrap_or_default();
                return (stale, true);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// INVARIANT: this client answers no server→client *requests* (the reader loop only replies
    /// `MethodNotFound`). `capabilities` must therefore stay minimal — advertising a richer
    /// capability (pull diagnostics, `workspace/configuration`, dynamic registration) makes a
    /// server send requests the client can't satisfy, stalling it. This minimal-capabilities
    /// choice is exactly why rust-analyzer/tsserver/pyright/gopls all work with no
    /// `initializationOptions`.
    /// Send `initialize` then `initialized`. `root` becomes the `rootUri`.
    #[allow(deprecated)] // InitializeParams.root_uri: rust-analyzer accepts rootUri fine (design choice).
    pub async fn initialize(&self, root: &std::path::Path) -> anyhow::Result<()> {
        let root_uri = path_to_file_uri(root)?;
        let params = lsp_types::InitializeParams {
            root_uri: Some(root_uri),
            capabilities: lsp_types::ClientCapabilities::default(),
            ..Default::default()
        };
        self.request(
            "initialize",
            serde_json::to_value(params)?,
            Duration::from_secs(30),
        )
        .await?;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }
}

/// Build a `file://` URI from an absolute or relative filesystem path.
pub fn path_to_file_uri(path: &std::path::Path) -> anyhow::Result<lsp_types::Uri> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    format!("file://{}", abs.display())
        .parse::<lsp_types::Uri>()
        .map_err(|e| anyhow::anyhow!("bad file uri for {}: {e}", abs.display()))
}

/// Spawn `bin args…` as a child process and wire an `LspClient` to its stdio.
///
/// ## Teardown invariant — two independent guards, and only one of them always holds
///
/// `kill_on_drop(true)` covers every path where this process unwinds or returns normally: a client
/// evicted mid-run, and also mcp-lsp's own ordinary shutdown, since `main` ends by returning from
/// `service.waiting()` on stdin EOF (an ordinary return, which *does* run destructors — there is no
/// `process::exit` on that path).
///
/// It does **not** cover mcp-lsp being killed abruptly, which is reachable: `otto serve` may be
/// `SIGKILL`ed by the desktop `PR_SET_PDEATHSIG` guard, and a `SIGKILL` cascade leaves no
/// opportunity for any destructor to run.
///
/// The guard that survives *both* cases is that the language server is itself on stdio: piping its
/// stdin below means the pipe closes when mcp-lsp's process ends by any means, the server reads EOF,
/// and it exits. A language server launched over a socket, or one that ignores stdin EOF, would keep
/// only the `Drop`-dependent guard — and orphan whenever that one cannot run, which for
/// `rust-analyzer` means leaking a process that can hold gigabytes.
///
/// The two overlap on the ordinary path, so a passing test does not tell you which one fired.
/// `crates/engine/tests/mcp_child_teardown.rs` therefore pins the *observable* — that the full
/// `otto serve` → `mcp-lsp` → `rust-analyzer` chain collapses when the top is hard-killed — rather
/// than attributing it to either mechanism.
pub fn spawn_process(
    bin: &str,
    args: &[&str],
) -> anyhow::Result<(LspClient, tokio::process::Child)> {
    let mut child = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("child process has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child process has no stdout"))?;
    let client = LspClient::spawn(tokio::io::BufReader::new(stdout), stdin);
    Ok((client, child))
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
            write_message(
                &mut sw,
                &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}),
            )
            .await
            .unwrap();
        });

        let result = client
            .request("ping", serde_json::json!({}), Duration::from_secs(2))
            .await
            .unwrap();
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
            write_message(
                &mut sw,
                &serde_json::json!({"jsonrpc": "2.0", "id": req2["id"], "result": "two"}),
            )
            .await
            .unwrap();
            write_message(
                &mut sw,
                &serde_json::json!({"jsonrpc": "2.0", "id": req1["id"], "result": "one"}),
            )
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
    async fn request_surfaces_an_error_response_as_err() {
        let (client, server_end) = duplex_client();
        let (sr, mut sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);

        tokio::spawn(async move {
            let req = read_message(&mut sr).await.unwrap();
            write_message(
                &mut sw,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "error": {"code": -32801, "message": "content modified"}
                }),
            )
            .await
            .unwrap();
        });

        let err = client
            .request("ping", serde_json::json!({}), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("content modified"));
        assert!(err.to_string().contains("-32801"));
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

    #[tokio::test]
    async fn wait_for_diagnostics_returns_fresh_entries() {
        let (client, server_end) = duplex_client();
        let generation = client.bump_generation("file:///a.rs").await;
        assert_eq!(generation, 1);

        let (_sr, mut sw) = tokio::io::split(server_end);
        write_message(
            &mut sw,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": "file:///a.rs",
                    "diagnostics": [{
                        "range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 5}},
                        "message": "unresolved symbol `foo`"
                    }]
                }
            }),
        )
        .await
        .unwrap();

        let (diags, timed_out) = client
            .wait_for_diagnostics("file:///a.rs", generation, Duration::from_secs(2))
            .await;
        assert!(!timed_out);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "unresolved symbol `foo`");
    }

    #[tokio::test]
    async fn wait_for_diagnostics_debounces_past_an_empty_pre_analysis_publish() {
        let (client, server_end) = duplex_client();
        let generation = client.bump_generation("file:///a.rs").await;

        let (_sr, mut sw) = tokio::io::split(server_end);
        let publisher = tokio::spawn(async move {
            // The rust-analyzer pattern: an empty pre-analysis publish first...
            write_message(
                &mut sw,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {"uri": "file:///a.rs", "diagnostics": []}
                }),
            )
            .await
            .unwrap();
            // ...then the real diagnostics shortly after.
            tokio::time::sleep(Duration::from_millis(100)).await;
            write_message(
                &mut sw,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": "file:///a.rs",
                        "diagnostics": [{
                            "range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 5}},
                            "message": "cannot find function `does_not_exist`"
                        }]
                    }
                }),
            )
            .await
            .unwrap();
        });

        // Wait comfortably longer than DIAGNOSTICS_QUIESCENCE so this exercises the debounced
        // early return, not the fresh-at-deadline fallback.
        let (diags, timed_out) = client
            .wait_for_diagnostics("file:///a.rs", generation, Duration::from_secs(10))
            .await;
        publisher.await.unwrap();
        assert!(!timed_out);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "cannot find function `does_not_exist`");
    }

    #[tokio::test]
    async fn wait_for_diagnostics_times_out_when_nothing_arrives() {
        let (client, _server_end) = duplex_client();
        let generation = client.bump_generation("file:///a.rs").await;
        let (diags, timed_out) = client
            .wait_for_diagnostics("file:///a.rs", generation, Duration::from_millis(200))
            .await;
        assert!(timed_out);
        assert!(diags.is_empty());
    }

    #[tokio::test]
    async fn wait_for_diagnostics_short_circuits_when_the_client_dies() {
        let (client, server_end) = duplex_client();
        let generation = client.bump_generation("file:///a.rs").await;
        drop(server_end); // server exits → reader hits EOF → `alive` flips false
        // Let the reader task observe EOF.
        for _ in 0..50 {
            if !client.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The budget is a full minute, but a dead client must return well under it rather than
        // polling the whole budget out.
        let start = tokio::time::Instant::now();
        let (diags, timed_out) = client
            .wait_for_diagnostics("file:///a.rs", generation, Duration::from_secs(60))
            .await;
        assert!(timed_out);
        assert!(diags.is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a dead client should short-circuit, not wait the full 60s budget"
        );
    }

    #[tokio::test]
    async fn initialize_sends_initialize_then_initialized() {
        let (client, server_end) = duplex_client();
        let (sr, mut sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);

        let root = std::env::current_dir().unwrap();
        let responder = tokio::spawn(async move {
            let init = read_message(&mut sr).await.unwrap();
            assert_eq!(init["method"], "initialize");
            write_message(
                &mut sw,
                &serde_json::json!({"jsonrpc": "2.0", "id": init["id"], "result": {"capabilities": {}}}),
            )
            .await
            .unwrap();
            let initialized = read_message(&mut sr).await.unwrap();
            assert_eq!(initialized["method"], "initialized");
        });

        client.initialize(&root).await.unwrap();
        responder.await.unwrap();
    }

    #[test]
    fn spawn_process_with_bogus_binary_errors() {
        assert!(spawn_process("definitely-not-a-real-binary-xyz", &[]).is_err());
    }

    #[tokio::test]
    async fn reader_replies_method_not_found_to_unknown_server_requests() {
        let (client, server_end) = duplex_client();
        let (sr, mut sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);

        write_message(
            &mut sw,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "workspace/configuration",
                "params": {}
            }),
        )
        .await
        .unwrap();

        let reply = read_message(&mut sr).await.unwrap();
        assert_eq!(reply["id"], 99);
        assert_eq!(reply["error"]["code"], -32601);
        drop(client);
    }

    #[tokio::test]
    async fn write_failure_marks_the_client_dead() {
        let (client, server_end) = duplex_client();
        assert!(client.is_alive());
        drop(server_end);
        let _ = client
            .notify("textDocument/didOpen", serde_json::json!({}))
            .await;
        assert!(!client.is_alive());
    }
}
