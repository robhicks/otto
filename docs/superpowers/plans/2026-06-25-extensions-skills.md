# Extensions Skills Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Discover Claude Code `skills/<name>/SKILL.md` from `~/.claude/` + the project `.claude/` and expose each as loadable instructions through a built-in, gated `skill` tool.

**Architecture:** Mirror the agents/commands slices in the leaf `extensions` crate: a `CustomSkillDef` + `parse_skill_md` parser, a one-level discovery walk added to `Extensions`/`discover()`, and a `SkillTool` (`Tool` named `skill`) that returns a skill's `instructions` + `resource_dir`. The engine binary registers the tool into the spine's tool registry when any skills are discovered. `extensions` stays a leaf (depends inward on `engine-core`/`protocol`); the orchestrator core never calls discovery, so the offline determinism suite is untouched.

**Tech Stack:** Rust (edition 2024), `async-trait`, `serde_json`, `anyhow`, `tempfile` (tests). Crates: `otto-extensions`, `otto-engine`.

**Spec:** `docs/superpowers/specs/2026-06-25-extensions-skills-design.md`

---

## File Structure

- **Create** `crates/extensions/src/skill_def.rs` — `CustomSkillDef` + `parse_skill_md(name, text)` (parsing only; `root` assigned by discovery).
- **Create** `crates/extensions/src/skill_tool.rs` — `SkillTool` (`Tool` named `skill`).
- **Modify** `crates/extensions/src/lib.rs` — module decls + re-exports; `Extensions.skills`; `read_skills_dir`; wire into `discover()`; update module/`discover` doc comments.
- **Modify** `crates/engine/src/main.rs` — `register_skills` helper; call it in the `cmd_run` spine path; two hermetic tests.
- **Modify** `CLAUDE.md` and `docs/ARCHITECTURE.md` — note slice 3 (skills) shipped.

Reference patterns (read before starting): `crates/extensions/src/command_def.rs` (frontmatter parsing), `crates/extensions/src/task_tool.rs` (`Tool` impl + tests), `crates/extensions/src/lib.rs` (`read_commands_dir`/`discover` + tests).

---

## Task 1: `CustomSkillDef` + `parse_skill_md`

**Files:**
- Create: `crates/extensions/src/skill_def.rs`
- Modify: `crates/extensions/src/lib.rs` (add `mod skill_def;` + re-export — done in this task so the file compiles)

- [ ] **Step 1: Declare the module and re-export so the crate compiles**

In `crates/extensions/src/lib.rs`, add to the existing `mod` block (after `mod markdown_agent;`):

```rust
mod skill_def;
```

And add to the existing `pub use` block (after the `command_expand` re-export):

```rust
pub use skill_def::{CustomSkillDef, parse_skill_md};
```

- [ ] **Step 2: Write `skill_def.rs` with the type, parser, and failing tests**

Create `crates/extensions/src/skill_def.rs`:

```rust
//! A discovered `skills/<name>/SKILL.md`: Claude-Code-compatible frontmatter
//! (`name`/`description`/`allowed-tools`) plus a markdown body that is the skill's instructions.
//! `description` is REQUIRED (Claude Code uses it to decide when a skill applies); a skill without
//! one is unusable and rejected here so discovery skips it rather than load empty guidance.
//! `allowed_tools` is parsed and preserved but inert this slice — otto's gate stays the sole
//! authority. `root` (the skill directory, used for resource lookup) is assigned by discovery,
//! not parsed.

use std::path::PathBuf;

/// One parsed skill. `name` defaults to the skill directory name (supplied by discovery); a
/// frontmatter `name` overrides it. `root` is filled in by discovery (empty until then).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomSkillDef {
    pub name: String,
    pub description: String,
    pub allowed_tools: Option<Vec<String>>,
    pub instructions: String,
    pub root: PathBuf,
}

/// Parse one `SKILL.md`. `name` is the fallback name (discovery derives it from the directory); a
/// frontmatter `name` overrides it. The body after the frontmatter is the instructions. A missing
/// or empty `description`, absent frontmatter, or an unterminated frontmatter fence is an error.
pub fn parse_skill_md(name: &str, text: &str) -> anyhow::Result<CustomSkillDef> {
    let rest = text
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md has no frontmatter (missing `description`)"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("unterminated frontmatter (no closing `---`)"))?;
    let front = &rest[..end];
    let instructions = rest[end + 4..].trim_start_matches(['\n', '\r']).to_string();

    let mut fm_name = None;
    let mut description = None;
    let mut allowed_tools = None;
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "name" if !value.is_empty() => fm_name = Some(value.to_string()),
            "description" if !value.is_empty() => description = Some(value.to_string()),
            // Present (even if empty) → Some(list); only an absent key stays None.
            "allowed-tools" => {
                let list: Vec<String> = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                allowed_tools = Some(list);
            }
            _ => {}
        }
    }

    let description = description
        .ok_or_else(|| anyhow::anyhow!("SKILL.md is missing a non-empty `description`"))?;

    Ok(CustomSkillDef {
        name: fm_name.unwrap_or_else(|| name.to_string()),
        description,
        allowed_tools,
        instructions,
        root: PathBuf::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_frontmatter_and_body() {
        let text = "---\nname: pdf\ndescription: Fill PDFs\nallowed-tools: bash, fs.read\n---\nUse the form.\n";
        let def = parse_skill_md("dir-name", text).unwrap();
        assert_eq!(def.name, "pdf", "frontmatter name overrides the dir name");
        assert_eq!(def.description, "Fill PDFs");
        assert_eq!(
            def.allowed_tools,
            Some(vec!["bash".to_string(), "fs.read".to_string()])
        );
        assert_eq!(def.instructions.trim(), "Use the form.");
        assert_eq!(def.root, PathBuf::new(), "root is assigned by discovery, not parsing");
    }

    #[test]
    fn allowed_tools_inline_list_form() {
        let text = "---\ndescription: d\nallowed-tools: [bash, fs.read]\n---\nbody\n";
        let def = parse_skill_md("s", text).unwrap();
        assert_eq!(
            def.allowed_tools,
            Some(vec!["bash".to_string(), "fs.read".to_string()])
        );
    }

    #[test]
    fn name_falls_back_to_supplied_when_frontmatter_omits_it() {
        let text = "---\ndescription: d\n---\nbody\n";
        let def = parse_skill_md("dir-name", text).unwrap();
        assert_eq!(def.name, "dir-name");
        assert_eq!(def.allowed_tools, None);
    }

    #[test]
    fn missing_description_errors() {
        let text = "---\nname: s\n---\nbody\n";
        assert!(parse_skill_md("s", text).is_err());
    }

    #[test]
    fn empty_description_errors() {
        let text = "---\ndescription:\n---\nbody\n";
        assert!(parse_skill_md("s", text).is_err());
    }

    #[test]
    fn no_frontmatter_errors() {
        let text = "Just a body, no frontmatter and so no description.\n";
        assert!(parse_skill_md("s", text).is_err());
    }

    #[test]
    fn unterminated_frontmatter_errors() {
        let text = "---\ndescription: oops\nno closing fence\n";
        assert!(parse_skill_md("s", text).is_err());
    }

    #[test]
    fn unknown_frontmatter_keys_ignored() {
        let text = "---\ndescription: d\nbogus: x\n---\nbody\n";
        let def = parse_skill_md("s", text).unwrap();
        assert_eq!(def.description, "d");
        assert_eq!(def.instructions.trim(), "body");
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions skill_def::`
Expected: PASS (8 tests in `skill_def::tests`).

- [ ] **Step 4: Format and lint**

Run: `cargo fmt --all && cargo clippy -p otto-extensions --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/skill_def.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): CustomSkillDef + parse_skill_md (skills slice)"
```

---

## Task 2: Skill discovery in `lib.rs`

**Files:**
- Modify: `crates/extensions/src/lib.rs`

- [ ] **Step 1: Add a failing discovery test**

In `crates/extensions/src/lib.rs`, inside the existing `#[cfg(test)] mod tests`, add a helper and tests (place after the `write_command` helper and command tests):

