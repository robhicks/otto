# Extensions Hooks (PreToolUse/PostToolUse) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Discover Claude Code `settings.json` `PreToolUse`/`PostToolUse` hooks and fire them around tool dispatch — a `PreToolUse` hook may block a tool call, `PostToolUse` observes — executed through otto's shared OS sandbox.

**Architecture:** A `HookedTool` decorator wraps each registered `Tool` in the `otto run` wiring. Because `ToolRegistry::call` runs the permission gate *before* dispatching, hooks fire strictly below the gate (they can deny an allowed call, never widen a denied one). The `extensions` crate owns parsing/discovery/matching plus a `HookExecutor` seam; the `engine` binary supplies a `SandboxedHookExecutor` built on a new stdin-capable variant of the shared `run_sandboxed` core. The only `engine-core` change is a generic `ToolRegistry::wrap_each(closure)` helper — it takes a closure, so `engine-core` gains no dependency on `extensions` and the orchestrator/offline-determinism suite is untouched.

**Tech Stack:** Rust (edition 2024), `async-trait`, `serde_json`, `tokio` (process/io-util), `anyhow`; tests with `tempfile`. Spec: `docs/superpowers/specs/2026-06-25-extensions-hooks-design.md`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/extensions/src/hook_def.rs` (create) | `HookCommand`/`HookMatcher`/`HookSet` data types + `parse_hooks(settings_json)`. |
| `crates/extensions/src/hook_exec.rs` (create) | `HookEvent`, `HookOutcome`, the `HookExecutor` seam, `matcher_selects`, and `impl HookSet { matched }`. |
| `crates/extensions/src/hooked_tool.rs` (create) | `HookedTool` decorator (`Tool` impl) + `HookedTool::wrap`. |
| `crates/extensions/src/lib.rs` (modify) | Declare/`pub use` the new modules; add `hooks` to `Extensions`; read `<base>/.claude/settings.json` in `discover`. |
| `crates/tools/src/sandbox.rs` (modify) | Add `run_sandboxed_with_stdin`; make `run_sandboxed` delegate. |
| `crates/tools/src/lib.rs` (modify) | Re-export `run_sandboxed_with_stdin`. |
| `crates/engine-core/src/tool.rs` (modify) | Add generic `ToolRegistry::wrap_each`. |
| `crates/engine/src/hooks.rs` (create) | `SandboxedHookExecutor` (`HookExecutor` over `run_sandboxed_with_stdin`, `Os` policy). |
| `crates/engine/src/lib.rs` (modify) | `mod hooks; pub use hooks::SandboxedHookExecutor;`. |
| `crates/engine/src/main.rs` (modify) | Wrap registered tools in `cmd_run` when hooks discovered + sandbox available; integration test. |
| `docs/ARCHITECTURE.md`, `CLAUDE.md` (modify) | Document the shipped hooks slice. |

---

## Task 1: `HookSet` types + `parse_hooks`

**Files:**
- Create: `crates/extensions/src/hook_def.rs`
- Modify: `crates/extensions/src/lib.rs` (add `mod hook_def;` + `pub use`)

- [ ] **Step 1: Declare the module so the test compiles**

In `crates/extensions/src/lib.rs`, add to the existing `mod` block (after `mod command_expand;`) and `pub use` block:

```rust
mod hook_def;
```
```rust
pub use hook_def::{HookCommand, HookMatcher, HookSet, parse_hooks};
```

- [ ] **Step 2: Write `crates/extensions/src/hook_def.rs` with the types, `parse_hooks`, and failing tests**

```rust
//! Claude-Code `settings.json` hooks. This slice parses the `PreToolUse` and `PostToolUse`
//! events only: each is a list of matcher entries, each entry carrying an optional `matcher`
//! (a tool-name selector) plus one or more `type: "command"` hooks. Other events (SessionStart,
//! Stop, …) parse without error but are not collected. Advanced JSON-stdout control is not parsed
//! here — the runner honors the exit-code contract only.

use serde_json::Value;

/// One `type: "command"` hook: the shell command plus an optional per-hook timeout (seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    pub command: String,
    pub timeout: Option<u64>,
}

/// A matcher entry: which tools it selects (`None`/`""`/`"*"` = all) and the hooks to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookMatcher {
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
}

/// All discovered tool-dispatch hooks. `Default` is the empty set (no hooks configured).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookSet {
    pub pre_tool_use: Vec<HookMatcher>,
    pub post_tool_use: Vec<HookMatcher>,
}

/// Parse a `settings.json` document into its tool-dispatch hooks. A missing `hooks` object (or a
/// settings file with no hooks) yields an empty `HookSet`. Invalid JSON is an error. Individual
/// hook entries that are not `type: "command"` or that lack a non-empty `command` are skipped; a
/// matcher entry left with no runnable commands is dropped.
pub fn parse_hooks(settings_json: &str) -> anyhow::Result<HookSet> {
    let v: Value = serde_json::from_str(settings_json)?;
    let Some(hooks) = v.get("hooks").and_then(|h| h.as_object()) else {
        return Ok(HookSet::default());
    };
    Ok(HookSet {
        pre_tool_use: parse_event(hooks.get("PreToolUse")),
        post_tool_use: parse_event(hooks.get("PostToolUse")),
    })
}

