//! `mcp-lsp <root>` — an MCP stdio server bridging agent-facing tool calls to a real
//! `rust-analyzer` language server over LSP-over-stdio. See
//! docs/superpowers/specs/2026-07-07-mcp-lsp-design.md.

mod lang;
mod lsp_client;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_client::LspClient;
use otto_engine_core::traits::WorkspaceRead;
use otto_workspace::LocalWorkspace;
use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Deserialize;
use tokio::sync::Mutex;

const DEFAULT_DIAGNOSTICS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn diagnostics_timeout() -> std::time::Duration {
    std::env::var("OTTO_MCP_LSP_DIAG_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(DEFAULT_DIAGNOSTICS_TIMEOUT)
}

fn severity_name(s: lsp_types::DiagnosticSeverity) -> String {
    match s {
        lsp_types::DiagnosticSeverity::ERROR => "error",
        lsp_types::DiagnosticSeverity::WARNING => "warning",
        lsp_types::DiagnosticSeverity::INFORMATION => "information",
        lsp_types::DiagnosticSeverity::HINT => "hint",
        _ => "unknown",
    }
    .to_string()
}

const NAVIGATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, serde::Serialize)]
pub struct LocationOut {
    path: String,
    line: u32,
    character: u32,
}

#[derive(Debug, serde::Serialize)]
pub struct DiagnosticOut {
    line: u32,
    character: u32,
    severity: Option<String>,
    message: String,
    code: Option<String>,
}

/// A spawned, initialized language server. `_child` is `Option` so a test can seed a
/// duplex-backed client with no OS process (a pipe has nothing for kill_on_drop to protect).
struct ReadyServer {
    client: Arc<LspClient>,
    _child: Option<tokio::process::Child>,
}

#[derive(Clone)]
pub struct LspServer {
    /// server key → its lazily-spawned slot. The outer lock is held only to get-or-insert a
    /// slot; the slow spawn+initialize happens under the per-key inner lock, so a cold `gopls`
    /// never blocks a warm `rust-analyzer` call.
    slots: Arc<Mutex<HashMap<&'static str, Arc<Mutex<Option<ReadyServer>>>>>>,
    /// server keys whose binary is definitely not on PATH — a permanent negative cache (avoids
    /// spawn hammering). Spawn/init errors are NOT cached here; they stay retry-eligible.
    absent: Arc<Mutex<HashSet<&'static str>>>,
    /// server keys that have returned a non-timed-out diagnostics result — after which the
    /// steady-state (short) diagnostics budget applies instead of the cold-start budget.
    served_diag: Arc<Mutex<HashSet<&'static str>>>,
    workspace: Arc<LocalWorkspace>,
    root: PathBuf,
    open_docs: Arc<Mutex<HashMap<String, i32>>>,
}

