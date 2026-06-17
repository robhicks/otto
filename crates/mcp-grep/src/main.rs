//! `mcp-grep <root>` — an MCP stdio server providing ripgrep-style regex search over <root>,
//! path-contained and never searching sensitive files. The engine spawns this and registers the
//! `grep` tool behind the gate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Search core
// ---------------------------------------------------------------------------

/// Max matches returned before the search stops and reports truncation.
const MAX_MATCHES: usize = 1000;

/// Substrings (lowercase) that mark a path as sensitive — mirrors the engine gate's floor so a
/// standalone `mcp-grep` never returns secret file contents (incl. non-dotfile `id_rsa`).
const SENSITIVE_SKIP: &[&str] = &[".env", ".ssh", ".git", "id_rsa", ".aws"];

fn is_sensitive(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    SENSITIVE_SKIP.iter().any(|m| lower.contains(m))
}

/// One match: workspace-relative path, 1-based line number, the matched line (trailing newline trimmed).
#[derive(Debug, Clone, Serialize)]
pub struct Match {
    pub path: String,
    pub line_number: u64,
    pub line: String,
}

/// Search `root` for `pattern` (regex), optionally limited to files matching `glob`. Skips hidden
/// files (ignore default) and sensitive-marked paths; stays rooted (no symlink follow). Returns
/// matches (capped at MAX_MATCHES, sorted by (path, line_number)) and whether the cap was hit.
pub fn search(
    root: &Path,
    pattern: &str,
    glob: Option<&str>,
) -> anyhow::Result<(Vec<Match>, bool)> {
    use grep_regex::RegexMatcher;
    use grep_searcher::Searcher;
    use grep_searcher::sinks::UTF8;
    use ignore::WalkBuilder;

    let matcher = RegexMatcher::new(pattern)?;
    let glob_matcher = match glob {
        Some(g) => Some(globset::Glob::new(g)?.compile_matcher()),
        None => None,
    };

    let mut matches: Vec<Match> = Vec::new();
    let mut truncated = false;

    let walk = WalkBuilder::new(root)
        .hidden(true) // skip dotfiles/dotdirs (.env/.ssh/.aws/.git)
        .git_ignore(true)
        .follow_links(false)
        .build();

    'walk: for entry in walk {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // skip unreadable entries
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let rel_str = rel.to_string_lossy().into_owned();
        if is_sensitive(&rel_str) {
            continue; // never search secret files (incl. non-dotfile id_rsa)
        }
        if let Some(gm) = &glob_matcher {
            if !gm.is_match(rel) {
                continue;
            }
        }

        // Collect matches from this file into a local vec first to avoid a
        // double-borrow on `matches` inside the closure (read `.len()` + push).
        let path = entry.path().to_path_buf();
        let rel_for_sink = rel_str.clone();
        let remaining = MAX_MATCHES.saturating_sub(matches.len());
        let mut file_matches: Vec<Match> = Vec::new();
        let mut file_capped = false;

        let mut searcher = Searcher::new();
        let _ = searcher.search_path(
            &matcher,
            &path,
            UTF8(|line_number, line| {
                if file_matches.len() >= remaining {
                    file_capped = true;
                    return Ok(false); // stop this file
                }
                file_matches.push(Match {
                    path: rel_for_sink.clone(),
                    line_number,
                    line: line.trim_end_matches(['\n', '\r']).to_string(),
                });
                Ok(true)
            }),
        );
        // per-file search errors (e.g. binary/invalid utf8) are silently skipped

        matches.extend(file_matches);

        if file_capped || matches.len() >= MAX_MATCHES {
            truncated = true;
            // Enforce the hard cap even if a single huge file overflowed
            matches.truncate(MAX_MATCHES);
            break 'walk;
        }
    }

    matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));
    Ok((matches, truncated))
}

// ---------------------------------------------------------------------------
// rmcp server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GrepServer {
    root: Arc<PathBuf>,
}

impl GrepServer {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(root),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GrepArgs {
    pattern: String,
    glob: Option<String>,
}

fn to_err(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

#[tool_router(server_handler)]
impl GrepServer {
    #[tool(name = "grep", description = "Regex search over the workspace (ripgrep-style)")]
    async fn grep(
        &self,
        Parameters(GrepArgs { pattern, glob }): Parameters<GrepArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = Arc::clone(&self.root);
        // The search is sync/CPU-bound; run it off the async executor.
        let (matches, truncated) = tokio::task::spawn_blocking(move || {
            search(&root, &pattern, glob.as_deref())
        })
        .await
        .map_err(|e| to_err(anyhow::anyhow!("search task failed: {e}")))?
        .map_err(to_err)?;

        Ok(CallToolResult::structured(
            serde_json::json!({ "matches": matches, "truncated": truncated }),
        ))
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-grep <root>"))?;
    let server = GrepServer::new(root);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(dir: &std::path::Path, files: &[(&str, &str)]) {
        for (rel, contents) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, contents).unwrap();
        }
    }

    #[test]
    fn finds_matches_with_shape() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), &[("a.txt", "alpha\nTODO: x\n"), ("src/b.rs", "// TODO: y\n")]);
        let (matches, truncated) = search(dir.path(), "TODO", None).unwrap();
        assert!(!truncated);
        // Both files matched; paths are relative; line numbers are 1-based.
        assert!(matches
            .iter()
            .any(|m| m.path == "a.txt" && m.line_number == 2 && m.line.contains("TODO")));
        assert!(matches.iter().any(|m| m.path == "src/b.rs" && m.line.contains("TODO")));
    }

    #[test]
    fn glob_narrows_results() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), &[("a.txt", "TODO\n"), ("src/b.rs", "TODO\n")]);
        let (matches, _) = search(dir.path(), "TODO", Some("*.rs")).unwrap();
        assert!(matches.iter().all(|m| m.path.ends_with(".rs")));
        assert!(matches.iter().any(|m| m.path == "src/b.rs"));
    }

    #[test]
    fn invalid_regex_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(search(dir.path(), "(unclosed", None).is_err());
    }

    #[test]
    fn cap_sets_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let many = "x\n".repeat(MAX_MATCHES + 50);
        seed(dir.path(), &[("big.txt", &many)]);
        let (matches, truncated) = search(dir.path(), "x", None).unwrap();
        assert!(truncated);
        assert_eq!(matches.len(), MAX_MATCHES);
    }

    #[test]
    fn does_not_search_dotfile_secret() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), &[(".env", "SECRET=hunter2\n"), ("ok.txt", "SECRET=visible\n")]);
        let (matches, _) = search(dir.path(), "SECRET", None).unwrap();
        // The dotfile secret is never searched; only the non-sensitive file matches.
        assert!(matches.iter().all(|m| m.path != ".env"));
        assert!(matches.iter().any(|m| m.path == "ok.txt"));
    }

    #[test]
    fn does_not_search_id_rsa_secret() {
        let dir = tempfile::tempdir().unwrap();
        // id_rsa is NOT a dotfile, so hidden-skip alone wouldn't exclude it — the marker skip does.
        seed(dir.path(), &[("id_rsa", "PRIVATE KEY material\n"), ("ok.txt", "PRIVATE matches\n")]);
        let (matches, _) = search(dir.path(), "PRIVATE", None).unwrap();
        assert!(matches.iter().all(|m| m.path != "id_rsa"));
        assert!(matches.iter().any(|m| m.path == "ok.txt"));
    }
}
