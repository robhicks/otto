# Plugin MCP tool enforcement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `PreToolUse`/`PostToolUse` hooks fire on plugin-bundled MCP tools, and let `settings.json` `permissions` rules and hook `matcher`s address those tools with the Claude Code `mcp__<plugin>[__<tool>]` idiom.

**Architecture:** A single pure helper (`mcp_specifier_matches`) in the `extensions` crate bridges the Claude-Code `mcp__…` addressing form to otto's internal `plugin__<plugin>__<serverkey>__<tool>` gate name (server key always wildcarded). Two matchers consume it identically — the permission gate (`permission_def::Rule::matches`) and hook matchers (`hook_exec::matcher_selects`). Separately, `build_composed_tools` (`engine`) is reordered so plugin MCP tools register *before* `register_hooks`, so hook-wrapping covers them.

**Tech Stack:** Rust (edition 2024, toolchain 1.85), `serde_json`. Tests are `#[cfg(test)] mod tests` next to code; the engine hook test uses `escargot` to build `mcp-fs` as an in-process plugin fixture (no network).

**Design doc:** `docs/superpowers/specs/2026-07-10-plugin-mcp-enforcement-design.md`

**Branch:** work continues on `plugin-mcp-enforcement` (already created; the spec is committed there).

## Global Constraints

- Determinism invariant: no new env reads, no network. With no `settings.json` and no plugins, tool composition must stay byte-for-byte identical.
- The internal gate name `plugin__<plugin>__<serverkey>__<tool>` is NOT renamed — `mcp__…` is purely an addressing alias resolved inside the two matchers.
- Sensitive-path floor stays inviolable: `PolicyGate` consults the base gate first and a base `Deny` short-circuits before any rule. This slice does not touch that path.
- A malformed `mcp__…` specifier (empty segment, or a parenthesized specifier) must never widen access — it resolves to "no match" (and, for permission rules, the rule is dropped at parse time).
- No Claude/AI self-attribution in commits (no `Co-Authored-By`, no emoji, no "Generated with" footers).

---

## File Structure

- **`crates/extensions/src/mcp_name.rs`** (create): the `mcp_specifier_matches` bridge + its unit tests. One responsibility: parse/compare `mcp__…` specifiers against `plugin__…` runtime names.
- **`crates/extensions/src/lib.rs`** (modify): declare `mod mcp_name;`.
- **`crates/extensions/src/permission_def.rs`** (modify): `Rule::matches` gains an `mcp__` branch; `build_specifier` drops a parenthesized `mcp__…` rule; new tests.
- **`crates/extensions/src/hook_exec.rs`** (modify): `matcher_selects` gains an `mcp__` token branch; new tests.
- **`crates/engine/src/main.rs`** (modify): reorder the plugin-MCP loop before `register_hooks` in `build_composed_tools`; update the doc/inline comments; flip one existing test; add one new test.
- **`docs/ARCHITECTURE.md`**, **`CLAUDE.md`** (modify): drop the deferral wording; describe the shipped behavior.

---

## Task 1: The `mcp_specifier_matches` bridge

**Files:**
- Create: `crates/extensions/src/mcp_name.rs`
- Modify: `crates/extensions/src/lib.rs:8` (module list — add `mod mcp_name;`)
- Test: `crates/extensions/src/mcp_name.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub fn mcp_specifier_matches(specifier: &str, tool_name: &str) -> bool` (in module `mcp_name`, called crate-internally as `crate::mcp_name::mcp_specifier_matches`).

- [ ] **Step 1: Write the failing tests**

Create `crates/extensions/src/mcp_name.rs` with ONLY the tests first (the `pub fn` comes in Step 3). Paste this whole file:

