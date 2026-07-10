//! Loads otto's native extension format — Claude Code's `.claude/` convention — and
//! registers each artifact into an existing otto primitive. Slice 1: custom agents.
//!
//! This crate is a leaf: it depends inward on `engine-core`/`protocol` and is wired only
//! by the `engine` binary, never by `engine-core`. The orchestrator core never calls
//! discovery, so the offline determinism suite is unaffected.

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

pub use agent_def::{CustomAgentDef, parse_agent_md};
pub use command_def::{CustomCommandDef, parse_command_md};
pub use command_expand::{expand_args, resolve_injections};
pub use hook_def::{HookCommand, HookMatcher, HookSet, parse_hooks};
pub use hook_exec::{HookEvent, HookExecutor, HookOutcome, matcher_selects};
pub use hooked_tool::HookedTool;
pub use markdown_agent::MarkdownAgent;
pub use marketplace_def::{
    Marketplace, MarketplaceEntry, PluginSource, RemoteClone, parse_marketplace_json,
    resolve_remote_source,
};
pub use marketplace_install::{MarketplaceLock, MarketplaceLockfile, set_enabled_plugin};
pub use permission_def::{PermissionRules, parse_permissions};
pub use plugin_def::{
    McpServersField, PluginManifest, PluginMcpServer, parse_mcp_servers, parse_plugin_json,
};
pub use skill_def::{CustomSkillDef, parse_skill_md};
pub use skill_tool::SkillTool;
pub use task_tool::TaskTool;

use std::path::Path;

/// Everything discovered from the `.claude/` directories. Slice 1: custom agents.
/// Slice 2: commands. Slice 3: skills. Slice 4: hooks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    pub agents: Vec<CustomAgentDef>,
    pub commands: Vec<CustomCommandDef>,
    pub skills: Vec<CustomSkillDef>,
    pub hooks: HookSet,
    pub mcp_servers: Vec<PluginMcpServer>,
    pub permissions: PermissionRules,
}

/// Discover `<home>/.claude/agents/*.md` then `<project_root>/.claude/agents/*.md`. Project
/// agents override user agents of the same `name`. Malformed files are skipped (never fatal).
/// Missing directories yield no agents. `home` is explicit (never read ambiently) so callers
/// and tests stay hermetic. Also discovers `<base>/.claude/skills/<name>/SKILL.md` (one level;
/// project overrides user by name).
pub fn discover(project_root: &Path, home: &Path) -> Extensions {
    // User-global first, then project — so a later project insert overrides by name.
    let mut agents: std::collections::BTreeMap<String, CustomAgentDef> =
        std::collections::BTreeMap::new();
    let mut commands: std::collections::BTreeMap<String, CustomCommandDef> =
        std::collections::BTreeMap::new();
    let mut skills: std::collections::BTreeMap<String, CustomSkillDef> =
        std::collections::BTreeMap::new();
    let mut hooks = HookSet::default();
    let mut mcp_servers: Vec<PluginMcpServer> = Vec::new();
    let mut permissions = PermissionRules::default();
    for base in [home, project_root] {
        let claude = base.join(".claude");
        for def in read_agents_dir(&claude.join("agents")) {
            agents.insert(def.name.clone(), def);
        }
        for def in read_commands_dir(&claude.join("commands")) {
            commands.insert(def.name.clone(), def);
        }
        for def in read_skills_dir(&claude.join("skills")) {
            skills.insert(def.name.clone(), def);
        }
        let mut base_hooks = read_settings_hooks(&claude.join("settings.json"));
        // Concatenate (user-base first, then project) — hooks are additive, not override-by-name.
        hooks.pre_tool_use.append(&mut base_hooks.pre_tool_use);
        hooks.post_tool_use.append(&mut base_hooks.post_tool_use);
        permissions.extend(read_settings_permissions(&claude.join("settings.json")));
    }
    fold_plugins(
        home,
        project_root,
        &mut agents,
        &mut commands,
        &mut skills,
        &mut hooks,
        &mut mcp_servers,
    );
    Extensions {
        agents: agents.into_values().collect(),
        commands: commands.into_values().collect(),
        skills: skills.into_values().collect(),
        hooks,
        mcp_servers,
        permissions,
    }
}

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

