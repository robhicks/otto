# Extensions Commands Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Claude Code `commands/*.md` support to the `extensions` crate — recursive namespaced discovery, template expansion (args + gated `!bash`/`@file` injection), and an `otto run --command <name> [args...]` CLI entry that dispatches the expanded text as a normal spine turn.

**Architecture:** `extensions` stays a leaf crate depending only on `engine-core`/`protocol`. Two new modules: `command_def` (parse `commands/*.md`) and `command_expand` (pure `expand_args` + async `resolve_injections` over the gated `ToolRegistry`). Discovery is folded into the existing `discover`/`Extensions`. The `engine` binary wires a `--command` entry that expands then calls the existing `run_goal`. No `engine-core` changes; injection reuses the existing permission gate (`fs.read`/`bash`), so the sensitive-path floor and sandbox apply unchanged. The orchestrator core never calls discovery/expansion, so the offline determinism suite is untouched.

**Tech Stack:** Rust (edition 2024), `anyhow`, `async-trait`, `serde_json`, `tokio` (tests), `tempfile` (tests). No new dependencies.

---

## File Structure

- **Create** `crates/extensions/src/command_def.rs` — `CustomCommandDef` struct + `parse_command_md(name, text)`. One responsibility: parse a single command file.
- **Create** `crates/extensions/src/command_expand.rs` — `expand_args` (pure arg substitution) + `resolve_injections` (async gated `!`/`@` resolution). One responsibility: turn a template + args into a final goal string.
- **Modify** `crates/extensions/src/lib.rs` — add `commands` to `Extensions`, recursive namespaced `read_commands_dir`, populate in `discover`, declare/re-export the two new modules.
- **Modify** `crates/engine/src/main.rs` — `parse_command_flag`, `run_command_in`, and a `--command` branch in `cmd_run`.
- **Modify** `crates/extensions/Cargo.toml` — nothing required (deps already present); listed only so the implementer does not go looking.
- **Modify** `CLAUDE.md` and `docs/ARCHITECTURE.md` — document the shipped commands artifact + usage.

Each task is one focused change with its own failing test → implementation → passing test → commit.

---

## Task 1: `CustomCommandDef` + `parse_command_md`

**Files:**
- Create: `crates/extensions/src/command_def.rs`
- Modify: `crates/extensions/src/lib.rs` (declare `mod command_def;` + re-export) — done in this task so the test compiles.

- [ ] **Step 1: Declare the module and re-export in `lib.rs`**

In `crates/extensions/src/lib.rs`, add to the existing `mod`/`pub use` block (currently lines 8-14):

```rust
mod agent_def;
mod command_def;
mod markdown_agent;
mod task_tool;

pub use agent_def::{CustomAgentDef, parse_agent_md};
pub use command_def::{CustomCommandDef, parse_command_md};
pub use markdown_agent::MarkdownAgent;
pub use task_tool::TaskTool;
```

- [ ] **Step 2: Write the failing test file**

Create `crates/extensions/src/command_def.rs` with the struct, a `parse_command_md` stub that returns an error, and the tests:

```rust
//! A discovered `commands/*.md`: optional Claude-Code-compatible frontmatter
//! (`description`, `argument-hint`, `model`, `allowed-tools`) plus a markdown body that is the
//! prompt template. Unlike agents, the command `name` comes from the file path (assigned by the
//! caller), not from frontmatter, and frontmatter is optional — a bare prompt file is valid.

/// One parsed custom command. All frontmatter fields are optional. `model` is preserved for a
/// later slice (not routed); `allowed_tools` is preserved for a later slice (not enforced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommandDef {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub template: String,
}

/// Parse one `commands/*.md`. `name` is supplied by the caller (discovery derives it from the
/// path). If `text` starts with `---`, the frontmatter block is split and parsed; otherwise the
/// whole text is the template and every frontmatter field is `None`. An unterminated frontmatter
/// block (`---` with no closing `---`) is an error.
pub fn parse_command_md(name: &str, text: &str) -> anyhow::Result<CustomCommandDef> {
    anyhow::bail!("not implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_frontmatter_and_body() {
        let text = "---\ndescription: Commit helper\nargument-hint: <message>\nmodel: claude-opus-4-8\nallowed-tools: bash, fs.read\n---\nCommit with message: $ARGUMENTS\n";
        let def = parse_command_md("git:commit", text).unwrap();
        assert_eq!(def.name, "git:commit");
        assert_eq!(def.description.as_deref(), Some("Commit helper"));
        assert_eq!(def.argument_hint.as_deref(), Some("<message>"));
        assert_eq!(def.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(
            def.allowed_tools,
            Some(vec!["bash".to_string(), "fs.read".to_string()])
        );
        assert_eq!(def.template.trim(), "Commit with message: $ARGUMENTS");
    }

    #[test]
    fn allowed_tools_inline_list_form() {
        let text = "---\nallowed-tools: [bash, fs.read]\n---\nbody\n";
        let def = parse_command_md("c", text).unwrap();
        assert_eq!(
            def.allowed_tools,
            Some(vec!["bash".to_string(), "fs.read".to_string()])
        );
    }

    #[test]
    fn no_frontmatter_whole_text_is_template() {
        let text = "Just a prompt with $1 and no frontmatter.\n";
        let def = parse_command_md("plain", text).unwrap();
        assert_eq!(def.description, None);
        assert_eq!(def.argument_hint, None);
        assert_eq!(def.model, None);
        assert_eq!(def.allowed_tools, None);
        assert_eq!(def.template, "Just a prompt with $1 and no frontmatter.\n");
    }

    #[test]
    fn unterminated_frontmatter_errors() {
        let text = "---\ndescription: oops\nno closing fence\n";
        assert!(parse_command_md("c", text).is_err());
    }

    #[test]
    fn unknown_frontmatter_keys_ignored() {
        let text = "---\ndescription: d\nbogus: x\n---\nbody\n";
        let def = parse_command_md("c", text).unwrap();
        assert_eq!(def.description.as_deref(), Some("d"));
        assert_eq!(def.template.trim(), "body");
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p otto-extensions command_def`
Expected: FAIL — the four tests fail on `bail!("not implemented")`.

- [ ] **Step 4: Implement `parse_command_md`**

Replace the `parse_command_md` body in `crates/extensions/src/command_def.rs`:

```rust
pub fn parse_command_md(name: &str, text: &str) -> anyhow::Result<CustomCommandDef> {
    let mut description = None;
    let mut argument_hint = None;
    let mut model = None;
    let mut allowed_tools = None;

    let template = if let Some(rest) = text.strip_prefix("---") {
        let end = rest
            .find("\n---")
            .ok_or_else(|| anyhow::anyhow!("unterminated frontmatter (no closing `---`)"))?;
        let front = &rest[..end];
        let body = rest[end + 4..].trim_start_matches(['\n', '\r']).to_string();

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
                "description" if !value.is_empty() => description = Some(value.to_string()),
                "argument-hint" if !value.is_empty() => argument_hint = Some(value.to_string()),
                "model" if !value.is_empty() => model = Some(value.to_string()),
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
        body
    } else {
        text.to_string()
    };

    Ok(CustomCommandDef {
        name: name.to_string(),
        description,
        argument_hint,
        model,
        allowed_tools,
        template,
    })
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p otto-extensions command_def`
Expected: PASS — all five `command_def` tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/extensions/src/command_def.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): parse commands/*.md frontmatter + body"
```

---

## Task 2: `expand_args` (pure argument substitution)

**Files:**
- Create: `crates/extensions/src/command_expand.rs`
- Modify: `crates/extensions/src/lib.rs` (declare `mod command_expand;` + re-export)

- [ ] **Step 1: Declare the module and re-export in `lib.rs`**

In `crates/extensions/src/lib.rs`, update the module/re-export block to add `command_expand`:

```rust
mod agent_def;
mod command_def;
mod command_expand;
mod markdown_agent;
mod task_tool;

pub use agent_def::{CustomAgentDef, parse_agent_md};
pub use command_def::{CustomCommandDef, parse_command_md};
pub use command_expand::{expand_args, resolve_injections};
pub use markdown_agent::MarkdownAgent;
pub use task_tool::TaskTool;
```

(`resolve_injections` is implemented in Task 3; declaring the re-export now keeps the module list in one place. The function is defined as a stub in Step 2 so this compiles.)

- [ ] **Step 2: Write the failing test for `expand_args`**

Create `crates/extensions/src/command_expand.rs`:

```rust
//! Command template expansion. Two stages: a pure `expand_args` (`$ARGUMENTS`, `$1..$9`) and an
//! async `resolve_injections` that resolves `` !`cmd` `` and `@path` through the gated
//! `ToolRegistry` (so the permission gate, sandbox, and sensitive-path floor all apply).

use otto_engine_core::tool::ToolRegistry;
use serde_json::json;

/// Substitute `$ARGUMENTS` (all args joined by a single space) and `$1`..`$9` (1-based
/// positional; a missing positional becomes the empty string). Pure — runs before injection so a
/// substituted arg may appear inside an injection target (e.g. `@$1`).
pub fn expand_args(template: &str, args: &[String]) -> String {
    String::new()
}

/// Resolve `` !`cmd` `` (gated `bash`) and `@path` (gated `fs.read`) injections. Implemented in
/// Task 3.
pub async fn resolve_injections(text: &str, tools: &ToolRegistry) -> anyhow::Result<String> {
    let _ = (text, tools, json!(null));
    anyhow::bail!("not implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_arguments_and_positionals() {
        let args = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(expand_args("all: $ARGUMENTS", &args), "all: alpha beta");
        assert_eq!(expand_args("first=$1 second=$2", &args), "first=alpha second=beta");
    }

    #[test]
    fn missing_positional_becomes_empty() {
        let args = vec!["only".to_string()];
        assert_eq!(expand_args("[$1][$2][$3]", &args), "[only][][]");
    }

    #[test]
    fn no_args_yields_empty_substitutions() {
        let args: Vec<String> = vec![];
        assert_eq!(expand_args("x=$ARGUMENTS y=$1", &args), "x= y=");
    }

    #[test]
    fn template_without_placeholders_is_verbatim() {
        let args = vec!["ignored".to_string()];
        assert_eq!(expand_args("plain template", &args), "plain template");
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p otto-extensions command_expand::tests`
Expected: FAIL — the `expand_args` tests fail (returns empty string).

- [ ] **Step 4: Implement `expand_args`**

Replace the `expand_args` body:

```rust
pub fn expand_args(template: &str, args: &[String]) -> String {
    let mut out = template.replace("$ARGUMENTS", &args.join(" "));
    for i in 1..=9usize {
        let val = args.get(i - 1).map(String::as_str).unwrap_or("");
        out = out.replace(&format!("${i}"), val);
    }
    out
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p otto-extensions command_expand::tests`
Expected: PASS — the four `expand_args` tests pass (the `resolve_injections` stub has no tests yet).

- [ ] **Step 6: Commit**

```bash
git add crates/extensions/src/command_expand.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): pure expand_args (\$ARGUMENTS, \$1..\$9)"
```

---

## Task 3: `resolve_injections` (gated `!bash` / `@file`)

**Files:**
- Modify: `crates/extensions/src/command_expand.rs` (replace the stub + add tests)

- [ ] **Step 1: Write the failing tests**

In `crates/extensions/src/command_expand.rs`, replace the `mod tests` block with one that keeps the `expand_args` tests and adds `resolve_injections` tests. Append these imports and tests inside `mod tests` (keep the four `expand_args` tests above them):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use otto_engine_core::tool::{Decision, DenyAsk, PermissionGate, Tool};
    use serde_json::Value;
    use std::sync::Arc;

    // ---- expand_args tests (unchanged) ----
    #[test]
    fn substitutes_arguments_and_positionals() {
        let args = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(expand_args("all: $ARGUMENTS", &args), "all: alpha beta");
        assert_eq!(expand_args("first=$1 second=$2", &args), "first=alpha second=beta");
    }

    #[test]
    fn missing_positional_becomes_empty() {
        let args = vec!["only".to_string()];
        assert_eq!(expand_args("[$1][$2][$3]", &args), "[only][][]");
    }

    #[test]
    fn no_args_yields_empty_substitutions() {
        let args: Vec<String> = vec![];
        assert_eq!(expand_args("x=$ARGUMENTS y=$1", &args), "x= y=");
    }

    #[test]
    fn template_without_placeholders_is_verbatim() {
        let args = vec!["ignored".to_string()];
        assert_eq!(expand_args("plain template", &args), "plain template");
    }

    // ---- resolve_injections tests ----

    /// `fs.read` stub: echoes the requested path so substitution is observable.
    struct StubRead;
    #[async_trait]
    impl Tool for StubRead {
        fn name(&self) -> &str {
            "fs.read"
        }
        async fn call(&self, args: Value) -> anyhow::Result<Value> {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "content": format!("FILE:{path}") }))
        }
    }

    /// `bash` stub: echoes the command with a trailing newline (to prove trimming).
    struct StubBash;
    #[async_trait]
    impl Tool for StubBash {
        fn name(&self) -> &str {
            "bash"
        }
        async fn call(&self, args: Value) -> anyhow::Result<Value> {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "stdout": format!("OUT:{cmd}\n"), "stderr": "", "exit_code": 0 }))
        }
    }

    struct AllowAll;
    impl PermissionGate for AllowAll {
        fn evaluate(&self, _t: &str, _a: &Value) -> Decision {
            Decision::Allow
        }
    }

    /// Denies any call whose `path` arg contains ".env" — a minimal stand-in for the
    /// sensitive-path floor, enough to prove resolve_injections propagates a gate Deny.
    struct DenyEnv;
    impl PermissionGate for DenyEnv {
        fn evaluate(&self, _t: &str, a: &Value) -> Decision {
            let path = a.get("path").and_then(Value::as_str).unwrap_or("");
            if path.contains(".env") {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
    }

    fn registry(gate: Arc<dyn PermissionGate>, tools: Vec<Arc<dyn Tool>>) -> ToolRegistry {
        let mut reg = ToolRegistry::new(gate, Arc::new(DenyAsk));
        for t in tools {
            reg.register(t);
        }
        reg
    }

    #[tokio::test]
    async fn resolves_file_and_command_injections() {
        let reg = registry(Arc::new(AllowAll), vec![Arc::new(StubRead), Arc::new(StubBash)]);
        let out = resolve_injections("see @src/a.rs then !`echo hi` done", &reg)
            .await
            .unwrap();
        assert!(out.contains("FILE:src/a.rs"), "got: {out}");
        assert!(out.contains("OUT:echo hi"), "got: {out}");
        // Trailing newline from bash stdout is trimmed.
        assert!(!out.contains("OUT:echo hi\n"), "got: {out}");
    }

    #[tokio::test]
    async fn plain_text_is_unchanged() {
        let reg = registry(Arc::new(AllowAll), vec![]);
        assert_eq!(
            resolve_injections("no markers here", &reg).await.unwrap(),
            "no markers here"
        );
    }

    #[tokio::test]
    async fn at_sign_mid_token_is_not_a_file_ref() {
        // `@` after a non-whitespace char (an email) must not trigger injection. No fs.read is
        // registered, so a wrongful match would error — asserting Ok proves it was left alone.
        let reg = registry(Arc::new(AllowAll), vec![]);
        let out = resolve_injections("ping rob@host now", &reg).await.unwrap();
        assert_eq!(out, "ping rob@host now");
    }

    #[tokio::test]
    async fn sensitive_file_injection_fails_closed() {
        let reg = registry(Arc::new(DenyEnv), vec![Arc::new(StubRead)]);
        let err = resolve_injections("secret: @.env", &reg).await;
        assert!(err.is_err(), "expected @.env to be denied");
    }

    #[tokio::test]
    async fn bash_absent_fails_closed() {
        // No `bash` tool registered → !`...` must error, not silently inline empty.
        let reg = registry(Arc::new(AllowAll), vec![Arc::new(StubRead)]);
        let err = resolve_injections("run !`echo hi`", &reg).await;
        assert!(err.is_err(), "expected absent bash to fail closed");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions command_expand::tests`
Expected: FAIL — the five `resolve_injections` tests fail on `bail!("not implemented")`.

- [ ] **Step 3: Implement `resolve_injections`**

Replace the `resolve_injections` stub (and drop the now-unused `json!(null)` line). The function single-passes the text, replacing markers via gated tool calls:

```rust
pub async fn resolve_injections(text: &str, tools: &ToolRegistry) -> anyhow::Result<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // `@path` only triggers at the start of input or right after whitespace, so `rob@host`
    // (an email) is never treated as a file reference.
    let mut at_boundary = true;

    while i < chars.len() {
        let c = chars[i];

        // !`cmd` — run via the gated bash tool, inline trimmed stdout.
        if c == '!' && i + 1 < chars.len() && chars[i + 1] == '`' {
            if let Some(close) = (i + 2..chars.len()).find(|&j| chars[j] == '`') {
                let cmd: String = chars[i + 2..close].iter().collect();
                let res = tools
                    .call("bash", json!({ "command": cmd }))
                    .await
                    .map_err(|e| anyhow::anyhow!("command injection `!{cmd}` failed: {e}"))?;
                let stdout = res.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(stdout.trim_end());
                i = close + 1;
                at_boundary = false;
                continue;
            }
        }

        // @path — read via the gated fs.read tool, inline file contents.
        if c == '@' && at_boundary {
            let mut k = i + 1;
            while k < chars.len() && !chars[k].is_whitespace() && chars[k] != '`' {
                k += 1;
            }
            if k > i + 1 {
                let path: String = chars[i + 1..k].iter().collect();
                let res = tools
                    .call("fs.read", json!({ "path": path }))
                    .await
                    .map_err(|e| anyhow::anyhow!("file injection `@{path}` failed: {e}"))?;
                let content = res.get("content").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(content);
                i = k;
                at_boundary = false;
                continue;
            }
        }

        out.push(c);
        at_boundary = c.is_whitespace();
        i += 1;
    }

    Ok(out)
}
```

Also remove the leftover stub line `let _ = (text, tools, json!(null));` — the new body uses all params.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions command_expand`
Expected: PASS — all nine `command_expand` tests (4 `expand_args` + 5 `resolve_injections`) pass.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/command_expand.rs
git commit -m "feat(extensions): resolve gated !bash/@file injections, fail-closed"
```

---

## Task 4: Recursive namespaced discovery

**Files:**
- Modify: `crates/extensions/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/extensions/src/lib.rs`, add these tests to the existing `mod tests` block (after `missing_dirs_yield_empty`). They use a new `write_command` helper — add it next to the existing `write_agent` helper:

```rust
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
        let dup: Vec<_> = ext.commands.iter().filter(|c| c.name == "git:commit").collect();
        assert_eq!(dup.len(), 1, "name collision should collapse to one");
        assert_eq!(dup[0].template.trim(), "PROJECT");
    }

    #[test]
    fn missing_command_dir_yields_no_commands() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_agent(proj.path(), "a.md", "---\nname: a\ndescription: d\n---\nb\n");
        let ext = discover(proj.path(), home.path());
        assert!(ext.commands.is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions discovers_commands_recursively_with_namespaces`
Expected: FAIL to compile — `Extensions` has no `commands` field.

- [ ] **Step 3: Add the `commands` field and recursive discovery**

In `crates/extensions/src/lib.rs`:

(a) Add the field to `Extensions`:

```rust
/// Everything discovered from the `.claude/` directories. Slice 1: custom agents.
/// Slice 2: commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    pub agents: Vec<CustomAgentDef>,
    pub commands: Vec<CustomCommandDef>,
}
```

(b) Add `CustomCommandDef`/`parse_command_md` to the imports the module needs — they are already re-exported via `pub use command_def::...` from Task 1, so `discover` can refer to `CustomCommandDef` directly. Replace the body of `discover` to populate both:

```rust
pub fn discover(project_root: &Path, home: &Path) -> Extensions {
    // User-global first, then project — so a later project insert overrides by name.
    let mut agents: std::collections::BTreeMap<String, CustomAgentDef> =
        std::collections::BTreeMap::new();
    let mut commands: std::collections::BTreeMap<String, CustomCommandDef> =
        std::collections::BTreeMap::new();
    for base in [home, project_root] {
        let claude = base.join(".claude");
        for def in read_agents_dir(&claude.join("agents")) {
            agents.insert(def.name.clone(), def);
        }
        for def in read_commands_dir(&claude.join("commands")) {
            commands.insert(def.name.clone(), def);
        }
    }
    Extensions {
        agents: agents.into_values().collect(),
        commands: commands.into_values().collect(),
    }
}
```

(c) Add the recursive reader + name helper below `read_agents_dir`:

```rust
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
                eprintln!("warning: skipping unreadable command {}: {e}", path.display());
                continue;
            }
        };
        match parse_command_md(&name, &text) {
            Ok(def) => out.push(def),
            Err(e) => eprintln!("warning: skipping malformed command {}: {e}", path.display()),
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions`
Expected: PASS — the three new discovery tests pass and all pre-existing `extensions` tests stay green.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/lib.rs
git commit -m "feat(extensions): recursive namespaced commands/*.md discovery"
```

---

## Task 5: CLI wiring — `otto run --command <name> [args...]`

**Files:**
- Modify: `crates/engine/src/main.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/engine/src/main.rs`, add to the `mod tests` block (after `run_custom_agent_dispatches_and_errors_on_unknown`):

```rust
    #[test]
    fn parse_command_flag_extracts_name_and_keeps_args() {
        let args = vec![
            "--command".to_string(),
            "git:commit".to_string(),
            "fix".to_string(),
            "parser".to_string(),
        ];
        let (name, rest) = parse_command_flag(&args);
        assert_eq!(name, Some("git:commit".to_string()));
        assert_eq!(rest, vec!["fix".to_string(), "parser".to_string()]);
    }

    #[test]
    fn parse_command_flag_absent_is_none() {
        let args = vec!["just a goal".to_string()];
        let (name, rest) = parse_command_flag(&args);
        assert_eq!(name, None);
        assert_eq!(rest, vec!["just a goal".to_string()]);
    }

    #[tokio::test]
    async fn run_command_expands_and_runs_spine() {
        use std::fs;
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global commands
        let cmds = proj.path().join(".claude").join("commands").join("greet");
        fs::create_dir_all(&cmds).unwrap();
        fs::write(cmds.join("hello.md"), "Say hello to $1.\n").unwrap();

        // Known command expands ($1 → "world") and runs an offline, deterministic spine turn.
        let ok = run_command_in(
            "greet:hello",
            &["world".to_string()],
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(ok.is_ok(), "expected command run to succeed: {ok:?}");

        // Unknown command name errors.
        let err = run_command_in(
            "nope",
            &[],
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("no command named"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine --bin otto parse_command_flag`
Expected: FAIL to compile — `parse_command_flag` and `run_command_in` do not exist yet.

- [ ] **Step 3: Add `parse_command_flag`**

In `crates/engine/src/main.rs`, add below `parse_agent_flag` (after line 72):

```rust
/// Parse `--command <name>` from args. Returns (Some(name), remaining) or (None, args). The
/// remaining args are the command's positional arguments ($1.., $ARGUMENTS).
fn parse_command_flag(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut name = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--command" {
            match it.next() {
                Some(v) => name = Some(v.clone()),
                None => {
                    eprintln!("error: --command requires a name");
                    std::process::exit(2);
                }
            }
        } else {
            rest.push(a.clone());
        }
    }
    (name, rest)
}
```

- [ ] **Step 4: Add the `--command` branch to `cmd_run`**

Replace the head of `cmd_run` (the current lines 195-205) so command parsing happens before the goal is required (a command may legitimately take zero args):

```rust
async fn cmd_run(args: Vec<String>) -> anyhow::Result<()> {
    let (root, after_root) = parse_root(&args);
    let (command_name, after_cmd) = parse_command_flag(&after_root);
    let (agent_name, positional) = parse_agent_flag(&after_cmd);

    if command_name.is_some() && agent_name.is_some() {
        eprintln!("error: --command and --agent are mutually exclusive");
        std::process::exit(2);
    }

    if let Some(cmd) = command_name {
        return run_command_in(&cmd, &positional, root, home_dir()).await;
    }

    let goal = positional.into_iter().next().unwrap_or_else(|| {
        eprintln!("error: missing goal");
        std::process::exit(2);
    });

    if let Some(name) = agent_name {
        return run_custom_agent(&name, &goal, root).await;
    }
```

(Leave the rest of `cmd_run` — from `let router ...` through the end — unchanged.)

- [ ] **Step 5: Add `run_command_in`**

Add after `run_custom_agent_in` (after line 291):

```rust
/// Expand a discovered command (`expand_args` then gated `!bash`/`@file` injection) and run the
/// result as the goal of a normal spine turn. `home` is injected so tests stay hermetic.
async fn run_command_in(
    name: &str,
    args: &[String],
    root: PathBuf,
    home: PathBuf,
) -> anyhow::Result<()> {
    use otto_extensions::{expand_args, resolve_injections};

    let ext = otto_extensions::discover(&root, &home);
    let def = ext
        .commands
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no command named '{name}' in ~/.claude/commands/ or {}/.claude/commands/",
                root.display()
            )
        })?;

    // The gated tool registry: injection reaches fs.read/bash through the same gate the spine
    // turn uses (bash only when a sandbox backend exists). Reused as the turn's tools.
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let (tools, _mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), false).await;
    // _mcp_conns is held until end of function so the mcp children stay alive.
    let tools = Arc::new(tools);

    let expanded = expand_args(&def.template, args);
    let goal = resolve_injections(&expanded, tools.as_ref()).await?;

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);
    let retriever = otto_engine::build_retriever(&root).await;

    let (events, outcome) =
        run_goal(&goal, store, router, orch_workspace, tools, retriever).await?;
    for event in &events {
        println!("[{:>3}] {:?}", event.seq, event.kind);
    }
    println!("turn ok = {}", outcome.ok);
    if !outcome.ok {
        std::process::exit(1);
    }
    Ok(())
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p otto-engine --bin otto`
Expected: PASS — `parse_command_flag_*` and `run_command_expands_and_runs_spine` pass alongside the existing `--agent` tests.

- [ ] **Step 7: Update the usage string**

In `crates/engine/src/main.rs`, update the `run` usage line (line 26) to mention `--command`:

```rust
                "usage:\n  otto run \"<goal>\" [--root <path>] [--agent <name> | --command <name> [args...]]\n  otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm] [--accept-promotions]"
```

Also update the module doc comment at the top of `main.rs` (line 1) to mention the command entry:

```rust
//! `otto run "<goal>" [--root <path>] [--agent <name> | --command <name> [args...]]` — run a single turn, a named custom agent, or an expanded command.
```

- [ ] **Step 8: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): otto run --command expands and dispatches a spine turn"
```

---

## Task 6: Workspace verification + docs

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Full workspace build, test, lint, format**

Run:
```bash
cargo fmt --all
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```
Expected: all succeed; the offline determinism suite is green (no `.claude/` in the test roots ⇒ no commands discovered, `otto run` unchanged).

If clippy flags anything in the new code, fix it and re-run before continuing.

- [ ] **Step 2: Update `CLAUDE.md`**

In `CLAUDE.md`, extend the `extensions` row of the crate table to note commands. Find the `extensions` row (it currently ends after the custom-agents description) and append:

```
**Slice 2** adds **commands**: recursive discovery of `commands/**.md` from `~/.claude/` + the project `.claude/` (project wins by **namespaced** name, `git/commit.md` → `git:commit`), Claude-Code-compatible parsing (optional frontmatter `description`/`argument-hint`/`model`/`allowed-tools`; a bare prompt file is valid), two-stage expansion (`expand_args` for `$ARGUMENTS`/`$1..$9`, then `resolve_injections` for `` !`cmd` ``/`@path` through the **gated** `bash`/`fs.read` — fail-closed, so the sensitive-path floor still denies `@.env`), and an `otto run --command <name> [args...]` entry that runs the expanded text as a normal spine turn. `model`/`allowed-tools` are parsed and preserved but not yet routed/enforced.
```

- [ ] **Step 3: Update `docs/ARCHITECTURE.md`**

In `docs/ARCHITECTURE.md`, the "Claude Code compatibility" bullet for commands currently reads:

```
- `commands/*.md` → command registry (prompt templates for the palette).
```

Replace it with the shipped state:

```
- `commands/*.md` → command registry (recursive, namespaced `git:commit`); expanded (`$ARGUMENTS`/`$1..$9` + gated `!bash`/`@file` injection) and run as a spine turn via `otto run --command`. (`model`/`allowed-tools` preserved, not yet routed/enforced.)
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md
git commit -m "docs: record shipped commands extensions slice"
```

---

## Done criteria

- `cargo test --workspace` passes; `cargo clippy --workspace --all-targets` is clean; `cargo fmt --all` leaves no diff.
- `otto run --command <name> [args...]` discovers a namespaced command, expands args, resolves gated `!bash`/`@file` injection (fail-closed), and runs the expansion through the normal spine turn.
- Every spec requirement is covered: recursive namespaced discovery (Task 4), optional-frontmatter parsing (Task 1), `expand_args` (Task 2), gated fail-closed injection (Task 3), CLI dispatch through `run_goal` (Task 5), `model`/`allowed-tools` parsed-but-inert (Tasks 1/Done), hermetic discovery + untouched determinism suite (Tasks 4-6).
- No `engine-core` changes; `extensions` still depends only on `engine-core`/`protocol`.
```
