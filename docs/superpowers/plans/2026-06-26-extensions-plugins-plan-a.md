# Extensions Plugins — Plan A (marketplace + static components) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Discover Claude Code plugins from on-disk marketplaces, gate them by the `enabledPlugins` allowlist, and fold each enabled plugin's `agents`/`commands`/`skills`/`hooks` — namespaced by plugin name, with `${CLAUDE_PLUGIN_ROOT}` expanded — into the existing `Extensions` so they register through the paths that already exist.

**Architecture:** All work lands in the hermetic `otto-extensions` crate. Two new pure-parse modules (`marketplace_def`, `plugin_def`) plus three helpers and a folding pass added to `discover()` in `lib.rs`. Plugin contributions reuse the existing `parse_agent_md`/`parse_command_md`/`parse_skill_md`/`parse_hooks` parsers and the existing `read_*_dir` directory readers — there is no new artifact format. The crate spawns nothing; bundled MCP servers (which require engine spawning) are Plan B. Because folded artifacts flow into the same `Extensions` vectors/`HookSet` the engine already consumes, **Plan A needs no new engine code** — only a confirming integration test and doc updates.

**Tech Stack:** Rust (edition 2024), `serde_json` for manifest parsing, `tempfile` for hermetic discovery tests, `anyhow` for fallible parses.

---

## File Structure

- **Create `crates/extensions/src/marketplace_def.rs`** — `Marketplace`, `MarketplaceEntry`, `PluginSource`, and `parse_marketplace_json`. Sole responsibility: parse `.claude-plugin/marketplace.json`.
- **Create `crates/extensions/src/plugin_def.rs`** — `PluginManifest` and `parse_plugin_json`. Sole responsibility: parse `.claude-plugin/plugin.json` (component path overrides; no MCP field this plan).
- **Modify `crates/extensions/src/lib.rs`** — declare/re-export the two new modules; add `parse_enabled_plugins`, `expand_plugin_root`, the private `read_enabled_plugins`/`read_plugin_hooks`/`fold_plugins`/`fold_one_plugin` helpers, and a `fold_plugins(...)` call at the end of `discover()`.
- **Modify `crates/engine/src/main.rs`** — add one sandbox-gated integration test proving an enabled plugin's `PreToolUse` hook blocks a tool call through the existing `register_hooks` path. No production code change.
- **Modify `docs/ARCHITECTURE.md` and `CLAUDE.md`** — update the plugins compatibility row / extensions paragraph to record Plan A's shipped behavior.

---

## Task 1: `marketplace_def` — parse `marketplace.json`

**Files:**
- Create: `crates/extensions/src/marketplace_def.rs`
- Modify: `crates/extensions/src/lib.rs` (module declaration + re-export)

- [ ] **Step 1: Write the failing tests**

Create `crates/extensions/src/marketplace_def.rs` with only the test module first (the types/function come next step but write them now so the file compiles after step 3; here, write the whole file including the impl so step 2 fails for the right reason — "module not declared"). To keep TDD honest, write the file with types + tests but an unimplemented body:

```rust
//! Parses a Claude Code marketplace manifest (`.claude-plugin/marketplace.json`): the marketplace
//! `name` plus the list of plugins it offers. A plugin's `source` is either a local path (relative
//! to the marketplace root, resolvable on disk) or a remote descriptor (not materialized by this
//! slice). No I/O here — pure parsing.

use serde_json::Value;

/// Where a plugin's files live, per its marketplace entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// A path relative to the marketplace root, e.g. `./plugins/foo`.
    LocalPath(String),
    /// A remote source (github/git/…). Kept verbatim; this slice does not materialize it.
    Remote(Value),
}

/// One plugin offered by a marketplace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceEntry {
    pub name: String,
    pub source: PluginSource,
    pub description: Option<String>,
}

/// A parsed `marketplace.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marketplace {
    pub name: String,
    pub plugins: Vec<MarketplaceEntry>,
}