fn parse_event(val: Option<&Value>) -> Vec<HookMatcher> {
    let Some(arr) = val.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in arr {
        let matcher = entry
            .get("matcher")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());
        let mut cmds = Vec::new();
        if let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) {
            for h in hooks {
                let is_command = h.get("type").and_then(|t| t.as_str()) == Some("command");
                let command = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                if !is_command || command.is_empty() {
                    continue;
                }
                cmds.push(HookCommand {
                    command: command.to_string(),
                    timeout: h.get("timeout").and_then(|t| t.as_u64()),
                });
            }
        }
        if !cmds.is_empty() {
            out.push(HookMatcher { matcher, hooks: cmds });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pre_and_post_with_matcher_and_timeout() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "bash",
                      "hooks": [ { "type": "command", "command": "block.sh", "timeout": 5 } ] }
                ],
                "PostToolUse": [
                    { "hooks": [ { "type": "command", "command": "log.sh" } ] }
                ]
            }
        }"#;
        let set = parse_hooks(json).unwrap();
        assert_eq!(set.pre_tool_use.len(), 1);
        assert_eq!(set.pre_tool_use[0].matcher.as_deref(), Some("bash"));
        assert_eq!(set.pre_tool_use[0].hooks[0].command, "block.sh");
        assert_eq!(set.pre_tool_use[0].hooks[0].timeout, Some(5));
        assert_eq!(set.post_tool_use.len(), 1);
        assert_eq!(set.post_tool_use[0].matcher, None);
        assert_eq!(set.post_tool_use[0].hooks[0].timeout, None);
    }

    #[test]
    fn missing_hooks_object_is_empty_ok() {
        let set = parse_hooks(r#"{ "model": "x" }"#).unwrap();
        assert_eq!(set, HookSet::default());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_hooks("{ not json").is_err());
    }

    #[test]
    fn non_command_and_commandless_hooks_are_skipped() {
        let json = r#"{
            "hooks": { "PreToolUse": [
                { "matcher": "bash", "hooks": [
                    { "type": "other", "command": "x" },
                    { "type": "command" },
                    { "type": "command", "command": "" }
                ] }
            ] }
        }"#;
        let set = parse_hooks(json).unwrap();
        // The entry has no runnable command → dropped entirely.
        assert!(set.pre_tool_use.is_empty());
    }

    #[test]
    fn unknown_event_keys_ignored() {
        let json = r#"{ "hooks": { "SessionStart": [
            { "hooks": [ { "type": "command", "command": "hi.sh" } ] } ] } }"#;
        let set = parse_hooks(json).unwrap();
        assert_eq!(set, HookSet::default());
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions hook_def::`
Expected: PASS (5 tests).

- [ ] **Step 4: Format**

Run: `cargo fmt --all`

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/hook_def.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): parse settings.json hooks into HookSet"
```

---

## Task 2: Matching + `HookExecutor` seam

**Files:**
- Create: `crates/extensions/src/hook_exec.rs`
- Modify: `crates/extensions/src/lib.rs` (add `mod hook_exec;` + `pub use`)

- [ ] **Step 1: Declare the module**

In `crates/extensions/src/lib.rs` add:

```rust
mod hook_exec;
```
```rust
pub use hook_exec::{HookEvent, HookExecutor, HookOutcome, matcher_selects};
```

- [ ] **Step 2: Write `crates/extensions/src/hook_exec.rs` with matching, the seam, and failing tests**

