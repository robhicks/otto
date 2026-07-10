# Remote plugin source materialization — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `otto plugin install <plugin>@<marketplace>` work for github/git-sourced marketplace entries by cloning the remote into `~/.claude/plugins/repos/`, tracking it in a nested-format lockfile, and having `discover()` fold the materialized plugin.

**Architecture:** Pure descriptor parsing (`resolve_remote_source`) and the nested lockfile model live in the `extensions` crate; all disk/process I/O (git clone, repos-dir management) lives at the CLI edge in `crates/engine/src/plugin_cli.rs`, mirroring the existing `marketplace_add` staging→rename→cleanup pattern. Discovery gains a repos-path resolution branch for `Remote` sources; it stays lockfile-free (existence check only).

**Tech Stack:** Rust (edition 2024), `serde_json`, `anyhow`, `tokio::process` for git, `tempfile` for tests. Fully offline/deterministic tests using `file://` git remotes.

**Design doc:** `docs/superpowers/specs/2026-07-10-extensions-plugin-remote-source-design.md`

**Branch:** work continues on `plugin-remote-source` (already created; the spec is committed there).

---

## File Structure

- **`crates/extensions/src/marketplace_def.rs`** (modify): add `RemoteClone` type + `resolve_remote_source` (pure parse of a `Remote` descriptor → clone URL + optional ref).
- **`crates/extensions/src/marketplace_install.rs`** (modify): extend `MarketplaceLockfile` with a `plugins` map and a nested, back-compatible JSON format.
- **`crates/extensions/src/lib.rs`** (modify): export the two new `marketplace_def` items; add the `Remote`-source repos-path branch in `fold_plugins`.
- **`crates/engine/src/plugin_cli.rs`** (modify): `repos_dir` + `validate_path_component` helpers; `materialize_remote_plugin` clone helper; `plugin_install` branches on `Remote`; `marketplace_remove` cleans up repos + plugin-lock entries; extend the test harness for a remote-sourced plugin fixture.
- **`docs/ARCHITECTURE.md`**, **`CLAUDE.md`** (modify): drop the "remote materialization still pending" wording.

---

## Task 1: `resolve_remote_source` in the `extensions` crate

**Files:**
- Modify: `crates/extensions/src/marketplace_def.rs`
- Modify: `crates/extensions/src/lib.rs:30` (export)
- Test: `crates/extensions/src/marketplace_def.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/extensions/src/marketplace_def.rs`:

```rust
    #[test]
    fn resolve_github_source_builds_https_url() {
        let v = serde_json::json!({"source": "github", "repo": "acme/foo"});
        let rc = resolve_remote_source(&v).unwrap();
        assert_eq!(rc.url, "https://github.com/acme/foo");
        assert_eq!(rc.git_ref, None);
    }

    #[test]
    fn resolve_git_source_is_verbatim_url() {
        let v = serde_json::json!({"source": "git", "url": "https://x.example/y.git"});
        let rc = resolve_remote_source(&v).unwrap();
        assert_eq!(rc.url, "https://x.example/y.git");
        assert_eq!(rc.git_ref, None);
    }

    #[test]
    fn resolve_ref_precedence_commit_wins_then_tag_branch_ref() {
        let all = serde_json::json!({
            "source": "git", "url": "u",
            "ref": "r", "branch": "b", "tag": "t", "commit": "c"
        });
        assert_eq!(resolve_remote_source(&all).unwrap().git_ref.as_deref(), Some("c"));

        let no_commit = serde_json::json!({"source":"git","url":"u","ref":"r","branch":"b","tag":"t"});
        assert_eq!(resolve_remote_source(&no_commit).unwrap().git_ref.as_deref(), Some("t"));

        let only_ref = serde_json::json!({"source":"git","url":"u","ref":"r"});
        assert_eq!(resolve_remote_source(&only_ref).unwrap().git_ref.as_deref(), Some("r"));
    }

    #[test]
    fn resolve_unknown_kind_errors_naming_the_kind() {
        let v = serde_json::json!({"source": "gitlab", "repo": "a/b"});
        let e = resolve_remote_source(&v).unwrap_err();
        assert!(e.to_string().contains("gitlab"), "got: {e}");
    }

    #[test]
    fn resolve_github_rejects_malformed_repo() {
        for bad in [
            serde_json::json!({"source":"github","repo":"noslash"}),
            serde_json::json!({"source":"github","repo":"../escape/x"}),
            serde_json::json!({"source":"github","repo":"-flag/x"}),
            serde_json::json!({"source":"github"}),
        ] {
            assert!(resolve_remote_source(&bad).is_err(), "should reject {bad}");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions resolve_ -- --nocapture`
