# Plugin Marketplace Install Action Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `otto plugin marketplace add|remove|update|list` and `otto plugin install|uninstall|list` CLI subcommands so a developer can clone a Claude-Code-compatible plugin marketplace onto disk under `~/.claude/plugins/marketplaces/` and flip the `enabledPlugins` allowlist in `~/.claude/settings.json` — the "network install action" the Slice 5 plugins design explicitly deferred.

**Architecture:** Pure logic (a JSON lockfile model + a `settings.json` `enabledPlugins`-merge function) lives in a new `crates/extensions/src/marketplace_install.rs` module, matching that crate's existing hand-rolled `serde_json::Value` parsing style (no `serde` derive dependency exists there today). All OS-touching work — `git` subprocess calls (with hardening duplicated from `mcp-git`, since `mcp-git` is bin-only and can't be linked), real file I/O against `~/.claude/`, and the `otto plugin` CLI dispatch — lives in a new `crates/engine/src/plugin_cli.rs` module wired into `main()` alongside `cmd_run`/`cmd_serve`.

**Tech Stack:** Rust workspace (`otto-extensions`, `otto-engine` crates), `tokio::process::Command` for git, `serde_json::Value` for JSON, `anyhow` for errors. No new crate dependencies.

**Design spec:** `docs/superpowers/specs/2026-07-06-extensions-plugin-install-design.md` — read it first for the full rationale (why hardening is duplicated rather than shared, why marketplace-repo-only, why user-global-only, why a single lockfile).

**Note on one implementation refinement vs. the design doc:** the design sketched an optional `otto plugin marketplace add <url> --name <alias>`. Building it revealed a correctness problem: `crates/extensions/src/lib.rs`'s `fold_plugins` keys `enabledPlugins` as `"<plugin>@<marketplace-json's-own-name-field>"`, not the on-disk directory name. An arbitrary `--name` alias would silently diverge from the key `install`/discovery actually use. This plan drops `--name`: the clone is always placed at `~/.claude/plugins/marketplaces/<name-from-marketplace.json>/`, so the directory name and the enable-key namespace are always the same value. `--ref` is unaffected.

---

### Task 1: `MarketplaceLock` / `MarketplaceLockfile` — pure JSON model

**Files:**
- Create: `crates/extensions/src/marketplace_install.rs`
- Modify: `crates/extensions/src/lib.rs:8-20` (module declarations) and `:22-36` (pub use list)

- [ ] **Step 1: Write the failing tests**

Create `crates/extensions/src/marketplace_install.rs`:

```rust
//! Pure logic for the `otto plugin` install action: the marketplace lockfile model and the
//! `settings.json` `enabledPlugins` merge function. No filesystem or process I/O — those live at
//! the CLI edge in `crates/engine/src/plugin_cli.rs`, matching this crate's existing convention
//! (discovery's `home` is always an explicit parameter; parsing functions take strings and return
//! data, never touch disk). Mirrors `plugin_def.rs`/`marketplace_def.rs`'s style of hand-rolled
//! `serde_json::Value` parsing rather than `#[derive(Deserialize)]` (this crate has no `serde`
//! dependency, only `serde_json`).

use std::collections::BTreeMap;

use serde_json::Value;

/// One locked marketplace: where it came from, what ref/commit it's pinned to, and when it was
/// last installed/updated. `updated_at_unix` is seconds since the Unix epoch (read at the CLI
/// edge via `SystemTime::now()`, never inside this pure module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceLock {
    pub url: String,
    pub git_ref: String,
    pub commit: String,
    pub updated_at_unix: u64,
}

/// The full lockfile: marketplace name -> its lock entry. A `BTreeMap` keeps serialized output in
/// sorted-key order, so the on-disk file stays git-diff-friendly across updates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarketplaceLockfile {
    pub entries: BTreeMap<String, MarketplaceLock>,
}