/// Parse a `marketplace.json` document. Errors on invalid JSON or a missing top-level
/// `name`/`plugins`. An empty `plugins` array is valid. A plugin entry missing a non-empty `name`
/// or a usable `source` is skipped. A string `source` is a `LocalPath`; an object `source` is
/// `Remote`; any other `source` shape skips the entry.
pub fn parse_marketplace_json(json: &str) -> anyhow::Result<Marketplace> {
    let v: Value = serde_json::from_str(json)?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("marketplace.json missing `name`"))?
        .to_string();
    let plugins_val = v
        .get("plugins")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow::anyhow!("marketplace.json missing `plugins` array"))?;

    let mut plugins = Vec::new();
    for entry in plugins_val {
        let Some(pname) = entry
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(src_val) = entry.get("source") else {
            continue;
        };
        let source = match src_val {
            Value::String(s) if !s.is_empty() => PluginSource::LocalPath(s.clone()),
            Value::Object(_) => PluginSource::Remote(src_val.clone()),
            _ => continue,
        };
        let description = entry
            .get("description")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string());
        plugins.push(MarketplaceEntry {
            name: pname.to_string(),
            source,
            description,
        });
    }
    Ok(Marketplace { name, plugins })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_local_and_remote_sources() {
        let json = r#"{
            "name": "acme",
            "owner": { "name": "x" },
            "plugins": [
                { "name": "foo", "source": "./plugins/foo", "description": "d" },
                { "name": "bar", "source": { "source": "github", "repo": "acme/bar" } }
            ]
        }"#;
        let mp = parse_marketplace_json(json).unwrap();
        assert_eq!(mp.name, "acme");
        assert_eq!(mp.plugins.len(), 2);
        assert_eq!(mp.plugins[0].name, "foo");
        assert_eq!(
            mp.plugins[0].source,
            PluginSource::LocalPath("./plugins/foo".to_string())
        );
        assert_eq!(mp.plugins[0].description.as_deref(), Some("d"));
        assert!(matches!(mp.plugins[1].source, PluginSource::Remote(_)));
    }

    #[test]
    fn empty_plugins_is_ok() {
        let mp = parse_marketplace_json(r#"{"name":"acme","plugins":[]}"#).unwrap();
        assert!(mp.plugins.is_empty());
    }

    #[test]
    fn missing_name_or_plugins_errors() {
        assert!(parse_marketplace_json(r#"{"plugins":[]}"#).is_err());
        assert!(parse_marketplace_json(r#"{"name":"acme"}"#).is_err());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_marketplace_json("{ not json").is_err());
    }

    #[test]
    fn entry_missing_name_or_source_is_skipped() {
        let json = r#"{"name":"acme","plugins":[
            { "source": "./x" },
            { "name": "ok", "source": "./ok" },
            { "name": "nosrc" }
        ]}"#;
        let mp = parse_marketplace_json(json).unwrap();
        let names: Vec<_> = mp.plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["ok"]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (module not declared)**

Run: `cargo test -p otto-extensions marketplace_def::tests`
Expected: FAIL to compile — `marketplace_def` is not yet a module of the crate.

- [ ] **Step 3: Declare and re-export the module in `lib.rs`**

In `crates/extensions/src/lib.rs`, add to the `mod` block (keep alphabetical with the existing list) after `mod hooked_tool;`:

```rust
mod marketplace_def;
```

And add to the `pub use` block after the `hooked_tool` re-export:

```rust
pub use marketplace_def::{Marketplace, MarketplaceEntry, PluginSource, parse_marketplace_json};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions marketplace_def::tests`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/marketplace_def.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): parse marketplace.json (plugins slice A)"
```

---

## Task 2: `plugin_def` — parse `plugin.json`

**Files:**
- Create: `crates/extensions/src/plugin_def.rs`
- Modify: `crates/extensions/src/lib.rs` (module declaration + re-export)

- [ ] **Step 1: Write the file with types, impl, and tests**

Create `crates/extensions/src/plugin_def.rs`:

```rust
//! Parses a Claude Code plugin manifest (`.claude-plugin/plugin.json`): the plugin `name` plus
//! optional component path overrides. An omitted component field means "use the convention dir"
//! (resolved by discovery, not here). Bundled MCP servers (`mcpServers`) are Plan B and are not
//! parsed in this module yet. Pure parsing — no I/O.

use serde_json::Value;

/// A parsed `plugin.json`. Each `Option<String>` component field is a path **relative to the
/// plugin root**; `None` means discovery falls back to the convention directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub commands: Option<String>,
    pub agents: Option<String>,
    pub skills: Option<String>,
    pub hooks: Option<String>,
}

/// Parse a `plugin.json` document. Errors on invalid JSON or a missing/empty `name`. Component
/// path overrides are read when present and non-empty; unknown keys (`author`, `homepage`, …) and
/// `mcpServers` (Plan B) are ignored.
pub fn parse_plugin_json(json: &str) -> anyhow::Result<PluginManifest> {
    let v: Value = serde_json::from_str(json)?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("plugin.json missing `name`"))?
        .to_string();
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    Ok(PluginManifest {
        name,
        version: s("version"),
        description: s("description"),
        commands: s("commands"),
        agents: s("agents"),
        skills: s("skills"),
        hooks: s("hooks"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_overrides() {
        let json = r#"{
            "name": "foo", "version": "1.0.0", "description": "d",
            "author": { "name": "x" },
            "commands": "./cmds", "agents": "./ag", "skills": "./sk", "hooks": "./h/hooks.json"
        }"#;
        let m = parse_plugin_json(json).unwrap();
        assert_eq!(m.name, "foo");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.description.as_deref(), Some("d"));
        assert_eq!(m.commands.as_deref(), Some("./cmds"));
        assert_eq!(m.agents.as_deref(), Some("./ag"));
        assert_eq!(m.skills.as_deref(), Some("./sk"));
        assert_eq!(m.hooks.as_deref(), Some("./h/hooks.json"));
    }

    #[test]
    fn absent_components_are_none() {
        let m = parse_plugin_json(r#"{"name":"foo"}"#).unwrap();
        assert_eq!(m.commands, None);
        assert_eq!(m.agents, None);
        assert_eq!(m.skills, None);
        assert_eq!(m.hooks, None);
        assert_eq!(m.version, None);
    }

    #[test]
    fn missing_name_errors() {
        assert!(parse_plugin_json(r#"{"version":"1.0.0"}"#).is_err());
        assert!(parse_plugin_json(r#"{"name":""}"#).is_err());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_plugin_json("not json").is_err());
    }

    #[test]
    fn mcp_servers_field_is_ignored_this_plan() {
        // Plan A does not parse mcpServers; presence must not break parsing.
        let m = parse_plugin_json(r#"{"name":"foo","mcpServers":{"s":{}}}"#).unwrap();
        assert_eq!(m.name, "foo");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (module not declared)**

Run: `cargo test -p otto-extensions plugin_def::tests`
Expected: FAIL to compile — `plugin_def` is not yet a module.

- [ ] **Step 3: Declare and re-export the module in `lib.rs`**

In `crates/extensions/src/lib.rs`, add to the `mod` block after `mod plugin_def;`'s alphabetical neighbor (place it after `mod markdown_agent;`):

```rust
mod plugin_def;
```

And to the `pub use` block (after the `markdown_agent` re-export):

```rust
pub use plugin_def::{PluginManifest, parse_plugin_json};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions plugin_def::tests`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/plugin_def.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): parse plugin.json manifest (plugins slice A)"
```

---

## Task 3: `parse_enabled_plugins` + `expand_plugin_root` helpers

**Files:**
- Modify: `crates/extensions/src/lib.rs` (add two public helpers + their tests)

- [ ] **Step 1: Write the failing tests**

In `crates/extensions/src/lib.rs`, inside the existing `#[cfg(test)] mod tests { ... }` block, add these tests (append near the end, before the closing `}` of the module):

```rust
    #[test]
    fn parse_enabled_plugins_reads_bool_map() {
        let map = parse_enabled_plugins(r#"{"enabledPlugins":{"foo@acme":true,"bar@acme":false}}"#);
        assert_eq!(map.get("foo@acme"), Some(&true));
        assert_eq!(map.get("bar@acme"), Some(&false));
    }

    #[test]
    fn parse_enabled_plugins_missing_is_empty() {
        assert!(parse_enabled_plugins(r#"{}"#).is_empty());
        assert!(parse_enabled_plugins("not json").is_empty());
        // non-bool values are ignored
        assert!(parse_enabled_plugins(r#"{"enabledPlugins":{"foo@acme":"yes"}}"#).is_empty());
    }

    #[test]
    fn expand_plugin_root_substitutes_all_occurrences() {
        let root = Path::new("/abs/plugin");
        let out = expand_plugin_root("${CLAUDE_PLUGIN_ROOT}/a ${CLAUDE_PLUGIN_ROOT}/b", root);
        assert_eq!(out, "/abs/plugin/a /abs/plugin/b");
        assert_eq!(expand_plugin_root("no token", root), "no token");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions tests::parse_enabled_plugins -- --exact` (and the expand test)
Expected: FAIL to compile — `parse_enabled_plugins` / `expand_plugin_root` are not defined.

- [ ] **Step 3: Implement both helpers**

In `crates/extensions/src/lib.rs`, add these two public functions at module scope (just below the `discover` function, above the private `read_agents_dir`):

```rust
/// Read the `enabledPlugins` allowlist from a `settings.json` document: a map of
/// `"<plugin>@<marketplace>"` → bool. A missing object, invalid JSON, or non-bool values yield an
/// empty map. A plugin activates only when its key maps to `true`.
pub fn parse_enabled_plugins(settings_json: &str) -> std::collections::BTreeMap<String, bool> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(settings_json) else {
        return out;
    };
    if let Some(obj) = v.get("enabledPlugins").and_then(|e| e.as_object()) {
        for (k, val) in obj {
            if let Some(b) = val.as_bool() {
                out.insert(k.clone(), b);
            }
        }
    }
    out
}

/// Replace every literal `${CLAUDE_PLUGIN_ROOT}` in `s` with `plugin_root`'s path. A textual
/// substitution only — it never reads the environment, preserving hermetic determinism.
pub fn expand_plugin_root(s: &str, plugin_root: &Path) -> String {
    s.replace("${CLAUDE_PLUGIN_ROOT}", &plugin_root.to_string_lossy())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions tests::parse_enabled tests::expand_plugin`
Expected: PASS (3 new tests).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/lib.rs
git commit -m "feat(extensions): enabledPlugins allowlist + CLAUDE_PLUGIN_ROOT expansion (plugins slice A)"
```

---

## Task 4: Fold enabled plugins into `discover()` (happy path)

**Files:**
- Modify: `crates/extensions/src/lib.rs` (private helpers + `discover` call + happy-path test)

- [ ] **Step 1: Write the failing happy-path test**

In `crates/extensions/src/lib.rs` test module, first add a shared layout helper, then the test. Add this helper alongside the other `write_*` helpers in the test module:

```rust
    /// Lay out, under `<base>/.claude/plugins/marketplaces/<mp>/`, a marketplace offering one
    /// local plugin `<plugin>` that bundles a command, an agent, a skill, and a PreToolUse hook
    /// (whose command references ${CLAUDE_PLUGIN_ROOT}).
    fn write_plugin_marketplace(base: &Path, mp: &str, plugin: &str) {
        let mp_dir = base
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join(mp);
        let cp = mp_dir.join(".claude-plugin");
        fs::create_dir_all(&cp).unwrap();
        fs::write(
            cp.join("marketplace.json"),
            format!(
                r#"{{"name":"{mp}","plugins":[{{"name":"{plugin}","source":"./plugins/{plugin}"}}]}}"#
            ),
        )
        .unwrap();

        let proot = mp_dir.join("plugins").join(plugin);
        let pcp = proot.join(".claude-plugin");
        fs::create_dir_all(&pcp).unwrap();
        fs::write(
            pcp.join("plugin.json"),
            format!(r#"{{"name":"{plugin}","version":"1.0.0"}}"#),
        )
        .unwrap();

        let cmds = proot.join("commands");
        fs::create_dir_all(&cmds).unwrap();
        fs::write(cmds.join("hello.md"), "Say hi $ARGUMENTS\n").unwrap();

        let agents = proot.join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(
            agents.join("helper.md"),
            "---\nname: helper\ndescription: helps\n---\nbody\n",
        )
        .unwrap();

        let skill = proot.join("skills").join("greet");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "---\ndescription: greets\n---\ninstructions\n").unwrap();

        let hooks_dir = proot.join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        fs::write(
            hooks_dir.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"${CLAUDE_PLUGIN_ROOT}/check.sh"}]}]}}"#,
        )
        .unwrap();
    }

    /// Write `<base>/.claude/settings.json` enabling `key` (e.g. "foo@acme").
    fn enable_plugin(base: &Path, key: &str) {
        let claude = base.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(
            claude.join("settings.json"),
            format!(r#"{{"enabledPlugins":{{"{key}":true}}}}"#),
        )
        .unwrap();
    }
```

Now the test:

```rust
    #[test]
    fn enabled_plugin_contributes_namespaced_artifacts() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_plugin_marketplace(proj.path(), "acme", "foo");
        enable_plugin(proj.path(), "foo@acme");

        let ext = discover(proj.path(), home.path());

        assert!(
            ext.commands.iter().any(|c| c.name == "foo:hello"),
            "commands: {:?}",
            ext.commands.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        assert!(ext.agents.iter().any(|a| a.name == "foo:helper"));
        assert!(ext.skills.iter().any(|s| s.name == "foo:greet"));

        // The plugin's PreToolUse hook is appended, with ${CLAUDE_PLUGIN_ROOT} expanded.
        assert_eq!(ext.hooks.pre_tool_use.len(), 1);
        let cmd = &ext.hooks.pre_tool_use[0].hooks[0].command;
        assert!(cmd.ends_with("/check.sh"), "got: {cmd}");
        assert!(!cmd.contains("${CLAUDE_PLUGIN_ROOT}"), "not expanded: {cmd}");
        assert!(cmd.contains("foo"), "expansion should include plugin dir: {cmd}");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-extensions tests::enabled_plugin_contributes`
Expected: FAIL to compile — `write_plugin_marketplace`/`enable_plugin` reference no `fold_plugins`; the assertions fail because `discover` does not yet fold plugins.

- [ ] **Step 3: Implement the folding helpers and wire them into `discover`**

In `crates/extensions/src/lib.rs`, add these private helpers at module scope (place them just after the existing private `read_settings_hooks` function):

```rust
/// Read the `enabledPlugins` allowlist from `<base>/.claude/settings.json`. Missing/unreadable →
/// empty map (never fatal).
fn read_enabled_plugins(path: &Path) -> std::collections::BTreeMap<String, bool> {
    match std::fs::read_to_string(path) {
        Ok(t) => parse_enabled_plugins(&t),
        Err(_) => std::collections::BTreeMap::new(),
    }
}

/// Read a plugin's bundled hooks file (same `{ "hooks": { ... } }` shape as settings.json).
/// Missing → empty; unreadable/malformed → empty with a warning (reuses the settings reader).
fn read_plugin_hooks(path: &Path) -> HookSet {
    read_settings_hooks(path)
}

/// Resolve a component path: the manifest override (with a leading `./` trimmed) or the convention
/// default, joined onto the plugin root.
fn component_dir(plugin_root: &Path, over: &Option<String>, default: &str) -> std::path::PathBuf {
    let rel = over.as_deref().unwrap_or(default).trim_start_matches("./");
    plugin_root.join(rel)
}

/// Fold one enabled plugin's bundled artifacts into the in-progress maps/hook set, namespaced by
/// `ns` (the plugin name). Existing (user/project, or earlier-plugin) entries win on a name
/// collision (`or_insert`). Hooks are additive with `${CLAUDE_PLUGIN_ROOT}` expanded.
fn fold_one_plugin(
    ns: &str,
    plugin_root: &Path,
    agents: &mut std::collections::BTreeMap<String, CustomAgentDef>,
    commands: &mut std::collections::BTreeMap<String, CustomCommandDef>,
    skills: &mut std::collections::BTreeMap<String, CustomSkillDef>,
    hooks: &mut HookSet,
) {
    let manifest_path = plugin_root.join(".claude-plugin").join("plugin.json");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => match parse_plugin_json(&t) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "warning: skipping malformed plugin manifest {}: {e}",
                    manifest_path.display()
                );
                return;
            }
        },
        Err(e) => {
            eprintln!(
                "warning: skipping plugin (unreadable {}): {e}",
                manifest_path.display()
            );
            return;
        }
    };

    for mut def in read_agents_dir(&component_dir(plugin_root, &manifest.agents, "agents")) {
        def.name = format!("{ns}:{}", def.name);
        agents.entry(def.name.clone()).or_insert(def);
    }
    for mut def in read_commands_dir(&component_dir(plugin_root, &manifest.commands, "commands")) {
        def.name = format!("{ns}:{}", def.name);
        commands.entry(def.name.clone()).or_insert(def);
    }
    for mut def in read_skills_dir(&component_dir(plugin_root, &manifest.skills, "skills")) {
        def.name = format!("{ns}:{}", def.name);
        skills.entry(def.name.clone()).or_insert(def);
    }

    let mut plugin_hooks =
        read_plugin_hooks(&component_dir(plugin_root, &manifest.hooks, "hooks/hooks.json"));
    for m in plugin_hooks
        .pre_tool_use
        .iter_mut()
        .chain(plugin_hooks.post_tool_use.iter_mut())
    {
        for h in &mut m.hooks {
            h.command = expand_plugin_root(&h.command, plugin_root);
        }
    }
    hooks.pre_tool_use.append(&mut plugin_hooks.pre_tool_use);
    hooks.post_tool_use.append(&mut plugin_hooks.post_tool_use);
}

/// Discover enabled plugins from on-disk marketplaces under both bases and fold their bundled
/// artifacts into the maps/hook set. Plugins are lowest precedence (user/project win); only plugins
/// whose `"<name>@<marketplace>"` key is enabled (`true`) activate. Remote sources and absent local
/// dirs are skipped with a warning; malformed marketplace/plugin manifests are skipped, never fatal.
fn fold_plugins(
    home: &Path,
    project_root: &Path,
    agents: &mut std::collections::BTreeMap<String, CustomAgentDef>,
    commands: &mut std::collections::BTreeMap<String, CustomCommandDef>,
    skills: &mut std::collections::BTreeMap<String, CustomSkillDef>,
    hooks: &mut HookSet,
) {
    // Merge the enable allowlist (project overrides user for the same key).
    let mut enabled = std::collections::BTreeMap::new();
    for base in [home, project_root] {
        for (k, v) in read_enabled_plugins(&base.join(".claude").join("settings.json")) {
            enabled.insert(k, v);
        }
    }

    for base in [home, project_root] {
        let mp_root = base
            .join(".claude")
            .join("plugins")
            .join("marketplaces");
        let entries = match std::fs::read_dir(&mp_root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let mp_dir = entry.path();
            if !mp_dir.is_dir() {
                continue;
            }
            let mp_manifest = mp_dir.join(".claude-plugin").join("marketplace.json");
            let text = match std::fs::read_to_string(&mp_manifest) {
                Ok(t) => t,
                // No marketplace.json → this subdir simply isn't a marketplace (silent).
                Err(_) => continue,
            };
            let mp = match parse_marketplace_json(&text) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "warning: skipping malformed marketplace {}: {e}",
                        mp_manifest.display()
                    );
                    continue;
                }
            };
            for plugin in &mp.plugins {
                let key = format!("{}@{}", plugin.name, mp.name);
                if enabled.get(&key).copied() != Some(true) {
                    continue;
                }
                let rel = match &plugin.source {
                    PluginSource::LocalPath(p) => p,
                    PluginSource::Remote(_) => {
                        eprintln!(
                            "warning: skipping enabled plugin {key}: remote source not materialized on disk"
                        );
                        continue;
                    }
                };
                let plugin_root = mp_dir.join(rel.trim_start_matches("./"));
                if !plugin_root.is_dir() {
                    eprintln!(
                        "warning: skipping enabled plugin {key}: source dir {} not found",
                        plugin_root.display()
                    );
                    continue;
                }
                fold_one_plugin(&plugin.name, &plugin_root, agents, commands, skills, hooks);
            }
        }
    }
}
```

Then, in `discover`, add the fold call immediately before the `Extensions { ... }` construction:

```rust
    fold_plugins(home, project_root, &mut agents, &mut commands, &mut skills, &mut hooks);
    Extensions {
        agents: agents.into_values().collect(),
        commands: commands.into_values().collect(),
        skills: skills.into_values().collect(),
        hooks,
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-extensions tests::enabled_plugin_contributes`
Expected: PASS.

- [ ] **Step 5: Run the full crate suite (no regressions)**

Run: `cargo test -p otto-extensions`
Expected: PASS (all existing + new tests).

- [ ] **Step 6: Commit**

```bash
git add crates/extensions/src/lib.rs
git commit -m "feat(extensions): fold enabled-plugin agents/commands/skills/hooks into discover (plugins slice A)"
```

---

## Task 5: Enable-gating, precedence, and robustness tests

**Files:**
- Modify: `crates/extensions/src/lib.rs` (tests only — exercises Task 4 code)

- [ ] **Step 1: Write the gating + precedence + robustness tests**

In `crates/extensions/src/lib.rs` test module, add:

```rust
    #[test]
    fn plugin_not_enabled_contributes_nothing() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_plugin_marketplace(proj.path(), "acme", "foo");
        // No enable_plugin → "foo@acme" is not enabled.

        let ext = discover(proj.path(), home.path());
        assert!(ext.commands.iter().all(|c| !c.name.starts_with("foo:")));
        assert!(ext.agents.iter().all(|a| !a.name.starts_with("foo:")));
        assert!(ext.skills.is_empty());
        assert!(ext.hooks.is_empty());
    }

    #[test]
    fn enabled_false_contributes_nothing() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_plugin_marketplace(proj.path(), "acme", "foo");
        fs::write(
            proj.path().join(".claude").join("settings.json"),
            r#"{"enabledPlugins":{"foo@acme":false}}"#,
        )
        .unwrap();

        let ext = discover(proj.path(), home.path());
        assert!(ext.skills.is_empty());
        assert!(ext.commands.iter().all(|c| !c.name.starts_with("foo:")));
    }

    #[test]
    fn project_command_wins_over_plugin_namespaced_collision() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        // A project command whose namespaced name is exactly "foo:hello".
        write_command(proj.path(), "foo/hello.md", "PROJECT WINS\n");
        write_plugin_marketplace(proj.path(), "acme", "foo"); // plugin command hello → foo:hello
        enable_plugin(proj.path(), "foo@acme");

        let ext = discover(proj.path(), home.path());
        let hits: Vec<_> = ext.commands.iter().filter(|c| c.name == "foo:hello").collect();
        assert_eq!(hits.len(), 1, "collision must collapse to one");
        assert_eq!(hits[0].template.trim(), "PROJECT WINS");
    }

    #[test]
    fn remote_source_enabled_is_skipped_not_fatal() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        let mp_dir = proj
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("acme");
        let cp = mp_dir.join(".claude-plugin");
        fs::create_dir_all(&cp).unwrap();
        fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"rem","source":{"source":"github","repo":"a/b"}}]}"#,
        )
        .unwrap();
        enable_plugin(proj.path(), "rem@acme");

        // Must not panic; contributes nothing.
        let ext = discover(proj.path(), home.path());
        assert!(ext.commands.iter().all(|c| !c.name.starts_with("rem:")));
        assert!(ext.skills.is_empty());
    }

    #[test]
    fn malformed_marketplace_json_skipped_not_fatal() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        let cp = proj
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("acme")
            .join(".claude-plugin");
        fs::create_dir_all(&cp).unwrap();
        fs::write(cp.join("marketplace.json"), "{ not json").unwrap();
        enable_plugin(proj.path(), "foo@acme");

        let ext = discover(proj.path(), home.path());
        assert!(ext.skills.is_empty());
    }

    #[test]
    fn user_marketplace_plugin_enabled_via_user_settings() {
        // A plugin installed under the HOME base, enabled by HOME settings, must activate.
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_plugin_marketplace(home.path(), "acme", "foo");
        enable_plugin(home.path(), "foo@acme");

        let ext = discover(proj.path(), home.path());
        assert!(ext.skills.iter().any(|s| s.name == "foo:greet"));
    }