/// Parse every `*.md` in `dir` (non-recursive). Missing dir → empty; unreadable/malformed
/// files are skipped with a warning, never fatal.
fn read_agents_dir(dir: &Path) -> Vec<CustomAgentDef> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("warning: skipping unreadable agent {}: {e}", path.display());
                continue;
            }
        };
        match parse_agent_md(&text) {
            Ok(def) => out.push(def),
            Err(e) => eprintln!("warning: skipping malformed agent {}: {e}", path.display()),
        }
    }
    out
}

/// Parse every `*.md` under `dir` **recursively**. Each command's name is its path relative to
/// `dir`, with the `.md` extension dropped and separators replaced by `:` (`git/commit.md` →
/// `git:commit`). Missing dir → empty; unreadable/malformed files are skipped, never fatal.
fn read_commands_dir(dir: &Path) -> Vec<CustomCommandDef> {
    let mut out = Vec::new();
    collect_commands(dir, dir, &mut out);
    out
}

fn collect_commands(base: &Path, dir: &Path, out: &mut Vec<CustomCommandDef>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_commands(base, &path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = command_name(base, &path);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "warning: skipping unreadable command {}: {e}",
                    path.display()
                );
                continue;
            }
        };
        match parse_command_md(&name, &text) {
            Ok(def) => out.push(def),
            Err(e) => eprintln!(
                "warning: skipping malformed command {}: {e}",
                path.display()
            ),
        }
    }
}

/// Parse every `<dir>/<skill>/SKILL.md` (one level of skill directories). The skill's fallback
/// `name` is its directory name; a frontmatter `name` overrides it. `root` is set to the skill
/// directory (for later resource lookup). Missing `dir` → empty; a skill directory without a
/// `SKILL.md` is silently not-a-skill; an unreadable/malformed `SKILL.md` is skipped with a
/// warning, never fatal.
fn read_skills_dir(dir: &Path) -> Vec<CustomSkillDef> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }
        let manifest = skill_dir.join("SKILL.md");
        let text = match std::fs::read_to_string(&manifest) {
            Ok(t) => t,
            // No SKILL.md → this subdir simply isn't a skill (silent, expected).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // SKILL.md exists but couldn't be read → skip with a warning, never fatal.
            Err(e) => {
                eprintln!(
                    "warning: skipping unreadable skill {}: {e}",
                    manifest.display()
                );
                continue;
            }
        };
        let name = skill_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match parse_skill_md(&name, &text) {
            Ok(mut def) => {
                def.root = skill_dir;
                out.push(def);
            }
            Err(e) => eprintln!(
                "warning: skipping malformed skill {}: {e}",
                manifest.display()
            ),
        }
    }
    out
}

/// Read `<base>/.claude/settings.json` and parse its tool-dispatch hooks. A missing file yields
/// no hooks; an unreadable file or one with invalid JSON is skipped with a warning, never fatal.
fn read_settings_hooks(path: &Path) -> HookSet {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HookSet::default(),
        Err(e) => {
            eprintln!(
                "warning: skipping unreadable settings {}: {e}",
                path.display()
            );
            return HookSet::default();
        }
    };
    match parse_hooks(&text) {
        Ok(set) => set,
        Err(e) => {
            eprintln!(
                "warning: skipping malformed settings {}: {e}",
                path.display()
            );
            HookSet::default()
        }
    }
}