impl LspServer {
    pub fn new(root: PathBuf) -> Self {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Self {
            slots: Arc::new(Mutex::new(HashMap::new())),
            absent: Arc::new(Mutex::new(HashSet::new())),
            served_diag: Arc::new(Mutex::new(HashSet::new())),
            workspace: Arc::new(LocalWorkspace::new(root.clone())),
            root,
            open_docs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Extension of `path`, lowercased, no leading dot (`""` if none).
    fn extension_of(path: &str) -> String {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default()
    }

    fn spec_for(&self, path: &str) -> Option<&'static lang::ServerSpec> {
        lang::config_for_extension(&Self::extension_of(path)).map(|(spec, _)| spec)
    }

    /// Get the client for `spec`, spawning + initializing it on first use. Per-key locked, so
    /// concurrent first-calls for the same server don't double-spawn and different languages
    /// proceed in parallel. A definitely-absent binary is cached in `absent` (never retried);
    /// spawn/init failures leave the slot empty (retry-eligible next call).
    async fn get_or_spawn(
        &self,
        spec: &'static lang::ServerSpec,
    ) -> anyhow::Result<Arc<LspClient>> {
        if self.absent.lock().await.contains(spec.key) {
            anyhow::bail!("no language server `{}` on PATH", lang::resolved_bin(spec));
        }
        let slot = {
            let mut slots = self.slots.lock().await;
            slots
                .entry(spec.key)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        let mut guard = slot.lock().await;
        if let Some(ready) = guard.as_ref() {
            return Ok(ready.client.clone());
        }
        let bin = lang::resolved_bin(spec);
        let path_var = std::env::var("PATH").unwrap_or_default();
        if lang::resolve_executable(&bin, &path_var).is_none() {
            self.absent.lock().await.insert(spec.key);
            anyhow::bail!("no language server `{bin}` on PATH");
        }
        let (client, child) = lsp_client::spawn_process(&bin, spec.args)?;
        client.initialize(&self.root).await?;
        let client = Arc::new(client);
        *guard = Some(ReadyServer {
            client: client.clone(),
            _child: Some(child),
        });
        Ok(client)
    }

    /// Drop a cached client (its server process died) so the next call re-spawns.
    async fn evict(&self, key: &'static str) {
        let slot = self.slots.lock().await.get(key).cloned();
        if let Some(slot) = slot {
            *slot.lock().await = None;
        }
    }

    /// The diagnostics wait budget for `spec`: the cold-start budget until the server has
    /// returned one non-timed-out result, then the steady-state default.
    async fn diag_wait_for(&self, spec: &lang::ServerSpec) -> std::time::Duration {
        if self.served_diag.lock().await.contains(spec.key) {
            diagnostics_timeout()
        } else {
            spec.first_open_diag_timeout
        }
    }

    /// Record that `spec` returned diagnostics; only a non-timed-out result flips it to
    /// steady-state (a cold server keeps the long budget until it actually responds).
    async fn mark_served(&self, spec: &lang::ServerSpec, timed_out: bool) {
        if !timed_out {
            self.served_diag.lock().await.insert(spec.key);
        }
    }

    #[cfg(test)]
    async fn seed_ready_for_test(&self, key: &'static str, client: LspClient) {
        let slot = {
            let mut slots = self.slots.lock().await;
            slots
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        *slot.lock().await = Some(ReadyServer {
            client: Arc::new(client),
            _child: None,
        });
    }

    #[cfg(test)]
    async fn slot_handle_for_test(&self, key: &'static str) -> Arc<Mutex<Option<ReadyServer>>> {
        let mut slots = self.slots.lock().await;
        slots
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    #[cfg(test)]
    async fn slot_is_ready(&self, key: &'static str) -> bool {
        let slot = self.slots.lock().await.get(key).cloned();
        match slot {
            Some(s) => s.lock().await.is_some(),
            None => false,
        }
    }

    #[cfg(test)]
    async fn mark_absent_for_test(&self, key: &'static str) {
        self.absent.lock().await.insert(key);
    }

    #[cfg(test)]
    async fn absent_contains(&self, key: &'static str) -> bool {
        self.absent.lock().await.contains(key)
    }

    fn uri_for(&self, path: &str) -> anyhow::Result<lsp_types::Uri> {
        lsp_client::path_to_file_uri(&self.root.join(path))
    }

    /// Ensure `path` is open (or up to date) in its language's server. Resolves the extension to
    /// a (server, languageId), spawns the server on first use, sends `didOpen`/`didChange` with
    /// that languageId, and returns the client so the caller issues its request against it. A
    /// notify write failure means the server's pipe is closed (it died) — evict so the next call
    /// re-spawns rather than leaving a dead client cached.
    async fn open_if_needed(&self, path: &str) -> anyhow::Result<(Arc<LspClient>, String, u64)> {
        let ext = Self::extension_of(path);
        let (spec, language_id) = lang::config_for_extension(&ext)
            .ok_or_else(|| anyhow::anyhow!("no language server configured for .{ext}"))?;
        let client = self.get_or_spawn(spec).await?;

        let content = String::from_utf8(self.workspace.read(Path::new(path)).await?)?;
        let uri = self.uri_for(path)?;
        let uri_str = uri.as_str().to_string();
        let generation = client.bump_generation(&uri_str).await;

        let (method, params) = {
            let mut open = self.open_docs.lock().await;
            match open.get(&uri_str).copied() {
                None => {
                    open.insert(uri_str.clone(), 1);
                    (
                        "textDocument/didOpen",
                        serde_json::to_value(lsp_types::DidOpenTextDocumentParams {
                            text_document: lsp_types::TextDocumentItem::new(
                                uri,
                                language_id.to_string(),
                                1,
                                content,
                            ),
                        })?,
                    )
                }
                Some(version) => {
                    let next = version + 1;
                    open.insert(uri_str.clone(), next);
                    (
                        "textDocument/didChange",
                        serde_json::to_value(lsp_types::DidChangeTextDocumentParams {
                            text_document: lsp_types::VersionedTextDocumentIdentifier::new(
                                uri, next,
                            ),
                            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                                range: None,
                                range_length: None,
                                text: content,
                            }],
                        })?,
                    )
                }
            }
        };

        if let Err(e) = client.notify(method, params).await {
            self.evict(spec.key).await;
            return Err(e);
        }
        Ok((client, uri_str, generation))
    }

    pub async fn do_diagnostics(&self, path: String) -> anyhow::Result<(Vec<DiagnosticOut>, bool)> {
        let spec = self
            .spec_for(&path)
            .ok_or_else(|| anyhow::anyhow!("no language server configured for `{path}`"))?;
        let wait = self.diag_wait_for(spec).await;
        let (out, timed_out) = self.do_diagnostics_with_timeout(path, wait).await?;
        self.mark_served(spec, timed_out).await;
        Ok((out, timed_out))
    }

    async fn do_diagnostics_with_timeout(
        &self,
        path: String,
        wait: std::time::Duration,
    ) -> anyhow::Result<(Vec<DiagnosticOut>, bool)> {
        let (client, uri, generation) = self.open_if_needed(&path).await?;
        let (diags, timed_out) = client.wait_for_diagnostics(&uri, generation, wait).await;
        if timed_out && !client.is_alive() {
            if let Some(spec) = self.spec_for(&path) {
                self.evict(spec.key).await;
            }
        }
        let out = diags
            .into_iter()
            .map(|d| DiagnosticOut {
                line: d.range.start.line + 1,
                character: d.range.start.character + 1,
                severity: d.severity.map(severity_name),
                message: d.message,
                code: d.code.map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s,
                }),
            })
            .collect();
        Ok((out, timed_out))
    }

