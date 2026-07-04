# Serve-Path Skills + Plugin MCP Servers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire skills (the gated `skill` tool) and plugin-bundled MCP servers into `otto serve`, closing the gap where `build_serve_tools` only composes the permission/approval gate with hooks, silently dropping any discovered `.claude/skills/` or enabled-plugin `.mcp.json` servers that `otto run` already wires up.

**Architecture:** `crates/engine/src/main.rs`'s `cmd_run` (the `otto run` spine) composes its tool registry in a specific, deliberate order: `build_tools_preferring_mcp` (permission/approval gate) → `register_skills` → `register_hooks` → a plugin-MCP connect loop that runs *after* hook-wrapping (so plugin tools are gate-guarded but not hook-wrapped this slice — an existing, documented tradeoff). `build_serve_tools` (`otto serve`'s equivalent composer, added in the prior hooks slice) currently stops after `register_hooks`. This plan extends `build_serve_tools` to perform the exact same two remaining steps in the exact same order, so `otto serve` reaches full composition parity with `otto run`. No changes to `cmd_serve` itself are needed — it already calls `build_serve_tools` and will automatically get the additional tools once the function does more.

**Tech Stack:** Rust, tokio, existing `otto-engine`/`otto-extensions` crates, `escargot` (already a dev-dependency of `otto-engine`, used to build the real `mcp-fs` binary as a stand-in "plugin" MCP server for a hermetic, real-process integration test). No new dependencies.

---

### Task 1: Wire skills into `build_serve_tools`

