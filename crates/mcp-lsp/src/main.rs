//! `mcp-lsp <root>` — an MCP stdio server bridging agent-facing tool calls to a real
//! `rust-analyzer` language server over LSP-over-stdio. See
//! docs/superpowers/specs/2026-07-07-mcp-lsp-design.md.

mod lsp_client;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_client::LspClient;
use otto_engine_core::traits::WorkspaceRead;
use otto_workspace::LocalWorkspace;
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

/// The server struct wrapping an `LspClient` bridged to `rust-analyzer`, plus a path-contained
/// `LocalWorkspace` used to read file content for `didOpen`/`didChange` sync.
#[derive(Clone)]
pub struct LspServer {
    lsp: Arc<LspClient>,
    workspace: Arc<LocalWorkspace>,
    root: PathBuf,
    open_docs: Arc<Mutex<HashMap<String, i32>>>, // workspace-relative path -> last-sent version
}

impl LspServer {
    pub fn new(lsp: LspClient, root: PathBuf) -> Self {
        Self {
            lsp: Arc::new(lsp),
            workspace: Arc::new(LocalWorkspace::new(root.clone())),
            root,
            open_docs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn uri_for(&self, path: &str) -> anyhow::Result<lsp_types::Uri> {
        lsp_client::path_to_file_uri(&self.root.join(path))
    }

    /// Ensure `path` is open (or up to date) in the language server via `didOpen`/`didChange`.
    /// Returns the document's URI string and the diagnostics generation to wait on for freshness.
    async fn open_if_needed(&self, path: &str) -> anyhow::Result<(String, u64)> {
        let content = String::from_utf8(self.workspace.read(Path::new(path)).await?)?;
        let uri = self.uri_for(path)?;
        let uri_str = uri.as_str().to_string();
        let generation = self.lsp.bump_generation(&uri_str).await;
        let mut open = self.open_docs.lock().await;
        match open.get(path) {
            None => {
                open.insert(path.to_string(), 1);
                self.lsp
                    .notify(
                        "textDocument/didOpen",
                        serde_json::to_value(lsp_types::DidOpenTextDocumentParams {
                            text_document: lsp_types::TextDocumentItem::new(
                                uri,
                                "rust".to_string(),
                                1,
                                content,
                            ),
                        })?,
                    )
                    .await?;
            }
            Some(&version) => {
                let next = version + 1;
                open.insert(path.to_string(), next);
                self.lsp
                    .notify(
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
                    .await?;
            }
        }
        Ok((uri_str, generation))
    }

    pub async fn do_diagnostics(&self, path: String) -> anyhow::Result<(Vec<DiagnosticOut>, bool)> {
        self.do_diagnostics_with_timeout(path, diagnostics_timeout())
            .await
    }

    async fn do_diagnostics_with_timeout(
        &self,
        path: String,
        wait: std::time::Duration,
    ) -> anyhow::Result<(Vec<DiagnosticOut>, bool)> {
        let (uri, generation) = self.open_if_needed(&path).await?;
        let (diags, timed_out) = self.lsp.wait_for_diagnostics(&uri, generation, wait).await;
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
        items
            .into_iter()
            .map(|(uri, range)| {
                Ok(LocationOut {
                    path: self.path_for(&uri)?,
                    line: range.start.line + 1,
                    character: range.start.character + 1,
                })
            })
            .collect()
    }

    pub async fn do_definition(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<LocationOut>> {
        let (uri, _generation) = self.open_if_needed(&path).await?;
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
        let result = self
            .lsp
            .request(
                "textDocument/definition",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await?;
        self.goto_response_to_locations(result)
    }

    pub async fn do_references(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<LocationOut>> {
        let (uri, _generation) = self.open_if_needed(&path).await?;
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
        let result = self
            .lsp
            .request(
                "textDocument/references",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await?;
        if result.is_null() {
            return Ok(vec![]);
        }
        let locs: Vec<lsp_types::Location> = serde_json::from_value(result)?;
        locs.into_iter()
            .map(|l| {
                Ok(LocationOut {
                    path: self.path_for(&l.uri)?,
                    line: l.range.start.line + 1,
                    character: l.range.start.character + 1,
                })
            })
            .collect()
    }

    pub async fn do_hover(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Option<String>> {
        let (uri, _generation) = self.open_if_needed(&path).await?;
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
        let result = self
            .lsp
            .request(
                "textDocument/hover",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await?;
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

fn main() {
    eprintln!("mcp-lsp: scaffold only, not yet implemented");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn duplex_server(root: &Path) -> (LspServer, tokio::io::DuplexStream) {
        let (client_end, server_end) = tokio::io::duplex(16384);
        let (cr, cw) = tokio::io::split(client_end);
        let lsp = LspClient::spawn(BufReader::new(cr), cw);
        (LspServer::new(lsp, root.to_path_buf()), server_end)
    }

    #[tokio::test]
    async fn open_if_needed_sends_did_open_then_did_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path());
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
        let (server, server_end) = duplex_server(dir.path());
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
        let (server, _server_end) = duplex_server(dir.path());
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
        let (server, server_end) = duplex_server(dir.path());
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
    async fn do_definition_returns_empty_for_a_null_response() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path());
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
        let (server, server_end) = duplex_server(dir.path());
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
        let (server, server_end) = duplex_server(dir.path());
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
        let (server, server_end) = duplex_server(dir.path());
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
}