    fn path_for(&self, uri: &lsp_types::Uri) -> anyhow::Result<String> {
        let s = uri.as_str();
        let prefix = format!("file://{}/", self.root.display());
        s.strip_prefix(&prefix)
            .map(|p| p.to_string())
            .ok_or_else(|| anyhow::anyhow!("uri {s} is outside the workspace root"))
    }

    fn goto_response_to_locations(
        &self,
        result: serde_json::Value,
    ) -> anyhow::Result<Vec<LocationOut>> {
        if result.is_null() {
            return Ok(vec![]);
        }
        let resp: lsp_types::GotoDefinitionResponse = serde_json::from_value(result)?;
        let items: Vec<(lsp_types::Uri, lsp_types::Range)> = match resp {
            lsp_types::GotoDefinitionResponse::Scalar(loc) => vec![(loc.uri, loc.range)],
            lsp_types::GotoDefinitionResponse::Array(locs) => {
                locs.into_iter().map(|l| (l.uri, l.range)).collect()
            }
            lsp_types::GotoDefinitionResponse::Link(links) => links
                .into_iter()
                .map(|l| (l.target_uri, l.target_range))
                .collect(),
        };
        // Out-of-root locations (std / dependency sources, e.g. `~/.cargo/registry`) are
        // skipped rather than erroring the whole call — the in-root subset is the answer.
        Ok(items
            .into_iter()
            .filter_map(|(uri, range)| {
                self.path_for(&uri).ok().map(|path| LocationOut {
                    path,
                    line: range.start.line + 1,
                    character: range.start.character + 1,
                })
            })
            .collect())
    }

    /// Evict `path`'s server if a request failed against a now-dead client.
    async fn evict_if_dead(&self, path: &str, client: &LspClient, failed: bool) {
        if failed && !client.is_alive() {
            if let Some(spec) = self.spec_for(path) {
                self.evict(spec.key).await;
            }
        }
    }