/// Read `<base>/.claude/settings.json` and parse its `permissions` block. Missing/unreadable →
/// empty (never fatal), matching every other `.claude/` reader.
fn read_settings_permissions(path: &Path) -> PermissionRules {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_permissions(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PermissionRules::default(),
        Err(e) => {
            eprintln!(
                "warning: skipping unreadable settings {}: {e}",
                path.display()
            );
            PermissionRules::default()
        }
    }
}

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
    mcp_servers: &mut Vec<PluginMcpServer>,
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

    let mut plugin_hooks = read_plugin_hooks(&component_dir(
        plugin_root,
        &manifest.hooks,
        "hooks/hooks.json",
    ));
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

    // Bundled MCP servers (Plan B): path override, inline object, or the convention `.mcp.json`.
    // A `Path` override is not root-contained (an absolute or `../` path would escape `plugin_root`),
    // matching the hook-path posture: the plugin is already user-trusted via `enabledPlugins`, this
    // only reads a JSON file whose contents become a command the user already trusts the plugin to
    // supply — so containment here adds no boundary the enable-gate doesn't already own.
    let raw: Option<serde_json::Value> = match &manifest.mcp_servers {
        Some(McpServersField::Inline(v)) => Some(v.clone()),
        Some(McpServersField::Path(p)) => {
            read_json_file(&plugin_root.join(p.trim_start_matches("./")))
        }
        None => read_json_file(&plugin_root.join(".mcp.json")),
    };
    if let Some(v) = raw {
        // Tolerate both the `.mcp.json` wrapper ({"mcpServers": {...}}) and a bare server map.
        // Known edge case: a server literally named "mcpServers" would be misread as the wrapper.
        // Accepted Claude-Code-compat heuristic; intentionally not made stricter.
        let servers = v.get("mcpServers").unwrap_or(&v);
        for mut spec in parse_mcp_servers(servers, ns) {
            expand_mcp_server_root(&mut spec, plugin_root);
            mcp_servers.push(spec);
        }
    }
}

/// Expand `${CLAUDE_PLUGIN_ROOT}` in every path-bearing field of a bundled MCP server spec
/// (`command`/`args`/`env` values/`cwd`), in place. Keeps the substitution in one spot so a new
/// path-bearing field can't be added to the spawn path without also being expanded.
fn expand_mcp_server_root(spec: &mut PluginMcpServer, plugin_root: &Path) {
    spec.command = expand_plugin_root(&spec.command, plugin_root);
    for a in &mut spec.args {
        *a = expand_plugin_root(a, plugin_root);
    }
    for v in spec.env.values_mut() {
        *v = expand_plugin_root(v, plugin_root);
    }
    if let Some(cwd) = &mut spec.cwd {
        *cwd = expand_plugin_root(cwd, plugin_root);
    }
}

/// Read + parse a JSON file. Missing file → `None` (silent, convention default may be absent);
/// unreadable-but-present or malformed → `None` with a warning. Never fatal.
fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("warning: skipping unreadable {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("warning: skipping malformed {}: {e}", path.display());
            None
        }
    }
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
    mcp_servers: &mut Vec<PluginMcpServer>,
) {
    // Merge the enable allowlist (project overrides user for the same key).
    let mut enabled = std::collections::BTreeMap::new();
    for base in [home, project_root] {
        for (k, v) in read_enabled_plugins(&base.join(".claude").join("settings.json")) {
            enabled.insert(k, v);
        }
    }

    for base in [home, project_root] {
        let mp_root = base.join(".claude").join("plugins").join("marketplaces");
        let mut mp_dirs: Vec<std::path::PathBuf> = match std::fs::read_dir(&mp_root) {
            Ok(e) => e.flatten().map(|entry| entry.path()).collect(),
            Err(_) => continue,
        };
        mp_dirs.sort();
        for mp_dir in mp_dirs {
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
                fold_one_plugin(
                    &plugin.name,
                    &plugin_root,
                    agents,
                    commands,
                    skills,
                    hooks,
                    mcp_servers,
                );
            }
        }
    }
}

