# Per-Artifact `allowed-tools` Enforcement (Commands) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a discovered command's `allowed_tools` narrow the tool registry for the duration of an `otto run --command <name>` invocation, using the same `ToolRegistry::subset` convention agents already use.

**Architecture:** A command run (`run_command_in` in `crates/engine/src/main.rs`) builds one gated tool registry and uses it for both `!bash`/`@file` injection resolution and the spine turn. This plan inserts a single narrowing step between building that registry and using it: `Some(list)` → `registry.subset(list)`, `None` → registry as-is. Narrowing shares the underlying gate (so the sensitive-path floor is preserved) and can only remove tools, never widen. Skills are out of scope (no invocation scope to narrow — see spec).

**Tech Stack:** Rust (edition 2024), `tokio`, `tempfile` for tests. The change is confined to the `otto-engine` binary crate (`crates/engine/src/main.rs`). No `engine-core`/protocol/`extensions` source changes; `ToolRegistry::subset`/`tool_names` already exist.

**Spec:** `docs/superpowers/specs/2026-06-29-extensions-allowed-tools-design.md`

---

## Context an implementer needs

- `run_command_in(name, args, root, home)` lives in `crates/engine/src/main.rs` (around lines 422–492). It: discovers extensions, builds the registry via `build_tools_preferring_mcp(...)`, `Arc`-wraps it, runs `expand_args` then `resolve_injections(&expanded, tools.as_ref())`, then `run_goal(..., tools, ...)`. On `!outcome.ok` it calls `std::process::exit(1)`.
- `CustomCommandDef` (in `crates/extensions/src/command_def.rs`) already has `pub allowed_tools: Option<Vec<String>>`, parsed by slice 2. This plan does not change parsing.
- `ToolRegistry::subset(&[String]) -> ToolRegistry` (`crates/engine-core/src/tool.rs:134`) returns a new registry with only the named tools, **sharing the same gate + ask-resolver**; names that don't exist are silently dropped (intersection). `ToolRegistry::tool_names() -> Vec<String>` (`:147`) lists registered tool names.
- `resolve_injections` (`crates/extensions/src/command_expand.rs:50`) calls `tools.call("bash", …)` for `` !`cmd` `` and `tools.call("fs.read", …)` for `@path`. If the named tool is absent from the registry, the call errors and `resolve_injections` returns `Err` (fail-closed) — this happens **before** the spine turn.
- The narrowing convention (must match agents exactly): absent `allowed_tools` (`None`) → all tools; present non-empty `Some(list)` → narrowed; present empty `Some([])` → no tools.
- The fs tools (`fs.read`, `fs.write`, `fs.list`) are always registered by `build_tool_registry`; `bash` only when an OS sandbox backend exists. Tests must not depend on `bash` being present unless they guard on `otto_tools::os_sandbox_available()`.

---

## Task 1: Narrow the command registry by `allowed_tools`

**Files:**
- Modify: `crates/engine/src/main.rs` — add a `narrow_for_command` helper just above `async fn run_command_in` (~line 421); change the registry `Arc`-wrap inside `run_command_in` (currently `let tools = Arc::new(tools);`, ~line 467).
- Test: `crates/engine/src/main.rs` — `#[cfg(test)] mod tests` (existing block at the bottom of the file).

- [ ] **Step 1: Write the failing behavioral test**

Add this test inside the existing `mod tests` block in `crates/engine/src/main.rs`:

```rust
#[tokio::test]
async fn command_allowed_tools_narrows_and_blocks_disallowed_injection() {
    use std::fs;
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap(); // empty → no user-global commands
    fs::write(proj.path().join("target.txt"), "file-body").unwrap();
    let cmds = proj.path().join(".claude").join("commands");
    fs::create_dir_all(&cmds).unwrap();
    // allowed-tools is present and excludes fs.read, so the @target.txt injection
    // must fail closed (fs.read is not in the narrowed registry).
    fs::write(
        cmds.join("peek.md"),
        "---\nallowed-tools: fs.write\n---\nShow @target.txt\n",
    )
    .unwrap();

    let res = run_command_in(
        "peek",
        &[],
        proj.path().to_path_buf(),
        home.path().to_path_buf(),
    )
    .await;
    assert!(
        res.is_err(),
        "expected fail-closed @-injection under a narrowed allowlist, got: {res:?}"
    );
    assert!(
        res.unwrap_err()
            .to_string()
            .contains("file injection `@target.txt`"),
        "expected the fs.read injection to be the failure cause"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine --bin otto command_allowed_tools_narrows -- --nocapture`