    pub async fn do_definition(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<LocationOut>> {
        let (client, uri, _generation) = self.open_if_needed(&path).await?;
        let params = lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.parse()? },
                position: lsp_types::Position::new(
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                ),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let result = client
            .request(
                "textDocument/definition",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await;
        self.evict_if_dead(&path, &client, result.is_err()).await;
        self.goto_response_to_locations(result?)
    }

    pub async fn do_references(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<LocationOut>> {
        let (client, uri, _generation) = self.open_if_needed(&path).await?;
        let params = lsp_types::ReferenceParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.parse()? },
                position: lsp_types::Position::new(
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                ),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_types::ReferenceContext {
                include_declaration: true,
            },
        };
        let result = client
            .request(
                "textDocument/references",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await;
        self.evict_if_dead(&path, &client, result.is_err()).await;
        let result = result?;
        if result.is_null() {
            return Ok(vec![]);
        }
        let locs: Vec<lsp_types::Location> = serde_json::from_value(result)?;
        Ok(locs
            .into_iter()
            .filter_map(|l| {
                self.path_for(&l.uri).ok().map(|path| LocationOut {
                    path,
                    line: l.range.start.line + 1,
                    character: l.range.start.character + 1,
                })
            })
            .collect())
    }

    pub async fn do_hover(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Option<String>> {
        let (client, uri, _generation) = self.open_if_needed(&path).await?;
        let params = lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.parse()? },
                position: lsp_types::Position::new(
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                ),
            },
            work_done_progress_params: Default::default(),
        };
        let result = client
            .request(
                "textDocument/hover",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await;
        self.evict_if_dead(&path, &client, result.is_err()).await;
        let result = result?;
        if result.is_null() {
            return Ok(None);
        }
        let hover: lsp_types::Hover = serde_json::from_value(result)?;
        Ok(Some(render_hover_contents(hover.contents)))
    }
}