impl MarketplaceLockfile {
    /// Parse a lockfile document. Malformed JSON or a non-object top level yields an empty
    /// lockfile (never fatal — matches every other `.claude/` reader in this crate). An entry
    /// missing a required field, or with a non-string/non-number value, is skipped.
    pub fn parse(json: &str) -> Self {
        let mut entries = BTreeMap::new();
        let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
            return Self { entries };
        };
        for (name, v) in root {
            let Some(url) = v.get("url").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(git_ref) = v.get("ref").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(commit) = v.get("commit").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(updated_at_unix) = v.get("updated_at_unix").and_then(|x| x.as_u64()) else {
                continue;
            };
            entries.insert(
                name,
                MarketplaceLock {
                    url: url.to_string(),
                    git_ref: git_ref.to_string(),
                    commit: commit.to_string(),
                    updated_at_unix,
                },
            );
        }
        Self { entries }
    }

    /// Serialize to pretty JSON. `BTreeMap` iteration order (sorted keys) is preserved by
    /// `serde_json::Map` (this workspace does not enable the `preserve_order` feature, so
    /// `serde_json::Map` is itself `BTreeMap`-backed).
    pub fn to_json(&self) -> String {
        let mut root = serde_json::Map::new();
        for (name, lock) in &self.entries {
            root.insert(
                name.clone(),
                serde_json::json!({
                    "url": lock.url,
                    "ref": lock.git_ref,
                    "commit": lock.commit,
                    "updated_at_unix": lock.updated_at_unix,
                }),
            );
        }
        serde_json::to_string_pretty(&Value::Object(root)).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let mut lf = MarketplaceLockfile::default();
        lf.entries.insert(
            "acme".to_string(),
            MarketplaceLock {
                url: "https://example.com/acme.git".to_string(),
                git_ref: "main".to_string(),
                commit: "abc123".to_string(),
                updated_at_unix: 1_720_000_000,
            },
        );
        let json = lf.to_json();
        let back = MarketplaceLockfile::parse(&json);
        assert_eq!(back, lf);
    }

    #[test]
    fn parse_empty_or_malformed_is_empty() {
        assert!(MarketplaceLockfile::parse("").entries.is_empty());
        assert!(MarketplaceLockfile::parse("not json").entries.is_empty());
        assert!(MarketplaceLockfile::parse("[]").entries.is_empty());
        assert!(MarketplaceLockfile::parse("{}").entries.is_empty());
    }

    #[test]
    fn entry_missing_a_required_field_is_skipped() {
        let json = r#"{
            "good": {"url":"u","ref":"main","commit":"c","updated_at_unix":1},
            "bad":  {"url":"u","ref":"main"}
        }"#;
        let lf = MarketplaceLockfile::parse(json);
        assert_eq!(lf.entries.len(), 1);
        assert!(lf.entries.contains_key("good"));
    }

    #[test]
    fn to_json_sorts_keys() {
        let mut lf = MarketplaceLockfile::default();
        for name in ["zeta", "alpha", "mid"] {
            lf.entries.insert(
                name.to_string(),
                MarketplaceLock {
                    url: "u".to_string(),
                    git_ref: "main".to_string(),
                    commit: "c".to_string(),
                    updated_at_unix: 1,
                },
            );
        }
        let json = lf.to_json();
        let alpha = json.find("alpha").unwrap();
        let mid = json.find("mid").unwrap();
        let zeta = json.find("zeta").unwrap();
        assert!(alpha < mid && mid < zeta, "expected sorted key order: {json}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions marketplace_install::`
Expected: PASS (this module has no external dependency on other Task work, so it should compile and pass immediately once wired into `lib.rs` in the next step).

- [ ] **Step 3: Wire the new module into `lib.rs`**

In `crates/extensions/src/lib.rs`, add the module declaration next to the others (currently lines 8-20):

```rust
mod agent_def;
mod command_def;
mod command_expand;
mod hook_def;
mod hook_exec;
mod hooked_tool;
mod markdown_agent;
mod marketplace_def;
mod marketplace_install;
mod permission_def;
mod plugin_def;
mod skill_def;
mod skill_tool;
mod task_tool;
```

And add the export next to the other `pub use` lines (currently lines 22-36), right after the `marketplace_def` line:

```rust
pub use marketplace_def::{Marketplace, MarketplaceEntry, PluginSource, parse_marketplace_json};
pub use marketplace_install::{MarketplaceLock, MarketplaceLockfile};
```

- [ ] **Step 4: Run the full extensions test suite**

Run: `cargo test -p otto-extensions`
Expected: PASS, no regressions.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/marketplace_install.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): add MarketplaceLockfile pure JSON model"
```

---

### Task 2: `set_enabled_plugin` — settings.json merge function

**Files:**
- Modify: `crates/extensions/src/marketplace_install.rs` (append)
- Modify: `crates/extensions/src/lib.rs` (pub use list)

- [ ] **Step 1: Write the failing tests**

Append to `crates/extensions/src/marketplace_install.rs` (before the existing `#[cfg(test)] mod tests` block's closing brace — add both the function above the test module and these new tests inside it):

```rust
/// Insert, remove, or flip an `"<plugin>@<marketplace>"` key in a `settings.json` document's
/// `enabledPlugins` object, returning the rewritten JSON. Every other top-level key (`hooks`,
/// `permissions`, other `enabledPlugins` entries, …) is preserved untouched.
///
/// - `enabled = Some(b)` inserts/overwrites `key` with the bool `b`.
/// - `enabled = None` removes `key` entirely (used by `uninstall`, so `settings.json` doesn't
///   accumulate dead entries for plugins that were never re-enabled).
///
/// Malformed or absent input is treated as `{}` (never fatal — the CLI is creating this file for
/// the first time in the common case).
pub fn set_enabled_plugin(settings_json: &str, key: &str, enabled: Option<bool>) -> String {
    let mut root: Value = serde_json::from_str(settings_json)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let root_obj = root.as_object_mut().expect("filtered to object above");

    let enabled_plugins = root_obj
        .entry("enabledPlugins".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !enabled_plugins.is_object() {
        *enabled_plugins = Value::Object(serde_json::Map::new());
    }
    let map = enabled_plugins.as_object_mut().expect("just ensured object");

    match enabled {
        Some(b) => {
            map.insert(key.to_string(), Value::Bool(b));
        }
        None => {
            map.remove(key);
        }
    }

    serde_json::to_string_pretty(&root).unwrap()
}
```

Add these tests inside the existing `mod tests` block in the same file:

```rust
    #[test]
    fn set_enabled_plugin_inserts_into_empty_settings() {
        let out = set_enabled_plugin("", "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_preserves_other_top_level_keys() {
        let existing = r#"{"hooks":{"PreToolUse":[]},"permissions":{"allow":["Read(**)"]}}"#;
        let out = set_enabled_plugin(existing, "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("hooks").is_some(), "hooks key must survive: {out}");
        assert!(
            v.get("permissions").is_some(),
            "permissions key must survive: {out}"
        );
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_preserves_other_enabled_plugins_entries() {
        let existing = r#"{"enabledPlugins":{"bar@acme":true}}"#;
        let out = set_enabled_plugin(existing, "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabledPlugins"]["bar@acme"], Value::Bool(true));
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_none_removes_the_key() {
        let existing = r#"{"enabledPlugins":{"foo@acme":true,"bar@acme":true}}"#;
        let out = set_enabled_plugin(existing, "foo@acme", None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["enabledPlugins"].get("foo@acme").is_none());
        assert_eq!(v["enabledPlugins"]["bar@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_tolerates_malformed_input() {
        let out = set_enabled_plugin("not json", "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }

    #[test]
    fn set_enabled_plugin_replaces_non_object_enabled_plugins() {
        // A hand-edited settings.json with a bogus `enabledPlugins` value must not panic.
        let existing = r#"{"enabledPlugins": "oops"}"#;
        let out = set_enabled_plugin(existing, "foo@acme", Some(true));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabledPlugins"]["foo@acme"], Value::Bool(true));
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p otto-extensions marketplace_install::`
Expected: PASS.

- [ ] **Step 3: Export `set_enabled_plugin` from `lib.rs`**

In `crates/extensions/src/lib.rs`, update the line added in Task 1:

```rust
pub use marketplace_install::{MarketplaceLock, MarketplaceLockfile, set_enabled_plugin};
```

- [ ] **Step 4: Run the full extensions test suite, then fmt/clippy**

Run: `cargo test -p otto-extensions`
Expected: PASS.

Run: `cargo fmt -p otto-extensions && cargo clippy -p otto-extensions --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/marketplace_install.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): add set_enabled_plugin settings.json merge"
```

---

### Task 3: `plugin_cli.rs` skeleton — git hardening + path helpers

**Files:**
- Create: `crates/engine/src/plugin_cli.rs`
- Modify: `crates/engine/src/main.rs:1-15` (add `mod plugin_cli;`)

- [ ] **Step 1: Write the failing tests**

Create `crates/engine/src/plugin_cli.rs`:

```rust
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
```

- [ ] **Step 2: Declare the module in `main.rs`**

In `crates/engine/src/main.rs`, add the module declaration right after the existing `use` block (after line 14, before the `#[tokio::main]` on line 16):

```rust
mod plugin_cli;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-engine plugin_cli::`
Expected: PASS.

- [ ] **Step 4: fmt/clippy**

Run: `cargo fmt -p otto-engine && cargo clippy -p otto-engine --all-targets`
Expected: no warnings. (`validate_clone_url`/`reject_leading_dash`/`is_scp_like`/`now_unix`/lockfile helpers are used by later tasks in this same module, so clippy's dead-code lint won't yet fire — if it does at this stage because nothing outside tests calls them yet, that's expected and resolves once Task 4 adds the first real caller; do not add `#[allow(dead_code)]` — just confirm the warning disappears after Task 4.)

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/plugin_cli.rs crates/engine/src/main.rs
git commit -m "feat(engine): add plugin_cli module with git hardening + path helpers"
```

---

### Task 4: `marketplace_add`

**Files:**
- Modify: `crates/engine/src/plugin_cli.rs` (append, before the `#[cfg(test)]` block)

- [ ] **Step 1: Write the failing tests**

Add this test-helper and these tests inside the existing `mod tests` block in `crates/engine/src/plugin_cli.rs` (after the tests added in Task 3):

```rust
    async fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]).await.unwrap();
        run_git(dir, &["config", "user.name", "Test"]).await.unwrap();
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
        run_git(src.path(), &["commit", "-m", "seed"]).await.unwrap();

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
        assert!(
            err.to_string().contains("already installed"),
            "got: {err}"
        );
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
        run_git(src.path(), &["commit", "-m", "seed"]).await.unwrap();
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
        assert!(err.to_string().contains("unsupported clone url"), "got: {err}");
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
        std::fs::write(cp.join("marketplace.json"), r#"{"name":"../evil","plugins":[]}"#).unwrap();
        run_git(src.path(), &["add", "-A"]).await.unwrap();
        run_git(src.path(), &["commit", "-m", "seed"]).await.unwrap();
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
        assert!(err.to_string().contains("invalid marketplace name"), "got: {err}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine plugin_cli::tests::marketplace_add`
Expected: FAIL with "cannot find function `marketplace_add`".

- [ ] **Step 3: Implement `marketplace_add`**

Add this function to `crates/engine/src/plugin_cli.rs`, above the `#[cfg(test)]` block:

```rust
/// Clone `url` into a staging directory under `~/.claude/plugins/marketplaces/`, verify it
/// contains a valid `marketplace.json`, then move it into place at
/// `~/.claude/plugins/marketplaces/<name-from-marketplace.json>/` (the directory name is always
/// the marketplace's own declared `name` — never a user-supplied alias — so it always matches the
/// `"<plugin>@<marketplace>"` enable-key namespace `otto_extensions::discover` uses). Records the
/// result in the lockfile. Returns the resolved marketplace name.
///
/// `ref_` optionally pins a branch/tag/sha (checked out after clone); omitted, the clone's default
/// branch is recorded as-is. Cleans up the staging directory on any failure — never leaves a
/// partial marketplace directory behind.
pub async fn marketplace_add(
    url: &str,
    ref_: Option<&str>,
    home: &Path,
) -> anyhow::Result<String> {
    validate_clone_url(url)?;
    if let Some(r) = ref_ {
        reject_leading_dash(r, "ref")?;
    }

    let mp_root = marketplaces_dir(home);
    std::fs::create_dir_all(&mp_root)?;

    let staging_name = format!(".staging-{}", std::process::id());
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

    let manifest_path = staging_path
        .join(".claude-plugin")
        .join("marketplace.json");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(_) => {
            return cleanup_and_err(anyhow::anyhow!(
                "cloned repository has no {} (not a valid marketplace)",
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
    std::fs::rename(&staging_path, &final_path)?;

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
    write_lockfile(home, &lock)?;

    Ok(mp.name)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine plugin_cli::tests::marketplace_add`
Expected: PASS (7 tests: clones-and-locks, rejects-duplicate, explicit-ref, cleans-up-on-missing-manifest, rejects-bad-scheme, rejects-leading-dash-ref, rejects-path-unsafe-name).

- [ ] **Step 5: fmt/clippy**

Run: `cargo fmt -p otto-engine && cargo clippy -p otto-engine --all-targets`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/plugin_cli.rs
git commit -m "feat(engine): add marketplace_add clone-and-lock action"
```

---

### Task 5: `marketplace_remove` and `marketplace_update`

**Files:**
- Modify: `crates/engine/src/plugin_cli.rs` (append, before the `#[cfg(test)]` block; tests inside it)

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` in `crates/engine/src/plugin_cli.rs`:

```rust
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
        run_git(src.path(), &["commit", "-m", "more"]).await.unwrap();
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine plugin_cli::tests::marketplace_remove plugin_cli::tests::marketplace_update`
Expected: FAIL with "cannot find function `marketplace_remove`"/`marketplace_update` (and `plugin_install`, added next task — comment out the `plugin_install` call in `marketplace_remove_leaves_stale_enabled_plugins_key_alone` for now by ignoring that one test with `#[ignore = "plugin_install lands in Task 6"]` above it, then un-ignore it in Task 6 Step 1).

- [ ] **Step 3: Implement `marketplace_remove` and `marketplace_update`**

Add to `crates/engine/src/plugin_cli.rs`, above the `#[cfg(test)]` block:

```rust
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
```

- [ ] **Step 4: Un-ignore the deferred test and run everything**

Remove the `#[ignore = "..."]` attribute you added in Step 2 above `marketplace_remove_leaves_stale_enabled_plugins_key_alone` (its `plugin_install` call will compile once Task 6 lands — for now leave it ignored if Task 6 hasn't been done yet, or implement Task 6 before running this if working strictly in order; either way, do not leave a passing test suite with a silently-ignored test at the end of this plan).

Run: `cargo test -p otto-engine plugin_cli::tests::marketplace_remove plugin_cli::tests::marketplace_update`
Expected: PASS for every test except (if not yet un-ignored) `marketplace_remove_leaves_stale_enabled_plugins_key_alone`.

- [ ] **Step 5: fmt/clippy**

Run: `cargo fmt -p otto-engine && cargo clippy -p otto-engine --all-targets`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/plugin_cli.rs
git commit -m "feat(engine): add marketplace_remove and marketplace_update"
```

---

### Task 6: `plugin_install` / `plugin_uninstall` / `plugin_list`

**Files:**
- Modify: `crates/engine/src/plugin_cli.rs` (append, before the `#[cfg(test)]` block; tests inside it)

- [ ] **Step 1: Write the failing tests**

First, remove the `#[ignore = "..."]` attribute added in Task 5 Step 2 above `marketplace_remove_leaves_stale_enabled_plugins_key_alone` (its `plugin_install` call now has an implementation to compile against once this task's Step 3 lands).

Add inside `mod tests` in `crates/engine/src/plugin_cli.rs`:

```rust
    #[tokio::test]
    async fn plugin_install_enables_local_path_plugin() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();

        plugin_install("foo@acme", home.path()).unwrap();

        let settings = std::fs::read_to_string(settings_path(home.path())).unwrap();
        let enabled = otto_extensions::parse_enabled_plugins(&settings);
        assert_eq!(enabled.get("foo@acme"), Some(&true));
    }

    #[tokio::test]
    async fn plugin_install_preserves_other_settings_keys() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();
        std::fs::write(
            settings_path(home.path()),
            r#"{"hooks":{"PreToolUse":[]}}"#,
        )
        .unwrap();

        plugin_install("foo@acme", home.path()).unwrap();

        let settings = std::fs::read_to_string(settings_path(home.path())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&settings).unwrap();
        assert!(v.get("hooks").is_some());
    }

    #[tokio::test]
    async fn plugin_install_unknown_marketplace_errors() {
        let home = tempfile::tempdir().unwrap();
        let err = plugin_install("foo@nope", home.path()).unwrap_err();
        assert!(err.to_string().contains("not installed"), "got: {err}");
    }

    #[tokio::test]
    async fn plugin_install_unknown_plugin_errors() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();

        let err = plugin_install("nope@acme", home.path()).unwrap_err();
        assert!(err.to_string().contains("no plugin named"), "got: {err}");
    }

    #[tokio::test]
    async fn plugin_install_rejects_remote_sourced_plugin() {
        let src = tempfile::tempdir().unwrap();
        init_repo(src.path()).await;
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"rem","source":{"source":"github","repo":"a/b"}}]}"#,
        )
        .unwrap();
        run_git(src.path(), &["add", "-A"]).await.unwrap();
        run_git(src.path(), &["commit", "-m", "seed"]).await.unwrap();
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

        let err = plugin_install("rem@acme", home.path()).unwrap_err();
        assert!(
            err.to_string().contains("remote-sourced"),
            "got: {err}"
        );
    }

    #[test]
    fn plugin_install_malformed_key_errors() {
        let home = tempfile::tempdir().unwrap();
        let err = plugin_install("no-at-sign", home.path()).unwrap_err();
        assert!(err.to_string().contains("<plugin>@<marketplace>"), "got: {err}");
    }

    #[tokio::test]
    async fn plugin_uninstall_removes_the_key() {
        let (_src, _bare, url) = bare_marketplace_remote("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&url, None, home.path()).await.unwrap();
        plugin_install("foo@acme", home.path()).unwrap();

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
        plugin_install("foo@acme", home.path()).unwrap();

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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine plugin_cli::tests::plugin_`
Expected: FAIL with "cannot find function `plugin_install`" (and `plugin_uninstall`/`plugin_list`).

- [ ] **Step 3: Implement `plugin_install`, `plugin_uninstall`, `plugin_list`**

Add to `crates/engine/src/plugin_cli.rs`, above the `#[cfg(test)]` block:

```rust
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
    let manifest_path = marketplaces_dir(home)
        .join(marketplace)
        .join(".claude-plugin")
        .join("marketplace.json");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|_| anyhow::anyhow!("marketplace '{marketplace}' is not installed"))?;
    otto_extensions::parse_marketplace_json(&text)
        .map_err(|e| anyhow::anyhow!("marketplace '{marketplace}' has an invalid manifest: {e}"))
}

/// Enable `"<plugin>@<marketplace>"` in `~/.claude/settings.json`. Errors if the marketplace isn't
/// installed, the plugin isn't offered by it, or the plugin's `source` is `Remote` (materializing
/// a plugin whose code lives outside its marketplace repo is a deferred follow-up — see the design
/// doc). Never activates any code — only flips the allowlist bit `discover()` reads.
pub fn plugin_install(key: &str, home: &Path) -> anyhow::Result<()> {
    let (plugin_name, marketplace) = split_plugin_key(key)?;
    let mp = read_marketplace_manifest(home, &marketplace)?;
    let entry = mp
        .plugins
        .iter()
        .find(|p| p.name == plugin_name)
        .ok_or_else(|| {
            anyhow::anyhow!("no plugin named '{plugin_name}' in marketplace '{marketplace}'")
        })?;
    if matches!(entry.source, otto_extensions::PluginSource::Remote(_)) {
        anyhow::bail!(
            "'{key}' is remote-sourced (its code lives outside its marketplace repo); \
             installing remote-sourced plugins isn't supported yet"
        );
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine plugin_cli::`
Expected: PASS — every test in the module, including the now-un-ignored
`marketplace_remove_leaves_stale_enabled_plugins_key_alone` from Task 5.

- [ ] **Step 5: fmt/clippy**

Run: `cargo fmt -p otto-engine && cargo clippy -p otto-engine --all-targets`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/plugin_cli.rs
git commit -m "feat(engine): add plugin_install/uninstall/list"
```

---

### Task 7: Wire `otto plugin` into the CLI dispatch

**Files:**
- Modify: `crates/engine/src/plugin_cli.rs` (append `cmd_plugin` + arg-parsing helpers, before `#[cfg(test)]`)
- Modify: `crates/engine/src/main.rs:1-2` (usage doc comment), `:16-32` (`main()` dispatch)

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` in `crates/engine/src/plugin_cli.rs`:

```rust
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
            vec![
                "marketplace".to_string(),
                "add".to_string(),
                url.clone(),
            ],
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine plugin_cli::tests::cmd_plugin plugin_cli::tests::parse_ref_flag`
Expected: FAIL with "cannot find function `cmd_plugin`"/`parse_ref_flag`.

- [ ] **Step 3: Implement `cmd_plugin` and its arg-parsing helpers**

Add to `crates/engine/src/plugin_cli.rs`, above the `#[cfg(test)]` block:

```rust
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
    otto plugin marketplace add <url> [--ref <ref>]\n  \
    otto plugin marketplace remove <name>\n  \
    otto plugin marketplace update [<name>]\n  \
    otto plugin marketplace list\n  \
    otto plugin install <plugin>@<marketplace>\n  \
    otto plugin uninstall <plugin>@<marketplace>\n  \
    otto plugin list";

/// Entry point for `otto plugin ...`, dispatched from `main()`. `home` is the user-global
/// `.claude/` base (`dirs::home_dir()` at the real CLI edge; an explicit tempdir in tests).
pub async fn cmd_plugin(args: Vec<String>, home: PathBuf) -> anyhow::Result<()> {
    let mut it = args.into_iter();
    let sub = it.next().unwrap_or_default();
    let rest: Vec<String> = it.collect();
    match sub.as_str() {
        "marketplace" => cmd_plugin_marketplace(rest, &home).await,
        "install" => {
            let key = rest
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("usage: otto plugin install <plugin>@<marketplace>"))?;
            plugin_install(&key, &home)?;
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
                println!("{} {key}", if enabled { "[enabled]  " } else { "[available]" });
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
        _ => anyhow::bail!(
            "usage: otto plugin marketplace add|remove|update|list ..."
        ),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine plugin_cli::`
Expected: PASS.

- [ ] **Step 5: Wire `cmd_plugin` into `main()`'s dispatch**

In `crates/engine/src/main.rs`, update the top doc comment (lines 1-2) to:

```rust
//! `otto run "<goal>" [--root <path>] [--agent <name>]` — run a single turn (or a named custom agent) and print output.
//! `otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm] [--accept-promotions]` — serve over WebSocket (needs OTTO_TOKEN).
//! `otto plugin marketplace add|remove|update|list` / `otto plugin install|uninstall|list` — manage Claude Code plugin marketplaces under `~/.claude/plugins/marketplaces/`.
```

Then update the `match` in `main()` (currently lines 22-31):

```rust
    match command.as_str() {
        "run" => cmd_run(rest).await,
        "serve" => cmd_serve(rest).await,
        "plugin" => plugin_cli::cmd_plugin(rest, home_dir()).await,
        _ => {
            eprintln!(
                "usage:\n  otto run \"<goal>\" [--root <path>] [--agent <name>]\n  otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm] [--accept-promotions]\n  otto plugin marketplace add|remove|update|list\n  otto plugin install|uninstall|list"
            );
            std::process::exit(2);
        }
    }
```

- [ ] **Step 6: Build the binary and smoke-test it manually**

Run: `cargo build -p otto-engine`
Expected: builds cleanly.

Run: `HOME=$(mktemp -d) ./target/debug/otto plugin list`
Expected: exits 0, prints nothing (no marketplaces installed in the fresh `$HOME`).

Run: `./target/debug/otto plugin bogus`
Expected: exits non-zero, prints the `usage: ...` message to stderr.

- [ ] **Step 7: fmt/clippy**

Run: `cargo fmt -p otto-engine && cargo clippy -p otto-engine --all-targets`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/engine/src/plugin_cli.rs crates/engine/src/main.rs
git commit -m "feat(engine): wire otto plugin CLI dispatch into main()"
```

---

### Task 8: Full workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Format the whole workspace**

Run: `cargo fmt --all`
Expected: no diff (everything already formatted per-crate in prior tasks); if it reformats anything, review the diff before proceeding.

- [ ] **Step 2: Lint the whole workspace**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test --workspace`
Expected: PASS — every existing test plus every new `marketplace_install::`/`plugin_cli::` test. This slice adds no new nondeterminism to the offline determinism suite: `plugin_cli`'s git-spawning tests are gated behind `#[tokio::test]`s that only run under `cargo test`, never touched by the spine/agent code path.

- [ ] **Step 4: Confirm `ARCHITECTURE.md` still lists the right deferrals**

Read `docs/ARCHITECTURE.md`'s "Claude Code compatibility" section (around line 337-346). Update the sentence "The network *install action* (marketplace `git clone`, lockfile) is still pending." to reflect that it now ships — e.g.:

```markdown
- plugins (`.claude-plugin/plugin.json`) → discovered from on-disk marketplaces under
  `.claude/plugins/marketplaces/`, gated by the `enabledPlugins` allowlist in `settings.json`, and
  each enabled plugin's bundled `agents`/`commands`/`skills`/`hooks` folded into the rows above —
  **namespaced by plugin name** (`foo:commit`), lowest precedence (user/project win),
  `${CLAUDE_PLUGIN_ROOT}` expanded in hook commands. Each enabled plugin's bundled MCP servers
  (`.mcp.json` or an inline `mcpServers` object) route straight into otto's MCP client: discovery
  emits `${CLAUDE_PLUGIN_ROOT}`-expanded `PluginMcpServer` specs and the engine spawns + registers
  them via `connect_plugin_server` under namespaced gate names (`plugin__{ns}__{key}__{tool}`, so a
  bundled tool can never impersonate a built-in). `otto plugin marketplace add|remove|update|list`
  and `otto plugin install|uninstall|list` clone/manage marketplaces under
  `~/.claude/plugins/marketplaces/` (tracked in `~/.claude/plugins/marketplaces.lock.json`) and
  flip the `enabledPlugins` allowlist — a CLI-operator-only action, never agent-facing. Installing a
  plugin whose marketplace entry is a `Remote` source (its code lives outside the marketplace repo,
  not yet materializable) is still pending, as is a project-level (non-user-global) marketplace
  install and an interactive `/plugin` UX.
```

- [ ] **Step 5: Commit the doc update**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: mark the plugin marketplace install action as shipped"
```