Expected: FAIL — `allowed_tools` is currently inert, so the full registry still contains `fs.read`; the `@target.txt` injection resolves and `run_command_in` returns `Ok(())`, so `assert!(res.is_err())` fails.

- [ ] **Step 3: Add the `narrow_for_command` helper**

Insert this function immediately above `async fn run_command_in` in `crates/engine/src/main.rs`:

```rust
/// Apply a command's `allowed_tools` to its tool registry, matching the agent narrowing
/// convention: an absent allowlist (`None`) keeps every tool; a present allowlist narrows to
/// exactly that intersection (a present-but-empty list yields no tools). `subset` shares the
/// underlying gate, so the inviolable sensitive-path floor is preserved within the narrowed set,
/// and narrowing can only remove tools — never widen access.
fn narrow_for_command(
    tools: otto_engine_core::tool::ToolRegistry,
    allowed: &Option<Vec<String>>,
) -> otto_engine_core::tool::ToolRegistry {
    match allowed {
        Some(list) => tools.subset(list),
        None => tools,
    }
}
```

- [ ] **Step 4: Wire the helper into `run_command_in`**

In `run_command_in`, replace the registry `Arc`-wrap (the line `let tools = Arc::new(tools);` that follows the `build_tools_preferring_mcp(...)` call and the `// _mcp_conns is held …` comment) with:

```rust
    // _mcp_conns is held until end of function so the mcp children stay alive.
    // Narrow the registry to the command's allowed-tools (None = all tools) before it is used for
    // BOTH injection resolution and the spine turn — so a disallowed tool is fail-closed.
    let tools = Arc::new(narrow_for_command(tools, &def.allowed_tools));
```

(Leave the subsequent `expand_args` / `resolve_injections(&expanded, tools.as_ref())` / `run_goal(..., tools, ...)` lines unchanged — they already use this `tools`.)

- [ ] **Step 5: Run the behavioral test to verify it passes**

Run: `cargo test -p otto-engine --bin otto command_allowed_tools_narrows -- --nocapture`
Expected: PASS — the narrowed registry (`subset(["fs.write"])`) has no `fs.read`, so `resolve_injections` errors on `@target.txt` and `run_command_in` returns that `Err` before reaching the spine turn.

- [ ] **Step 6: Add the helper unit test (narrowing convention)**

Add this test inside the same `mod tests` block. It exercises the exact narrowing decision without running the spine:

```rust
#[test]
fn narrow_for_command_applies_the_allowlist_convention() {
    use otto_workspace::LocalWorkspace;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let build = || {
        otto_engine::build_tool_registry(
            Arc::new(LocalWorkspace::new(root.clone())),
            root.clone(),
        )
    };

    // None → all tools unchanged (fs.write is always registered).
    let all: std::collections::BTreeSet<String> =
        build().tool_names().into_iter().collect();
    let kept: std::collections::BTreeSet<String> =
        narrow_for_command(build(), &None).tool_names().into_iter().collect();
    assert_eq!(kept, all, "None must keep every base tool");

    // Some(list) → narrowed to exactly the intersection.
    let only_read =
        narrow_for_command(build(), &Some(vec!["fs.read".to_string()])).tool_names();
    assert_eq!(only_read, vec!["fs.read".to_string()]);

    // Some([]) → no tools.
    assert!(
        narrow_for_command(build(), &Some(vec![])).tool_names().is_empty(),
        "an empty allowlist must yield no tools"
    );

    // An unknown name is silently dropped (intersection), never an error.
    let unknown =
        narrow_for_command(build(), &Some(vec!["does.not.exist".to_string()])).tool_names();
    assert!(unknown.is_empty(), "unknown tool names are dropped");
}
```

- [ ] **Step 7: Run the unit test to verify it passes**

Run: `cargo test -p otto-engine --bin otto narrow_for_command_applies -- --nocapture`
Expected: PASS.

- [ ] **Step 8: Confirm the existing command test still passes (omitted-allowlist parity)**

Run: `cargo test -p otto-engine --bin otto run_command_expands_and_runs_spine`
Expected: PASS — that command has no `allowed-tools` frontmatter, so `narrow_for_command` returns the registry as-is and behavior is unchanged.

- [ ] **Step 9: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(extensions): enforce command allowed-tools via registry subset