```rust
//! Hook matching + the execution seam. `extensions` stays hermetic by depending only on a
//! `HookExecutor` trait; the engine binary supplies the sandboxed implementation. Matching is
//! intentionally simple this slice: `None`/`""`/`"*"` selects every tool, otherwise the matcher
//! is a `|`-separated list of exact tool names (regex is future work).

use std::time::Duration;

use async_trait::async_trait;

use crate::hook_def::{HookCommand, HookSet};

/// Which tool-dispatch lifecycle point a hook fires at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
}

/// The result of running one hook command. `exit_code` is `None` if the process was killed
/// (e.g. by a signal) rather than exiting normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a single hook command, piping `stdin_json` to its stdin, killed after `timeout`. The
/// engine supplies a sandboxed implementation; tests supply a fake. An `Err` means the command
/// could not be run (no backend, spawn failure, timeout) — the caller treats that as
/// non-blocking.
#[async_trait]
pub trait HookExecutor: Send + Sync {
    async fn run(
        &self,
        command: &str,
        stdin_json: &str,
        timeout: Duration,
    ) -> anyhow::Result<HookOutcome>;
}

/// Does `matcher` select `tool_name`? `None`/`""`/`"*"` match everything; otherwise the matcher
/// is split on `|` and each token compared for exact equality (trimmed).
pub fn matcher_selects(matcher: &Option<String>, tool_name: &str) -> bool {
    match matcher.as_deref() {
        None | Some("") | Some("*") => true,
        Some(pat) => pat.split('|').any(|t| t.trim() == tool_name),
    }
}

impl HookSet {
    /// The hook commands that should fire for `event` on `tool_name`, in declaration order
    /// (user-base entries first, then project — see discovery).
    pub fn matched(&self, event: HookEvent, tool_name: &str) -> Vec<HookCommand> {
        let matchers = match event {
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PostToolUse => &self.post_tool_use,
        };
        matchers
            .iter()
            .filter(|m| matcher_selects(&m.matcher, tool_name))
            .flat_map(|m| m.hooks.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_def::{HookMatcher, HookSet};

    fn cmd(c: &str) -> HookCommand {
        HookCommand { command: c.to_string(), timeout: None }
    }

    #[test]
    fn matcher_wildcard_and_none_match_all() {
        assert!(matcher_selects(&None, "bash"));
        assert!(matcher_selects(&Some("".to_string()), "bash"));
        assert!(matcher_selects(&Some("*".to_string()), "bash"));
    }

    #[test]
    fn matcher_exact_and_alternation() {
        assert!(matcher_selects(&Some("bash".to_string()), "bash"));
        assert!(!matcher_selects(&Some("bash".to_string()), "fs.read"));
        assert!(matcher_selects(&Some("bash | fs.read".to_string()), "fs.read"));
        assert!(!matcher_selects(&Some("bash|grep".to_string()), "fs.write"));
    }

    #[test]
    fn matched_collects_in_order_for_event_and_tool() {
        let set = HookSet {
            pre_tool_use: vec![
                HookMatcher { matcher: Some("bash".to_string()), hooks: vec![cmd("a")] },
                HookMatcher { matcher: None, hooks: vec![cmd("b")] },
                HookMatcher { matcher: Some("grep".to_string()), hooks: vec![cmd("c")] },
            ],
            post_tool_use: vec![HookMatcher { matcher: None, hooks: vec![cmd("d")] }],
        };
        let pre: Vec<_> = set
            .matched(HookEvent::PreToolUse, "bash")
            .into_iter()
            .map(|h| h.command)
            .collect();
        assert_eq!(pre, vec!["a", "b"]); // "grep"-only entry excluded
        let post: Vec<_> = set
            .matched(HookEvent::PostToolUse, "bash")
            .into_iter()
            .map(|h| h.command)
            .collect();
        assert_eq!(post, vec!["d"]);
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-extensions hook_exec::`
Expected: PASS (3 tests).

- [ ] **Step 4: Format + commit**

```bash
cargo fmt --all
git add crates/extensions/src/hook_exec.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): hook matcher + HookExecutor seam"
```

---

## Task 3: `HookedTool` decorator

**Files:**
- Create: `crates/extensions/src/hooked_tool.rs`
- Modify: `crates/extensions/src/lib.rs` (add `mod hooked_tool;` + `pub use`)

- [ ] **Step 1: Declare the module**

In `crates/extensions/src/lib.rs` add:

```rust
mod hooked_tool;
```
```rust
pub use hooked_tool::HookedTool;
```

- [ ] **Step 2: Write `crates/extensions/src/hooked_tool.rs` with the decorator and failing tests**