```rust
    fn write_skill(dir: &Path, name: &str, body: &str) {
        let skill = dir.join(".claude").join("skills").join(name);
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn discovers_skills_one_level_with_root_and_name_default() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_skill(proj.path(), "greeter", "---\ndescription: greets\n---\nSay hi.\n");

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["greeter"], "name defaults to the skill dir name");
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
        write_skill(proj.path(), "dir", "---\nname: real\ndescription: d\n---\nbody\n");

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
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p otto-extensions skills 2>&1 | head -20`
Expected: FAIL — compile errors (`Extensions` has no field `skills`).

- [ ] **Step 3: Add the `skills` field to `Extensions`**

In `crates/extensions/src/lib.rs`, update the struct and its doc comment:

```rust
/// Everything discovered from the `.claude/` directories. Slice 1: custom agents.
/// Slice 2: commands. Slice 3: skills.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    pub agents: Vec<CustomAgentDef>,
    pub commands: Vec<CustomCommandDef>,
    pub skills: Vec<CustomSkillDef>,
}
```

- [ ] **Step 4: Wire skills into `discover()`**

In `discover()`, add a `skills` map alongside the existing `agents`/`commands` maps and populate it in the per-base loop. The function becomes:

```rust
pub fn discover(project_root: &Path, home: &Path) -> Extensions {
    // User-global first, then project — so a later project insert overrides by name.
    let mut agents: std::collections::BTreeMap<String, CustomAgentDef> =
        std::collections::BTreeMap::new();
    let mut commands: std::collections::BTreeMap<String, CustomCommandDef> =
        std::collections::BTreeMap::new();
    let mut skills: std::collections::BTreeMap<String, CustomSkillDef> =
        std::collections::BTreeMap::new();
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
    }
    Extensions {
        agents: agents.into_values().collect(),
        commands: commands.into_values().collect(),
        skills: skills.into_values().collect(),
    }
}
```

Also update the `discover` doc comment's first line to mention skills, e.g. append a sentence: `Also discovers <…>/.claude/skills/<name>/SKILL.md (one level; project overrides user by name).`

- [ ] **Step 5: Add `read_skills_dir`**

Add this function next to `read_commands_dir` in `crates/extensions/src/lib.rs`:

```rust
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
            Err(_) => continue, // no SKILL.md → not a skill (not an error)
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
```

- [ ] **Step 6: Run the discovery tests**

Run: `cargo test -p otto-extensions`
Expected: PASS (all existing tests plus the 5 new skill discovery tests).

- [ ] **Step 7: Format and lint**

Run: `cargo fmt --all && cargo clippy -p otto-extensions --all-targets`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/extensions/src/lib.rs
git commit -m "feat(extensions): discover skills/<name>/SKILL.md (skills slice)"
```

---

## Task 3: `SkillTool`

**Files:**
- Create: `crates/extensions/src/skill_tool.rs`
- Modify: `crates/extensions/src/lib.rs` (module decl + re-export)

- [ ] **Step 1: Declare the module and re-export**

In `crates/extensions/src/lib.rs`, add to the `mod` block:

```rust
mod skill_tool;
```

And to the `pub use` block:

```rust
pub use skill_tool::SkillTool;
```

- [ ] **Step 2: Write `skill_tool.rs` with the tool and failing tests**

Create `crates/extensions/src/skill_tool.rs`:

```rust
//! `skill`: a built-in tool that loads a discovered skill's instructions into the current turn.
//! Given `{"skill": "<name>"}` it returns the skill body plus the skill's `resource_dir`, so the
//! agent can read any bundled resource on demand through the gated `fs.read`. The skill name is a
//! registry key (never a filesystem path), so the tool adds no traversal surface of its own; the
//! call carries no `path`/`bash`, so the gate's read-only `Allow` is correct.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use serde_json::{Value, json};

use crate::skill_def::CustomSkillDef;

/// Serves discovered skills by name. Built once from the discovered set; holds each skill's
/// instructions and resource directory.
pub struct SkillTool {
    skills: HashMap<String, (String, PathBuf)>,
}