```rust
//! Claude-Code `mcp__` addressing for plugin-bundled MCP tools. otto registers each plugin MCP
//! tool under the internal gate name `plugin__<plugin>__<serverkey>__<tool>`; operators address
//! them with the Claude Code idiom `mcp__<plugin>` (whole plugin) or `mcp__<plugin>__<tool>` (that
//! tool across any of the plugin's servers — the server key is always wildcarded). This bridge is
//! consumed identically by the permission gate (`permission_def`) and hook matchers (`hook_exec`).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_level_matches_any_tool_of_that_plugin() {
        assert!(mcp_specifier_matches("mcp__acme", "plugin__acme__srv__search"));
        assert!(mcp_specifier_matches("mcp__acme", "plugin__acme__other__list"));
    }

    #[test]
    fn tool_level_matches_that_tool_across_servers() {
        assert!(mcp_specifier_matches("mcp__acme__search", "plugin__acme__s1__search"));
        assert!(mcp_specifier_matches("mcp__acme__search", "plugin__acme__s2__search"));
        assert!(!mcp_specifier_matches("mcp__acme__search", "plugin__acme__s1__list"));
    }

    #[test]
    fn wrong_plugin_does_not_match() {
        assert!(!mcp_specifier_matches("mcp__acme", "plugin__other__srv__search"));
    }

    #[test]
    fn dotted_tool_tail_is_verbatim() {
        assert!(mcp_specifier_matches(
            "mcp__acme__fs.read",
            "plugin__acme__srv__fs.read"
        ));
    }

    #[test]
    fn non_mcp_specifier_never_matches() {
        assert!(!mcp_specifier_matches("bash", "plugin__acme__srv__search"));
        assert!(!mcp_specifier_matches("fs.read", "plugin__acme__srv__search"));
    }

    #[test]
    fn non_plugin_runtime_name_never_matches() {
        assert!(!mcp_specifier_matches("mcp__acme", "bash"));
        assert!(!mcp_specifier_matches("mcp__acme", "fs.read"));
    }

    #[test]
    fn malformed_specifier_never_widens() {
        assert!(!mcp_specifier_matches("mcp__", "plugin__acme__srv__search"));
        assert!(!mcp_specifier_matches("mcp__acme__", "plugin__acme__srv__search"));
    }

    #[test]
    fn malformed_runtime_name_never_matches() {
        // too few segments after the prefix
        assert!(!mcp_specifier_matches("mcp__acme", "plugin__acme__srv"));
        assert!(!mcp_specifier_matches("mcp__acme", "plugin__acme"));
    }
}
```

- [ ] **Step 2: Add the module and run the tests to verify they fail**

Add to `crates/extensions/src/lib.rs` in the module block (alphabetical, after `mod marketplace_install;` at line 16):

```rust
mod mcp_name;
```

Run: `cargo test -p otto-extensions mcp_name`
Expected: FAIL to compile — `cannot find function 'mcp_specifier_matches' in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Insert this ABOVE the `#[cfg(test)] mod tests` block in `crates/extensions/src/mcp_name.rs`:

```rust
/// True if a settings-side specifier addresses the given runtime tool name. Fires ONLY when
/// `specifier` is an `mcp__…` form AND `tool_name` is a `plugin__…` form; returns `false`
/// otherwise so ordinary exact-match handles everything else.
pub fn mcp_specifier_matches(specifier: &str, tool_name: &str) -> bool {
    let Some((plugin, tool)) = parse_plugin_tool(tool_name) else {
        return false;
    };
    let Some((spec_plugin, spec_tool)) = parse_mcp_specifier(specifier) else {
        return false;
    };
    spec_plugin == plugin && spec_tool.is_none_or(|t| t == tool)
}

/// `plugin__<plugin>__<serverkey>__<tool>` → `(plugin, tool)`. The `<tool>` tail is verbatim (it may
/// contain `.`/`_`, e.g. `fs.read`); the server key is discarded (always wildcarded in the `mcp__`
/// form). Wrong prefix, fewer than three segments, or an empty segment → `None`.
fn parse_plugin_tool(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("plugin__")?;
    let mut parts = rest.splitn(3, "__");
    let plugin = parts.next().filter(|s| !s.is_empty())?;
    let _serverkey = parts.next().filter(|s| !s.is_empty())?;
    let tool = parts.next().filter(|s| !s.is_empty())?;
    Some((plugin, tool))
}

/// `mcp__<plugin>` → `(plugin, None)`; `mcp__<plugin>__<tool>` → `(plugin, Some(tool))`. The tool
/// tail is verbatim. An empty plugin or tool segment → `None` (a malformed specifier never widens).
fn parse_mcp_specifier(spec: &str) -> Option<(&str, Option<&str>)> {
    let rest = spec.strip_prefix("mcp__")?;
    let (plugin, tool) = match rest.split_once("__") {
        Some((p, t)) => (p, Some(t)),
        None => (rest, None),
    };
    if plugin.is_empty() || tool == Some("") {
        return None;
    }
    Some((plugin, tool))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions mcp_name`
