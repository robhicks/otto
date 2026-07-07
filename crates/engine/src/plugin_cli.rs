//! `otto plugin ...` — the CLI-operator-only marketplace/plugin install action. Clones and
//! updates Claude-Code-compatible plugin marketplaces under `~/.claude/plugins/marketplaces/`,
//! tracks them in a lockfile, and flips the `enabledPlugins` allowlist in `~/.claude/settings.json`.
//! This never runs mid-turn and never routes through `ToolRegistry`/`PermissionGate` — those gate
//! *agent-initiated* tool calls during a session; this is an operator running a command before any
//! session exists, exactly like running `git clone` by hand.
//!
//! Git-URL hardening (`validate_clone_url`/`reject_leading_dash`/`is_scp_like`) is duplicated from
//! `crates/mcp-git/src/main.rs` rather than shared: `mcp-git` is a `[[bin]]`-only crate per
//! `CLAUDE.md`'s architecture rule that MCP tool crates are standalone binaries the engine only
//! talks to over stdio, never by linking.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Git hardening (duplicated from crates/mcp-git/src/main.rs — see module doc)
// ---------------------------------------------------------------------------

/// Reject any user-supplied positional that starts with `-`. A leading dash is reparsed by git as
/// a flag, enabling argv flag-smuggling attacks such as `git clone --upload-pack=<cmd>`.
fn reject_leading_dash(value: &str, what: &str) -> anyhow::Result<()> {
    if value.starts_with('-') {
        anyhow::bail!("invalid {what}: must not start with '-': {value}");
    }
    Ok(())
}

/// Allow only well-known URL schemes for `git clone`. Blocks `ext::`, `fd::`, bare relative paths,
/// and any other transport that could be weaponised.
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

/// `user@host:path` scp-like SSH syntax (no scheme): a `:` whose left side has no `/` and
/// contains `@`.
fn is_scp_like(url: &str) -> bool {
    match url.split_once(':') {
        Some((host, _)) => host.contains('@') && !host.contains('/'),
        None => false,
    }
}

/// A marketplace name must be a single path-safe component: non-empty, no `/`/`\`, and not `.`
/// or `..`. Applied to the `name` field read out of a *cloned* `marketplace.json` before it is
/// used as a directory name — guards against a malformed/malicious manifest trying to escape
/// `~/.claude/plugins/marketplaces/` (e.g. `"name": "../../etc"`).
fn validate_marketplace_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        anyhow::bail!("invalid marketplace name: {name:?}");
    }
    if name.contains('/') || name.contains('\\') {
        anyhow::bail!("invalid marketplace name (must be a single path component): {name:?}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path helpers (home is always an explicit parameter — hermetic, testable)
// ---------------------------------------------------------------------------

fn marketplaces_dir(home: &Path) -> PathBuf {
    home.join(".claude").join("plugins").join("marketplaces")
}

fn lockfile_path(home: &Path) -> PathBuf {
    home.join(".claude")
        .join("plugins")
        .join("marketplaces.lock.json")
}

fn settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs()
}

// ---------------------------------------------------------------------------
// Lockfile read/write (thin I/O wrapper around otto_extensions::MarketplaceLockfile)
// ---------------------------------------------------------------------------

fn read_lockfile(home: &Path) -> otto_extensions::MarketplaceLockfile {
    match std::fs::read_to_string(lockfile_path(home)) {
        Ok(text) => otto_extensions::MarketplaceLockfile::parse(&text),
        Err(_) => otto_extensions::MarketplaceLockfile::default(),
    }
}

fn write_lockfile(home: &Path, lock: &otto_extensions::MarketplaceLockfile) -> anyhow::Result<()> {
    let path = lockfile_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, lock.to_json())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// run_git (duplicated shape from crates/mcp-git/src/main.rs's run_git)
// ---------------------------------------------------------------------------

async fn run_git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(cwd)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_clone_url_accepts_known_schemes_and_scp_like() {
        assert!(validate_clone_url("https://example.com/r.git").is_ok());
        assert!(validate_clone_url("http://example.com/r.git").is_ok());
        assert!(validate_clone_url("ssh://git@example.com/r.git").is_ok());
        assert!(validate_clone_url("file:///tmp/r").is_ok());
        assert!(validate_clone_url("git@github.com:acme/r.git").is_ok());
    }

    #[test]
    fn validate_clone_url_rejects_bad_schemes_and_flag_injection() {
        assert!(validate_clone_url("ext::sh -c id").is_err());
        assert!(validate_clone_url("fd::0").is_err());
        assert!(validate_clone_url("./relative/path").is_err());
        assert!(validate_clone_url("--upload-pack=touch /tmp/pwn").is_err());
    }

    #[test]
    fn reject_leading_dash_rejects_dash_prefixed_values() {
        assert!(reject_leading_dash("main", "ref").is_ok());
        assert!(reject_leading_dash("-x", "ref").is_err());
        assert!(reject_leading_dash("--exec=sh -c id", "ref").is_err());
    }

    #[test]
    fn validate_marketplace_name_rejects_path_escape_attempts() {
        assert!(validate_marketplace_name("acme").is_ok());
        assert!(validate_marketplace_name("").is_err());
        assert!(validate_marketplace_name(".").is_err());
        assert!(validate_marketplace_name("..").is_err());
        assert!(validate_marketplace_name("../../etc").is_err());
        assert!(validate_marketplace_name("a/b").is_err());
    }

    #[tokio::test]
    async fn run_git_reports_stderr_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_git(dir.path(), &["not-a-real-git-subcommand"])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("git"));
    }

    #[tokio::test]
    async fn lockfile_round_trips_through_disk() {
        let home = tempfile::tempdir().unwrap();
        let mut lock = otto_extensions::MarketplaceLockfile::default();
        lock.entries.insert(
            "acme".to_string(),
            otto_extensions::MarketplaceLock {
                url: "https://example.com/acme.git".to_string(),
                git_ref: "main".to_string(),
                commit: "abc123".to_string(),
                updated_at_unix: now_unix(),
            },
        );
        write_lockfile(home.path(), &lock).unwrap();
        let back = read_lockfile(home.path());
        assert_eq!(back, lock);
    }

    #[test]
    fn read_lockfile_missing_file_is_empty() {
        let home = tempfile::tempdir().unwrap();
        assert!(read_lockfile(home.path()).entries.is_empty());
    }
}