fn render_hover_contents(contents: lsp_types::HoverContents) -> String {
    fn render_marked(m: lsp_types::MarkedString) -> String {
        match m {
            lsp_types::MarkedString::String(s) => s,
            lsp_types::MarkedString::LanguageString(ls) => ls.value,
        }
    }
    match contents {
        lsp_types::HoverContents::Scalar(m) => render_marked(m),
        lsp_types::HoverContents::Array(ms) => ms
            .into_iter()
            .map(render_marked)
            .collect::<Vec<_>>()
            .join("\n\n"),
        lsp_types::HoverContents::Markup(mc) => mc.value,
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PathArgs {
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PositionArgs {
    path: String,
    line: u32,
    character: u32,
}

fn to_err(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

#[tool_router(server_handler)]
impl LspServer {
    #[tool(
        name = "lsp.diagnostics",
        description = "Get structured compiler/language-server diagnostics for a file"
    )]
    async fn diagnostics(
        &self,
        Parameters(PathArgs { path }): Parameters<PathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let (diagnostics, timed_out) = self.do_diagnostics(path).await.map_err(to_err)?;
        Ok(CallToolResult::structured(serde_json::json!({
            "diagnostics": diagnostics,
            "timed_out": timed_out,
        })))
    }

    #[tool(
        name = "lsp.definition",
        description = "Go to the definition of the symbol at a position"
    )]
    async fn definition(
        &self,
        Parameters(PositionArgs {
            path,
            line,
            character,
        }): Parameters<PositionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let locations = self
            .do_definition(path, line, character)
            .await
            .map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "locations": locations }),
        ))
    }

    #[tool(
        name = "lsp.references",
        description = "Find references to the symbol at a position"
    )]
    async fn references(
        &self,
        Parameters(PositionArgs {
            path,
            line,
            character,
        }): Parameters<PositionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let locations = self
            .do_references(path, line, character)
            .await
            .map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "locations": locations }),
        ))
    }

    #[tool(
        name = "lsp.hover",
        description = "Get hover information for the symbol at a position"
    )]
    async fn hover(
        &self,
        Parameters(PositionArgs {
            path,
            line,
            character,
        }): Parameters<PositionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let contents = self.do_hover(path, line, character).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "contents": contents }),
        ))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-lsp <root>"))?;
    let root = std::fs::canonicalize(&root)?;

    // Additive-absence gate: if no supported language server is on PATH, exit before the MCP
    // handshake so the engine's `connect_lsp` fails and registers no `lsp.*` tools. If ≥1 is
    // present, serve — individual servers spawn lazily on first use of their language.
    if !lang::any_server_available() {
        anyhow::bail!(
            "no supported language server on PATH \
             (rust-analyzer / typescript-language-server / pyright-langserver / gopls); \
             lsp tools will be unavailable"
        );
    }

    let server = LspServer::new(root);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::BufReader;

    async fn duplex_server(root: &Path) -> (LspServer, tokio::io::DuplexStream) {
        seeded_duplex_server(root, "rust-analyzer").await
    }

    /// A server with one duplex-backed client seeded under `key` (no real process spawned).
    async fn seeded_duplex_server(
        root: &Path,
        key: &'static str,
    ) -> (LspServer, tokio::io::DuplexStream) {
        let (client_end, server_end) = tokio::io::duplex(16384);
        let (cr, cw) = tokio::io::split(client_end);
        let lsp = LspClient::spawn(BufReader::new(cr), cw);
        let server = LspServer::new(root.to_path_buf());
        server.seed_ready_for_test(key, lsp).await;
        (server, server_end)
    }

    #[tokio::test]
    async fn open_if_needed_sends_did_open_then_did_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        let (sr, _sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);

        server.open_if_needed("a.rs").await.unwrap();
        let first = lsp_client::read_message(&mut sr).await.unwrap();
        assert_eq!(first["method"], "textDocument/didOpen");
        assert_eq!(first["params"]["textDocument"]["version"], 1);

        std::fs::write(dir.path().join("a.rs"), "fn a() { let _ = 1; }").unwrap();
        server.open_if_needed("a.rs").await.unwrap();
        let second = lsp_client::read_message(&mut sr).await.unwrap();
        assert_eq!(second["method"], "textDocument/didChange");
        assert_eq!(second["params"]["textDocument"]["version"], 2);
    }

    #[tokio::test]
    async fn do_diagnostics_returns_fresh_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        let (sr, sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        let mut sw = sw;

        let responder = tokio::spawn(async move {
            let opened = lsp_client::read_message(&mut sr).await.unwrap();
            let uri = opened["params"]["textDocument"]["uri"].clone();
            lsp_client::write_message(
                &mut sw,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": uri,
                        "diagnostics": [{
                            "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}},
                            "message": "unused variable"
                        }]
                    }
                }),
            )
            .await
            .unwrap();
        });

        let (diags, timed_out) = server.do_diagnostics("a.rs".to_string()).await.unwrap();
        responder.await.unwrap();
        assert!(!timed_out);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 1);
        assert_eq!(diags[0].character, 4);
        assert_eq!(diags[0].message, "unused variable");
    }

    #[tokio::test]
    async fn do_diagnostics_times_out_without_a_response() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, _server_end) = duplex_server(dir.path()).await;
        let (diags, timed_out) = server
            .do_diagnostics_with_timeout("a.rs".to_string(), std::time::Duration::from_millis(200))
            .await
            .unwrap();
        assert!(timed_out);
        assert!(diags.is_empty());
    }

    #[tokio::test]
    async fn do_definition_parses_a_scalar_location_response() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() { a(); }").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        let (sr, sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        let mut sw = sw;

        let target_uri = lsp_client::path_to_file_uri(&dir.path().join("a.rs")).unwrap();
        let target_uri_str = target_uri.as_str().to_string();
        let responder = tokio::spawn(async move {
            let _opened = lsp_client::read_message(&mut sr).await.unwrap();
            let req = lsp_client::read_message(&mut sr).await.unwrap();
            assert_eq!(req["method"], "textDocument/definition");
            lsp_client::write_message(
                &mut sw,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": {
                        "uri": target_uri_str,
                        "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}
                    }
                }),
            )
            .await
            .unwrap();
        });

        let locations = server
            .do_definition("a.rs".to_string(), 2, 10)
            .await
            .unwrap();
        responder.await.unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, "a.rs");
        assert_eq!(locations[0].line, 1);
        assert_eq!(locations[0].character, 4);
    }

    #[tokio::test]
    async fn do_definition_skips_out_of_root_locations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() { a(); }").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        let (sr, sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        let mut sw = sw;

        let in_root_uri = lsp_client::path_to_file_uri(&dir.path().join("a.rs"))
            .unwrap()
            .as_str()
            .to_string();
        let responder = tokio::spawn(async move {
            let _opened = lsp_client::read_message(&mut sr).await.unwrap();
            let req = lsp_client::read_message(&mut sr).await.unwrap();
            lsp_client::write_message(
                &mut sw,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": [
                        {"uri": in_root_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}},
                        {"uri": "file:///definitely/elsewhere/x.rs", "range": {"start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 1}}}
                    ]
                }),
            )
            .await
            .unwrap();
        });

        let locations = server
            .do_definition("a.rs".to_string(), 2, 10)
            .await
            .unwrap();
        responder.await.unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, "a.rs");
    }

    #[tokio::test]
    async fn do_definition_returns_empty_for_a_null_response() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        let (sr, sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        let mut sw = sw;

        let responder = tokio::spawn(async move {
            let _opened = lsp_client::read_message(&mut sr).await.unwrap();
            let req = lsp_client::read_message(&mut sr).await.unwrap();
            lsp_client::write_message(
                &mut sw,
                &serde_json::json!({"jsonrpc": "2.0", "id": req["id"], "result": null}),
            )
            .await
            .unwrap();
        });

        let locations = server
            .do_definition("a.rs".to_string(), 1, 1)
            .await
            .unwrap();
        responder.await.unwrap();
        assert!(locations.is_empty());
    }

    #[tokio::test]
    async fn do_references_parses_an_array_of_locations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() { a(); a(); }").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        let (sr, sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        let mut sw = sw;

        let uri = lsp_client::path_to_file_uri(&dir.path().join("a.rs"))
            .unwrap()
            .as_str()
            .to_string();
        let responder = tokio::spawn(async move {
            let _opened = lsp_client::read_message(&mut sr).await.unwrap();
            let req = lsp_client::read_message(&mut sr).await.unwrap();
            assert_eq!(req["method"], "textDocument/references");
            lsp_client::write_message(
                &mut sw,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": [
                        {"uri": uri, "range": {"start": {"line": 1, "character": 9}, "end": {"line": 1, "character": 10}}},
                        {"uri": uri, "range": {"start": {"line": 1, "character": 14}, "end": {"line": 1, "character": 15}}}
                    ]
                }),
            )
            .await
            .unwrap();
        });

        let locations = server
            .do_references("a.rs".to_string(), 1, 1)
            .await
            .unwrap();
        responder.await.unwrap();
        assert_eq!(locations.len(), 2);
        assert_eq!(locations[0].line, 2);
        assert_eq!(locations[1].line, 2);
    }

    #[tokio::test]
    async fn do_hover_renders_markup_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        let (sr, sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        let mut sw = sw;

        let responder = tokio::spawn(async move {
            let _opened = lsp_client::read_message(&mut sr).await.unwrap();
            let req = lsp_client::read_message(&mut sr).await.unwrap();
            assert_eq!(req["method"], "textDocument/hover");
            lsp_client::write_message(
                &mut sw,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": {"contents": {"kind": "markdown", "value": "`fn a()`"}}
                }),
            )
            .await
            .unwrap();
        });

        let hover = server.do_hover("a.rs".to_string(), 1, 4).await.unwrap();
        responder.await.unwrap();
        assert_eq!(hover, Some("`fn a()`".to_string()));
    }

    #[tokio::test]
    async fn do_hover_returns_none_for_a_null_response() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        let (sr, sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        let mut sw = sw;

        let responder = tokio::spawn(async move {
            let _opened = lsp_client::read_message(&mut sr).await.unwrap();
            let req = lsp_client::read_message(&mut sr).await.unwrap();
            lsp_client::write_message(
                &mut sw,
                &serde_json::json!({"jsonrpc": "2.0", "id": req["id"], "result": null}),
            )
            .await
            .unwrap();
        });

        let hover = server.do_hover("a.rs".to_string(), 1, 1).await.unwrap();
        responder.await.unwrap();
        assert!(hover.is_none());
    }

    #[tokio::test]
    async fn path_for_round_trips_with_a_non_canonical_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        // A messy but valid spelling of the same root: "<dir>/./"
        let messy = dir.path().join(".");
        let server = LspServer::new(messy);
        let uri = server.uri_for("a.rs").unwrap();
        assert_eq!(server.path_for(&uri).unwrap(), "a.rs");
    }

    #[tokio::test]
    async fn open_if_needed_sends_python_language_id_for_py_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let (server, server_end) = seeded_duplex_server(dir.path(), "pyright-langserver").await;
        let (sr, _sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        server.open_if_needed("a.py").await.unwrap();
        let msg = lsp_client::read_message(&mut sr).await.unwrap();
        assert_eq!(msg["method"], "textDocument/didOpen");
        assert_eq!(msg["params"]["textDocument"]["languageId"], "python");
    }

    #[tokio::test]
    async fn open_if_needed_sends_go_language_id_for_go_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.go"), "package main\n").unwrap();
        let (server, server_end) = seeded_duplex_server(dir.path(), "gopls").await;
        let (sr, _sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        server.open_if_needed("a.go").await.unwrap();
        let msg = lsp_client::read_message(&mut sr).await.unwrap();
        assert_eq!(msg["params"]["textDocument"]["languageId"], "go");
    }

    #[tokio::test]
    async fn open_if_needed_lowercases_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.PY"), "x = 1\n").unwrap();
        let (server, server_end) = seeded_duplex_server(dir.path(), "pyright-langserver").await;
        let (sr, _sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        server.open_if_needed("A.PY").await.unwrap();
        let msg = lsp_client::read_message(&mut sr).await.unwrap();
        assert_eq!(msg["params"]["textDocument"]["languageId"], "python");
    }

    #[tokio::test]
    async fn open_if_needed_rejects_an_unsupported_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hi\n").unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        let err = server.open_if_needed("a.txt").await.err().unwrap();
        assert!(err.to_string().contains("no language server configured"));
    }

    #[tokio::test]
    async fn a_busy_server_slot_does_not_block_a_warm_call_to_another() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, _server_end) = duplex_server(dir.path()).await;
        let gopls_slot = server.slot_handle_for_test("gopls").await;
        let _held = gopls_slot.lock().await;
        let opened =
            tokio::time::timeout(Duration::from_secs(2), server.open_if_needed("a.rs")).await;
        assert!(
            opened.is_ok(),
            "a warm rust-analyzer call blocked behind gopls's slot lock"
        );
    }

    #[tokio::test]
    async fn diagnostics_budget_uses_first_open_then_steady_state() {
        let dir = tempfile::tempdir().unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        assert_eq!(
            server.diag_wait_for(&lang::RUST_ANALYZER).await,
            lang::RUST_ANALYZER.first_open_diag_timeout
        );
        server.mark_served(&lang::RUST_ANALYZER, true).await;
        assert_eq!(
            server.diag_wait_for(&lang::RUST_ANALYZER).await,
            lang::RUST_ANALYZER.first_open_diag_timeout
        );
        server.mark_served(&lang::RUST_ANALYZER, false).await;
        assert_eq!(
            server.diag_wait_for(&lang::RUST_ANALYZER).await,
            diagnostics_timeout()
        );
    }

    #[tokio::test]
    async fn an_absent_server_is_cached_and_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        server.mark_absent_for_test("gopls").await;
        assert!(server.get_or_spawn(&lang::GOPLS).await.is_err());
        assert!(server.get_or_spawn(&lang::GOPLS).await.is_err());
        assert!(server.absent_contains("gopls").await);
    }

    #[tokio::test]
    async fn a_dead_client_is_evicted_so_the_next_call_respawns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        assert!(server.slot_is_ready("rust-analyzer").await);
        drop(server_end);
        let res = server.do_definition("a.rs".to_string(), 1, 1).await;
        assert!(res.is_err());
        assert!(
            !server.slot_is_ready("rust-analyzer").await,
            "a dead client should have been evicted"
        );
    }
}