Expected: PASS (all 8 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/mcp_name.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): mcp__ addressing bridge for plugin MCP tool names"
```

---

## Task 2: Permission rules honor `mcp__` specifiers

**Files:**
- Modify: `crates/extensions/src/permission_def.rs` (`Rule::matches` ~line 150; `build_specifier` ~line 78)
- Test: `crates/extensions/src/permission_def.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::mcp_name::mcp_specifier_matches` (Task 1); `PermissionRules::decision(&self, tool: &str, args: &Value) -> Option<Decision>` (existing, `pub`).

- [ ] **Step 1: Write the failing tests**

Add these to the existing `#[cfg(test)] mod tests` block at the bottom of `crates/extensions/src/permission_def.rs`. (The module already has `use super::*;` and uses `serde_json::json` / `Decision` in its existing tests — match whatever those tests import; add `use serde_json::json;` and `use otto_engine_core::tool::Decision;` at the top of the test module only if not already present.)

```rust
    #[test]
    fn mcp_deny_plugin_level_matches_any_tool_of_that_plugin() {
        let rules = parse_permissions(r#"{"permissions":{"deny":["mcp__acme"]}}"#);
        assert_eq!(
            rules.decision("plugin__acme__srv__search", &json!({})),
            Some(Decision::Deny)
        );
        assert_eq!(
            rules.decision("plugin__other__srv__search", &json!({})),
            None
        );
    }

    #[test]
    fn mcp_allow_tool_level_matches_that_tool_across_servers() {
        let rules = parse_permissions(r#"{"permissions":{"allow":["mcp__acme__search"]}}"#);
        assert_eq!(
            rules.decision("plugin__acme__s1__search", &json!({})),
            Some(Decision::Allow)
        );
        assert_eq!(
            rules.decision("plugin__acme__s2__search", &json!({})),
            Some(Decision::Allow)
        );
        assert_eq!(rules.decision("plugin__acme__s1__list", &json!({})), None);
    }

    #[test]
    fn mcp_rule_with_a_specifier_is_dropped() {
        // A parenthesized specifier is meaningless for an MCP tool → the whole rule is dropped,
        // so it neither denies nor widens.
        let rules = parse_permissions(r#"{"permissions":{"deny":["mcp__acme(foo)"]}}"#);
        assert_eq!(rules.decision("plugin__acme__srv__search", &json!({})), None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions permission_def::tests::mcp_`
Expected: FAIL — `mcp_deny_plugin_level…` and `mcp_allow_tool_level…` assert `Some(...)` but get `None` (exact `self.tool != tool` rejects the `plugin__…` name); `mcp_rule_with_a_specifier_is_dropped` gets `Some(Deny)` because the rule currently survives with a `PathGlob` specifier.

- [ ] **Step 3: Add the `mcp__` branch to `Rule::matches`**

In `crates/extensions/src/permission_def.rs`, `Rule::matches` currently starts:

```rust
    fn matches(&self, tool: &str, args: &Value) -> bool {
        if self.tool != tool {
            return false;
        }
```

Insert the `mcp__` short-circuit as the first lines of the function body, above the existing `if self.tool != tool`:

```rust
    fn matches(&self, tool: &str, args: &Value) -> bool {
        // An `mcp__…` rule addresses a plugin-bundled MCP tool (`plugin__…` runtime name) via the
        // shared bridge; it carries no path/command specifier (guaranteed by `build_specifier`).
        if self.tool.starts_with("mcp__") {
            return crate::mcp_name::mcp_specifier_matches(&self.tool, tool);
        }
        if self.tool != tool {
            return false;
        }
```