/// Namespaced command name: path relative to `base`, extension stripped, components joined by `:`.
fn command_name(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path).with_extension("");
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_agent(dir: &Path, file: &str, body: &str) {
        let agents = dir.join(".claude").join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join(file), body).unwrap();
    }

    #[test]
    fn discovers_from_both_roots() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_agent(
            home.path(),
            "u.md",
            "---\nname: u\ndescription: user agent\n---\nbody\n",
        );
        write_agent(
            proj.path(),
            "p.md",
            "---\nname: p\ndescription: proj agent\n---\nbody\n",
        );

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.agents.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"u"));
        assert!(names.contains(&"p"));
    }

    #[test]
    fn project_overrides_user_by_name() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_agent(
            home.path(),
            "dup.md",
            "---\nname: dup\ndescription: USER\n---\nbody\n",
        );
        write_agent(
            proj.path(),
            "dup.md",
            "---\nname: dup\ndescription: PROJECT\n---\nbody\n",
        );

        let ext = discover(proj.path(), home.path());
        let dup: Vec<_> = ext.agents.iter().filter(|a| a.name == "dup").collect();
        assert_eq!(dup.len(), 1, "name collision should collapse to one");
        assert_eq!(dup[0].description, "PROJECT");
    }

    #[test]
    fn malformed_files_are_skipped_not_fatal() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_agent(
            proj.path(),
            "good.md",
            "---\nname: good\ndescription: d\n---\nbody\n",
        );
        write_agent(proj.path(), "bad.md", "no frontmatter here");

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["good"]);
    }

    #[test]
    fn missing_dirs_yield_empty() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        assert_eq!(discover(proj.path(), home.path()), Extensions::default());
    }

    fn write_command(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(".claude").join("commands").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn discovers_commands_recursively_with_namespaces() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_command(proj.path(), "review.md", "Review $ARGUMENTS\n");
        write_command(proj.path(), "git/commit.md", "Commit $1\n");

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"review"), "got: {names:?}");
        assert!(names.contains(&"git:commit"), "got: {names:?}");
    }

    #[test]
    fn project_command_overrides_user_by_namespaced_name() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_command(home.path(), "git/commit.md", "USER\n");
        write_command(proj.path(), "git/commit.md", "PROJECT\n");

        let ext = discover(proj.path(), home.path());
        let dup: Vec<_> = ext
            .commands
            .iter()
            .filter(|c| c.name == "git:commit")
            .collect();
        assert_eq!(dup.len(), 1, "name collision should collapse to one");
        assert_eq!(dup[0].template.trim(), "PROJECT");
    }

    #[test]
    fn missing_command_dir_yields_no_commands() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_agent(
            proj.path(),
            "a.md",
            "---\nname: a\ndescription: d\n---\nb\n",
        );
        let ext = discover(proj.path(), home.path());
        assert!(ext.commands.is_empty());
    }

    fn write_settings(dir: &Path, body: &str) {
        let claude = dir.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("settings.json"), body).unwrap();
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let skill = dir.join(".claude").join("skills").join(name);
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn discovers_skills_one_level_with_root_and_name_default() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_skill(
            proj.path(),
            "greeter",
            "---\ndescription: greets\n---\nSay hi.\n",
        );

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["greeter"],
            "name defaults to the skill dir name"
        );
        let greeter = &ext.skills[0];
        assert_eq!(
            greeter.root,
            proj.path().join(".claude").join("skills").join("greeter"),
            "root points at the skill directory"
        );
    }

    #[test]
    fn skill_frontmatter_name_overrides_dir_name() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_skill(
            proj.path(),
            "dir",
            "---\nname: real\ndescription: d\n---\nbody\n",
        );

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    #[test]
    fn project_skill_overrides_user_by_name() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_skill(home.path(), "dup", "---\ndescription: USER\n---\nu\n");
        write_skill(proj.path(), "dup", "---\ndescription: PROJECT\n---\np\n");

        let ext = discover(proj.path(), home.path());
        let dup: Vec<_> = ext.skills.iter().filter(|s| s.name == "dup").collect();
        assert_eq!(dup.len(), 1, "name collision should collapse to one");
        assert_eq!(dup[0].description, "PROJECT");
    }

    #[test]
    fn missing_skills_dir_and_dir_without_manifest_yield_no_skills() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        // A skill dir with NO SKILL.md is not a skill.
        let bare = proj.path().join(".claude").join("skills").join("bare");
        fs::create_dir_all(&bare).unwrap();
        fs::write(bare.join("notes.txt"), "ignore me").unwrap();

        let ext = discover(proj.path(), home.path());
        assert!(ext.skills.is_empty());
    }

    #[test]
    fn discovers_and_concatenates_hooks_from_both_bases() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_settings(
            home.path(),
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"user.sh"}]}]}}"#,
        );
        write_settings(
            proj.path(),
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"proj.sh"}]}]}}"#,
        );

        let ext = discover(proj.path(), home.path());
        let cmds: Vec<_> = ext
            .hooks
            .pre_tool_use
            .iter()
            .flat_map(|m| m.hooks.iter().map(|h| h.command.clone()))
            .collect();
        assert_eq!(cmds, vec!["user.sh", "proj.sh"], "user first, then project");
    }

    #[test]
    fn missing_settings_yields_no_hooks() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        assert_eq!(discover(proj.path(), home.path()).hooks, HookSet::default());
    }

    #[test]
    fn malformed_settings_skipped_not_fatal() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_settings(proj.path(), "{ not json");
        assert_eq!(discover(proj.path(), home.path()).hooks, HookSet::default());
    }

    #[test]
    fn discovers_and_unions_permissions_across_bases() {
        use std::fs;
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let home_claude = home.path().join(".claude");
        let proj_claude = proj.path().join(".claude");
        fs::create_dir_all(&home_claude).unwrap();
        fs::create_dir_all(&proj_claude).unwrap();
        fs::write(
            home_claude.join("settings.json"),
            r#"{ "permissions": { "allow": ["Read(src/**)"] } }"#,
        )
        .unwrap();
        fs::write(
            proj_claude.join("settings.json"),
            r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#,
        )
        .unwrap();

        let ext = discover(proj.path(), home.path());
        assert!(!ext.permissions.is_empty());
        // user allow + project deny are both present (unioned).
        assert_eq!(
            ext.permissions
                .decision("fs.read", &serde_json::json!({"path": "src/a.rs"})),
            Some(otto_engine_core::tool::Decision::Allow)
        );
        assert_eq!(
            ext.permissions
                .decision("fs.write", &serde_json::json!({"path": "dist/x"})),
            Some(otto_engine_core::tool::Decision::Deny)
        );
    }

    #[test]
    fn no_permissions_block_yields_empty() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let ext = discover(proj.path(), home.path());
        assert!(ext.permissions.is_empty());
    }

    #[test]
    fn malformed_skill_skipped_others_kept() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_skill(proj.path(), "good", "---\ndescription: ok\n---\nbody\n");
        // Missing description → parse error → skipped.
        write_skill(proj.path(), "bad", "---\nname: bad\n---\nbody\n");

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["good"]);
    }

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
        fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: greets\n---\ninstructions\n",
        )
        .unwrap();

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
        assert!(
            !cmd.contains("${CLAUDE_PLUGIN_ROOT}"),
            "not expanded: {cmd}"
        );
        assert!(
            cmd.contains("foo"),
            "expansion should include plugin dir: {cmd}"
        );
    }

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
    fn hook_order_is_deterministic_across_marketplaces() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        // Two marketplaces whose sorted dir order is aaa < bbb.
        write_plugin_marketplace(proj.path(), "aaa", "p1");
        write_plugin_marketplace(proj.path(), "bbb", "p2");
        // Enable both (single settings.json — enable_plugin would overwrite).
        fs::write(
            proj.path().join(".claude").join("settings.json"),
            r#"{"enabledPlugins":{"p1@aaa":true,"p2@bbb":true}}"#,
        )
        .unwrap();

        let ext = discover(proj.path(), home.path());
        let cmds: Vec<&str> = ext
            .hooks
            .pre_tool_use
            .iter()
            .flat_map(|m| m.hooks.iter().map(|h| h.command.as_str()))
            .collect();
        // Both plugins contributed one PreToolUse hook each, in sorted-marketplace order.
        assert_eq!(cmds.len(), 2, "expected two plugin hooks, got: {cmds:?}");
        assert!(
            cmds[0].contains("/aaa/"),
            "first hook should be from marketplace aaa: {cmds:?}"
        );
        assert!(
            cmds[1].contains("/bbb/"),
            "second hook should be from marketplace bbb: {cmds:?}"
        );
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
        let hits: Vec<_> = ext
            .commands
            .iter()
            .filter(|c| c.name == "foo:hello")
            .collect();
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

    /// Lay out a marketplace under `<base>/.claude/plugins/marketplaces/<mp>/` offering a local
    /// plugin `<plugin>` whose `plugin.json` is exactly `plugin_json`. If `dot_mcp_json` is `Some`,
    /// also writes `<plugin_root>/.mcp.json` with that content. Returns the plugin root path.
    fn write_plugin_with_mcp(
        base: &Path,
        mp: &str,
        plugin: &str,
        plugin_json: &str,
        dot_mcp_json: Option<&str>,
    ) -> std::path::PathBuf {
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
        fs::write(pcp.join("plugin.json"), plugin_json).unwrap();
        if let Some(content) = dot_mcp_json {
            fs::write(proot.join(".mcp.json"), content).unwrap();
        }
        proot
    }

    #[test]
    fn enabled_plugin_mcp_server_from_dot_mcp_json_is_folded_namespaced_and_expanded() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_plugin_with_mcp(
            proj.path(),
            "acme",
            "foo",
            r#"{"name":"foo"}"#,
            Some(
                r#"{"mcpServers":{"my-server":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/s.js"]}}}"#,
            ),
        );
        enable_plugin(proj.path(), "foo@acme");

        let ext = discover(proj.path(), home.path());
        assert_eq!(ext.mcp_servers.len(), 1);
        let s = &ext.mcp_servers[0];
        assert_eq!(s.namespace, "foo");
        assert_eq!(s.server_key, "my-server");
        // ${CLAUDE_PLUGIN_ROOT} expanded to the absolute plugin root in args; no token remains.
        assert!(s.args.iter().all(|a| !a.contains("${CLAUDE_PLUGIN_ROOT}")));
        assert!(
            s.args
                .iter()
                .any(|a| a.contains("plugins") && a.ends_with("s.js"))
        );
    }

    #[test]
    fn inline_mcp_servers_in_plugin_json_are_folded() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_plugin_with_mcp(
            proj.path(),
            "acme",
            "foo",
            r#"{"name":"foo","mcpServers":{"s":{"command":"node"}}}"#,
            None,
        );
        enable_plugin(proj.path(), "foo@acme");

        let ext = discover(proj.path(), home.path());
        assert_eq!(ext.mcp_servers.len(), 1);
        assert_eq!(ext.mcp_servers[0].server_key, "s");
    }

    #[test]
    fn enabled_plugin_mcp_server_from_path_field_is_folded_and_expanded() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        // plugin.json declares mcpServers as a *path* to a sibling JSON file.
        let proot = write_plugin_with_mcp(
            proj.path(),
            "acme",
            "foo",
            r#"{"name":"foo","mcpServers":"./servers.json"}"#,
            None,
        );
        // The referenced file uses the wrapped `{"mcpServers": {...}}` shape.
        fs::write(
            proot.join("servers.json"),
            r#"{"mcpServers":{"my-server":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/s.js"]}}}"#,
        )
        .unwrap();
        enable_plugin(proj.path(), "foo@acme");

        let ext = discover(proj.path(), home.path());
        assert_eq!(ext.mcp_servers.len(), 1);
        let s = &ext.mcp_servers[0];
        assert_eq!(s.namespace, "foo");
        assert_eq!(s.server_key, "my-server");
        assert!(s.args.iter().all(|a| !a.contains("${CLAUDE_PLUGIN_ROOT}")));
        assert!(
            s.args
                .iter()
                .any(|a| a.contains("plugins") && a.ends_with("s.js"))
        );
    }

    #[test]
    fn enabled_plugin_mcp_server_from_bare_map_dot_mcp_json_is_folded() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        // .mcp.json whose top-level object IS the server map (no "mcpServers" wrapper).
        write_plugin_with_mcp(
            proj.path(),
            "acme",
            "foo",
            r#"{"name":"foo"}"#,
            Some(r#"{"bare-server":{"command":"node"}}"#),
        );
        enable_plugin(proj.path(), "foo@acme");

        let ext = discover(proj.path(), home.path());
        assert_eq!(ext.mcp_servers.len(), 1);
        assert_eq!(ext.mcp_servers[0].server_key, "bare-server");
        assert_eq!(ext.mcp_servers[0].namespace, "foo");
    }

    #[test]
    fn no_plugins_yields_no_mcp_servers() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        assert!(discover(proj.path(), home.path()).mcp_servers.is_empty());
    }

    #[test]
    fn disabled_plugin_contributes_no_mcp_servers() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_plugin_with_mcp(
            proj.path(),
            "acme",
            "foo",
            r#"{"name":"foo"}"#,
            Some(r#"{"mcpServers":{"my-server":{"command":"node"}}}"#),
        );
        fs::write(
            proj.path().join(".claude").join("settings.json"),
            r#"{"enabledPlugins":{"foo@acme":false}}"#,
        )
        .unwrap();

        let ext = discover(proj.path(), home.path());
        assert!(ext.mcp_servers.is_empty());
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
}
