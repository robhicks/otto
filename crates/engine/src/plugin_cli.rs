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

/// A path component must be a single safe segment: non-empty, no `/`/`\`, and not `.`/`..`.
/// Applied to any name read out of a manifest or key before it is used as a directory component,
/// guarding against escapes like `"../../etc"`.
fn validate_path_component(value: &str, what: &str) -> anyhow::Result<()> {
    if value.is_empty() || value == "." || value == ".." {
        anyhow::bail!("invalid {what}: {value:?}");
    }
    if value.contains('/') || value.contains('\\') {
        anyhow::bail!("invalid {what} (must be a single path component): {value:?}");
    }
    Ok(())
}

/// A marketplace name must be a single path-safe component. Applied to the `name` field read
/// out of a *cloned* `marketplace.json` before it is used as a directory name — guards against
/// a malformed/malicious manifest trying to escape `~/.claude/plugins/marketplaces/` (e.g.
/// `"name": "../../etc"`).
fn validate_marketplace_name(name: &str) -> anyhow::Result<()> {
    validate_path_component(name, "marketplace name")
}

// ---------------------------------------------------------------------------
// Path helpers (home is always an explicit parameter — hermetic, testable)
// ---------------------------------------------------------------------------

fn marketplaces_dir(home: &Path) -> PathBuf {
    home.join(".claude").join("plugins").join("marketplaces")
}

