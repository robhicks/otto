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
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Lockfile read/write (thin I/O wrapper around otto_extensions::MarketplaceLockfile)
// ---------------------------------------------------------------------------

fn read_lockfile(home: &Path) -> otto_extensions::MarketplaceLockfile {
    let path = lockfile_path(home);
    match std::fs::read_to_string(&path) {
        Ok(text) => otto_extensions::MarketplaceLockfile::parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            otto_extensions::MarketplaceLockfile::default()
        }
        Err(e) => {
            eprintln!(
                "warning: skipping unreadable lockfile {}: {e}",
                path.display()
            );
            otto_extensions::MarketplaceLockfile::default()
        }
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

/// Clone `url` into a staging directory under `~/.claude/plugins/marketplaces/`, verify it
/// contains a valid `marketplace.json`, then move it into place at
/// `~/.claude/plugins/marketplaces/<name-from-marketplace.json>/` (the directory name is always
/// the marketplace's own declared `name` — never a user-supplied alias — so it always matches the
/// `"<plugin>@<marketplace>"` enable-key namespace `otto_extensions::discover` uses). Records the
/// result in the lockfile. Returns the resolved marketplace name.
///
/// `ref_` optionally pins a branch/tag/sha (checked out after clone); omitted, the clone's default
/// branch is recorded as-is. Cleans up the staging directory on any failure — never leaves a
/// partial marketplace directory behind, and any failure after the rename (resolving the ref,
/// reading the commit, or writing the lockfile) removes the newly-installed final directory too,
/// so a failed `marketplace_add` never leaves a "phantom installed" marketplace with no lockfile
/// entry.
///
/// Note: `url` is stored verbatim in the lockfile — avoid embedding credentials
/// (e.g. `https://user:token@host/...`) in the URL you pass here.
pub async fn marketplace_add(url: &str, ref_: Option<&str>, home: &Path) -> anyhow::Result<String> {
    validate_clone_url(url)?;
    if let Some(r) = ref_ {
        reject_leading_dash(r, "ref")?;
    }

    let mp_root = marketplaces_dir(home);
    std::fs::create_dir_all(&mp_root)?;

    let staging_name = format!(".staging-{}-{}", std::process::id(), uuid::Uuid::new_v4());
    let staging_path = mp_root.join(&staging_name);
    if staging_path.exists() {
        std::fs::remove_dir_all(&staging_path)?;
    }

    // `git clone -- <url> <staging_name>`, cwd = mp_root, mirrors mcp-git's do_clone shape.
    run_git(&mp_root, &["clone", "--", url, &staging_name]).await?;

    let cleanup_and_err = |e: anyhow::Error| {
        let _ = std::fs::remove_dir_all(&staging_path);
        Err(e)
    };

    if let Some(r) = ref_ {
        if let Err(e) = run_git(&staging_path, &["checkout", r]).await {
            return cleanup_and_err(e);
        }
    }

    let manifest_path = staging_path.join(".claude-plugin").join("marketplace.json");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => {
            return cleanup_and_err(anyhow::anyhow!(
                "cloned repository has no {} (not a valid marketplace): {e}",
                manifest_path.display()
            ));
        }
    };
    let mp = match otto_extensions::parse_marketplace_json(&text) {
        Ok(mp) => mp,
        Err(e) => return cleanup_and_err(anyhow::anyhow!("invalid marketplace.json: {e}")),
    };

    if let Err(e) = validate_marketplace_name(&mp.name) {
        return cleanup_and_err(e);
    }

    let final_path = mp_root.join(&mp.name);
    if final_path.exists() {
        return cleanup_and_err(anyhow::anyhow!(
            "marketplace '{}' already installed; use `otto plugin marketplace update {}`",
            mp.name,
            mp.name
        ));
    }
    if let Err(e) = std::fs::rename(&staging_path, &final_path) {
        return cleanup_and_err(anyhow::anyhow!("failed to install marketplace: {e}"));
    }

    // Past this point `staging_path` no longer exists — `final_path` is the installed directory.
    // Any failure from here on must remove `final_path` (not `staging_path`) before returning, so
    // a failed install never leaves a "phantom installed" marketplace with no lockfile entry.
    let resolved: anyhow::Result<(String, String)> = async {
        let resolved_ref = match ref_ {
            Some(r) => r.to_string(),
            None => run_git(&final_path, &["rev-parse", "--abbrev-ref", "HEAD"])
                .await?
                .trim()
                .to_string(),
        };
        let commit = run_git(&final_path, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_string();
        Ok((resolved_ref, commit))
    }
    .await;

    let (resolved_ref, commit) = match resolved {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&final_path);
            return Err(e);
        }
    };

    let mut lock = read_lockfile(home);
    lock.entries.insert(
        mp.name.clone(),
        otto_extensions::MarketplaceLock {
            url: url.to_string(),
            git_ref: resolved_ref,
            commit,
            updated_at_unix: now_unix(),
        },
    );
    if let Err(e) = write_lockfile(home, &lock) {
        let _ = std::fs::remove_dir_all(&final_path);
        return Err(e);
    }

    Ok(mp.name)
}