```rust
//! `HookedTool`: a `Tool` decorator that fires matched `PreToolUse` hooks before the inner call
//! (a `PreToolUse` hook may BLOCK by exiting 2) and `PostToolUse` hooks after (observe-only). It
//! wraps the inner tool only when at least one hook matches the tool's name — otherwise `wrap`
//! returns the inner `Arc` unchanged, so un-hooked tools pay zero overhead. The decorator runs
//! INSIDE the inner tool's `call`, which `ToolRegistry::call` reaches only after the permission
//! gate allows — so a hook can deny an allowed call but never widen a denied one.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use serde_json::{Value, json};

use crate::hook_def::{HookCommand, HookSet};
use crate::hook_exec::{HookEvent, HookExecutor};

/// Default per-hook timeout when a `HookCommand` does not specify one.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

pub struct HookedTool {
    inner: Arc<dyn Tool>,
    pre: Vec<HookCommand>,
    post: Vec<HookCommand>,
    executor: Arc<dyn HookExecutor>,
}

impl HookedTool {
    /// Wrap `inner` with the hooks matching `inner.name()`. Returns the inner `Arc` unchanged
    /// when no pre/post hook matches.
    pub fn wrap(
        inner: Arc<dyn Tool>,
        hooks: &HookSet,
        executor: Arc<dyn HookExecutor>,
    ) -> Arc<dyn Tool> {
        let pre = hooks.matched(HookEvent::PreToolUse, inner.name());
        let post = hooks.matched(HookEvent::PostToolUse, inner.name());
        if pre.is_empty() && post.is_empty() {
            return inner;
        }
        Arc::new(HookedTool { inner, pre, post, executor })
    }

    /// Run a list of hooks against `input`. When `blocking`, an exit code of 2 aborts with an
    /// error (used for `PreToolUse`). Otherwise — and for any other nonzero exit, an executor
    /// error, or a timeout — the hook is a non-blocking warning and execution proceeds.
    async fn fire(&self, hooks: &[HookCommand], input: &str, blocking: bool) -> anyhow::Result<()> {
        let name = self.inner.name();
        for hook in hooks {
            let timeout =
                Duration::from_secs(hook.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS));
            match self.executor.run(&hook.command, input, timeout).await {
                Ok(out) if out.exit_code == Some(2) && blocking => {
                    anyhow::bail!("tool '{name}' blocked by PreToolUse hook: {}", out.stderr.trim());
                }
                Ok(out) if out.exit_code != Some(0) => {
                    eprintln!(
                        "warning: hook for '{name}' exited {:?} (non-blocking): {}",
                        out.exit_code,
                        out.stderr.trim()
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!("warning: hook for '{name}' failed to run (non-blocking): {e}"),
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for HookedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let name = self.inner.name();
        if !self.pre.is_empty() {
            let input = json!({
                "hook_event_name": "PreToolUse",
                "tool_name": name,
                "tool_input": args.clone(),
            })
            .to_string();
            self.fire(&self.pre, &input, true).await?;
        }

        let result = self.inner.call(args.clone()).await?;

        if !self.post.is_empty() {
            let input = json!({
                "hook_event_name": "PostToolUse",
                "tool_name": name,
                "tool_input": args,
                "tool_response": result.clone(),
            })
            .to_string();
            // PostToolUse is observe-only: never blocks (errors are logged inside `fire`).
            let _ = self.fire(&self.post, &input, false).await;
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_def::{HookMatcher, HookSet};
    use std::sync::Mutex;

    // --- A recording inner tool ---
    struct SpyTool {
        called: Mutex<u32>,
    }
    #[async_trait]
    impl Tool for SpyTool {
        fn name(&self) -> &str {
            "fs.read"
        }
        async fn call(&self, _args: Value) -> anyhow::Result<Value> {
            *self.called.lock().unwrap() += 1;
            Ok(json!({ "ok": true }))
        }
    }

    // --- A scripted executor that records what it was asked to run ---
    struct FakeExec {
        exit: Option<i32>,
        seen: Mutex<Vec<(String, String)>>, // (command, stdin_json)
    }
    #[async_trait]
    impl HookExecutor for FakeExec {
        async fn run(
            &self,
            command: &str,
            stdin_json: &str,
            _timeout: Duration,
        ) -> anyhow::Result<HookOutcome> {
            self.seen
                .lock()
                .unwrap()
                .push((command.to_string(), stdin_json.to_string()));
            Ok(HookOutcome {
                exit_code: self.exit,
                stdout: String::new(),
                stderr: "nope".to_string(),
            })
        }
    }
    use crate::hook_exec::HookOutcome;

    fn pre_set(matcher: &str, command: &str) -> HookSet {
        HookSet {
            pre_tool_use: vec![HookMatcher {
                matcher: Some(matcher.to_string()),
                hooks: vec![HookCommand { command: command.to_string(), timeout: None }],
            }],
            post_tool_use: vec![],
        }
    }

    #[tokio::test]
    async fn wrap_returns_inner_when_no_hooks_match() {
        let inner: Arc<dyn Tool> = Arc::new(SpyTool { called: Mutex::new(0) });
        let exec = Arc::new(FakeExec { exit: Some(0), seen: Mutex::new(vec![]) });
        // Hook targets "bash", inner is "fs.read" → no match → identity.
        let wrapped = HookedTool::wrap(inner.clone(), &pre_set("bash", "x.sh"), exec);
        assert!(Arc::ptr_eq(&inner, &wrapped), "should return the same Arc");
    }

    #[tokio::test]
    async fn pre_exit_2_blocks_and_inner_not_called() {
        let spy = Arc::new(SpyTool { called: Mutex::new(0) });
        let exec = Arc::new(FakeExec { exit: Some(2), seen: Mutex::new(vec![]) });
        let wrapped =
            HookedTool::wrap(spy.clone(), &pre_set("fs.read", "block.sh"), exec.clone());
        let err = wrapped.call(json!({ "path": "a" })).await.unwrap_err();
        assert!(err.to_string().contains("blocked by PreToolUse hook"));
        assert_eq!(*spy.called.lock().unwrap(), 0, "inner must not run when blocked");
        // The hook received PreToolUse input naming the tool.
        let seen = exec.seen.lock().unwrap();
        assert_eq!(seen[0].0, "block.sh");
        assert!(seen[0].1.contains("\"hook_event_name\":\"PreToolUse\""));
        assert!(seen[0].1.contains("\"tool_name\":\"fs.read\""));
    }

    #[tokio::test]
    async fn pre_exit_0_allows_inner_to_run() {
        let spy = Arc::new(SpyTool { called: Mutex::new(0) });
        let exec = Arc::new(FakeExec { exit: Some(0), seen: Mutex::new(vec![]) });
        let wrapped = HookedTool::wrap(spy.clone(), &pre_set("fs.read", "ok.sh"), exec);
        let out = wrapped.call(json!({ "path": "a" })).await.unwrap();
        assert_eq!(out, json!({ "ok": true }));
        assert_eq!(*spy.called.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn pre_other_nonzero_is_non_blocking() {
        let spy = Arc::new(SpyTool { called: Mutex::new(0) });
        let exec = Arc::new(FakeExec { exit: Some(1), seen: Mutex::new(vec![]) });
        let wrapped = HookedTool::wrap(spy.clone(), &pre_set("*", "warn.sh"), exec);
        // exit 1 ≠ 2 → warning, inner still runs.
        assert!(wrapped.call(json!({})).await.is_ok());
        assert_eq!(*spy.called.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn post_runs_after_inner_with_tool_response() {
        let spy = Arc::new(SpyTool { called: Mutex::new(0) });
        let exec = Arc::new(FakeExec { exit: Some(2), seen: Mutex::new(vec![]) });
        let set = HookSet {
            pre_tool_use: vec![],
            post_tool_use: vec![HookMatcher {
                matcher: None,
                hooks: vec![HookCommand { command: "log.sh".to_string(), timeout: None }],
            }],
        };
        let wrapped = HookedTool::wrap(spy.clone(), &set, exec.clone());
        // PostToolUse exit 2 must NOT block — the result is still returned.
        let out = wrapped.call(json!({ "path": "a" })).await.unwrap();
        assert_eq!(out, json!({ "ok": true }));
        assert_eq!(*spy.called.lock().unwrap(), 1);
        let seen = exec.seen.lock().unwrap();
        assert!(seen[0].1.contains("\"hook_event_name\":\"PostToolUse\""));
        assert!(seen[0].1.contains("\"tool_response\":{\"ok\":true}"));
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-extensions hooked_tool::`
Expected: PASS (5 tests).

