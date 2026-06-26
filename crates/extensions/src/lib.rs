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
pub use marketplace_def::{Marketplace, MarketplaceEntry, PluginSource, parse_marketplace_json};
pub use plugin_def::{PluginManifest, parse_plugin_json};
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
    }
    Extensions {
        agents: agents.into_values().collect(),
        commands: commands.into_values().collect(),
        skills: skills.into_values().collect(),
        hooks,
    }
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
}