Expected: FAIL — `cannot find function resolve_remote_source in this scope`.

- [ ] **Step 3: Implement `RemoteClone` + `resolve_remote_source`**

Insert into `crates/extensions/src/marketplace_def.rs`, immediately after the `PluginSource` enum (around line 15):

```rust
/// A `PluginSource::Remote` descriptor resolved to a `git clone` target. Pure data — the CLI edge
/// consumes this to clone into the repos cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteClone {
    pub url: String,
    /// An optional pin (commit/tag/branch/ref) checked out after clone. `None` = the default branch.
    pub git_ref: Option<String>,
}

/// A single URL/path segment is safe iff it is non-empty, not `-`-prefixed (argv injection), not
/// `.`/`..`, and contains no `/` or `\`.
fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
}

/// Resolve a `Remote` plugin source (`{"source":"github","repo":"owner/name"}` or
/// `{"source":"git","url":"…"}`) to a clone URL plus optional ref. Pure — no I/O. Errors, naming
/// the unsupported shape, on an unknown `source` kind or a malformed descriptor.
///
/// Pin precedence, most-specific first: `commit` > `tag` > `branch` > `ref`.
pub fn resolve_remote_source(src: &Value) -> anyhow::Result<RemoteClone> {
    let obj = src
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("remote plugin source must be a JSON object"))?;
    let kind = obj
        .get("source")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("remote plugin source missing string `source` field"))?;

    let git_ref = ["commit", "tag", "branch", "ref"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let url = match kind {
        "github" => {
            let repo = obj
                .get("repo")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("github source missing string `repo` field"))?;
            let (owner, name) = repo.split_once('/').ok_or_else(|| {
                anyhow::anyhow!("github `repo` must be 'owner/name', got '{repo}'")
            })?;
            if !valid_segment(owner) || !valid_segment(name) {
                anyhow::bail!("github `repo` has an invalid segment: '{repo}'");
            }
            format!("https://github.com/{owner}/{name}")
        }
        "git" => obj
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("git source missing string `url` field"))?
            .to_string(),
        other => {
            anyhow::bail!("unsupported remote source kind '{other}' (supported: github, git)")
        }
    };

    Ok(RemoteClone { url, git_ref })
}
```

- [ ] **Step 4: Export the new items**

In `crates/extensions/src/lib.rs`, change line 30 from:

```rust
pub use marketplace_def::{Marketplace, MarketplaceEntry, PluginSource, parse_marketplace_json};
```

to:

```rust
pub use marketplace_def::{
    Marketplace, MarketplaceEntry, PluginSource, RemoteClone, parse_marketplace_json,
    resolve_remote_source,
};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions resolve_`
Expected: PASS (5 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/extensions/src/marketplace_def.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): resolve_remote_source parses github/git plugin descriptors"
```

---

## Task 2: Nested, back-compatible lockfile with a plugins map

**Files:**
- Modify: `crates/extensions/src/marketplace_install.rs`
- Test: `crates/extensions/src/marketplace_install.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/extensions/src/marketplace_install.rs`:

```rust
    #[test]
    fn nested_round_trip_with_marketplaces_and_plugins() {
        let mut lf = MarketplaceLockfile::default();
        lf.entries.insert(
            "acme".to_string(),
            MarketplaceLock {
                url: "u".to_string(),
                git_ref: "main".to_string(),
                commit: "c".to_string(),
                updated_at_unix: 1,
            },
        );
        lf.plugins.insert(
            "foo@acme".to_string(),
            MarketplaceLock {
                url: "g".to_string(),
                git_ref: "v1".to_string(),
                commit: "d".to_string(),
                updated_at_unix: 2,
            },
        );
        let back = MarketplaceLockfile::parse(&lf.to_json());
        assert_eq!(back, lf);
    }

    #[test]
    fn flat_legacy_format_parses_as_marketplaces_only() {
        let legacy = r#"{"acme":{"url":"u","ref":"main","commit":"c","updated_at_unix":1}}"#;
        let lf = MarketplaceLockfile::parse(legacy);
        assert_eq!(lf.entries.len(), 1);
        assert_eq!(lf.entries["acme"].git_ref, "main");
        assert!(lf.plugins.is_empty());
    }

    #[test]
    fn nested_with_only_plugins_section_parses() {
        let json = r#"{"plugins":{"foo@acme":{"url":"g","ref":"v1","commit":"d","updated_at_unix":2}}}"#;
        let lf = MarketplaceLockfile::parse(json);
        assert!(lf.entries.is_empty());
        assert_eq!(lf.plugins.len(), 1);
        assert_eq!(lf.plugins["foo@acme"].commit, "d");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions -- marketplace_install`
Expected: FAIL — `no field plugins on type MarketplaceLockfile`.

- [ ] **Step 3: Add the `plugins` field and a shared map parser**

In `crates/extensions/src/marketplace_install.rs`, change the struct (around line 26):

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarketplaceLockfile {
    /// Installed marketplaces, keyed by declared marketplace name.
    pub entries: BTreeMap<String, MarketplaceLock>,
    /// Materialized remote-sourced plugins, keyed by the `"<plugin>@<marketplace>"` enable-key.
    pub plugins: BTreeMap<String, MarketplaceLock>,
}
```

Add this free function just above the `impl MarketplaceLockfile` block:

```rust
/// Parse a `name -> lock` object into a `BTreeMap`, skipping any entry missing a required field
/// (matching every other `.claude/` reader in this crate — tolerant, never fatal).
fn parse_lock_map(map: Option<&serde_json::Map<String, Value>>) -> BTreeMap<String, MarketplaceLock> {
    let mut out = BTreeMap::new();
    let Some(map) = map else {
        return out;
    };
    for (name, v) in map {
        let (Some(url), Some(git_ref), Some(commit), Some(updated_at_unix)) = (
            v.get("url").and_then(|x| x.as_str()),
            v.get("ref").and_then(|x| x.as_str()),
            v.get("commit").and_then(|x| x.as_str()),
            v.get("updated_at_unix").and_then(|x| x.as_u64()),
        ) else {
            continue;
        };
        out.insert(
            name.clone(),
            MarketplaceLock {
                url: url.to_string(),
                git_ref: git_ref.to_string(),
                commit: commit.to_string(),
                updated_at_unix,
            },
        );
    }
    out
}
```

Replace the body of `parse` with:

```rust
    pub fn parse(json: &str) -> Self {
        let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
            return Self::default();
        };
        // Nested format: a top-level object carrying a `marketplaces` and/or `plugins` object.
        // Otherwise treat the whole object as the legacy flat `name -> lock` marketplaces map.
        // (Edge case: a legacy marketplace literally named "marketplaces"/"plugins" — accepted as
        // out of scope; these are single-user pre-release lockfiles.)
        let nested = root.get("marketplaces").map(Value::is_object).unwrap_or(false)
            || root.get("plugins").map(Value::is_object).unwrap_or(false);
        if nested {
            Self {
                entries: parse_lock_map(root.get("marketplaces").and_then(Value::as_object)),
                plugins: parse_lock_map(root.get("plugins").and_then(Value::as_object)),
            }
        } else {
            Self {
                entries: parse_lock_map(Some(&root)),
                plugins: BTreeMap::new(),
            }
        }
    }
```

Replace the body of `to_json` with:

```rust
    pub fn to_json(&self) -> String {
        let to_obj = |map: &BTreeMap<String, MarketplaceLock>| {
            let mut o = serde_json::Map::new();
            for (name, lock) in map {
                o.insert(
                    name.clone(),
                    serde_json::json!({
                        "url": lock.url,
                        "ref": lock.git_ref,
                        "commit": lock.commit,
                        "updated_at_unix": lock.updated_at_unix,
                    }),
                );
            }
            Value::Object(o)
        };
        let mut root = serde_json::Map::new();
        root.insert("marketplaces".to_string(), to_obj(&self.entries));
        root.insert("plugins".to_string(), to_obj(&self.plugins));
        serde_json::to_string_pretty(&Value::Object(root)).unwrap()
    }
```

- [ ] **Step 4: Run the whole crate's tests to verify pass + no regression**

Run: `cargo test -p otto-extensions`
Expected: PASS. In particular the existing `round_trips_through_json`, `parse_empty_or_malformed_is_empty`, `entry_missing_a_required_field_is_skipped`, and `to_json_sorts_keys` still pass (they exercise only `entries`, which now round-trips through the `marketplaces` section; the legacy flat inputs they parse still load via the back-compat branch).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/marketplace_install.rs
git commit -m "feat(extensions): nested lockfile with a materialized-plugins map (back-compatible)"
```

---

## Task 3: `repos_dir` + `validate_path_component` helpers in the CLI

**Files:**
- Modify: `crates/engine/src/plugin_cli.rs`
- Test: `crates/engine/src/plugin_cli.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/engine/src/plugin_cli.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine -- plugin_cli::tests::validate_path_component plugin_cli::tests::repos_dir`
Expected: FAIL — `cannot find function validate_path_component` / `repos_dir`.

- [ ] **Step 3: Add the helpers and refactor `validate_marketplace_name` to reuse one**

In `crates/engine/src/plugin_cli.rs`, replace the existing `validate_marketplace_name` (lines 55–63) with:

```rust
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

fn validate_marketplace_name(name: &str) -> anyhow::Result<()> {
    validate_path_component(name, "marketplace name")
}
```

Add next to the other path helpers (just after `marketplaces_dir`, around line 71):

```rust
fn repos_dir(home: &Path) -> PathBuf {
    home.join(".claude").join("plugins").join("repos")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine -- plugin_cli::tests::validate plugin_cli::tests::repos_dir`
Expected: PASS. The existing `validate_marketplace_name_rejects_path_escape_attempts` also still passes (behavior unchanged — it only asserts `is_ok`/`is_err`).

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/plugin_cli.rs
git commit -m "refactor(plugin-cli): repos_dir + shared validate_path_component helper"
```

---

## Task 4: Materialize a remote plugin on `plugin_install`

**Files:**
- Modify: `crates/engine/src/plugin_cli.rs`
- Test: `crates/engine/src/plugin_cli.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Add a test helper that builds a remote-sourced marketplace fixture**

In the `mod tests` block, add this helper next to `bare_marketplace_remote`. It builds a marketplace whose single plugin is `git`-sourced from a *second* bare repo (the plugin's own code), so install must clone that second repo. It returns the marketplace's `file://` URL and the plugin repo's `file://` URL, plus the tempdirs that must stay alive.

```rust
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
        run_git(psrc.path(), &["commit", "-m", "plugin"]).await.unwrap();
        let pbare = tempfile::tempdir().unwrap();
        run_git(pbare.path(), &["clone", "--bare", psrc.path().to_str().unwrap(), "."])
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
        run_git(msrc.path(), &["commit", "-m", "marketplace"]).await.unwrap();
        let mbare = tempfile::tempdir().unwrap();
        run_git(mbare.path(), &["clone", "--bare", msrc.path().to_str().unwrap(), "."])
            .await
            .unwrap();
        let mp_url = format!("file://{}", mbare.path().display());

        (vec![psrc, pbare, msrc, mbare], mp_url)
    }
```

- [ ] **Step 2: Write the failing integration test**

Add to the `mod tests` block:

```rust
    #[tokio::test]
    async fn plugin_install_materializes_a_remote_source() {
        let (_keep, mp_url) = bare_remote_plugin_marketplace("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&mp_url, None, home.path()).await.unwrap();

        plugin_install("foo@acme", home.path()).await.unwrap();

        // The plugin's code was cloned into the repos cache.
        let plugin_root = repos_dir(home.path()).join("acme").join("foo");
        assert!(
            plugin_root.join(".claude-plugin").join("plugin.json").exists(),
            "expected materialized plugin at {}",
            plugin_root.display()
        );
        assert!(plugin_root.join("commands").join("hello.md").exists());

        // It is recorded in the lockfile's plugins map and enabled in settings.
        let lock = read_lockfile(home.path());
        let entry = lock.plugins.get("foo@acme").expect("plugin locked");
        assert_eq!(entry.url, mp_url_of_plugin(&lock)); // see note below
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
```

NOTE on the first test: replace the `assert_eq!(entry.url, mp_url_of_plugin(&lock));` line with a direct check against the plugin repo URL. Since `bare_remote_plugin_marketplace` does not currently return the plugin URL, simplify that assertion to:

```rust
        assert!(entry.url.starts_with("file://"), "plugin url recorded: {}", entry.url);
```

Use that simplified assertion — do **not** introduce a `mp_url_of_plugin` helper (it does not exist).

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p otto-engine -- plugin_install_materializes plugin_install_remote_is_idempotent`
Expected: FAIL — `plugin_install` is not `async` (missing `.await` compiles differently) / it still bails on a remote source, so the clone/lock assertions fail.

- [ ] **Step 4: Implement `materialize_remote_plugin` and make `plugin_install` async + branch on Remote**

In `crates/engine/src/plugin_cli.rs`, add these two helpers (place them just above `plugin_install`, around line 377):

```rust
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
    let commit = run_git(dir, &["rev-parse", "HEAD"]).await?.trim().to_string();
    Ok((resolved_ref, commit))
}

/// Clone a remote-sourced plugin into `repos/<marketplace>/<plugin>/` and record it in the
/// lockfile's `plugins` map. Reuses an existing clone (no re-clone) if the directory is already
/// present. New clones use staging-dir → atomic rename → cleanup-on-failure, mirroring
/// `marketplace_add`; a lockfile-write failure removes a clone we created this call (never a reused
/// one).
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

    let (resolved_ref, commit) = if newly_created {
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
        head
    } else {
        read_repo_head(&final_path, rc.git_ref.as_deref()).await?
    };

    let mut lock = read_lockfile(home);
    lock.plugins.insert(
        key.to_string(),
        otto_extensions::MarketplaceLock {
            url: rc.url.clone(),
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
```

Replace the whole `plugin_install` function (lines 381–406) with:

```rust
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
```

- [ ] **Step 5: Update `plugin_install` call sites to `.await`**

Run: `grep -rn "plugin_install(" crates/engine/src/plugin_cli.rs`
Update every non-definition call to add `.await`:
- The dispatch in `cmd_plugin` (around line 483): `plugin_install(&key, &home).await?;`
- The existing end-to-end test `cmd_plugin_install_then_list_end_to_end` (around line 1147) if it calls `plugin_install`/`cmd_plugin` — `cmd_plugin` is already async so its call is unchanged; a direct `plugin_install(...)` call needs `.await`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p otto-engine -- plugin_install_materializes plugin_install_remote_is_idempotent cmd_plugin`
Expected: PASS (new tests + the existing `cmd_plugin_*` tests still green).

- [ ] **Step 7: Commit**

```bash
git add crates/engine/src/plugin_cli.rs
git commit -m "feat(plugin-cli): materialize github/git plugin sources on install"
```

---

## Task 5: Clean up materialized clones on `marketplace remove`

**Files:**
- Modify: `crates/engine/src/plugin_cli.rs`
- Test: `crates/engine/src/plugin_cli.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block:

```rust
    #[tokio::test]
    async fn marketplace_remove_cleans_up_materialized_plugins() {
        let (_keep, mp_url) = bare_remote_plugin_marketplace("acme", "foo").await;
        let home = tempfile::tempdir().unwrap();
        marketplace_add(&mp_url, None, home.path()).await.unwrap();
        plugin_install("foo@acme", home.path()).await.unwrap();

        assert!(repos_dir(home.path()).join("acme").exists());
        assert!(read_lockfile(home.path()).plugins.contains_key("foo@acme"));

        marketplace_remove("acme", home.path()).unwrap();

        assert!(!repos_dir(home.path()).join("acme").exists(), "repos tree removed");
        assert!(
            !read_lockfile(home.path()).plugins.contains_key("foo@acme"),
            "plugin lock entry dropped"
        );
        assert!(!read_lockfile(home.path()).entries.contains_key("acme"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine -- marketplace_remove_cleans_up_materialized_plugins`
Expected: FAIL — the repos tree and the `foo@acme` plugin lock entry survive removal.

- [ ] **Step 3: Extend `marketplace_remove`**

In `crates/engine/src/plugin_cli.rs`, replace the body of `marketplace_remove` (lines 266–280) with:

```rust
pub fn marketplace_remove(name: &str, home: &Path) -> anyhow::Result<()> {
    let mut lock = read_lockfile(home);
    if !lock.entries.contains_key(name) {
        anyhow::bail!("marketplace '{name}' is not installed");
    }

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
    let suffix = format!("@{name}");
    lock.plugins.retain(|k, _| !k.ends_with(&suffix));

    lock.entries.remove(name);
    write_lockfile(home, &lock)?;
    Ok(())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-engine -- marketplace_remove`
Expected: PASS (new test + existing `marketplace_remove*` tests green).

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/plugin_cli.rs
git commit -m "feat(plugin-cli): marketplace remove cleans up materialized plugin clones"
```

---

## Task 6: Discovery folds materialized remote plugins

**Files:**
- Modify: `crates/extensions/src/lib.rs` (the `Remote` branch in `fold_plugins`, lines 490–498)
- Test: `crates/extensions/src/lib.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/extensions/src/lib.rs`:

```rust
    #[test]
    fn discover_folds_a_materialized_remote_plugin() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let base = home.path().join(".claude").join("plugins");

        // Marketplace listing one git-remote plugin.
        let mp = base.join("marketplaces").join("acme").join(".claude-plugin");
        std::fs::create_dir_all(&mp).unwrap();
        std::fs::write(
            mp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"foo","source":{"source":"git","url":"file:///x"}}]}"#,
        )
        .unwrap();

        // Materialized plugin code in the repos cache, with one command.
        let proot = base.join("repos").join("acme").join("foo");
        std::fs::create_dir_all(proot.join(".claude-plugin")).unwrap();
        std::fs::write(proot.join(".claude-plugin").join("plugin.json"), r#"{"name":"foo"}"#)
            .unwrap();
        std::fs::create_dir_all(proot.join("commands")).unwrap();
        std::fs::write(proot.join("commands").join("hello.md"), "hi").unwrap();

        // Enable it.
        std::fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"enabledPlugins":{"foo@acme":true}}"#,
        )
        .unwrap();

        let ext = discover(project.path(), home.path());
        assert!(
            ext.commands.iter().any(|c| c.name == "foo:hello"),
            "expected foo:hello command, got: {:?}",
            ext.commands.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn discover_skips_an_unmaterialized_remote_plugin() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mp = home
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("acme")
            .join(".claude-plugin");
        std::fs::create_dir_all(&mp).unwrap();
        std::fs::write(
            mp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"foo","source":{"source":"git","url":"file:///x"}}]}"#,
        )
        .unwrap();
        std::fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"enabledPlugins":{"foo@acme":true}}"#,
        )
        .unwrap();

        // No repos/acme/foo — discovery must skip without folding anything.
        let ext = discover(project.path(), home.path());
        assert!(ext.commands.iter().all(|c| c.name != "foo:hello"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions -- discover_folds_a_materialized_remote_plugin discover_skips_an_unmaterialized_remote_plugin`
Expected: `discover_folds_…` FAILS (the current `Remote` branch warns-and-skips, so `foo:hello` is absent). `discover_skips_…` may already pass.

- [ ] **Step 3: Replace the `Remote` warn-skip with repos-path resolution**

In `crates/extensions/src/lib.rs`, replace the block that currently computes `rel`/`plugin_root` and warns on `Remote` (lines 490–506) with:

```rust
                let plugin_root = match &plugin.source {
                    PluginSource::LocalPath(p) => mp_dir.join(p.trim_start_matches("./")),
                    PluginSource::Remote(_) => base
                        .join(".claude")
                        .join("plugins")
                        .join("repos")
                        .join(&mp.name)
                        .join(&plugin.name),
                };
                if !plugin_root.is_dir() {
                    let hint = if matches!(plugin.source, PluginSource::Remote(_)) {
                        format!(" (run 'otto plugin install {key}')")
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "warning: skipping enabled plugin {key}: source dir {} not found{hint}",
                        plugin_root.display()
                    );
                    continue;
                }
```

The `fold_one_plugin(...)` call immediately below this block is unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions`
Expected: PASS (both new tests + no regression).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/lib.rs
git commit -m "feat(extensions): discovery folds materialized remote plugins from the repos cache"
```

---

## Task 7: Workspace check + docs

**Files:**
- Modify: `docs/ARCHITECTURE.md` (the "still pending" plugin-install sentence, ~line 354)
- Modify: `CLAUDE.md` (the Slice-5 Plan B deferral sentence mentioning "network install action")

- [ ] **Step 1: Full workspace build + test + lint**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets && cargo fmt --all -- --check`
Expected: all green. If `fmt --check` reports diffs, run `cargo fmt --all` and re-run.

- [ ] **Step 2: Update `docs/ARCHITECTURE.md`**

Find the sentence (around line 354):

```
Installing a
plugin whose marketplace entry is a `Remote` source (its code lives outside the marketplace repo,
not yet materializable) is still pending, as is a project-level (non-user-global) marketplace
install and an interactive `/plugin` UX.
```

Replace with:

```
Installing a plugin whose marketplace entry is a `Remote` source (github/git — its code lives
outside the marketplace repo) materializes it: `otto plugin install` clones the plugin repo into
`~/.claude/plugins/repos/<marketplace>/<plugin>/`, records it in the lockfile's `plugins` map, and
discovery folds it from there. A project-level (non-user-global) marketplace install and an
interactive `/plugin` UX are still pending.
```

- [ ] **Step 3: Update `CLAUDE.md`**

In the Slice 5 Plan B description, find the deferral clause:

```
The network install action (marketplace `git clone`, lockfile, `/plugin` UX), `model`/`allowed-tools` enforcement, and hook-wrapping of plugin MCP tools remain deferred
```

The marketplace `git clone`/lockfile shipped in the 2026-07-06 install slice, and remote plugin-source materialization now ships too. Update the clause to:

```
An interactive `/plugin` UX and project-level marketplace installs, plus `model`/`allowed-tools` enforcement and hook-wrapping of plugin MCP tools, remain deferred (marketplace install/lockfile and github/git remote plugin-source materialization are shipped)
```

- [ ] **Step 4: Commit**

```bash
git add docs/ARCHITECTURE.md CLAUDE.md
git commit -m "docs: remote plugin-source materialization is shipped"
```

---

## Done criteria

- `cargo test --workspace` green; `cargo clippy --workspace --all-targets` clean; `cargo fmt --all -- --check` clean.
- `otto plugin install foo@acme` clones a github/git-sourced plugin into `~/.claude/plugins/repos/`, records it in the nested lockfile's `plugins` map, and flips the enable bit; a local-path plugin still only flips the bit.
- `otto plugin marketplace remove acme` deletes that marketplace's `repos/` tree and its plugin lock entries.
- `discover()` folds a materialized remote plugin's artifacts (namespaced `foo:…`), and warns-and-skips an enabled-but-unmaterialized one.
- Legacy flat lockfiles still load (back-compat).
- All tests offline/deterministic (`file://` remotes only) — the workspace determinism invariant holds.
