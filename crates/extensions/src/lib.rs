//! Loads otto's native extension format — Claude Code's `.claude/` convention — and
//! registers each artifact into an existing otto primitive. Slice 1: custom agents.
//!
//! This crate is a leaf: it depends inward on `engine-core`/`protocol` and is wired only
//! by the `engine` binary, never by `engine-core`. The orchestrator core never calls
//! discovery, so the offline determinism suite is unaffected.

mod agent_def;
mod markdown_agent;

pub use agent_def::{CustomAgentDef, parse_agent_md};
pub use markdown_agent::MarkdownAgent;

use std::path::Path;

/// Everything discovered from the `.claude/` directories. Slice 1: custom agents only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    pub agents: Vec<CustomAgentDef>,
}

/// Discover `<home>/.claude/agents/*.md` then `<project_root>/.claude/agents/*.md`. Project
/// agents override user agents of the same `name`. Malformed files are skipped (never fatal).
/// Missing directories yield no agents. `home` is explicit (never read ambiently) so callers
/// and tests stay hermetic.
pub fn discover(project_root: &Path, home: &Path) -> Extensions {
    // User-global first, then project — so a later project insert overrides by name.
    let mut by_name: std::collections::BTreeMap<String, CustomAgentDef> =
        std::collections::BTreeMap::new();
    for base in [home, project_root] {
        for def in read_agents_dir(&base.join(".claude").join("agents")) {
            by_name.insert(def.name.clone(), def);
        }
    }
    Extensions {
        agents: by_name.into_values().collect(),
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
}