fn repos_dir(home: &Path) -> PathBuf {
    home.join(".claude").join("plugins").join("repos")
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

pub(crate) fn read_lockfile(home: &Path) -> otto_extensions::MarketplaceLockfile {
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

/// Delete `~/.claude/plugins/marketplaces/<name>/` and its lockfile entry, along with any
/// materialized remote-plugin clones under `~/.claude/plugins/repos/<name>/` and their `plugins`
/// lock entries — so removal leaves no orphaned clones or stale rows. Does **not** scrub any
/// `enabledPlugins` keys referencing this marketplace — a stale key simply becomes inert on the
/// next `discover()` (no matching directory to fold), a deliberate simplification over a
/// cross-cutting `settings.json` cleanup.
pub fn marketplace_remove(name: &str, home: &Path) -> anyhow::Result<()> {
    let mut lock = read_lockfile(home);
    if !lock.entries.contains_key(name) {
        anyhow::bail!("marketplace '{name}' is not installed");
    }

    // Defense-in-depth: guard both recursive deletes against a hand-tampered lockfile key.
    validate_path_component(name, "marketplace name")?;

    let mp_dir = marketplaces_dir(home).join(name);
    if mp_dir.exists() {
        std::fs::remove_dir_all(&mp_dir)?;
    }

    // Also remove any materialized remote-plugin clones + their lock entries for this marketplace,
    // so removal leaves no orphaned clones or stale rows.
    let repos_mp = repos_dir(home).join(name);
    if repos_mp.exists() {
        std::fs::remove_dir_all(&repos_mp)?;
    }
    // Keys are "<plugin>@<marketplace>"; the marketplace is everything after the first '@'.
    lock.plugins
        .retain(|k, _| k.split_once('@').map(|(_, mp)| mp) != Some(name));

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

/// Split `"<plugin>@<marketplace>"` into its two non-empty parts.
fn split_plugin_key(key: &str) -> anyhow::Result<(String, String)> {
    let (plugin, marketplace) = key
        .split_once('@')
        .ok_or_else(|| anyhow::anyhow!("expected '<plugin>@<marketplace>', got '{key}'"))?;
    if plugin.is_empty() || marketplace.is_empty() {
        anyhow::bail!("expected '<plugin>@<marketplace>', got '{key}'");
    }
    Ok((plugin.to_string(), marketplace.to_string()))
}

fn read_marketplace_manifest(
    home: &Path,
    marketplace: &str,
) -> anyhow::Result<otto_extensions::Marketplace> {
    let lock = read_lockfile(home);
    if !lock.entries.contains_key(marketplace) {
        anyhow::bail!("marketplace '{marketplace}' is not installed");
    }

    let manifest_path = marketplaces_dir(home)
        .join(marketplace)
        .join(".claude-plugin")
        .join("marketplace.json");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|_| anyhow::anyhow!("marketplace '{marketplace}' is not installed"))?;
    otto_extensions::parse_marketplace_json(&text)
        .map_err(|e| anyhow::anyhow!("marketplace '{marketplace}' has an invalid manifest: {e}"))
}

/// Resolve `(ref, commit)` for a checked-out repo at `dir`: the pinned ref if one was requested,
/// else the current branch name, plus `HEAD`'s commit sha.
async fn read_repo_head(dir: &Path, pinned_ref: Option<&str>) -> anyhow::Result<(String, String)> {
    let resolved_ref = match pinned_ref {
        Some(r) => r.to_string(),
        None => run_git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await?
            .trim()
            .to_string(),
    };
    let commit = run_git(dir, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string();
    Ok((resolved_ref, commit))
}

/// Clone a remote-sourced plugin into `repos/<marketplace>/<plugin>/` and record it in the
/// lockfile's `plugins` map. New clones use staging-dir → atomic rename → cleanup-on-failure,
/// mirroring `marketplace_add`; a lockfile-write failure removes a clone we created this call (never
/// a reused one).
///
/// An already-materialized clone is **reused as-is** — refreshing/re-pointing it is out of scope
/// (see the plugins design). If a lockfile entry already exists for this key, its recorded
/// provenance is preserved verbatim (so a pinned tag/sha isn't degraded to detached `HEAD`); a
/// requested source/ref that differs from the locked one is a no-op and emits a `note:` pointing at
/// uninstall+reinstall. A clone directory that exists with *no* lock entry (e.g. a crashed prior
/// install) records the on-disk origin/HEAD instead.
async fn materialize_remote_plugin(
    key: &str,
    plugin: &str,
    marketplace: &str,
    rc: &otto_extensions::RemoteClone,
    home: &Path,
) -> anyhow::Result<()> {
    validate_path_component(plugin, "plugin name")?;
    validate_path_component(marketplace, "marketplace name")?;
    validate_clone_url(&rc.url)?;
    if let Some(r) = &rc.git_ref {
        reject_leading_dash(r, "ref")?;
    }

    let mp_repos = repos_dir(home).join(marketplace);
    std::fs::create_dir_all(&mp_repos)?;
    let final_path = mp_repos.join(plugin);
    let newly_created = !final_path.exists();

    let mut lock = read_lockfile(home);
    let existing = lock.plugins.get(key).cloned();

    let (recorded_url, resolved_ref, commit) = if newly_created {
        let staging_name = format!(".staging-{}-{}", std::process::id(), uuid::Uuid::new_v4());
        let staging_path = mp_repos.join(&staging_name);
        if staging_path.exists() {
            std::fs::remove_dir_all(&staging_path)?;
        }
        run_git(&mp_repos, &["clone", "--", &rc.url, &staging_name]).await?;

        let cleanup_and_err = |e: anyhow::Error| {
            let _ = std::fs::remove_dir_all(&staging_path);
            Err(e)
        };
        if let Some(r) = &rc.git_ref {
            if let Err(e) = run_git(&staging_path, &["checkout", r]).await {
                return cleanup_and_err(e);
            }
        }
        let head = match read_repo_head(&staging_path, rc.git_ref.as_deref()).await {
            Ok(h) => h,
            Err(e) => return cleanup_and_err(e),
        };
        if let Err(e) = std::fs::rename(&staging_path, &final_path) {
            return cleanup_and_err(anyhow::anyhow!("failed to materialize plugin: {e}"));
        }
        let (resolved_ref, commit) = head;
        (rc.url.clone(), resolved_ref, commit)
    } else if let Some(prev) = existing {
        // True reuse: the clone and its lock entry were installed together. Reuse is a no-op
        // (refreshing a materialized clone is out of scope — see the plugins design), so keep the
        // recorded provenance rather than degrading a detached-HEAD pin to "HEAD". Note only when
        // the caller asked for a different source/ref.
        if prev.url != rc.url || rc.git_ref.as_deref().is_some_and(|r| r != prev.git_ref) {
            eprintln!(
                "note: plugin '{key}' is already materialized at {} (locked to {} @ {}); keeping it. \
                 Run `otto plugin uninstall {key}` and reinstall to change its source or ref.",
                final_path.display(),
                prev.url,
                prev.git_ref
            );
        }
        (prev.url, prev.git_ref, prev.commit)
    } else {
        // Clone dir exists but no lock entry (e.g. a crashed prior install): record on-disk truth.
        let origin = run_git(&final_path, &["config", "--get", "remote.origin.url"])
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| rc.url.clone());
        let (resolved_ref, commit) = read_repo_head(&final_path, None).await?;
        (origin, resolved_ref, commit)
    };

    lock.plugins.insert(
        key.to_string(),
        otto_extensions::MarketplaceLock {
            url: recorded_url,
            git_ref: resolved_ref,
            commit,
            updated_at_unix: now_unix(),
        },
    );
    if let Err(e) = write_lockfile(home, &lock) {
        if newly_created {
            let _ = std::fs::remove_dir_all(&final_path);
        }
        return Err(e);
    }
    Ok(())
}

/// Enable `"<plugin>@<marketplace>"` in `~/.claude/settings.json`, materializing the plugin's code
/// first if its source is remote (github/git): clone it into `~/.claude/plugins/repos/` and record
/// it in the lockfile. Errors if the marketplace isn't installed, the plugin isn't offered by it,
/// or the remote descriptor is malformed/unsupported. A local-path plugin only flips the bit.
pub async fn plugin_install(key: &str, home: &Path) -> anyhow::Result<()> {
    let (plugin_name, marketplace) = split_plugin_key(key)?;
    let mp = read_marketplace_manifest(home, &marketplace)?;
    let entry = mp
        .plugins
        .iter()
        .find(|p| p.name == plugin_name)
        .ok_or_else(|| {
            anyhow::anyhow!("no plugin named '{plugin_name}' in marketplace '{marketplace}'")
        })?;

    // Resolve the remote descriptor (if any) before dropping the borrow of `mp` across the clone.
    let remote = match &entry.source {
        otto_extensions::PluginSource::Remote(src) => {
            Some(otto_extensions::resolve_remote_source(src)?)
        }
        otto_extensions::PluginSource::LocalPath(_) => None,
    };
    if let Some(rc) = remote {
        materialize_remote_plugin(key, &plugin_name, &marketplace, &rc, home).await?;
    }

    let path = settings_path(home);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = otto_extensions::set_enabled_plugin(&existing, key, Some(true));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, updated)?;
    Ok(())
}

/// Remove `"<plugin>@<marketplace>"` from `~/.claude/settings.json`'s `enabledPlugins` entirely
/// (rather than writing `false`), so the file doesn't accumulate dead entries.
pub fn plugin_uninstall(key: &str, home: &Path) -> anyhow::Result<()> {
    split_plugin_key(key)?; // validates shape so a typo'd key surfaces a clear error
    let path = settings_path(home);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = otto_extensions::set_enabled_plugin(&existing, key, None);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, updated)?;
    Ok(())
}