impl SkillTool {
    pub fn new(skills: &[CustomSkillDef]) -> Self {
        let skills = skills
            .iter()
            .map(|s| (s.name.clone(), (s.instructions.clone(), s.root.clone())))
            .collect();
        Self { skills }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let name = args
            .get("skill")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("skill: missing string `skill`"))?;
        let (instructions, root) = self
            .skills
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("skill: no skill named '{name}'"))?;
        Ok(json!({
            "instructions": instructions,
            "resource_dir": root.to_string_lossy(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, instructions: &str, root: &str) -> CustomSkillDef {
        CustomSkillDef {
            name: name.to_string(),
            description: "d".to_string(),
            allowed_tools: None,
            instructions: instructions.to_string(),
            root: PathBuf::from(root),
        }
    }

    #[tokio::test]
    async fn returns_instructions_and_resource_dir() {
        let tool = SkillTool::new(&[def("greeter", "Say hi.", "/p/.claude/skills/greeter")]);
        let out = tool.call(json!({ "skill": "greeter" })).await.unwrap();
        assert_eq!(out["instructions"], "Say hi.");
        assert_eq!(out["resource_dir"], "/p/.claude/skills/greeter");
    }

    #[tokio::test]
    async fn unknown_skill_errors() {
        let tool = SkillTool::new(&[def("greeter", "x", "/p")]);
        assert!(tool.call(json!({ "skill": "ghost" })).await.is_err());
    }

    #[tokio::test]
    async fn missing_or_non_string_skill_arg_errors() {
        let tool = SkillTool::new(&[def("greeter", "x", "/p")]);
        assert!(tool.call(json!({})).await.is_err());
        assert!(tool.call(json!({ "skill": 7 })).await.is_err());
    }

    #[test]
    fn tool_is_named_skill() {
        let tool = SkillTool::new(&[]);
        assert_eq!(tool.name(), "skill");
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions skill_tool::`
Expected: PASS (4 tests).

- [ ] **Step 4: Format and lint**

Run: `cargo fmt --all && cargo clippy -p otto-extensions --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/skill_tool.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): SkillTool built-in `skill` tool (skills slice)"
```

---

## Task 4: Engine wiring — register `skill` in the spine

**Files:**
- Modify: `crates/engine/src/main.rs`

- [ ] **Step 1: Add failing engine tests**

In `crates/engine/src/main.rs`, inside `#[cfg(test)] mod tests` (after `run_command_expands_and_runs_spine`), add:

```rust
    #[test]
    fn register_skills_adds_skill_tool_when_present() {
        use std::fs;
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global skills
        let skill = proj.path().join(".claude").join("skills").join("greeter");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: greeter\ndescription: greets\n---\nSay hi.\n",
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        let mut reg = otto_engine::build_tool_registry(
            Arc::new(LocalWorkspace::new(proj.path().to_path_buf())),
            proj.path().to_path_buf(),
        );
        register_skills(&mut reg, &ext.skills);
        assert!(reg.tool_names().iter().any(|n| n == "skill"));
    }

    #[test]
    fn register_skills_is_noop_when_absent() {
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let ext = otto_extensions::discover(proj.path(), home.path());
        let mut reg = otto_engine::build_tool_registry(
            Arc::new(LocalWorkspace::new(proj.path().to_path_buf())),
            proj.path().to_path_buf(),
        );
        register_skills(&mut reg, &ext.skills);
        assert!(!reg.tool_names().iter().any(|n| n == "skill"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p otto-engine register_skills 2>&1 | head -20`
Expected: FAIL — `cannot find function register_skills`.

- [ ] **Step 3: Add the `register_skills` helper**

In `crates/engine/src/main.rs`, add this free function (place it just above `async fn cmd_run`):

```rust
/// Register the built-in `skill` tool when any skills were discovered. No-op otherwise, so a
/// workspace with no `.claude/skills/` leaves the spine's tool set byte-for-byte unchanged.
fn register_skills(
    registry: &mut otto_engine_core::tool::ToolRegistry,
    skills: &[otto_extensions::CustomSkillDef],
) {
    if !skills.is_empty() {
        registry.register(Arc::new(otto_extensions::SkillTool::new(skills)));
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine register_skills`
Expected: PASS (2 tests).

- [ ] **Step 5: Wire `register_skills` into the `cmd_run` spine path**

In `crates/engine/src/main.rs`, in `cmd_run`, change the tool-build block (currently):

```rust
    let (tools, _mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), false).await;
    // _mcp_conns is held until end of function so the mcp children stay alive.
    let tools = Arc::new(tools);
```

to:

```rust
    let (mut tools, _mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), false).await;
    // _mcp_conns is held until end of function so the mcp children stay alive.
    // Register discovered skills as the gated `skill` tool so spine agents can load them mid-turn.
    let ext = otto_extensions::discover(&root, &home_dir());
    register_skills(&mut tools, &ext.skills);
    let tools = Arc::new(tools);
```

- [ ] **Step 6: Verify the whole engine crate builds and tests pass**

Run: `cargo test -p otto-engine`
Expected: PASS (existing tests plus the 2 new ones). The default determinism suite is unchanged: with no `.claude/skills/`, `register_skills` is a no-op.

- [ ] **Step 7: Format and lint**

Run: `cargo fmt --all && cargo clippy -p otto-engine --all-targets`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): register gated `skill` tool in the spine when skills exist"
```

---

## Task 5: Documentation

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Update the `extensions` row in `CLAUDE.md`**

In `CLAUDE.md`, find the `extensions` crate row in the Architecture table (it currently ends with the Slice 2 commands description: "… `model`/`allowed-tools` are parsed and preserved but not yet routed/enforced."). Append a Slice 3 sentence to that cell:

```
Slice 3 adds skills: discovery of `skills/<name>/SKILL.md` from `~/.claude/` + the project `.claude/` (one level; project wins by name; frontmatter `name` overrides the dir name, `description` required), parsed into a `CustomSkillDef`, and exposed through a built-in gated `SkillTool` (`skill`) that returns a skill's `instructions` + `resource_dir` (bundled resources are read lazily through the gated `fs.read`, sensitive-path floor intact). Registered into the spine's tool registry when any skills are discovered. `allowed-tools` is parsed and preserved but inert (gate stays the sole authority); no CLI entry — skills are mid-turn capabilities, not entrypoints.
```

- [ ] **Step 2: Update `docs/ARCHITECTURE.md`**

In `docs/ARCHITECTURE.md`:

1. Update the crate-tree comment (around line 38-39) to note skills shipped, e.g. change the `extensions` line that says "Slice 1 shipped: custom agents …" to also mention "Slices 2-3 shipped: commands, skills."
2. In the "Claude Code compatibility" list, change the skills bullet from a future-tense description to shipped, e.g.:

```
- `skills` (`SKILL.md` + resources) → discovered one-level (`skills/<name>/SKILL.md`, project overrides user by name) and exposed via a built-in gated `skill` tool returning `instructions` + `resource_dir`; bundled resources read lazily through gated `fs.read`. (`allowed-tools` parsed, inert until the permissions slice.)
```

- [ ] **Step 3: Verify the full workspace still builds and tests green**

Run: `cargo test --workspace`
Expected: PASS (offline, deterministic — no network/keys).

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md
git commit -m "docs(extensions): note skills slice (slice 3) shipped"
```

---

## Done-when

- `cargo test --workspace` is green; `cargo clippy --workspace --all-targets` is clean.
- A `.claude/skills/<name>/SKILL.md` (in `~/.claude/` or the project) is discovered, project overriding user by name, frontmatter `name` overriding the dir name, `description` required.
- The spine registers a gated `skill` tool when skills exist; `skill {"skill": "<name>"}` returns `{instructions, resource_dir}`, unknown names error.
- With no `.claude/skills/`, the spine's tool set and the offline determinism suite are unchanged.
- `allowed-tools` is parsed and preserved but inert; no eager resource bundling; no CLI subcommand.
