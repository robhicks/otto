# Serve-Path Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `settings.json` `PreToolUse`/`PostToolUse` hooks into `otto serve`, closing the gap where `cmd_serve` only warns that hooks aren't enforced instead of enforcing them (as `otto run` already does).

**Architecture:** Extract a small `build_serve_tools` helper in `crates/engine/src/main.rs` that composes `build_tools_preferring_mcp` (permissions/approval gate) with the existing `register_hooks` helper (hook-wrapping), mirroring exactly how `cmd_run` already composes the two. `cmd_serve` calls this helper instead of `build_tools_preferring_mcp` directly and drops its now-stale "hooks not enforced" warning. `--agent`/`--command`/skills/plugin-MCP on the serve path remain out of scope — this plan covers hooks only.

**Tech Stack:** Rust, tokio, existing `otto-engine`/`otto-extensions` crates. No new dependencies.

---

### Task 1: Extract `build_serve_tools` and prove it wraps hooks around the permission/approval gate

**Files:**
- Modify: `crates/engine/src/main.rs` (new private fn near `build_tools_preferring_mcp`/`register_hooks`, ~line 226)
- Test: `crates/engine/src/main.rs` (`#[cfg(test)] mod tests`, alongside `serve_path_registry_composes_permissions_with_approval_mode` at ~line 1153)

- [ ] **Step 1: Write the failing test**

Add this test in the `mod tests` block in `crates/engine/src/main.rs`, right after `serve_path_registry_composes_permissions_with_approval_mode` (ends ~line 1199-1201 with the closing `}` of that test):

```rust
    #[tokio::test]
    async fn build_serve_tools_wraps_hooks_around_permission_and_approval_gate() {
        use otto_workspace::LocalWorkspace;

        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping serve hooks composition test: no OS sandbox backend");
            return;
        }
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{
                "permissions": { "deny": ["Write(dist/**)"] },
                "hooks": { "PreToolUse": [
                    {"matcher": "fs.read", "hooks": [{"type": "command", "command": "exit 2"}]}
                ] }
            }"#,
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.permissions.is_empty());
        assert!(!ext.hooks.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_serve_tools(&ext, ws, proj.path().to_path_buf(), true).await;

        // The hook fires even though fs.read is otherwise allowed by the permission/approval gate.
        let err = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("blocked by PreToolUse hook"),
            "got: {err}"
        );
        // The permission-gate deny still wins for an unrelated tool call (composition intact).
        assert_eq!(
            tools.check(
                "fs.write",
                &serde_json::json!({"path": "dist/x.txt"})
            ),
            otto_engine_core::tool::Decision::Deny
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine build_serve_tools_wraps_hooks_around_permission_and_approval_gate`
Expected: FAIL to compile — `error[E0433]: failed to resolve: could not find 'build_serve_tools' in 'main'` (the function doesn't exist yet).

- [ ] **Step 3: Implement `build_serve_tools`**

In `crates/engine/src/main.rs`, add this function immediately after `register_hooks` (which ends at line 264, right before `async fn cmd_run`):

```rust
/// The tool-registry composition `otto serve` uses: the permission/approval gate from
/// `build_tools_preferring_mcp`, then hook-wrapping on top via `register_hooks` — the same two
/// steps `cmd_run` performs inline. Skills and plugin MCP servers are NOT registered here; that
/// remains deferred for the serve path.
async fn build_serve_tools(
    ext: &otto_extensions::Extensions,
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
    let (mut tools, conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), approve_edits, &ext.permissions)
            .await;
    register_hooks(&mut tools, &ext.hooks, &root);
    (tools, conns)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-engine build_serve_tools_wraps_hooks_around_permission_and_approval_gate`
Expected: PASS. (If it prints `skipping serve hooks composition test: no OS sandbox backend` and exits 0, that's also an accepted outcome on a machine without bwrap/sandbox-exec — matches the existing hook tests' convention.)

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): add build_serve_tools composing hooks with the serve permission gate"
```

---

### Task 2: Wire `cmd_serve` to use `build_serve_tools`, dropping the stale warning

**Files:**
- Modify: `crates/engine/src/main.rs:583-605` (inside `cmd_serve`)

- [ ] **Step 1: Replace the warning + tool-building block**

Find this block in `cmd_serve` (currently lines 583-605):

```rust
    let ext = otto_extensions::discover(&root, &home_dir());
    if !ext.hooks.is_empty() {
        eprintln!(
            "warning: settings.json hooks are configured but are NOT enforced on the serve \
             path (hooks are wired only on the `otto run` spine for now)."
        );
    }

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    // NOTE: hooks/skills/plugin MCP servers are wired only in the main `otto run` spine for now;
    // the --agent/--command paths are deferred (extensions hooks slice). Permissions ARE
    // enforced here, composed with --approve-edits when both are configured (see
    // build_tool_registry_inner).
    let (tools, _mcp_conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &ext.permissions,
    )
    .await;
    let tools = Arc::new(tools);