/// List every plugin offered by every locked marketplace, paired with whether it's currently
/// enabled. Order matches the lockfile's sorted marketplace order, then manifest order within.
pub fn plugin_list(home: &Path) -> anyhow::Result<Vec<(String, bool)>> {
    let lock = read_lockfile(home);
    let settings_text = std::fs::read_to_string(settings_path(home)).unwrap_or_default();
    let enabled = otto_extensions::parse_enabled_plugins(&settings_text);

    let mut out = Vec::new();
    for mp_name in lock.entries.keys() {
        let mp = match read_marketplace_manifest(home, mp_name) {
            Ok(mp) => mp,
            Err(e) => {
                eprintln!("warning: skipping '{mp_name}': {e}");
                continue;
            }
        };
        for plugin in &mp.plugins {
            let key = format!("{}@{}", plugin.name, mp.name);
            let is_enabled = enabled.get(&key).copied().unwrap_or(false);
            out.push((key, is_enabled));
        }
    }
    Ok(out)
}

/// Parse `--ref <value>` out of `args`. Returns `(Some(value), remaining positionals)`.
fn parse_ref_flag(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut r = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--ref" {
            r = it.next().cloned();
        } else {
            rest.push(a.clone());
        }
    }
    (r, rest)
}

const USAGE: &str = "usage:\n  \
    otto plugin                            interactive TUI (default)\n  \
    otto plugin list\n  \
    otto plugin install <plugin>@<marketplace>\n  \
    otto plugin uninstall <plugin>@<marketplace>\n  \
    otto plugin marketplace add <url> [--ref <ref>]\n  \
    otto plugin marketplace remove <name>\n  \
    otto plugin marketplace update [<name>]\n  \
    otto plugin marketplace list";