```

- [ ] **Step 2: Run the new tests to verify they pass**

Run: `cargo test -p otto-extensions tests::plugin_not_enabled tests::enabled_false tests::project_command_wins tests::remote_source tests::malformed_marketplace tests::user_marketplace`
Expected: PASS (6 tests). If any fail, fix the Task 4 logic they expose (do not weaken the test).

- [ ] **Step 3: Run the full crate suite**

Run: `cargo test -p otto-extensions`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/extensions/src/lib.rs
git commit -m "test(extensions): plugin enable-gating, precedence, and robustness (plugins slice A)"
```

---

## Task 6: Engine integration — a plugin's PreToolUse hook blocks a call

**Files:**
- Modify: `crates/engine/src/main.rs` (test only — no production change)

- [ ] **Step 1: Write the sandbox-gated integration test**

In `crates/engine/src/main.rs`, inside the existing `#[cfg(test)] mod tests { ... }` block (alongside `discovered_pretooluse_hook_blocks_a_tool_call`), add:

```rust
    #[tokio::test]
    async fn enabled_plugin_pretooluse_hook_blocks_a_tool_call() {
        use otto_engine_core::tool::ToolRegistry;
        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping plugin hook blocking test: no OS sandbox backend");
            return;
        }
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        // A marketplace under the project base offering one local plugin whose bundled hooks.json
        // blocks fs.read; enabled via project settings.json.
        let mp_dir = proj
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("acme");
        let cp = mp_dir.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"guard","source":"./plugins/guard"}]}"#,
        )
        .unwrap();
        let proot = mp_dir.join("plugins").join("guard");
        let pcp = proot.join(".claude-plugin");
        std::fs::create_dir_all(&pcp).unwrap();
        std::fs::write(pcp.join("plugin.json"), r#"{"name":"guard"}"#).unwrap();
        let hooks_dir = proot.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"fs.read","hooks":[{"type":"command","command":"exit 2"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            proj.path().join(".claude").join("settings.json"),
            r#"{"enabledPlugins":{"guard@acme":true}}"#,
        )
        .unwrap();

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let mut tools: ToolRegistry =
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf());
        let ext = otto_extensions::discover(proj.path(), home.path());
        super::register_hooks(&mut tools, &ext.hooks, proj.path());

        let err = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("blocked by PreToolUse hook"),
            "got: {err}"
        );
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p otto-engine enabled_plugin_pretooluse_hook_blocks_a_tool_call`
Expected: PASS where an OS sandbox backend exists; otherwise the test prints "skipping…" and returns (still PASS). Confirm the print or the assertion in the output.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "test(engine): enabled-plugin PreToolUse hook blocks a tool call (plugins slice A)"
```

---

## Task 7: Docs + full-workspace verification

**Files:**
- Modify: `docs/ARCHITECTURE.md` (plugins compatibility row)
- Modify: `CLAUDE.md` (extensions paragraph)

- [ ] **Step 1: Update `docs/ARCHITECTURE.md`**

Replace the plugins bullet in the "Claude Code compatibility" list (the line beginning `- plugins (\`.claude-plugin/plugin.json\`) →`) with:

```markdown
- plugins (`.claude-plugin/plugin.json`) → discovered from on-disk marketplaces under
  `.claude/plugins/marketplaces/`, gated by the `enabledPlugins` allowlist in `settings.json`, and
  each enabled plugin's bundled `agents`/`commands`/`skills`/`hooks` folded into the rows above —
  **namespaced by plugin name** (`foo:commit`), lowest precedence (user/project win),
  `${CLAUDE_PLUGIN_ROOT}` expanded in hook commands. Bundled MCP servers (route straight into otto's
  MCP client) and the network *install action* (marketplace `git clone`, lockfile) are pending.
```

- [ ] **Step 2: Update `CLAUDE.md`**

In the `extensions` crate row of the crate table, append after the slice-4 hooks sentence (the text ending "…the offline determinism suite is untouched.") a slice-5 sentence:

```markdown
Slice 5 (plan A) adds **plugins**: discovery of Claude Code plugins from on-disk marketplaces
(`<base>/.claude/plugins/marketplaces/*/` with `.claude-plugin/marketplace.json`), gated by the
`enabledPlugins` allowlist in `settings.json` (key `"<plugin>@<marketplace>"`, project overrides
user), folding each enabled plugin's bundled agents/commands/skills/hooks into `Extensions` via the
existing parsers — **namespaced by plugin name** (`foo:commit`/`foo:helper`/`foo:greet`), lowest
precedence (a user/project artifact of the same final name wins), with `${CLAUDE_PLUGIN_ROOT}`
expanded in plugin hook commands. The crate spawns nothing (it stays hermetic); bundled MCP servers
(`.mcp.json` → otto's MCP client) and the network install action are plan B / deferred. Plan A needs
no new engine code — folded artifacts flow through the existing `register_skills`/`register_hooks`
and `--command`/`--agent` paths.
```

- [ ] **Step 3: Verify formatting, lints, and the full suite**

Run each and confirm the expected result:

```bash
cargo fmt --all
cargo clippy -p otto-extensions -p otto-engine --all-targets
cargo test -p otto-extensions
cargo test --workspace
```

Expected: `fmt` makes no further changes after the edits; `clippy` is clean (no warnings); `otto-extensions` tests pass; the full `--workspace` suite passes (offline determinism intact — with no `.claude/plugins/`, `Extensions` is unchanged).

- [ ] **Step 4: Commit**

```bash
git add docs/ARCHITECTURE.md CLAUDE.md
git commit -m "docs(extensions): record plugins slice A (marketplace + static components)"
```

---

## Spec coverage check

- Marketplace discovery over `.claude/plugins/marketplaces/` → Task 4 (`fold_plugins`).
- `enabledPlugins` allowlist gate, project-over-user merge → Tasks 3, 4, 5.
- `plugin.json` parse + component path overrides → Tasks 2, 4 (`component_dir`).
- Namespacing by plugin name (commands/agents/skills) → Task 4, asserted in Tasks 4 & 5.
- Plugins lowest precedence (user/project win) → Task 4 (`or_insert`), asserted in Task 5.
- Additive hooks with `${CLAUDE_PLUGIN_ROOT}` expansion → Tasks 3, 4, asserted in Tasks 4 & 6.
- Remote/absent source skipped, malformed manifests skipped (never fatal) → Task 5.
- Hermetic + determinism (no `.claude/plugins/` → unchanged) → Task 7 (`cargo test --workspace`).
- End-to-end engine wiring (plugin hook fires) → Task 6.
- **Deferred to Plan B:** bundled MCP servers (`mcpServers`/`.mcp.json` → `connect_plugin_server`). The network install action remains out of scope for the whole slice.