/// Delete `~/.claude/plugins/marketplaces/<name>/` and its lockfile entry. Does **not** scrub any
/// `enabledPlugins` keys referencing this marketplace — a stale key simply becomes inert on the
/// next `discover()` (no matching directory to fold), a deliberate simplification over a
/// cross-cutting `settings.json` cleanup.
pub fn marketplace_remove(name: &str, home: &Path) -> anyhow::Result<()> {
    let mp_dir = marketplaces_dir(home).join(name);
    if !mp_dir.exists() {
        anyhow::bail!("marketplace '{name}' is not installed");
    }
    std::fs::remove_dir_all(&mp_dir)?;

    let mut lock = read_lockfile(home);
    lock.entries.remove(name);
    write_lockfile(home, &lock)?;
    Ok(())
}

/// Refresh one (`Some(name)`) or every (`None`) locked marketplace: `git fetch origin`, then try
/// fast-forwarding a branch ref (`reset --hard origin/<ref>`); if that fails (the ref is a pinned
/// tag/sha, not a remote-tracking branch), fall back to a direct `checkout <ref>` — a no-op
/// fetch-and-confirm for a pin that hasn't moved. Refreshes `commit`/`updated_at_unix` in the
/// lockfile. A marketplace whose directory has gone missing out from under the lockfile is
/// reported and skipped, never fatal to the rest of the batch. Returns the names actually updated.
pub async fn marketplace_update(name: Option<&str>, home: &Path) -> anyhow::Result<Vec<String>> {
    let mut lock = read_lockfile(home);

    let names: Vec<String> = match name {
        Some(n) => {
            if !lock.entries.contains_key(n) {
                anyhow::bail!("marketplace '{n}' is not installed");
            }
            vec![n.to_string()]
        }
        None => lock.entries.keys().cloned().collect(),
    };

    let mut updated = Vec::new();
    for n in &names {
        let entry = lock.entries.get(n).expect("checked above").clone();
        let mp_dir = marketplaces_dir(home).join(n);
        if !mp_dir.is_dir() {
            eprintln!(
                "warning: marketplace '{n}' is locked but {} is missing; skipping",
                mp_dir.display()
            );
            continue;
        }

        if let Err(e) = run_git(&mp_dir, &["fetch", "origin"]).await {
            eprintln!("warning: failed to fetch '{n}': {e}; skipping");
            continue;
        }

        let branch_reset = run_git(
            &mp_dir,
            &["reset", "--hard", &format!("origin/{}", entry.git_ref)],
        )
        .await;
        if branch_reset.is_err() {
            reject_leading_dash(&entry.git_ref, "ref")?;
            run_git(&mp_dir, &["checkout", &entry.git_ref]).await?;
        }

        let commit = run_git(&mp_dir, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_string();
        lock.entries.insert(
            n.clone(),
            otto_extensions::MarketplaceLock {
                commit,
                updated_at_unix: now_unix(),
                ..entry
            },
        );
        updated.push(n.clone());
    }

    write_lockfile(home, &lock)?;
    Ok(updated)
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

    async fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]).await.unwrap();
        run_git(dir, &["config", "user.name", "Test"])
            .await
            .unwrap();
        run_git(dir, &["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        run_git(dir, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
    }

    /// Build a source repo containing a `.claude-plugin/marketplace.json` (declaring internal
    /// name `mp_name`, offering one `LocalPath` plugin `plugin_name`), commit it, then a bare
    /// clone to serve as a `file://` remote. Returns `(src_dir, bare_dir, bare_url)` — both
    /// tempdirs must stay alive for the URL to remain valid.
    async fn bare_marketplace_remote(
        mp_name: &str,
        plugin_name: &str,
    ) -> (tempfile::TempDir, tempfile::TempDir, String) {
        let src = tempfile::tempdir().unwrap();
        init_repo(src.path()).await;
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            format!(
                r#"{{"name":"{mp_name}","plugins":[{{"name":"{plugin_name}","source":"./plugins/{plugin_name}"}}]}}"#
            ),
        )
        .unwrap();
        let proot = src.path().join("plugins").join(plugin_name);
        std::fs::create_dir_all(proot.join(".claude-plugin")).unwrap();
        std::fs::write(
            proot.join(".claude-plugin").join("plugin.json"),
            format!(r#"{{"name":"{plugin_name}"}}"#),
        )
        .unwrap();
        run_git(src.path(), &["add", "-A"]).await.unwrap();
        run_git(src.path(), &["commit", "-m", "seed"])
            .await
            .unwrap();

        let bare = tempfile::tempdir().unwrap();
        run_git(
            bare.path(),
            &["clone", "--bare", src.path().to_str().unwrap(), "."],
        )
        .await
        .unwrap();
        let url = format!("file://{}", bare.path().display());
        (src, bare, url)
    }

    #[tokio::test]
    async fn marketplace_add_clones_and_locks_by_declared_name() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();

        let name = marketplace_add(&url, None, home.path()).await.unwrap();
        assert_eq!(name, "acme");

        let mp_dir = marketplaces_dir(home.path()).join("acme");
        assert!(
            mp_dir
                .join(".claude-plugin")
                .join("marketplace.json")
                .exists()
        );
        assert!(mp_dir.join("plugins").join("foo").exists());

        let lock = read_lockfile(home.path());
        let entry = lock.entries.get("acme").expect("locked");
        assert_eq!(entry.url, url);
        assert_eq!(entry.git_ref, "main");
        assert!(!entry.commit.is_empty());
    }

    #[tokio::test]
    async fn marketplace_add_rejects_duplicate_name() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();

        let err = marketplace_add(&url, None, home.path()).await.unwrap_err();
        assert!(err.to_string().contains("already installed"), "got: {err}");
    }

    #[tokio::test]
    async fn marketplace_add_with_explicit_ref() {
        let (src, _bare, _url) = bare_marketplace_remote("acme", "foo").await;
        // Create a second commit + tag on the source, then re-derive the bare remote so the tag
        // is present in it too.
        std::fs::write(src.path().join("extra.txt"), "x").unwrap();
        run_git(src.path(), &["add", "-A"]).await.unwrap();
        run_git(src.path(), &["commit", "-m", "second"])
            .await
            .unwrap();
        run_git(src.path(), &["tag", "v1"]).await.unwrap();
        let bare2 = tempfile::tempdir().unwrap();
        run_git(
            bare2.path(),
            &["clone", "--bare", src.path().to_str().unwrap(), "."],
        )
        .await
        .unwrap();
        let url2 = format!("file://{}", bare2.path().display());

        let home = tempfile::tempdir().unwrap();
        let name = marketplace_add(&url2, Some("v1"), home.path())
            .await
            .unwrap();
        assert_eq!(name, "acme");
        let lock = read_lockfile(home.path());
        assert_eq!(lock.entries.get("acme").unwrap().git_ref, "v1");
    }

    #[tokio::test]
    async fn marketplace_add_cleans_up_on_missing_marketplace_json() {
        let src = tempfile::tempdir().unwrap();
        init_repo(src.path()).await;
        std::fs::write(src.path().join("readme.txt"), "no marketplace.json here").unwrap();
        run_git(src.path(), &["add", "-A"]).await.unwrap();
        run_git(src.path(), &["commit", "-m", "seed"])
            .await
            .unwrap();
        let bare = tempfile::tempdir().unwrap();
        run_git(
            bare.path(),
            &["clone", "--bare", src.path().to_str().unwrap(), "."],
        )
        .await
        .unwrap();
        let url = format!("file://{}", bare.path().display());

        let home = tempfile::tempdir().unwrap();
        let err = marketplace_add(&url, None, home.path()).await.unwrap_err();
        assert!(err.to_string().contains("marketplace.json"), "got: {err}");
        // No leftover staging directory under marketplaces/.
        let mp_root = marketplaces_dir(home.path());
        let leftovers: Vec<_> = std::fs::read_dir(&mp_root)
            .map(|it| it.flatten().collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "expected no leftover dirs, found: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn marketplace_add_rejects_bad_scheme_url() {
        let home = tempfile::tempdir().unwrap();
        let err = marketplace_add("ext::sh -c id", None, home.path())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unsupported clone url"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn marketplace_add_rejects_leading_dash_ref() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        let err = marketplace_add(&url, Some("--exec=sh -c id"), home.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid ref"), "got: {err}");
    }

    #[tokio::test]
    async fn marketplace_add_rejects_path_unsafe_declared_name() {
        // The marketplace.json's OWN "name" field attempts a path escape.
        let src = tempfile::tempdir().unwrap();
        init_repo(src.path()).await;
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"../evil","plugins":[]}"#,
        )
        .unwrap();
        run_git(src.path(), &["add", "-A"]).await.unwrap();
        run_git(src.path(), &["commit", "-m", "seed"])
            .await
            .unwrap();
        let bare = tempfile::tempdir().unwrap();
        run_git(
            bare.path(),
            &["clone", "--bare", src.path().to_str().unwrap(), "."],
        )
        .await
        .unwrap();
        let url = format!("file://{}", bare.path().display());

        let home = tempfile::tempdir().unwrap();
        let err = marketplace_add(&url, None, home.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("invalid marketplace name"),
            "got: {err}"
        );
    }

    /// Fix-2 regression test: a failure *after* the rename (here, `write_lockfile` failing
    /// because its parent directory is read-only) must remove the newly-installed `final_path`
    /// rather than leaving a "phantom installed" marketplace directory with no lockfile entry.
    ///
    /// Uses Unix permission bits (`chmod 0o555` on the `plugins/` dir, which owns
    /// `marketplaces.lock.json`) to force `std::fs::write` to fail with `EACCES` without touching
    /// any code path — `marketplaces/` itself stays writable so the clone/rename steps that
    /// precede the lockfile write still succeed. This is Unix-only (permission bits don't carry
    /// the same meaning on Windows), matching the sandbox/CI target for this crate.
    #[tokio::test]
    #[cfg(unix)]
    async fn marketplace_add_removes_final_dir_when_lockfile_write_fails_after_rename() {
        use std::os::unix::fs::PermissionsExt;

        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();

        // Pre-create marketplaces/ (writable) so create_dir_all is a no-op and the clone/rename
        // steps still have write access to it, then lock down its parent plugins/ so the
        // lockfile write (a sibling file of marketplaces/) fails with permission denied.
        let mp_root = marketplaces_dir(home.path());
        std::fs::create_dir_all(&mp_root).unwrap();
        let plugins_dir = home.path().join(".claude").join("plugins");
        std::fs::set_permissions(&plugins_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = marketplace_add(&url, None, home.path()).await;

        // Restore write permission before any tempdir cleanup runs (Drop needs to remove files
        // under plugins/), regardless of the assertion outcome below.
        std::fs::set_permissions(&plugins_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("permission denied")
                || err.to_string().contains("os error 13"),
            "expected a permission-denied error, got: {err}"
        );

        let final_path = mp_root.join("acme");
        assert!(
            !final_path.exists(),
            "final_path should have been removed after the post-rename failure, but exists: {}",
            final_path.display()
        );
        // No lockfile entry either — the failure was genuinely all-or-nothing.
        let lock = read_lockfile(home.path());
        assert!(!lock.entries.contains_key("acme"));
    }

    #[tokio::test]
    async fn marketplace_remove_deletes_dir_and_lock_entry() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();

        marketplace_remove("acme", home.path()).unwrap();

        assert!(!marketplaces_dir(home.path()).join("acme").exists());
        assert!(!read_lockfile(home.path()).entries.contains_key("acme"));
    }

    /// Compile-only placeholder for the real `plugin_install` landing in Task 6. `#[ignore]`
    /// only skips *running* a test, not compiling it, so this crate-local stub (shadowing the
    /// `use super::*;` glob for the one caller below) keeps the crate building without
    /// implementing the real function here — its body is never reached since the sole caller is
    /// `#[ignore]`d.
    #[cfg(test)]
    fn plugin_install(_spec: &str, _home: &Path) -> anyhow::Result<()> {
        unimplemented!("plugin_install lands in Task 6")
    }

    #[tokio::test]
    #[ignore = "plugin_install lands in Task 6"]
    async fn marketplace_remove_leaves_stale_enabled_plugins_key_alone() {
        // Documented limitation: remove does not scrub settings.json.
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();
        plugin_install("foo@acme", home.path()).unwrap();

        marketplace_remove("acme", home.path()).unwrap();

        let settings = std::fs::read_to_string(settings_path(home.path())).unwrap();
        let enabled = otto_extensions::parse_enabled_plugins(&settings);
        assert_eq!(
            enabled.get("foo@acme"),
            Some(&true),
            "stale key must remain (documented limitation)"
        );
    }

    #[test]
    fn marketplace_remove_unknown_name_errors() {
        let home = tempfile::tempdir().unwrap();
        let err = marketplace_remove("nope", home.path()).unwrap_err();
        assert!(err.to_string().contains("not installed"), "got: {err}");
    }

    #[tokio::test]
    async fn marketplace_update_pulls_new_commit_and_refreshes_lock() {
        let (src, bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();
        let before = read_lockfile(home.path())
            .entries
            .get("acme")
            .unwrap()
            .commit
            .clone();

        // Push a new commit to the source, then mirror it into the bare remote.
        std::fs::write(src.path().join("new.txt"), "x").unwrap();
        run_git(src.path(), &["add", "-A"]).await.unwrap();
        run_git(src.path(), &["commit", "-m", "more"])
            .await
            .unwrap();
        run_git(
            src.path(),
            &[
                "push",
                bare.path().to_str().unwrap(),
                "main:main",
                "--force",
            ],
        )
        .await
        .unwrap();

        let updated = marketplace_update(Some("acme"), home.path()).await.unwrap();
        assert_eq!(updated, vec!["acme".to_string()]);
        let after = read_lockfile(home.path())
            .entries
            .get("acme")
            .unwrap()
            .commit
            .clone();
        assert_ne!(before, after);
        assert!(
            marketplaces_dir(home.path())
                .join("acme")
                .join("new.txt")
                .exists()
        );
    }

    #[tokio::test]
    async fn marketplace_update_all_when_name_omitted() {
        let (_src1, _bare1, url1) = bare_marketplace_remote("acme", "foo").await;
        let (_src2, _bare2, url2) = bare_marketplace_remote("beta", "bar").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url1, None, home.path()).await.unwrap();
        marketplace_add(&url2, None, home.path()).await.unwrap();

        let mut updated = marketplace_update(None, home.path()).await.unwrap();
        updated.sort();
        assert_eq!(updated, vec!["acme".to_string(), "beta".to_string()]);
    }

    #[tokio::test]
    async fn marketplace_update_unknown_name_errors() {
        let home = tempfile::tempdir().unwrap();
        let err = marketplace_update(Some("nope"), home.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not installed"), "got: {err}");
    }
}