/// Entry point for `otto plugin ...`, dispatched from `main()`. `home` is the user-global
/// `.claude/` base (`dirs::home_dir()` at the real CLI edge; an explicit tempdir in tests).
pub async fn cmd_plugin(args: Vec<String>, home: PathBuf) -> anyhow::Result<()> {
    let mut it = args.into_iter();
    let sub = it.next().unwrap_or_default();
    let rest: Vec<String> = it.collect();
    match sub.as_str() {
        "" | "interactive" => super::plugin_tui::interactive_plugin_ui(home).await,
        "marketplace" => cmd_plugin_marketplace(rest, &home).await,
        "install" => {
            let key = rest.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("usage: otto plugin install <plugin>@<marketplace>")
            })?;
            plugin_install(&key, &home).await?;
            println!("installed {key}");
            Ok(())
        }
        "uninstall" => {
            let key = rest.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("usage: otto plugin uninstall <plugin>@<marketplace>")
            })?;
            plugin_uninstall(&key, &home)?;
            println!("uninstalled {key}");
            Ok(())
        }
        "list" => {
            for (key, enabled) in plugin_list(&home)? {
                println!(
                    "{} {key}",
                    if enabled {
                        "[enabled]  "
                    } else {
                        "[available]"
                    }
                );
            }
            Ok(())
        }
        _ => anyhow::bail!(USAGE),
    }
}