```

Replace it with:

```rust
    let ext = otto_extensions::discover(&root, &home_dir());

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    // NOTE: skills/plugin MCP servers are wired only in the main `otto run` spine for now; the
    // --agent/--command paths are deferred (extensions hooks slice). Permissions and hooks ARE
    // enforced here via `build_serve_tools`, composed with --approve-edits when both are
    // configured (see build_tool_registry_inner / register_hooks).
    let (tools, _mcp_conns) =
        build_serve_tools(&ext, tools_workspace, root.clone(), approve_edits).await;
    let tools = Arc::new(tools);
```

Note `register_hooks` (called inside `build_serve_tools`) already prints its own loud warning when hooks are configured but no OS sandbox backend is available — that's why the old `if !ext.hooks.is_empty() { eprintln!(...) }` block is deleted rather than kept alongside.

- [ ] **Step 2: Build to confirm it compiles**

Run: `cargo build -p otto-engine`
Expected: clean build, no errors or new warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): enforce settings.json hooks on the otto serve path"
```

---

### Task 3: Record the change in CLAUDE.md

**Files:**
- Modify: `CLAUDE.md` (the `extensions` row in the crate table)

- [ ] **Step 1: Append a "Slice 10" sentence to the `extensions` row**

Find the end of the `extensions` row (it currently ends with the sentence added by Slice 9): `` Hooks/skills/plugin MCP servers and the `--agent`/`--command` subpaths remain the other deferred serve-path threads. | ``

Replace that final sentence with:

```
Slice 10 enforces **hooks on `otto serve`**: `cmd_serve` now builds its tool registry through a new `build_serve_tools` helper — the same `build_tools_preferring_mcp` (permission/approval gate) composed with the existing `register_hooks` wrap that `cmd_run` already performs inline — replacing the old "hooks not enforced on serve" warning with actual enforcement (a configured `PreToolUse`/`PostToolUse` hook now fires on served tool calls, same fail-open-without-sandbox warning as `otto run`). Skills, plugin MCP servers, and the `--agent`/`--command` subpaths remain the other deferred serve-path threads. |
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: record extensions slice 10 (hooks enforced on otto serve)"
```

---

### Task 4: Full regression check

**Files:** none (verification only).

- [ ] **Step 1: Build the whole workspace**

Run: `cargo build --workspace`
Expected: clean build, no errors or new warnings.

- [ ] **Step 2: Run the full offline test suite**

Run: `cargo test --workspace`
Expected: all tests pass (fully offline/deterministic — no network or API keys needed).

- [ ] **Step 3: Format check**

Run: `cargo fmt --all -- --check`
Expected: no diff.

- [ ] **Step 4: Lint**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings introduced by this change.

- [ ] **Step 5: No commit this task** (verification only — nothing changed).

---

## Plan coverage check

- Enforce hooks on the serve path instead of only warning → Task 1, Task 2.
- Regression safety: existing `otto run` hook tests and the `serve_path_registry_composes_permissions_with_approval_mode` test untouched → no modifications to those tests.
- New coverage proving hooks compose correctly with the permission/approval gate on the exact path `cmd_serve` uses → Task 1 Step 1.
- Docs reflect the shipped state → Task 3.
- No regression to `cargo build --workspace` / the offline determinism suite → Task 4.
- Explicitly out of scope (per user's chosen thread): skills, plugin MCP servers, and `--agent`/`--command` on the serve path — left deferred, noted in Task 3's doc update.