- [ ] **Step 4: Drop a parenthesized `mcp__` rule at parse time**

In the same file, `build_specifier` currently starts:

```rust
fn build_specifier(tool: &str, inner: &str) -> Option<Specifier> {
    if tool == "bash" {
```

Add an `mcp__` guard as the first statement so a parenthesized `mcp__…` rule returns `None` (which `parse_rule`'s `build_specifier(&tool, inner)?` propagates, dropping the whole rule):

```rust
fn build_specifier(tool: &str, inner: &str) -> Option<Specifier> {
    // MCP tools are not path/command-vetted; a specifier on an `mcp__…` rule is malformed → drop.
    if tool.starts_with("mcp__") {
        return None;
    }
    if tool == "bash" {
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions permission_def`
Expected: PASS (the three new tests plus all pre-existing permission tests still green).

- [ ] **Step 6: Commit**

```bash
git add crates/extensions/src/permission_def.rs
git commit -m "feat(extensions): permission rules match plugin MCP tools via mcp__ specifiers"
```

---

## Task 3: Hook matchers honor `mcp__` tokens

**Files:**
- Modify: `crates/extensions/src/hook_exec.rs` (`matcher_selects`, line 44)
- Test: `crates/extensions/src/hook_exec.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::mcp_name::mcp_specifier_matches` (Task 1).

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` block in `crates/extensions/src/hook_exec.rs`:

```rust
    #[test]
    fn mcp_matcher_selects_plugin_tool() {
        assert!(matcher_selects(
            &Some("mcp__acme".to_string()),
            "plugin__acme__srv__search"
        ));
        assert!(matcher_selects(
            &Some("mcp__acme__search".to_string()),
            "plugin__acme__s2__search"
        ));
        assert!(!matcher_selects(
            &Some("mcp__acme".to_string()),
            "plugin__other__srv__search"
        ));
        assert!(!matcher_selects(&Some("mcp__acme".to_string()), "bash"));
    }

    #[test]
    fn mcp_matcher_in_alternation() {
        assert!(matcher_selects(
            &Some("bash|mcp__acme".to_string()),
            "plugin__acme__srv__x"
        ));
        assert!(matcher_selects(&Some("bash|mcp__acme".to_string()), "bash"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions hook_exec::tests::mcp_`
Expected: FAIL — the current exact-equality matcher compares `"mcp__acme" == "plugin__acme__srv__search"` → false.

- [ ] **Step 3: Add the `mcp__` token branch**

Replace the `Some(pat) => …` arm of `matcher_selects` in `crates/extensions/src/hook_exec.rs`. Current:

```rust
pub fn matcher_selects(matcher: &Option<String>, tool_name: &str) -> bool {
    match matcher.as_deref() {
        None | Some("") | Some("*") => true,
        Some(pat) => pat.split('|').any(|t| t.trim() == tool_name),
    }
}
```

New:

```rust
pub fn matcher_selects(matcher: &Option<String>, tool_name: &str) -> bool {
    match matcher.as_deref() {
        None | Some("") | Some("*") => true,
        Some(pat) => pat.split('|').any(|t| {
            let t = t.trim();
            // An `mcp__…` token addresses a plugin-bundled MCP tool via the shared bridge; every
            // other token is an exact tool-name match (regex matchers are a later slice).
            if t.starts_with("mcp__") {
                crate::mcp_name::mcp_specifier_matches(t, tool_name)
            } else {
                t == tool_name
            }
        }),
    }
}
```

Also update the doc comment just above (`/// … otherwise the matcher is split on `|` and each token compared for exact equality (trimmed).`) to note the `mcp__` case:

```rust
/// Does `matcher` select `tool_name`? `None`/`""`/`"*"` match everything; otherwise the matcher is
/// split on `|` and each trimmed token compared: an `mcp__…` token resolves against a plugin MCP
/// tool name via `mcp_name::mcp_specifier_matches`, every other token by exact equality.
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions hook_exec`
Expected: PASS (new tests plus the pre-existing `matcher_*` tests).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/hook_exec.rs
git commit -m "feat(extensions): hook matchers select plugin MCP tools via mcp__ tokens"
```

---

## Task 4: Hook-wrap plugin MCP tools (engine reorder + tests)

**Files:**
- Modify: `crates/engine/src/main.rs` (`build_composed_tools`, lines ~249-287; the test at line ~1592)
- Test: `crates/engine/src/main.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: the `mcp__` hook-matcher behavior from Task 3 (a `"matcher": "mcp__…"` hook now selects a `plugin__…` tool).

- [ ] **Step 1: Flip the existing ordering test and add the mcp-matcher test**

In `crates/engine/src/main.rs`, the test currently named
`build_composed_tools_plugin_tools_are_gate_guarded_but_not_hook_wrapped` (line ~1592) asserts the
plugin tool is NOT blocked. Rename it and flip the final assertion so the plugin tool IS blocked by
the `"*"` `PreToolUse` hook. Replace the test's name line and its final block (from the
`// The plugin tool was registered after the hook wrap…` comment through the closing `);`) with:

Rename line 1592:

```rust
    async fn build_composed_tools_hook_wraps_plugin_mcp_tools() {
```

Replace the final assertion block (currently lines ~1652-1664):

```rust
        // The plugin tool now registers BEFORE the hook wrap — the same "*" hook must block it too.
        let plugin_blocked = tools
            .call(
                "plugin__testplugin__fs__fs.read",
                serde_json::json!({ "path": "target.txt" }),
            )
            .await
            .unwrap_err();
        assert!(
            plugin_blocked
                .to_string()
                .contains("blocked by PreToolUse hook"),
            "expected the wrapped plugin tool to be blocked, got: {plugin_blocked}"
        );
    }
```

Also update the mid-test comment block (lines ~1624-1626) that explains the old expectation:

```rust
        // A "*" PreToolUse hook blocks every tool in the registry when register_hooks wraps it.
        // Both fs.read and the plugin tool register before the wrap now, so both must be blocked.
```

Then add a NEW test immediately after it, proving a plugin-specific `mcp__` matcher both fires on the
plugin tool (Task 4 wrapping) and resolves specifically (Task 3 matching):

```rust
    #[tokio::test]
    async fn build_composed_tools_mcp_matcher_hook_fires_on_plugin_tool_only() {
        use otto_extensions::PluginMcpServer;
        use otto_workspace::LocalWorkspace;

        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping mcp-matcher hook test: no OS sandbox backend, hooks would be skipped");
            return;
        }

        let bin = escargot::CargoBuild::new()
            .package("otto-mcp-fs")
            .bin("mcp-fs")
            .run()
            .expect("build mcp-fs")
            .path()
            .to_path_buf();

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        // Matcher targets ONLY this plugin's MCP tools — fs.read must be untouched.
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks": { "PreToolUse": [
                {"matcher": "mcp__testplugin", "hooks": [{"type": "command", "command": "exit 2"}]}
            ] }}"#,
        )
        .unwrap();

        let mut ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.hooks.is_empty());
        ext.mcp_servers.push(PluginMcpServer {
            namespace: "testplugin".to_string(),
            server_key: "fs".to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![proj.path().to_string_lossy().into_owned()],
            env: Default::default(),
            cwd: None,
        });

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        // fs.read is NOT selected by the mcp__testplugin matcher → it runs.
        let ok = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap();
        assert!(ok.to_string().contains("hi"), "fs.read should not be blocked, got: {ok}");

        // The plugin tool IS selected → blocked.
        let blocked = tools
            .call(
                "plugin__testplugin__fs__fs.read",
                serde_json::json!({ "path": "target.txt" }),
            )
            .await
            .unwrap_err();
        assert!(
            blocked.to_string().contains("blocked by PreToolUse hook"),
            "expected the plugin tool to be blocked by the mcp__ matcher, got: {blocked}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine build_composed_tools_hook_wraps_plugin_mcp_tools build_composed_tools_mcp_matcher_hook_fires_on_plugin_tool_only`
Expected: FAIL — both plugin-tool calls currently succeed (plugin tools register AFTER `register_hooks`, so they are unwrapped) instead of being blocked. (If the machine has no OS sandbox backend both tests early-return and "pass" trivially — run on a Linux box with `bwrap` to get real coverage.)

- [ ] **Step 3: Reorder the plugin-MCP loop before `register_hooks`**

In `crates/engine/src/main.rs`, `build_composed_tools` currently reads (lines ~260-286):

```rust
    let (mut tools, mut conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &ext.permissions,
    )
    .await;
    register_skills(&mut tools, &ext.skills);
    register_hooks(&mut tools, &ext.hooks, &root);
    // Bundled plugin MCP servers register AFTER register_hooks, mirroring cmd_run exactly: plugin
    // tools are gate-guarded but not hook-wrapped this slice (see cmd_run's identical loop). A
    // server that won't spawn is logged and skipped — additive, never fatal.
    for spec in &ext.mcp_servers {
        match mcp_connect_plugin_server(spec).await {
            Ok((conn, mcp_tools)) => {
                for t in mcp_tools {
                    tools.register(t);
                }
                conns.push(conn);
            }
            Err(e) => eprintln!(
                "plugin mcp server {}:{} unavailable ({e}); skipping",
                spec.namespace, spec.server_key
            ),
        }
    }
    (tools, conns)
```

Move the `for spec in &ext.mcp_servers { … }` loop to run BEFORE `register_hooks`, and update the comment:

```rust
    let (mut tools, mut conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &ext.permissions,
    )
    .await;
    register_skills(&mut tools, &ext.skills);
    // Bundled plugin MCP servers register BEFORE register_hooks so hook-wrapping covers them too:
    // a `PreToolUse`/`PostToolUse` hook (matched via an `mcp__…` matcher or `*`) fires on plugin
    // tool calls. A server that won't spawn is logged and skipped — additive, never fatal.
    for spec in &ext.mcp_servers {
        match mcp_connect_plugin_server(spec).await {
            Ok((conn, mcp_tools)) => {
                for t in mcp_tools {
                    tools.register(t);
                }
                conns.push(conn);
            }
            Err(e) => eprintln!(
                "plugin mcp server {}:{} unavailable ({e}); skipping",
                spec.namespace, spec.server_key
            ),
        }
    }
    register_hooks(&mut tools, &ext.hooks, &root);
    (tools, conns)
```

Also update the function's doc comment (lines ~249-253) — change the ordering clause from "then hook-wrapping … then bundled plugin MCP servers" to reflect the new order:

```rust
/// The tool-registry composition every entrypoint shares (`otto run`, `otto run --command`,
/// `otto run --agent`, `otto serve`): the permission/approval gate from
/// `build_tools_preferring_mcp`, then skill registration via `register_skills`, then bundled plugin
/// MCP servers via `mcp_connect_plugin_server`, then hook-wrapping over all of them via
/// `register_hooks` (so hooks fire on plugin tools too). `approve_edits` is true only for
/// `otto serve --approve-edits`.
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine build_composed_tools_`
Expected: PASS — both new/flipped tests green, and the other `build_composed_tools_*` tests
(`…_registers_a_plugin_mcp_server`, `…_skips_an_unreachable_plugin_mcp_server`,
`…_matches_direct_call_when_nothing_is_configured`, `…_registers_skill_tool_when_present`,
`…_wraps_hooks_around_permission_and_approval_gate`, `…_enforces_hooks_on_the_plain_gate_branch`)
still green.

- [ ] **Step 5: Full workspace test + fmt/clippy**

Run: `cargo test --workspace && cargo fmt --all && cargo clippy --workspace --all-targets`
Expected: all green; no fmt diff; no clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): hook-wrap plugin MCP tools by registering them before register_hooks"
```

---

## Task 5: Documentation

**Files:**
- Modify: `CLAUDE.md` (the Slice 5 Plan B deferral sentence)
- Modify: `docs/ARCHITECTURE.md` (the plugins bullet ~line 357 and the hooks bullet ~line 339)

**Interfaces:** none (docs only).

- [ ] **Step 1: Update `CLAUDE.md`**

Find the deferral clause in the Slice 5 Plan B description (search for `remain deferred`):

```
An interactive `/plugin` UX and project-level marketplace installs, plus `model`/`allowed-tools` enforcement and hook-wrapping of plugin MCP tools, remain deferred (marketplace install/lockfile and github/git remote plugin-source materialization are shipped)
```

Replace with:

```
Plugin MCP tools are now **fully enforced**: they hook-wrap (a `PreToolUse`/`PostToolUse` hook fires on plugin tool calls, since they register before `register_hooks` on every entrypoint) and are addressable from `settings.json` `permissions` rules and hook `matcher`s via the Claude-Code idiom `mcp__<plugin>` (whole plugin) / `mcp__<plugin>__<tool>` (that tool across any of the plugin's servers — the internal `plugin__<plugin>__<serverkey>__<tool>` name's server key is wildcarded), bridged by `mcp_specifier_matches`. (`model` never applied to MCP servers — it routes a plugin's folded agents/commands, already enforced in Slice 8/14.) An interactive `/plugin` UX and project-level (non-user-global) marketplace installs remain deferred (marketplace install/lockfile and github/git remote plugin-source materialization are shipped)
```

- [ ] **Step 2: Update `docs/ARCHITECTURE.md` plugins bullet**

Find (near line 357):

```
A project-level (non-user-global) marketplace install and an
interactive `/plugin` UX are still pending.
```

Replace with:

```
Plugin MCP tools hook-wrap (they register before `register_hooks`) and are addressable from
`permissions` rules and hook matchers via the `mcp__<plugin>[__<tool>]` idiom (server key
wildcarded), bridged by `mcp_specifier_matches`. A project-level (non-user-global) marketplace
install and an interactive `/plugin` UX are still pending.
```

- [ ] **Step 3: Update `docs/ARCHITECTURE.md` hooks bullet**

Find (near line 339):

```
  Enforced on every entrypoint (spine, serve, and the
  `--command`/`--agent` subpaths, all via the shared `build_composed_tools`). Lifecycle hooks,
  JSON-stdout control, regex matchers, and `settings.local.json` are pending.
```

Replace with:

```
  Enforced on every entrypoint (spine, serve, and the
  `--command`/`--agent` subpaths, all via the shared `build_composed_tools`) — including
  plugin-bundled MCP tools, which register before `register_hooks` and can be selected by an
  `mcp__<plugin>[__<tool>]` matcher. Lifecycle hooks, JSON-stdout control, regex matchers, and
  `settings.local.json` are pending.
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md
git commit -m "docs: plugin MCP tool enforcement (hook-wrapping + mcp__ addressing) is shipped"
```

---

## Self-Review

- **Spec coverage:** Naming model → Task 1 (helper) + Tasks 2/3 (both consumers). #2 permissions → Task 2. #2 hook matchers → Task 3. #1 hook-wrapping → Task 4. `model`-is-N/A + deferral removal → Task 5. Fail-closed on malformed specifier → Task 1 (`malformed_specifier_never_widens`) + Task 2 (`mcp_rule_with_a_specifier_is_dropped`). Determinism unchanged → covered by the pre-existing `…_matches_direct_call_when_nothing_is_configured` test (Task 4 Step 4). All spec sections map to a task.
- **Placeholder scan:** none — every code step shows complete code; every run step shows the command and expected result.
- **Type consistency:** `mcp_specifier_matches(specifier: &str, tool_name: &str) -> bool` is defined once (Task 1) and called with that exact signature in Tasks 2 and 3. `PermissionRules::decision`, `matcher_selects`, `build_composed_tools`, and `PluginMcpServer` field names match the current source. Internal names `plugin__testplugin__fs__fs.read` / `mcp__testplugin` are consistent across the engine tests.