async fn cmd_plugin_marketplace(args: Vec<String>, home: &Path) -> anyhow::Result<()> {
    let mut it = args.into_iter();
    let sub = it.next().unwrap_or_default();
    let rest: Vec<String> = it.collect();
    match sub.as_str() {
        "add" => {
            let (ref_flag, positional) = parse_ref_flag(&rest);
            let url = positional.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("usage: otto plugin marketplace add <url> [--ref <ref>]")
            })?;
            let name = marketplace_add(&url, ref_flag.as_deref(), home).await?;
            println!("installed marketplace '{name}'");
            Ok(())
        }
        "remove" => {
            let name = rest
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("usage: otto plugin marketplace remove <name>"))?;
            marketplace_remove(&name, home)?;
            println!("removed marketplace '{name}'");
            Ok(())
        }
        "update" => {
            let name = rest.into_iter().next();
            let updated = marketplace_update(name.as_deref(), home).await?;
            if updated.is_empty() {
                println!("nothing to update");
            } else {
                println!("updated: {}", updated.join(", "));
            }
            Ok(())
        }
        "list" => {
            let lock = read_lockfile(home);
            for (name, entry) in &lock.entries {
                let short_commit = &entry.commit[..entry.commit.len().min(12)];
                println!("{name}\t{}\t{}\t{short_commit}", entry.url, entry.git_ref);
            }
            Ok(())
        }
        _ => anyhow::bail!("usage: otto plugin marketplace add|remove|update|list ..."),
    }
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

    #[test]
    fn validate_path_component_rejects_escapes_and_dashes() {
        assert!(validate_path_component("foo", "plugin name").is_ok());
        assert!(validate_path_component("", "plugin name").is_err());
        assert!(validate_path_component(".", "plugin name").is_err());
        assert!(validate_path_component("..", "plugin name").is_err());
        assert!(validate_path_component("a/b", "plugin name").is_err());
        assert!(validate_path_component("a\\b", "plugin name").is_err());
    }

    #[test]
    fn repos_dir_is_under_claude_plugins() {
        let home = std::path::Path::new("/home/x");
        assert_eq!(
            repos_dir(home),
            std::path::Path::new("/home/x/.claude/plugins/repos")
        );
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

    /// Build a marketplace repo whose one plugin is remote (`{"source":"git","url":"file://…"}`)
    /// pointing at a *separate* bare repo that contains the plugin's code (a `plugin.json` plus a
    /// `commands/hello.md` command). Returns keep-alive tempdirs and the marketplace `file://` URL.
    async fn bare_remote_plugin_marketplace(
        mp_name: &str,
        plugin_name: &str,
    ) -> (Vec<tempfile::TempDir>, String) {
        // 1. The plugin's own repo (its code), bare-cloned to a file:// remote.
        let psrc = tempfile::tempdir().unwrap();
        init_repo(psrc.path()).await;
        let cp = psrc.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("plugin.json"),
            format!(r#"{{"name":"{plugin_name}"}}"#),
        )
        .unwrap();
        let cmds = psrc.path().join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("hello.md"), "say hi").unwrap();
        run_git(psrc.path(), &["add", "-A"]).await.unwrap();
        run_git(psrc.path(), &["commit", "-m", "plugin"])
            .await
            .unwrap();
        let pbare = tempfile::tempdir().unwrap();
        run_git(
            pbare.path(),
            &["clone", "--bare", psrc.path().to_str().unwrap(), "."],
        )
        .await
        .unwrap();
        let plugin_url = format!("file://{}", pbare.path().display());

        // 2. The marketplace repo, listing the plugin as a git-remote source.
        let msrc = tempfile::tempdir().unwrap();
        init_repo(msrc.path()).await;
        let mcp = msrc.path().join(".claude-plugin");
        std::fs::create_dir_all(&mcp).unwrap();
        std::fs::write(
            mcp.join("marketplace.json"),
            format!(
                r#"{{"name":"{mp_name}","plugins":[{{"name":"{plugin_name}","source":{{"source":"git","url":"{plugin_url}"}}}}]}}"#
            ),
        )
        .unwrap();
        run_git(msrc.path(), &["add", "-A"]).await.unwrap();
        run_git(msrc.path(), &["commit", "-m", "marketplace"])
            .await
            .unwrap();
        let mbare = tempfile::tempdir().unwrap();
        run_git(
            mbare.path(),
            &["clone", "--bare", msrc.path().to_str().unwrap(), "."],
        )
        .await
        .unwrap();
        let mp_url = format!("file://{}", mbare.path().display());

        (vec![psrc, pbare, msrc, mbare], mp_url)
    }

    #[tokio::test]
    async fn plugin_install_materializes_a_remote_source() {
        let (_keep, mp_url) = bare_remote_plugin_marketplace("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&mp_url, None, home.path()).await.unwrap();

        plugin_install("foo@acme", home.path()).await.unwrap();

        // The plugin's code was cloned into the repos cache.
        let plugin_root = repos_dir(home.path()).join("acme").join("foo");
        assert!(
            plugin_root
                .join(".claude-plugin")
                .join("plugin.json")
                .exists(),
            "expected materialized plugin at {}",
            plugin_root.display()
        );
        assert!(plugin_root.join("commands").join("hello.md").exists());

        // It is recorded in the lockfile's plugins map and enabled in settings.
        let lock = read_lockfile(home.path());
        let entry = lock.plugins.get("foo@acme").expect("plugin locked");
        assert!(
            entry.url.starts_with("file://"),
            "plugin url recorded: {}",
            entry.url
        );
        assert!(!entry.commit.is_empty());
        let settings = std::fs::read_to_string(settings_path(home.path())).unwrap();
        assert!(settings.contains("foo@acme"));
    }

    #[tokio::test]
    async fn plugin_install_remote_is_idempotent() {
        let (_keep, mp_url) = bare_remote_plugin_marketplace("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&mp_url, None, home.path()).await.unwrap();

        plugin_install("foo@acme", home.path()).await.unwrap();
        // Second install reuses the existing clone without error.
        plugin_install("foo@acme", home.path()).await.unwrap();

        assert!(repos_dir(home.path()).join("acme").join("foo").exists());
    }

    #[tokio::test]
    async fn materialize_reuse_records_on_disk_origin_not_requested() {
        let (_keep, mp_url) = bare_remote_plugin_marketplace("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&mp_url, None, home.path()).await.unwrap();
        plugin_install("foo@acme", home.path()).await.unwrap();
        let on_disk_url = read_lockfile(home.path()).plugins["foo@acme"].url.clone();

        // The clone already exists; call materialize again with a DIFFERENT requested url.
        // Reuse must record the on-disk origin, never fetch/record the bogus requested url.
        let rc = otto_extensions::RemoteClone {
            url: "file:///nonexistent-repo".to_string(),
            git_ref: None,
        };
        materialize_remote_plugin("foo@acme", "foo", "acme", &rc, home.path())
            .await
            .unwrap();
        let after = read_lockfile(home.path()).plugins["foo@acme"].url.clone();
        assert_eq!(
            after, on_disk_url,
            "reuse must record the on-disk origin, not the requested url"
        );
    }

    #[tokio::test]
    async fn materialize_reuse_preserves_a_pinned_ref() {
        // A plugin repo carrying a tag `v1`.
        let psrc = tempfile::tempdir().unwrap();
        init_repo(psrc.path()).await;
        let cp = psrc.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(cp.join("plugin.json"), r#"{"name":"foo"}"#).unwrap();
        run_git(psrc.path(), &["add", "-A"]).await.unwrap();
        run_git(psrc.path(), &["commit", "-m", "p"]).await.unwrap();
        run_git(psrc.path(), &["tag", "v1"]).await.unwrap();
        let pbare = tempfile::tempdir().unwrap();
        run_git(
            pbare.path(),
            &["clone", "--bare", psrc.path().to_str().unwrap(), "."],
        )
        .await
        .unwrap();
        let plugin_url = format!("file://{}", pbare.path().display());

        let home = tempfile::tempdir().unwrap();
        let rc = otto_extensions::RemoteClone {
            url: plugin_url,
            git_ref: Some("v1".to_string()),
        };
        // First materialize: clones + checks out the tag (detached HEAD) → records ref "v1".
        materialize_remote_plugin("foo@acme", "foo", "acme", &rc, home.path())
            .await
            .unwrap();
        assert_eq!(read_lockfile(home.path()).plugins["foo@acme"].git_ref, "v1");

        // Reuse: must preserve the pin, not degrade it to the detached-HEAD string "HEAD".
        materialize_remote_plugin("foo@acme", "foo", "acme", &rc, home.path())
            .await
            .unwrap();
        assert_eq!(
            read_lockfile(home.path()).plugins["foo@acme"].git_ref,
            "v1",
            "reuse must preserve the pinned ref, not record detached HEAD"
        );
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

    #[tokio::test]
    async fn marketplace_remove_leaves_stale_enabled_plugins_key_alone() {
        // Documented limitation: remove does not scrub settings.json.
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();
        plugin_install("foo@acme", home.path()).await.unwrap();

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
    async fn marketplace_remove_cleans_up_materialized_plugins() {
        let (_keep, mp_url) = bare_remote_plugin_marketplace("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&mp_url, None, home.path()).await.unwrap();
        plugin_install("foo@acme", home.path()).await.unwrap();

        assert!(repos_dir(home.path()).join("acme").exists());
        assert!(read_lockfile(home.path()).plugins.contains_key("foo@acme"));

        marketplace_remove("acme", home.path()).unwrap();

        assert!(
            !repos_dir(home.path()).join("acme").exists(),
            "repos tree removed"
        );
        assert!(
            !read_lockfile(home.path()).plugins.contains_key("foo@acme"),
            "plugin lock entry dropped"
        );
        assert!(!read_lockfile(home.path()).entries.contains_key("acme"));
    }

    #[test]
    fn marketplace_remove_only_drops_its_own_plugin_keys() {
        let home = tempfile::tempdir().unwrap();
        let mk = |u: &str| otto_extensions::MarketplaceLock {
            url: u.to_string(),
            git_ref: "main".to_string(),
            commit: "c".to_string(),
            updated_at_unix: 1,
        };
        let mut lock = read_lockfile(home.path());
        lock.entries.insert("acme".to_string(), mk("m"));
        lock.plugins.insert("foo@acme".to_string(), mk("a"));
        lock.plugins.insert("bar@other".to_string(), mk("b"));
        lock.plugins.insert("baz@x@acme".to_string(), mk("c")); // marketplace literally "x@acme"
        write_lockfile(home.path(), &lock).unwrap();

        marketplace_remove("acme", home.path()).unwrap();

        let after = read_lockfile(home.path());
        assert!(!after.plugins.contains_key("foo@acme"), "own key dropped");
        assert!(
            after.plugins.contains_key("bar@other"),
            "unrelated marketplace kept"
        );
        assert!(
            after.plugins.contains_key("baz@x@acme"),
            "'x@acme' must not be falsely matched by removing 'acme'"
        );
    }

    #[tokio::test]
    async fn marketplace_remove_rejects_unlocked_name_even_if_directory_exists() {
        let home = tempfile::tempdir().unwrap();
        // Simulate an attacker-relevant sibling directory that exists on disk but was never
        // added via marketplace_add (so it's not in the lockfile) — e.g. a path-traversal
        // attempt like "../../victim_data" would resolve outside marketplaces_dir entirely,
        // but even a plain unlocked name inside marketplaces_dir must not be deletable.
        let mp_root = marketplaces_dir(home.path());
        std::fs::create_dir_all(&mp_root).unwrap();
        let victim = mp_root.join("not-in-lockfile");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("important.txt"), "do not delete me").unwrap();

        let err = marketplace_remove("not-in-lockfile", home.path()).unwrap_err();
        assert!(err.to_string().contains("not installed"), "got: {err}");
        assert!(
            victim.exists(),
            "unlocked directory must survive a remove attempt"
        );
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

    #[tokio::test]
    async fn plugin_install_enables_local_path_plugin() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();

        plugin_install("foo@acme", home.path()).await.unwrap();

        let settings = std::fs::read_to_string(settings_path(home.path())).unwrap();
        let enabled = otto_extensions::parse_enabled_plugins(&settings);
        assert_eq!(enabled.get("foo@acme"), Some(&true));
    }

    #[tokio::test]
    async fn plugin_install_preserves_other_settings_keys() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();
        std::fs::write(settings_path(home.path()), r#"{"hooks":{"PreToolUse":[]}}"#).unwrap();

        plugin_install("foo@acme", home.path()).await.unwrap();

        let settings = std::fs::read_to_string(settings_path(home.path())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&settings).unwrap();
        assert!(v.get("hooks").is_some());
    }

    #[tokio::test]
    async fn plugin_install_unknown_marketplace_errors() {
        let home = tempfile::tempdir().unwrap();
        let err = plugin_install("foo@nope", home.path()).await.unwrap_err();
        assert!(err.to_string().contains("not installed"), "got: {err}");
    }

    #[tokio::test]
    async fn plugin_install_unknown_plugin_errors() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();

        let err = plugin_install("nope@acme", home.path()).await.unwrap_err();
        assert!(err.to_string().contains("no plugin named"), "got: {err}");
    }

    /// A remote source whose descriptor `resolve_remote_source` itself rejects (an unsupported
    /// `source` kind) must fail fast, pure-data, with no `git clone` attempted at all — proven here
    /// by never handing it a reachable URL.
    #[tokio::test]
    async fn plugin_install_rejects_malformed_remote_source() {
        let src = tempfile::tempdir().unwrap();
        init_repo(src.path()).await;
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"rem","source":{"source":"svn","repo":"a/b"}}]}"#,
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
        marketplace_add(&url, None, home.path()).await.unwrap();

        let err = plugin_install("rem@acme", home.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("unsupported remote source kind"),
            "got: {err}"
        );
        // No clone was attempted — the repos cache stays empty.
        assert!(!repos_dir(home.path()).join("acme").join("rem").exists());
    }

    #[tokio::test]
    async fn plugin_install_malformed_key_errors() {
        let home = tempfile::tempdir().unwrap();
        let err = plugin_install("no-at-sign", home.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("<plugin>@<marketplace>"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn plugin_uninstall_removes_the_key() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();
        plugin_install("foo@acme", home.path()).await.unwrap();

        plugin_uninstall("foo@acme", home.path()).unwrap();

        let settings = std::fs::read_to_string(settings_path(home.path())).unwrap();
        let enabled = otto_extensions::parse_enabled_plugins(&settings);
        assert_eq!(enabled.get("foo@acme"), None);
    }

    #[tokio::test]
    async fn plugin_list_reports_enabled_and_available() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();
        plugin_install("foo@acme", home.path()).await.unwrap();

        let listed = plugin_list(home.path()).unwrap();
        assert_eq!(listed, vec![("foo@acme".to_string(), true)]);
    }

    #[tokio::test]
    async fn plugin_list_reports_not_enabled_as_available() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();

        let listed = plugin_list(home.path()).unwrap();
        assert_eq!(listed, vec![("foo@acme".to_string(), false)]);
    }

    #[test]
    fn parse_ref_flag_extracts_value() {
        let args = vec!["--ref".to_string(), "v1".to_string(), "url".to_string()];
        let (r, rest) = parse_ref_flag(&args);
        assert_eq!(r, Some("v1".to_string()));
        assert_eq!(rest, vec!["url".to_string()]);
    }

    #[test]
    fn parse_ref_flag_absent_is_none() {
        let args = vec!["url".to_string()];
        let (r, rest) = parse_ref_flag(&args);
        assert_eq!(r, None);
        assert_eq!(rest, vec!["url".to_string()]);
    }

    #[tokio::test]
    async fn cmd_plugin_install_then_list_end_to_end() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();

        cmd_plugin(
            vec!["marketplace".to_string(), "add".to_string(), url.clone()],
            home.path().to_path_buf(),
        )
        .await
        .unwrap();

        cmd_plugin(
            vec!["install".to_string(), "foo@acme".to_string()],
            home.path().to_path_buf(),
        )
        .await
        .unwrap();

        let settings = std::fs::read_to_string(settings_path(home.path())).unwrap();
        let enabled = otto_extensions::parse_enabled_plugins(&settings);
        assert_eq!(enabled.get("foo@acme"), Some(&true));
    }

    #[tokio::test]
    async fn cmd_plugin_unknown_subcommand_errors() {
        let home = tempfile::tempdir().unwrap();
        let err = cmd_plugin(vec!["bogus".to_string()], home.path().to_path_buf())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("usage"), "got: {err}");
    }
}