- [ ] **Step 4: Format + commit**

```bash
cargo fmt --all
git add crates/extensions/src/hooked_tool.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): HookedTool decorator firing pre/post hooks"
```

---

## Task 4: Discovery — read `settings.json` from both bases

**Files:**
- Modify: `crates/extensions/src/lib.rs` (add `hooks` field + read settings.json in `discover`)
- Test: in `crates/extensions/src/lib.rs` `#[cfg(test)] mod tests`

- [ ] **Step 1: Add the field to `Extensions`**

In `crates/extensions/src/lib.rs`, extend the struct:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    pub agents: Vec<CustomAgentDef>,
    pub commands: Vec<CustomCommandDef>,
    pub skills: Vec<CustomSkillDef>,
    pub hooks: HookSet,
}
```

- [ ] **Step 2: Read + concatenate hooks inside `discover`**

In `discover`, before the loop add a `HookSet` accumulator, and inside the `for base in [home, project_root]` loop read `settings.json`. Replace the existing loop + return with:

```rust
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
```

- [ ] **Step 3: Add the `read_settings_hooks` helper**

After `read_skills_dir` (before `command_name`), add:

```rust
/// Read `<base>/.claude/settings.json` and parse its tool-dispatch hooks. A missing file yields
/// no hooks; an unreadable file or one with invalid JSON is skipped with a warning, never fatal.
fn read_settings_hooks(path: &Path) -> HookSet {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HookSet::default(),
        Err(e) => {
            eprintln!("warning: skipping unreadable settings {}: {e}", path.display());
            return HookSet::default();
        }
    };
    match parse_hooks(&text) {
        Ok(set) => set,
        Err(e) => {
            eprintln!("warning: skipping malformed settings {}: {e}", path.display());
            HookSet::default()
        }
    }
}
```

- [ ] **Step 4: Add discovery tests**

In `crates/extensions/src/lib.rs` `mod tests`, add a `write_settings` helper near `write_skill` and these tests:

```rust
    fn write_settings(dir: &Path, body: &str) {
        let claude = dir.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("settings.json"), body).unwrap();
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
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p otto-extensions`
Expected: PASS (all extensions tests, including the 3 new discovery tests). Note `missing_dirs_yield_empty` still passes because `Extensions::default()` now includes an empty `HookSet`.

- [ ] **Step 6: Format + commit**

```bash
cargo fmt --all
git add crates/extensions/src/lib.rs
git commit -m "feat(extensions): discover settings.json hooks (concatenated across bases)"
```

---

## Task 5: stdin-capable sandbox core

**Files:**
- Modify: `crates/tools/src/sandbox.rs`
- Modify: `crates/tools/src/lib.rs` (re-export)

- [ ] **Step 1: Add a failing stdin test**

In `crates/tools/src/sandbox.rs` `mod tests`, add:

```rust
    #[tokio::test]
    async fn run_sandboxed_with_stdin_pipes_payload() {
        use std::time::Duration;
        let root = std::path::PathBuf::from(".");
        let out = run_sandboxed_with_stdin(
            &SandboxPolicy::None,
            &root,
            "cat", // echoes stdin to stdout
            Duration::from_secs(5),
            Some("hello-stdin"),
        )
        .await
        .unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("hello-stdin"));
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p otto-tools run_sandboxed_with_stdin_pipes_payload`
Expected: FAIL — `cannot find function run_sandboxed_with_stdin`.

- [ ] **Step 3: Implement `run_sandboxed_with_stdin` and make `run_sandboxed` delegate**

In `crates/tools/src/sandbox.rs`, replace the entire `run_sandboxed` function (lines beginning `pub async fn run_sandboxed(`) with:

```rust
pub async fn run_sandboxed(
    policy: &SandboxPolicy,
    root: &Path,
    command: &str,
    timeout: Duration,
) -> anyhow::Result<Value> {
    run_sandboxed_with_stdin(policy, root, command, timeout, None).await
}

/// Like [`run_sandboxed`], but optionally pipes `stdin` to the command's standard input (closed
/// after writing, so the child sees EOF). Used to feed a hook its JSON event on stdin. When
/// `stdin` is `None` this behaves exactly as before (stdin is `/dev/null`).
pub async fn run_sandboxed_with_stdin(
    policy: &SandboxPolicy,
    root: &Path,
    command: &str,
    timeout: Duration,
    stdin: Option<&str>,
) -> anyhow::Result<Value> {
    use tokio::io::AsyncWriteExt;

    let (program, argv) = build_argv(policy, root, command)?;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(argv).current_dir(root).env_clear();
    for (key, val) in curated_env() {
        cmd.env(key, val);
    }
    cmd.env("HOME", root)
        .env("TMPDIR", root)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    if let Some(payload) = stdin {
        if let Some(mut handle) = child.stdin.take() {
            handle.write_all(payload.as_bytes()).await?;
            handle.shutdown().await?; // close stdin → child gets EOF
        }
    }
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => anyhow::bail!("bash command timed out after {} ms", timeout.as_millis()),
        Ok(result) => result?,
    };

    Ok(json!({
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "exit_code": output.status.code(),
    }))
}
```

- [ ] **Step 4: Re-export from the crate root**

In `crates/tools/src/lib.rs`, change the sandbox re-export line to:

```rust
pub use sandbox::{
    SandboxPolicy, build_argv, os_sandbox_available, run_sandboxed, run_sandboxed_with_stdin,
};
```

- [ ] **Step 5: Run the full tools suite**

Run: `cargo test -p otto-tools`
Expected: PASS — the new stdin test passes and the existing `run_sandboxed_none_echo_exit_and_timeout` (which exercises the `None`-delegate path) still passes.

- [ ] **Step 6: Format + commit**

```bash
cargo fmt --all
git add crates/tools/src/sandbox.rs crates/tools/src/lib.rs
git commit -m "feat(tools): run_sandboxed_with_stdin (stdin-capable sandbox core)"
```

---

## Task 6: generic `ToolRegistry::wrap_each`

**Files:**
- Modify: `crates/engine-core/src/tool.rs`

- [ ] **Step 1: Add a failing test**

In `crates/engine-core/src/tool.rs` `mod tests`, add (it can reuse `EchoTool`/`PingTool`/`AllowAll`/`DenyAsk` already defined there):

```rust
    struct PrefixTool {
        inner: Arc<dyn Tool>,
    }
    #[async_trait]
    impl Tool for PrefixTool {
        fn name(&self) -> &str {
            self.inner.name()
        }
        async fn call(&self, args: Value) -> anyhow::Result<Value> {
            let inner = self.inner.call(args).await?;
            Ok(json!({ "wrapped": inner }))
        }
    }

    #[tokio::test]
    async fn wrap_each_replaces_every_tool_preserving_name() {
        let mut r = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        r.register(Arc::new(EchoTool));
        r.wrap_each(|t| Arc::new(PrefixTool { inner: t }));
        // Same name still dispatches; output now passes through the wrapper.
        let out = r.call("echo", json!({ "x": 1 })).await.unwrap();
        assert_eq!(out, json!({ "wrapped": { "echoed": { "x": 1 } } }));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p otto-engine-core wrap_each_replaces_every_tool_preserving_name`
Expected: FAIL — `no method named wrap_each`.

- [ ] **Step 3: Implement `wrap_each`**

In `crates/engine-core/src/tool.rs`, inside `impl ToolRegistry`, after `tool_names`, add:

```rust
    /// Replace every registered tool with `f(tool)`, keying by the wrapper's `name()` (wrappers
    /// are expected to preserve the inner name). The gate and ask-resolver are unchanged. This is
    /// a generic capability — the engine uses it to wrap tools with hook decorators — so the core
    /// stays free of any concrete decorator type.
    pub fn wrap_each(&mut self, mut f: impl FnMut(Arc<dyn Tool>) -> Arc<dyn Tool>) {
        let names: Vec<String> = self.tools.keys().cloned().collect();
        for name in names {
            if let Some(tool) = self.tools.remove(&name) {
                let wrapped = f(tool);
                self.tools.insert(wrapped.name().to_string(), wrapped);
            }
        }
    }
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p otto-engine-core tool::`
Expected: PASS.

- [ ] **Step 5: Format + commit**

```bash
cargo fmt --all
git add crates/engine-core/src/tool.rs
git commit -m "feat(engine-core): ToolRegistry::wrap_each generic decorator helper"
```

---

## Task 7: `SandboxedHookExecutor`

**Files:**
- Create: `crates/engine/src/hooks.rs`
- Modify: `crates/engine/src/lib.rs` (`mod hooks;` + `pub use`)

- [ ] **Step 1: Declare the module**

In `crates/engine/src/lib.rs`, add to the `mod` block (after `mod approval;`):

```rust
mod hooks;
```
and to the `pub use` section:

```rust
pub use hooks::SandboxedHookExecutor;
```

- [ ] **Step 2: Write `crates/engine/src/hooks.rs` with the executor and a sandbox-gated test**

```rust
//! `SandboxedHookExecutor`: runs `settings.json` hook commands through the shared OS sandbox
//! core (`SandboxPolicy::Os`), piping the hook's JSON event on stdin. It is the engine-side
//! implementation of `otto_extensions::HookExecutor`; the orchestrator never constructs it, so
//! the offline determinism suite is unaffected. Built only when an OS sandbox backend exists.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use otto_extensions::{HookExecutor, HookOutcome};
use otto_tools::{SandboxPolicy, run_sandboxed_with_stdin};

pub struct SandboxedHookExecutor {
    root: PathBuf,
}

impl SandboxedHookExecutor {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl HookExecutor for SandboxedHookExecutor {
    async fn run(
        &self,
        command: &str,
        stdin_json: &str,
        timeout: Duration,
    ) -> anyhow::Result<HookOutcome> {
        let out = run_sandboxed_with_stdin(
            &SandboxPolicy::Os { allow_net: false },
            &self.root,
            command,
            timeout,
            Some(stdin_json),
        )
        .await?;
        Ok(HookOutcome {
            exit_code: out["exit_code"].as_i64().map(|c| c as i32),
            stdout: out["stdout"].as_str().unwrap_or("").to_string(),
            stderr: out["stderr"].as_str().unwrap_or("").to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_stdin_and_reports_exit_code() {
        if !otto_tools::os_sandbox_available() {
            return; // fail-closed: no backend → nothing to test here
        }
        let dir = tempfile::tempdir().unwrap();
        let exec = SandboxedHookExecutor::new(dir.path().to_path_buf());
        // Exit 2 if stdin contains "PreToolUse", else 0 — proves stdin is delivered.
        let out = exec
            .run(
                "grep -q PreToolUse && exit 2 || exit 0",
                r#"{"hook_event_name":"PreToolUse"}"#,
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(2));
    }
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p otto-engine hooks::`
Expected: PASS (the assertion runs only when a sandbox backend is present; otherwise the test no-ops).

- [ ] **Step 4: Format + commit**

```bash
cargo fmt --all
git add crates/engine/src/hooks.rs crates/engine/src/lib.rs
git commit -m "feat(engine): SandboxedHookExecutor over run_sandboxed_with_stdin"
```

---

## Task 8: wire hooks into `cmd_run` + integration test

**Files:**
- Modify: `crates/engine/src/main.rs` (`cmd_run` wiring + a `register_hooks` helper + test)

- [ ] **Step 1: Add a `register_hooks` helper**

In `crates/engine/src/main.rs`, after the existing `register_skills` function (around line 226), add:

```rust
/// Wrap every registered tool with hook decorators when hooks were discovered AND an OS sandbox
/// backend exists. No hooks, or no sandbox backend → no wrapping, so the tool set is unchanged
/// (fail-closed, mirroring how `bash` is absent without a sandbox).
fn register_hooks(
    registry: &mut otto_engine_core::tool::ToolRegistry,
    hooks: &otto_extensions::HookSet,
    root: &std::path::Path,
) {
    if *hooks == otto_extensions::HookSet::default() || !otto_tools::os_sandbox_available() {
        return;
    }
    let exec: Arc<dyn otto_extensions::HookExecutor> =
        Arc::new(otto_engine::SandboxedHookExecutor::new(root.to_path_buf()));
    registry.wrap_each(|t| otto_extensions::HookedTool::wrap(t, hooks, exec.clone()));
}
```

- [ ] **Step 2: Call it in `cmd_run`**

In `cmd_run`, find the lines (around 258-260):

```rust
    let ext = otto_extensions::discover(&root, &home_dir());
    register_skills(&mut tools, &ext.skills);
    let tools = Arc::new(tools);
```

Insert the hooks wiring between `register_skills` and the `Arc::new`:

```rust
    let ext = otto_extensions::discover(&root, &home_dir());
    register_skills(&mut tools, &ext.skills);
    register_hooks(&mut tools, &ext.hooks, &root);
    let tools = Arc::new(tools);
```

- [ ] **Step 3: Add an integration test**

In `crates/engine/src/main.rs` `#[cfg(test)] mod tests` (the module that already builds registries over tempdirs, around line 655), add:

```rust
    #[tokio::test]
    async fn discovered_pretooluse_hook_blocks_a_tool_call() {
        use otto_engine_core::tool::ToolRegistry;
        if !otto_tools::os_sandbox_available() {
            return; // no sandbox backend → hooks fail-closed (not wired); nothing to assert
        }
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        // A PreToolUse hook on fs.read that always blocks (exit 2).
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"fs.read","hooks":[{"type":"command","command":"exit 2"}]}]}}"#,
        )
        .unwrap();

        let ws: Arc<dyn Workspace> =
            Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let mut tools: ToolRegistry =
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf());
        let ext = otto_extensions::discover(proj.path(), home.path());
        super::register_hooks(&mut tools, &ext.hooks, proj.path());

        // fs.read is gate-Allowed, then the PreToolUse hook blocks it.
        let err = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("blocked by PreToolUse hook"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn no_settings_leaves_tools_unwrapped() {
        use otto_engine_core::tool::ToolRegistry;
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        let ws: Arc<dyn Workspace> =
            Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let mut tools: ToolRegistry =
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf());
        let ext = otto_extensions::discover(proj.path(), home.path());
        super::register_hooks(&mut tools, &ext.hooks, proj.path());

        // No hooks → fs.read behaves normally.
        let out = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap();
        assert!(out.to_string().contains("hi"));
    }
```

Note: confirm `tempfile`, `LocalWorkspace`, and `Workspace` are already in scope in this test module (they are used by the existing tests around line 655). If `tempfile` is not a dev-dependency of `otto-engine`, add `tempfile.workspace = true` under `[dev-dependencies]` in `crates/engine/Cargo.toml`.

- [ ] **Step 4: Run the engine tests**

Run: `cargo test -p otto-engine`
Expected: PASS — both new tests pass (the blocking one runs its assertion only with a sandbox backend present).

- [ ] **Step 5: Full workspace build + test (offline determinism intact)**

Run: `cargo test --workspace`
Expected: PASS — the orchestrator/offline suite is unchanged (no `.claude/` in those tests → no wrapping).

- [ ] **Step 6: Format, clippy, commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
git add crates/engine/src/main.rs crates/engine/Cargo.toml
git commit -m "feat(engine): wire PreToolUse/PostToolUse hooks into otto run"
```

---

## Task 9: documentation

**Files:**
- Modify: `docs/ARCHITECTURE.md` (the `extensions` row + the hooks bullet)
- Modify: `CLAUDE.md` (the `extensions` crate row)

- [ ] **Step 1: Update `docs/ARCHITECTURE.md`**

In the "Claude Code compatibility" list, replace the hooks bullet:

```
- `settings.json` hooks → `HookRegistry`, fired at the same lifecycle points.
```

with:

```
- `settings.json` hooks → discovered (`PreToolUse`/`PostToolUse`, concatenated across user+project)
  and fired around tool dispatch via a `HookedTool` decorator, executed through the shared OS
  sandbox; a `PreToolUse` hook may block a call (exit 2), `PostToolUse` observes. Hooks compose
  *below* the permission gate (deny-only). Lifecycle hooks, JSON-stdout control, and serve-path
  wiring are pending.
```

- [ ] **Step 2: Update the `extensions` row in `CLAUDE.md`**

In `CLAUDE.md`, at the end of the `extensions` crate row (after the skills "Slice 3" sentence), append:

```
Slice 4 adds hooks: discovery of `settings.json` `PreToolUse`/`PostToolUse` hooks (concatenated
across `~/.claude` + project, additive — not override-by-name), a `HookExecutor` seam + a
`HookedTool` decorator (fires matched hooks around `ToolRegistry` dispatch; a `PreToolUse` hook
blocks on exit 2, `PostToolUse` observes), executed by the engine's `SandboxedHookExecutor` through
a new stdin-capable `run_sandboxed_with_stdin` (`SandboxPolicy::Os`). Wired into `otto run` by
wrapping every registered tool (`ToolRegistry::wrap_each`) when hooks exist and a sandbox backend is
present — fail-closed otherwise. Hooks compose *below* the gate (deny-only; never widen). Lifecycle
hooks, JSON-stdout control, regex matchers, `settings.local.json`, and serve-path wiring are
deferred.
```

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md CLAUDE.md
git commit -m "docs(extensions): record shipped hooks slice"
```

---

## Definition of Done

- [ ] `cargo test --workspace` passes (offline determinism suite unchanged).
- [ ] `cargo clippy --workspace --all-targets` is clean.
- [ ] `cargo fmt --all` produces no diff.
- [ ] A project `.claude/settings.json` with a `PreToolUse` hook blocks the matched tool during an `otto run` turn (when a sandbox backend exists); with no `settings.json`, the tool set is byte-for-byte unchanged.
- [ ] Spec requirements covered: parse (Task 1), matching + executor seam (Task 2), decorator with block/observe semantics (Task 3), concatenated discovery (Task 4), sandboxed stdin execution (Tasks 5, 7), generic registry wrap (Task 6), fail-closed `otto run` wiring (Task 8), docs (Task 9).
```
