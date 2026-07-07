//! `mcp-git <root>` — an MCP stdio server performing git operations on the repo at <root> by
//! shelling out to `git`/`gh`. The engine spawns this and registers its tools behind the gate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// run_git / run_gh helpers
// ---------------------------------------------------------------------------

/// Run `git` with cwd = `root`. Ok(stdout) on exit 0; Err carrying stderr otherwise.
pub async fn run_git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to spawn git: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `gh` with cwd = `root`. Errors if `gh` is missing or the command fails.
pub async fn run_gh(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("gh")
        .current_dir(root)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to spawn gh (is it installed?): {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "gh {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Sensitive-path guard (mirrors engine gate + mcp-grep)
// ---------------------------------------------------------------------------

/// Substrings (lowercase) that mark a path as sensitive — MIRRORS the engine gate's
/// `SENSITIVE_MARKERS` in `crates/tools/src/gate.rs` (also mirrored by `mcp-grep`). KEEP IN
/// SYNC: a standalone `mcp-git` refuses to STAGE these so an agent can't commit a secret.
/// (The gate lists `.ssh/`/`.git/`/`.aws/` variants too; the substring `contains` check here
/// makes the trailing-slash forms redundant.)
const SENSITIVE_SKIP: &[&str] = &[".env", ".ssh", ".git", "id_rsa", ".aws"];

fn is_sensitive(p: &str) -> bool {
    let lower = p.to_ascii_lowercase();
    SENSITIVE_SKIP.iter().any(|m| lower.contains(m))
}

// ---------------------------------------------------------------------------
// Argument-injection guards
// ---------------------------------------------------------------------------

// KEEP IN SYNC: `reject_leading_dash`/`validate_clone_url`/`is_scp_like` below are duplicated
// in `crates/engine/src/plugin_cli.rs` (that crate cannot depend on this `[[bin]]`-only crate —
// see `plugin_cli.rs`'s module doc). If this hardening changes here, mirror the change there too.

/// Reject any user-supplied positional that starts with `-`.  A leading dash is reparsed
/// by git as a flag, enabling argv flag-smuggling attacks such as
/// `git clone --upload-pack=<cmd>` or `git clone ext::sh -c <cmd>`.
fn reject_leading_dash(value: &str, what: &str) -> anyhow::Result<()> {
    if value.starts_with('-') {
        anyhow::bail!("invalid {what}: must not start with '-': {value}");
    }
    Ok(())
}

/// Allow only well-known URL schemes for `git clone`.  This blocks `ext::`, `fd::`,
/// bare relative paths, and any other transport that could be weaponised by an LLM agent.
fn validate_clone_url(url: &str) -> anyhow::Result<()> {
    if url.starts_with('-') {
        anyhow::bail!("invalid clone url: {url}");
    }
    const ALLOWED_SCHEMES: &[&str] = &["https://", "http://", "ssh://", "file://"];
    let scheme_ok = ALLOWED_SCHEMES.iter().any(|s| url.starts_with(s)) || is_scp_like(url);
    if !scheme_ok {
        anyhow::bail!("unsupported clone url (allowed: https/http/ssh/file/scp-like): {url}");
    }
    Ok(())
}

/// `user@host:path` scp-like SSH syntax (no scheme): a `:` whose left side has no `/`
/// and contains `@`.
fn is_scp_like(url: &str) -> bool {
    match url.split_once(':') {
        Some((host, _)) => host.contains('@') && !host.contains('/'),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// GitServer core types
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GitServer {
    root: Arc<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub branch: String,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub hash: String,
    pub summary: String,
    pub author: String,
    pub date: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchInfo {
    pub current: String,
    pub branches: Vec<String>,
}

// ---------------------------------------------------------------------------
// GitServer impl: core do_* methods
// ---------------------------------------------------------------------------

impl GitServer {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(root),
        }
    }

    pub async fn do_status(&self) -> anyhow::Result<Status> {
        let out = run_git(&self.root, &["status", "--porcelain=v1", "-b"]).await?;
        let mut branch = String::new();
        let mut changes = Vec::new();
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("## ") {
                branch = if let Some(b) = rest.strip_prefix("No commits yet on ") {
                    b.trim().to_string()
                } else {
                    // "main" or "main...origin/main [ahead 1]" -> "main"
                    rest.split([' ', '.']).next().unwrap_or("").to_string()
                };
            } else if line.len() >= 3 {
                let (code, path) = line.split_at(2);
                changes.push(Change {
                    path: path.trim().to_string(),
                    status: code.trim().to_string(),
                });
            }
        }
        Ok(Status { branch, changes })
    }

    pub async fn do_log(&self, max: Option<u32>) -> anyhow::Result<Vec<Commit>> {
        let n = format!("-n{}", max.unwrap_or(20));
        // \x1f field separator, \x1e record separator.
        let fmt = "--format=%H%x1f%s%x1f%an%x1f%aI%x1e";
        let out = match run_git(&self.root, &["log", &n, fmt]).await {
            Ok(o) => o,
            Err(e) => {
                // A fresh repo with no commits → empty log; any other failure propagates.
                if e.to_string().contains("does not have any commits") {
                    return Ok(Vec::new());
                }
                return Err(e);
            }
        };
        let mut commits = Vec::new();
        for record in out.split('\u{1e}') {
            let record = record.trim();
            if record.is_empty() {
                continue;
            }
            let mut f = record.split('\u{1f}');
            commits.push(Commit {
                hash: f.next().unwrap_or("").to_string(),
                summary: f.next().unwrap_or("").to_string(),
                author: f.next().unwrap_or("").to_string(),
                date: f.next().unwrap_or("").to_string(),
            });
        }
        Ok(commits)
    }

    pub async fn do_diff(&self, staged: bool, path: Option<String>) -> anyhow::Result<String> {
        let mut args: Vec<String> = vec!["diff".into()];
        if staged {
            args.push("--cached".into());
        }
        if let Some(p) = path {
            reject_leading_dash(&p, "diff path")?;
            args.push("--".into());
            args.push(p);
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(&self.root, &arg_refs).await
    }

    pub async fn do_add(&self, paths: Vec<String>) -> anyhow::Result<Vec<String>> {
        // Fast-path literal checks: dash injection + known-sensitive marker in the pathspec itself.
        for p in &paths {
            reject_leading_dash(p, "path")?;
            if is_sensitive(p) {
                anyhow::bail!("refusing to stage sensitive path: {p}");
            }
        }
        let mut args: Vec<String> = vec!["add".into(), "--".into()];
        args.extend(paths.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(&self.root, &arg_refs).await?;

        // Authoritative guard: a pathspec like "." or a directory can transitively stage a
        // contained secret whose marker never appeared in the pathspec.  Inspect what was
        // actually staged and refuse (+ best-effort unstage) if any sensitive file snuck in.
        let staged = run_git(&self.root, &["diff", "--cached", "--name-only"]).await?;
        let offending: Vec<String> = staged
            .lines()
            .filter(|f| is_sensitive(f))
            .map(str::to_string)
            .collect();
        if !offending.is_empty() {
            // Best-effort: unstage the offending paths so the secret is not left staged.
            let mut reset: Vec<String> = vec!["reset".into(), "-q".into(), "--".into()];
            reset.extend(offending.iter().cloned());
            let reset_refs: Vec<&str> = reset.iter().map(String::as_str).collect();
            let _ = run_git(&self.root, &reset_refs).await;
            anyhow::bail!("refusing to stage sensitive path(s): {offending:?}");
        }
        Ok(paths)
    }

    pub async fn do_commit(&self, message: String) -> anyhow::Result<String> {
        run_git(&self.root, &["commit", "-m", &message]).await?;
        let hash = run_git(&self.root, &["rev-parse", "HEAD"]).await?;
        Ok(hash.trim().to_string())
    }

    pub async fn do_branch(&self) -> anyhow::Result<BranchInfo> {
        let current = run_git(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        let listing = run_git(&self.root, &["branch", "--format=%(refname:short)"]).await?;
        let branches = listing
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        Ok(BranchInfo {
            current: current.trim().to_string(),
            branches,
        })
    }

    pub async fn do_checkout(&self, name: String, create: bool) -> anyhow::Result<String> {
        reject_leading_dash(&name, "branch name")?;
        if create {
            run_git(&self.root, &["checkout", "-b", &name]).await?;
        } else {
            run_git(&self.root, &["checkout", &name]).await?;
        }
        Ok(name)
    }

    /// Clone `url` into `dir` (default "repo") UNDER root; the target is rejected if it escapes.
    pub async fn do_clone(&self, url: String, dir: Option<String>) -> anyhow::Result<String> {
        let dir = dir.unwrap_or_else(|| "repo".to_string());
        // Containment: no absolute, no parent-dir components.
        let rel = Path::new(&dir);
        if rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::RootDir
                )
            })
        {
            anyhow::bail!("clone target escapes root: {dir}");
        }
        // Argv injection: reject leading-dash on both url and dir.
        reject_leading_dash(&url, "clone url")?;
        reject_leading_dash(&dir, "clone dir")?;
        // Scheme allowlist: blocks ext::, fd::, bare local relative paths, and other
        // transports that could be weaponised (e.g. --upload-pack= via ext::).
        validate_clone_url(&url)?;
        // Use `--` so git treats the next argument as a path/URL, not a flag.
        run_git(&self.root, &["clone", "--", &url, &dir]).await?;
        Ok(dir)
    }

    /// Push to a remote. Manual/credentialed; errors cleanly when no remote/creds are configured.
    pub async fn do_push(
        &self,
        remote: Option<String>,
        branch: Option<String>,
    ) -> anyhow::Result<String> {
        if branch.is_some() && remote.is_none() {
            anyhow::bail!("git.push: `branch` requires `remote`");
        }
        let mut args: Vec<String> = vec!["push".into()];
        if let Some(r) = remote {
            reject_leading_dash(&r, "remote")?;
            args.push(r);
            if let Some(b) = branch {
                reject_leading_dash(&b, "branch")?;
                args.push(b);
            }
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(&self.root, &arg_refs).await
    }

    /// Open a PR via `gh`. Manual/external: requires `gh` installed + authenticated; otherwise
    /// returns an error (no CI test for the success path).
    pub async fn do_pr_open(
        &self,
        title: String,
        body: Option<String>,
        base: Option<String>,
    ) -> anyhow::Result<String> {
        let mut args: Vec<String> = vec!["pr".into(), "create".into(), "--title".into(), title];
        args.push("--body".into());
        args.push(body.unwrap_or_default());
        if let Some(b) = base {
            args.push("--base".into());
            args.push(b);
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_gh(&self.root, &arg_refs).await?;
        // gh prints the PR URL; take the last non-empty line.
        Ok(out
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string())
    }
}

// ---------------------------------------------------------------------------
// Arg structs for rmcp parameter deserialization
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct AddArgs {
    paths: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CommitArgs {
    message: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DiffArgs {
    staged: Option<bool>,
    path: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct LogArgs {
    max: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CheckoutArgs {
    name: String,
    create: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CloneArgs {
    url: String,
    dir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PushArgs {
    remote: Option<String>,
    branch: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PrArgs {
    title: String,
    body: Option<String>,
    base: Option<String>,
}

// ---------------------------------------------------------------------------
// rmcp tool wrappers (thin shims over the do_* methods)
// ---------------------------------------------------------------------------

fn to_err(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

#[tool_router(server_handler)]
impl GitServer {
    #[tool(name = "git.status", description = "Working-tree status")]
    async fn status(&self) -> Result<CallToolResult, ErrorData> {
        let s = self.do_status().await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::to_value(s).map_err(|e| to_err(e.into()))?,
        ))
    }

    #[tool(
        name = "git.diff",
        description = "Show diff of working tree or staged changes"
    )]
    async fn diff(
        &self,
        Parameters(DiffArgs { staged, path }): Parameters<DiffArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let d = self
            .do_diff(staged.unwrap_or(false), path)
            .await
            .map_err(to_err)?;
        Ok(CallToolResult::structured(serde_json::json!({ "diff": d })))
    }

    #[tool(name = "git.log", description = "Show commit log")]
    async fn log(
        &self,
        Parameters(LogArgs { max }): Parameters<LogArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let commits = self.do_log(max).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "commits": commits }),
        ))
    }

    #[tool(
        name = "git.add",
        description = "Stage paths (refuses sensitive paths)"
    )]
    async fn add(
        &self,
        Parameters(AddArgs { paths }): Parameters<AddArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let added = self.do_add(paths).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "added": added }),
        ))
    }

    #[tool(name = "git.commit", description = "Commit staged changes")]
    async fn commit(
        &self,
        Parameters(CommitArgs { message }): Parameters<CommitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let hash = self.do_commit(message).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "hash": hash }),
        ))
    }

    #[tool(
        name = "git.branch",
        description = "List branches and show current branch"
    )]
    async fn branch(&self) -> Result<CallToolResult, ErrorData> {
        let info = self.do_branch().await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::to_value(info).map_err(|e| to_err(e.into()))?,
        ))
    }

    #[tool(name = "git.checkout", description = "Checkout or create a branch")]
    async fn checkout(
        &self,
        Parameters(CheckoutArgs { name, create }): Parameters<CheckoutArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let branch = self
            .do_checkout(name, create.unwrap_or(false))
            .await
            .map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "branch": branch }),
        ))
    }

    #[tool(
        name = "git.clone",
        description = "Clone a repository into a subdirectory of root"
    )]
    async fn clone(
        &self,
        Parameters(CloneArgs { url, dir }): Parameters<CloneArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = self.do_clone(url, dir).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "path": path }),
        ))
    }

    #[tool(
        name = "git.push",
        description = "Push to a remote (requires credentials)"
    )]
    async fn push(
        &self,
        Parameters(PushArgs { remote, branch }): Parameters<PushArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let output = self.do_push(remote, branch).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "output": output }),
        ))
    }

    #[tool(
        name = "git.pr_open",
        description = "Open a pull request via gh (requires gh installed + authenticated)"
    )]
    async fn pr_open(
        &self,
        Parameters(PrArgs { title, body, base }): Parameters<PrArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self.do_pr_open(title, body, base).await.map_err(to_err)?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "url": url }),
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
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-git <root>"))?;
    let server = GitServer::new(root);
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

    /// Create a tempdir, `git init`, set isolated local identity, return (dir, server).
    async fn repo() -> (tempfile::TempDir, GitServer) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        run_git(&root, &["init", "-q"]).await.unwrap();
        run_git(&root, &["config", "user.name", "Test"])
            .await
            .unwrap();
        run_git(&root, &["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        run_git(&root, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
        let server = GitServer::new(root);
        (dir, server)
    }

    async fn write_file(dir: &tempfile::TempDir, rel: &str, contents: &str) {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    // --- Task 2 tests ---

    #[tokio::test]
    async fn status_reflects_a_new_file() {
        let (dir, server) = repo().await;
        write_file(&dir, "a.txt", "hi\n").await;
        let st = server.do_status().await.unwrap();
        assert!(st.changes.iter().any(|c| c.path == "a.txt"));
    }

    #[tokio::test]
    async fn log_returns_seeded_commit() {
        let (dir, server) = repo().await;
        write_file(&dir, "a.txt", "hi\n").await;
        server.do_add(vec!["a.txt".into()]).await.unwrap();
        let hash = server.do_commit("seed".into()).await.unwrap();
        let commits = server.do_log(Some(10)).await.unwrap();
        assert_eq!(commits.len(), 1);
        assert!(
            hash.starts_with(&commits[0].hash[..7.min(commits[0].hash.len())])
                || commits[0].hash == hash
        );
        assert_eq!(commits[0].summary, "seed");
    }

    #[tokio::test]
    async fn diff_shows_a_change() {
        let (dir, server) = repo().await;
        write_file(&dir, "a.txt", "one\n").await;
        server.do_add(vec!["a.txt".into()]).await.unwrap();
        server.do_commit("c1".into()).await.unwrap();
        write_file(&dir, "a.txt", "two\n").await;
        let d = server.do_diff(false, None).await.unwrap();
        assert!(d.contains("two"));
    }

    // --- Task 3 tests ---

    #[tokio::test]
    async fn add_then_commit_makes_a_commit() {
        let (dir, server) = repo().await;
        write_file(&dir, "a.txt", "hi\n").await;
        let added = server.do_add(vec!["a.txt".into()]).await.unwrap();
        assert_eq!(added, vec!["a.txt".to_string()]);
        let hash = server.do_commit("msg".into()).await.unwrap();
        assert!(!hash.is_empty());
        let st = server.do_status().await.unwrap();
        assert!(st.changes.is_empty(), "working tree clean after commit");
    }

    #[tokio::test]
    async fn add_refuses_sensitive_path() {
        let (dir, server) = repo().await;
        write_file(&dir, ".env", "SECRET=x\n").await;
        let err = server.do_add(vec![".env".into()]).await;
        assert!(err.is_err(), "staging a sensitive path must be refused");
        // Nothing staged: status still shows .env as untracked, not staged.
        let st = server.do_status().await.unwrap();
        assert!(
            st.changes
                .iter()
                .any(|c| c.path == ".env" && c.status.contains('?'))
        );
    }

    // --- Task 4 tests ---

    #[tokio::test]
    async fn branch_create_and_checkout() {
        let (dir, server) = repo().await;
        write_file(&dir, "a.txt", "hi\n").await;
        server.do_add(vec!["a.txt".into()]).await.unwrap();
        server.do_commit("c1".into()).await.unwrap();

        let b = server.do_checkout("feature".into(), true).await.unwrap();
        assert_eq!(b, "feature");
        let info = server.do_branch().await.unwrap();
        assert_eq!(info.current, "feature");
        assert!(info.branches.contains(&"feature".to_string()));
    }

    // --- Task 5 tests ---

    #[tokio::test]
    async fn clone_from_local_bare_remote() {
        // Build a source repo with a commit, then a bare clone of it to serve as the remote.
        let (src_dir, src) = repo().await;
        write_file(&src_dir, "a.txt", "hi\n").await;
        src.do_add(vec!["a.txt".into()]).await.unwrap();
        src.do_commit("c1".into()).await.unwrap();
        let bare = tempfile::tempdir().unwrap();
        run_git(
            bare.path(),
            &["clone", "--bare", src_dir.path().to_str().unwrap(), "."],
        )
        .await
        .unwrap();
        let bare_url = format!("file://{}", bare.path().display());

        // Clone into a subdir of a fresh root.
        let dest_root = tempfile::tempdir().unwrap();
        let server = GitServer::new(dest_root.path().to_path_buf());
        let path = server
            .do_clone(bare_url, Some("checkout".into()))
            .await
            .unwrap();
        assert_eq!(path, "checkout");
        assert!(dest_root.path().join("checkout/a.txt").exists());
    }

    #[tokio::test]
    async fn clone_rejects_escaping_dir() {
        let dir = tempfile::tempdir().unwrap();
        let server = GitServer::new(dir.path().to_path_buf());
        assert!(
            server
                .do_clone("file:///nope".into(), Some("../escape".into()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn push_without_remote_errors_gracefully() {
        let (_dir, server) = repo().await;
        // No remote configured → push errors (not a panic).
        assert!(server.do_push(None, None).await.is_err());
    }

    // --- Security hardening tests ---

    #[tokio::test]
    async fn clone_rejects_flag_and_bad_scheme_urls() {
        let dir = tempfile::tempdir().unwrap();
        let server = GitServer::new(dir.path().to_path_buf());
        // Leading-dash url → argv flag injection.
        assert!(
            server
                .do_clone("--upload-pack=touch /tmp/pwn".into(), Some("d".into()))
                .await
                .is_err()
        );
        // ext:: transport → RCE via custom helper.
        assert!(
            server
                .do_clone("ext::sh -c id".into(), Some("d".into()))
                .await
                .is_err()
        );
        // Valid url but leading-dash dir → argv injection via dir positional.
        assert!(
            server
                .do_clone("https://example.com/r.git".into(), Some("-x".into()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn add_directory_containing_secret_is_refused() {
        let (dir, server) = repo().await;
        write_file(&dir, "src/main.rs", "fn main() {}\n").await;
        write_file(&dir, "src/.env", "SECRET=x\n").await;
        // Staging the directory would transitively stage src/.env — must be refused.
        assert!(
            server.do_add(vec!["src".into()]).await.is_err(),
            "staging a directory containing a secret must be refused"
        );
        // src/.env must not be left staged after the refusal.
        let staged = run_git(dir.path(), &["diff", "--cached", "--name-only"])
            .await
            .unwrap();
        assert!(
            !staged.contains(".env"),
            "secret must not remain staged after refusal"
        );
    }

    #[tokio::test]
    async fn checkout_and_push_reject_leading_dash() {
        let (_d, server) = repo().await;
        // Leading-dash branch name → git would reparse as a flag (e.g. --orphan).
        assert!(server.do_checkout("--orphan".into(), false).await.is_err());
        // Leading-dash remote → would be reparsed as a flag (e.g. --exec=sh -c id).
        assert!(
            server
                .do_push(Some("--exec=sh -c id".into()), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn diff_refuses_leading_dash_path() {
        let (_d, server) = repo().await;
        assert!(server.do_diff(false, Some("-x".into())).await.is_err());
    }

    #[tokio::test]
    async fn status_branch_on_fresh_repo() {
        let (_d, server) = repo().await;
        let st = server.do_status().await.unwrap();
        // Whatever the default branch is, it must not be the bogus "No".
        assert!(
            !st.branch.is_empty() && st.branch != "No",
            "got branch {:?}",
            st.branch
        );
    }
}