A command's allowed_tools now narrows the --command registry (used for
both injection resolution and the spine turn) the same way agents narrow
via ToolRegistry::subset: absent = all tools, present = intersection,
empty = none. Narrowing shares the gate, so the sensitive-path floor is
preserved and access can only shrink. Skills remain deferred."
```

---

## Task 2: Update documentation

**Files:**
- Modify: `crates/extensions/src/command_def.rs:7-8` (the `CustomCommandDef` doc comment)
- Modify: `CLAUDE.md:144` (the `extensions` table row)

- [ ] **Step 1: Update the `CustomCommandDef` doc comment**

In `crates/extensions/src/command_def.rs`, replace this doc comment above the struct:

```rust
/// One parsed custom command. All frontmatter fields are optional. `model` is preserved for a
/// later slice (not routed); `allowed_tools` is preserved for a later slice (not enforced).
```

with:

```rust
/// One parsed custom command. All frontmatter fields are optional. `model` is preserved for a
/// later slice (not routed). `allowed_tools` is enforced on the `otto run --command` path, where
/// it narrows the tool registry via `ToolRegistry::subset` (absent = all tools, present =
/// intersection, empty = none); it is inert on any other path.
```

- [ ] **Step 2: Update CLAUDE.md (Slice 2 sentence)**

In `CLAUDE.md` line 144, find this sentence (inside the Slice 2 description):

```
`model`/`allowed-tools` are parsed and preserved but not yet routed/enforced.
```

Replace it with:

```
`model` is parsed and preserved but not yet routed; `allowed-tools` is enforced on the `--command` path (see Slice 7).
```

- [ ] **Step 3: Update CLAUDE.md (append Slice 7)**

In `CLAUDE.md` line 144, find the final sentence of the Slice 6 description:

```
Wired into the `otto run` spine only (inserted only when rules exist, so a workspace with no `permissions` is byte-for-byte unchanged); per-artifact `allowed-tools` enforcement, `model` routing, and serve-path/`--command`/`--agent` wiring remain deferred.
```

Replace it with (same text, then a new Slice 7 sentence appended):

```
Wired into the `otto run` spine only (inserted only when rules exist, so a workspace with no `permissions` is byte-for-byte unchanged); per-artifact `allowed-tools` enforcement, `model` routing, and serve-path/`--command`/`--agent` wiring remain deferred. Slice 7 adds per-artifact `allowed-tools` enforcement for **commands**: `run_command_in` narrows the command's tool registry via `ToolRegistry::subset` before it is used for both `!bash`/`@file` injection resolution and the spine turn (absent `allowed-tools` = all tools, present = intersection, empty = none; narrowing shares the gate so the sensitive-path floor and any future command-path `PolicyGate` are preserved and access can only shrink). Skills' `allowed-tools` stays inert (a skill loads instructions into the ongoing turn and has no invocation scope to narrow); `model` routing and serve-path wiring remain the other open extensions threads.
```

- [ ] **Step 4: Commit**

```bash
git add crates/extensions/src/command_def.rs CLAUDE.md
git commit -m "docs(extensions): record command allowed-tools enforcement (slice 7)"
```

---

## Task 3: Full verification

**Files:** none (verification only).

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Expected: no diff (or only formatting of the lines just edited — review and keep).

- [ ] **Step 2: Lint**

Run: `cargo clippy --workspace --all-targets`
Expected: no new warnings. In particular, `narrow_for_command` must not be reported as dead code (it is called by `run_command_in`).

- [ ] **Step 3: Run the full offline test suite**

Run: `cargo test --workspace`
Expected: PASS — all existing tests stay green, plus the two new tests in `crates/engine/src/main.rs`. The default offline/deterministic path is unchanged for the plain `otto run` spine (no artifact → no narrowing).

- [ ] **Step 4: Commit any formatting changes (if Step 1 produced a diff)**

```bash
git add -A
git commit -m "style(extensions): cargo fmt after allowed-tools slice"
```

(Skip this step if `cargo fmt --all` produced no diff.)

---

## Spec coverage check

- Command `allowed_tools` narrows the `--command` registry via `subset` for both injection and the spine turn → Task 1 (Steps 3–5).
- Narrowing convention (None=all / Some=intersection / empty=none / unknown dropped) → Task 1 Step 6 unit test.
- Fail-closed injection under a narrowed allowlist → Task 1 Step 1 behavioral test.
- Omitted-allowlist parity (unchanged behavior) → Task 1 Step 8 (existing test stays green).
- Sensitive floor preserved within the subset → guaranteed by `subset` sharing the gate (documented in `narrow_for_command`; no behavior change to assert beyond existing gate tests).
- Skills explicitly deferred → no code; recorded in docs (Task 2) and the spec.
- No regression → Task 3 (`cargo test --workspace`).