**Files:**
- Modify: `crates/engine/src/main.rs:232-247` (the `build_serve_tools` fn)
- Test: `crates/engine/src/main.rs` (`#[cfg(test)] mod tests`, appended after `build_serve_tools_matches_direct_call_when_nothing_is_configured`, which currently ends at line 1327, right before the module's closing `}` at line 1328)

- [ ] **Step 1: Write the failing test**

Add this test at the end of the `mod tests` block in `crates/engine/src/main.rs`, immediately after `build_serve_tools_matches_direct_call_when_nothing_is_configured` (before the module's final closing `}`):

```rust
    #[tokio::test]
    async fn build_serve_tools_registers_skill_tool_when_present() {
        use otto_workspace::LocalWorkspace;

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let skill_dir = proj.path().join(".claude").join("skills").join("greeter");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greeter\ndescription: greets\n---\nSay hi.\n",
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.skills.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_serve_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        assert!(
            tools.tool_names().iter().any(|n| n == "skill"),
            "expected the `skill` tool to be registered, got: {:?}",
            tools.tool_names()
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine --bin otto build_serve_tools_registers_skill_tool_when_present`
Expected: FAIL — the assertion `tools.tool_names().iter().any(|n| n == "skill")` is false, because `build_serve_tools` does not yet call `register_skills`.

- [ ] **Step 3: Implement — call `register_skills` in `build_serve_tools`**

In `crates/engine/src/main.rs`, replace the current `build_serve_tools` (lines 232-247):

```rust
async fn build_serve_tools(
    ext: &otto_extensions::Extensions,
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
    let (mut tools, conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &ext.permissions,
    )
    .await;
    register_hooks(&mut tools, &ext.hooks, &root);
    (tools, conns)
}
```

with:

```rust
async fn build_serve_tools(
    ext: &otto_extensions::Extensions,
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
    let (mut tools, conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &ext.permissions,
    )
    .await;
    register_skills(&mut tools, &ext.skills);
    register_hooks(&mut tools, &ext.hooks, &root);
    (tools, conns)
}
```

(This mirrors `cmd_run`'s order exactly: skills are registered — and thus become hook-wrappable — before `register_hooks` runs.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-engine --bin otto build_serve_tools_registers_skill_tool_when_present`
Expected: PASS.

- [ ] **Step 5: Run the existing `build_serve_tools` tests to confirm no regression**

Run: `cargo test -p otto-engine --bin otto build_serve_tools`
Expected: all 4 tests pass (`build_serve_tools_wraps_hooks_around_permission_and_approval_gate`, `build_serve_tools_enforces_hooks_on_the_plain_gate_branch`, `build_serve_tools_matches_direct_call_when_nothing_is_configured`, `build_serve_tools_registers_skill_tool_when_present`).

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): register discovered skills on the otto serve path"
```

---

### Task 2: Wire plugin-bundled MCP servers into `build_serve_tools`

**Files:**
- Modify: `crates/engine/src/main.rs` (the `build_serve_tools` fn, as left by Task 1)
- Test: `crates/engine/src/main.rs` (`#[cfg(test)] mod tests`, appended after the test added in Task 1)

- [ ] **Step 1: Write the failing tests**

Add these two tests at the end of the `mod tests` block, after `build_serve_tools_registers_skill_tool_when_present`:

```rust
    #[tokio::test]
    async fn build_serve_tools_connects_and_registers_a_plugin_mcp_server() {
        use otto_extensions::{Extensions, PluginMcpServer};
        use otto_workspace::LocalWorkspace;

        // Use the real, already-built mcp-fs binary as a stand-in "plugin" MCP server — a real
        // stdio server, so this proves the actual connect-and-register path, not a mock.
        let bin = escargot::CargoBuild::new()
            .package("otto-mcp-fs")
            .bin("mcp-fs")
            .run()
            .expect("build mcp-fs")
            .path()
            .to_path_buf();

        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        let mut ext = Extensions::default();
        ext.mcp_servers.push(PluginMcpServer {
            namespace: "testplugin".to_string(),
            server_key: "fs".to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![proj.path().to_string_lossy().into_owned()],
            env: Default::default(),
            cwd: None,
        });

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, conns) =
            super::build_serve_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        assert!(
            tools
                .tool_names()
                .iter()
                .any(|n| n == "plugin__testplugin__fs__fs.read"),
            "expected the namespaced plugin tool to be registered, got: {:?}",
            tools.tool_names()
        );
        // The connection must be retained in the returned Vec — otherwise the caller would drop
        // it and kill the child process the instant build_serve_tools returns.
        assert!(!conns.is_empty());
    }

    #[tokio::test]
    async fn build_serve_tools_skips_an_unreachable_plugin_mcp_server() {
        use otto_extensions::{Extensions, PluginMcpServer};
        use otto_workspace::LocalWorkspace;

        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        let mut ext = Extensions::default();
        ext.mcp_servers.push(PluginMcpServer {
            namespace: "testplugin".to_string(),
            server_key: "bogus".to_string(),
            command: "definitely-not-a-real-binary-xyz".to_string(),
            args: vec![],
            env: Default::default(),
            cwd: None,
        });

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, conns) =
            super::build_serve_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        // An unreachable plugin server is logged and skipped, never fatal — matches cmd_run.
        assert!(
            !tools.tool_names().iter().any(|n| n.starts_with("plugin__")),
            "expected no plugin tools to be registered, got: {:?}",
            tools.tool_names()
        );
        assert!(conns.is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine --bin otto build_serve_tools_connects_and_registers_a_plugin_mcp_server build_serve_tools_skips_an_unreachable_plugin_mcp_server`
Expected: `build_serve_tools_connects_and_registers_a_plugin_mcp_server` FAILs (the assertion that `plugin__testplugin__fs__fs.read` is registered is false — `build_serve_tools` doesn't connect plugin servers yet). `build_serve_tools_skips_an_unreachable_plugin_mcp_server` currently PASSes vacuously (no plugin loop runs at all, so no plugin tools and no conns either way) — that's fine; Step 4 re-confirms it still passes once the loop exists.

- [ ] **Step 3: Implement — add the plugin-MCP connect loop after `register_hooks`**

In `crates/engine/src/main.rs`, replace `build_serve_tools` (as left by Task 1) with:

```rust
async fn build_serve_tools(
    ext: &otto_extensions::Extensions,
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
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
}
```

Note `conns` changes from `let (mut tools, conns)` to `let (mut tools, mut conns)` since the loop now pushes onto it. `mcp_connect_plugin_server` is already imported at the top of `main.rs` (used by `cmd_run`), so no new `use` is needed.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine --bin otto build_serve_tools`
Expected: all 6 tests pass (the 4 from before Task 2 plus the 2 new plugin tests).

- [ ] **Step 5: Run the full existing `otto run` plugin-MCP tests to confirm nothing else regressed**

Run: `cargo test -p otto-engine --lib`
Run: `cargo test -p otto-engine --bin otto`
Expected: no failures — this change only adds behavior to `build_serve_tools`; `cmd_run`'s own composition is untouched.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): connect plugin-bundled MCP servers on the otto serve path"
```

---

### Task 3: Record the change in CLAUDE.md

**Files:**
- Modify: `CLAUDE.md` (the `extensions` row in the crate table, line 149)

- [ ] **Step 1: Append a "Slice 11" sentence and trim the stale deferred-scope wording**

In `CLAUDE.md`, find this sentence at the end of the `extensions` row (it currently ends the row, right before the closing ` |`):

```
Slice 10 enforces **hooks on `otto serve`**: `cmd_serve` now builds its tool registry through a new `build_serve_tools` helper — the same `build_tools_preferring_mcp` (permission/approval gate) composed with the existing `register_hooks` wrap that `cmd_run` already performs inline — replacing the old "hooks not enforced on serve" warning with actual enforcement (a configured `PreToolUse`/`PostToolUse` hook now fires on served tool calls, same fail-open-without-sandbox warning as `otto run`). Skills, plugin MCP servers, and the `--agent`/`--command` subpaths remain the other deferred serve-path threads. |
```

Replace it with:

```
Slice 10 enforces **hooks on `otto serve`**: `cmd_serve` now builds its tool registry through a new `build_serve_tools` helper — the same `build_tools_preferring_mcp` (permission/approval gate) composed with the existing `register_hooks` wrap that `cmd_run` already performs inline — replacing the old "hooks not enforced on serve" warning with actual enforcement (a configured `PreToolUse`/`PostToolUse` hook now fires on served tool calls, same fail-open-without-sandbox warning as `otto run`). Slice 11 completes serve-path composition parity with `otto run`: `build_serve_tools` now also calls `register_skills` (so a served session's Coder/agents can reach the gated `skill` tool) and runs the same plugin-MCP connect loop `cmd_run` uses — spawning each enabled plugin's bundled MCP servers and registering their namespaced (`plugin__{ns}__{key}__{tool}`) tools — in the identical order `cmd_run` uses (skills → hooks → plugin MCP servers, so plugin tools are gate-guarded but not hook-wrapped, same tradeoff as `otto run`). An unreachable plugin server is logged and skipped, never fatal. The `--agent`/`--command` subpaths remain the only deferred serve-path thread. |
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: record extensions slice 11 (skills + plugin MCP servers enforced on otto serve)"
```

---

### Task 4: Full regression check

**Files:** none (verification only).

- [ ] **Step 1: Build the whole workspace**

Run: `cargo build --workspace`
Expected: clean build, no errors or new warnings.

- [ ] **Step 2: Run the full offline test suite**

Run: `cargo test --workspace`
Expected: all tests pass (fully offline/deterministic except for the two new plugin tests, which spawn the real, already-built `mcp-fs` binary over stdio loopback — no network involved, matching the existing `crates/engine/tests/mcp_fs.rs` precedent).

- [ ] **Step 3: Format check**

Run: `cargo fmt --all -- --check`
Expected: no diff.

- [ ] **Step 4: Lint**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings introduced by this change.

- [ ] **Step 5: No commit this task** (verification only — nothing changed).

---

## Plan coverage check

- Register discovered skills on the serve path → Task 1.
- Connect and register enabled plugins' bundled MCP servers on the serve path, in `cmd_run`'s exact order and with its exact fail-open-and-skip error handling → Task 2.
- Composition order matches `cmd_run` exactly (skills → hooks → plugin MCP loop, so plugin tools are gate-guarded but not hook-wrapped) → Task 1 Step 3, Task 2 Step 3.
- Regression safety: all 4 pre-existing `build_serve_tools` tests (permissions+approval+hooks, plain-gate+hooks, nothing-configured, and now skills) re-verified passing after each task → Task 1 Step 5, Task 2 Step 4-5.
- Docs reflect the shipped state, including trimming the now-stale "skills, plugin MCP servers... deferred" wording → Task 3.
- No regression to `cargo build --workspace` / the offline determinism suite → Task 4.
- Explicitly out of scope (per user's chosen thread, matching the prior serve-hooks plan's convention): `--agent`/`--command` subpaths on serve, and hook-wrapping of plugin MCP tools (an existing, documented `cmd_run` tradeoff this plan intentionally mirrors rather than changes) — left deferred, noted in Task 3's doc update.