/// Exercises the full stack against a real `rust-analyzer`. Self-skips (prints a message,
/// doesn't fail) when `rust-analyzer` isn't on `PATH` — the offline-determinism suite must not
/// require it, matching the `os_sandbox_available()`-gated test pattern used elsewhere in this
/// codebase for optional external tools.
#[cfg(test)]
mod rust_analyzer_integration {
    use super::*;

    fn rust_analyzer_available() -> bool {
        std::process::Command::new(lang::resolved_bin(&lang::RUST_ANALYZER))
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn full_round_trip_against_a_real_rust_analyzer() {
        if !rust_analyzer_available() {
            eprintln!("skipping: rust-analyzer not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn greet() -> &'static str {\n    \"hi\"\n}\n\npub fn broken() -> i32 {\n    does_not_exist()\n}\n",
        )
        .unwrap();

        let server = LspServer::new(dir.path().to_path_buf());
        let (diags, timed_out) = server
            .do_diagnostics_with_timeout(
                "src/lib.rs".to_string(),
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert!(!timed_out, "rust-analyzer did not respond within 60s");
        assert!(
            diags.iter().any(|d| d.message.contains("does_not_exist")),
            "expected an unresolved-symbol diagnostic, got: {diags:?}"
        );
        let hover = server
            .do_hover("src/lib.rs".to_string(), 1, 8)
            .await
            .unwrap();
        assert!(hover.is_some(), "expected hover info for `greet`");
    }
}
