# Serve-path permissions + PolicyGate×ApprovalModeGate composition Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `.claude/settings.json` permission rules into `otto serve` (today they're discovered only to print an unenforced warning), and make them compose correctly with `otto serve --approve-edits`, replacing the hard `assert!` in `build_tool_registry_inner` that currently forbids combining the two.

**Architecture:** `PolicyGate` and `ApprovalModeGate` are both plain `PermissionGate` decorators. `ApprovalModeGate` only upgrades a *permitted* (`Allow`) `fs.write` to `Ask`; it passes `Deny` and any existing `Ask` straight through unchanged. So wrapping `ApprovalModeGate::new(Arc::new(PolicyGate::new(base, rules, sandboxed)))` gives exactly the right precedence: the sensitive-path floor and any `deny`/`ask` permission rule still win, and an ordinary rule-`Allow`ed write still gets upgraded to interactive approval. The orchestrator's edit-apply path (`crates/engine-core/src/orchestrator.rs`) already treats any `Ask` verdict on `fs.write` uniformly — it emits `ApprovalRequest` and awaits the `Approver` — regardless of which gate produced the `Ask`, so no orchestrator change is needed. The only wiring changes are: (1) `build_tool_registry_with_permissions` gains an `approve_edits: bool` parameter and threads it into the existing gate-selection `match` in `build_tool_registry_inner` (replacing the assert with real composition), and (2) `cmd_serve` in `crates/engine/src/main.rs` passes its already-discovered `ext.permissions` into `build_tools_preferring_mcp` instead of an empty default, and drops the now-stale "permissions not enforced" warning.

**Tech Stack:** Rust, existing `otto-engine-core`/`otto-engine`/`otto-extensions`/`otto-tools` crates. No new dependencies.

---

### Task 1: Compose `PolicyGate` with `ApprovalModeGate` in `build_tool_registry_inner`

**Files:**
- Modify: `crates/engine/src/lib.rs:140-149` (`build_tool_registry_with_permissions`)
- Modify: `crates/engine/src/lib.rs:158-201` (`build_tool_registry_inner`)
- Modify: `crates/engine/src/lib.rs:437-462` (existing test, add the new `approve_edits` arg)
- Test: `crates/engine/src/lib.rs` (new tests in the same `mod tests`)

- [ ] **Step 1: Update the existing test call site to the new 4-arg signature**

In `crates/engine/src/lib.rs`, find the existing test `registry_with_permissions_denies_matched_write` (around line 437) and change the `build_tool_registry_with_permissions` call from:

```rust
        let reg = build_tool_registry_with_permissions(ws, dir.path().to_path_buf(), &rules);
```

to:

```rust
        let reg = build_tool_registry_with_permissions(ws, dir.path().to_path_buf(), &rules, false);
```

- [ ] **Step 2: Add two new tests for the approve_edits + permissions composition**

Immediately after the `registry_with_permissions_denies_matched_write` test (still inside `mod tests`, before the closing `}` of the module), add:

```rust
    #[tokio::test]
    async fn registry_with_permissions_and_approval_upgrades_ordinary_write_to_ask() {
        use otto_engine_core::tool::Decision;
        use otto_extensions::parse_permissions;
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path().to_path_buf()));
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#);
        let reg =
            build_tool_registry_with_permissions(ws, dir.path().to_path_buf(), &rules, true);

        // An ordinary write (no matching rule) is upgraded from the PolicyGate's Allow to Ask
        // for interactive approval, not silently applied.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "src/x.txt"})),
            Decision::Ask
        );
        // A rule-driven deny still wins over approval mode.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "dist/x.txt"})),
            Decision::Deny
        );
        // The sensitive-path floor still wins over everything.
        assert_eq!(
            reg.check("fs.write", &json!({"path": ".env"})),
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn registry_with_permissions_and_approval_preserves_rule_driven_ask() {
        use otto_engine_core::tool::Decision;
        use otto_extensions::parse_permissions;
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path().to_path_buf()));
        let rules = parse_permissions(r#"{ "permissions": { "ask": ["Write(secrets/**)"] } }"#);
        let reg =
            build_tool_registry_with_permissions(ws, dir.path().to_path_buf(), &rules, true);

        // A rule-driven `ask` on write is unaffected by the ApprovalModeGate wrap (it only
        // upgrades Allow, never re-classifies an existing Ask) — it still reaches interactive
        // approval, same as an ordinary write would.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "secrets/x.txt"})),
            Decision::Ask
        );
    }
```

- [ ] **Step 3: Run the new tests to see them fail to compile (signature mismatch)**

Run: `cargo test -p otto-engine registry_with_permissions --lib`
Expected: FAIL to compile — `build_tool_registry_with_permissions` takes 3 arguments but 4 were supplied.

- [ ] **Step 4: Update `build_tool_registry_with_permissions`'s signature and doc comment**

In `crates/engine/src/lib.rs`, replace:

```rust
/// Build the tool registry with a `PolicyGate` applying `permissions` over the default gate.
/// Used by the `otto run` spine when `.claude/settings.json` declares any permission rules; the
/// PolicyGate owns the bash decision, so it pairs with a plain `DenyAsk` resolver.
pub fn build_tool_registry_with_permissions(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    permissions: &PermissionRules,
) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, false, Some(permissions))
}
```

with:

```rust
/// Build the tool registry with a `PolicyGate` applying `permissions` over the default gate,
/// optionally composed with approval mode. Used by the `otto run` spine (`approve_edits =
/// false`) and by `otto serve --approve-edits` (`approve_edits` reflects the flag) when
/// `.claude/settings.json` declares any permission rules. The PolicyGate always owns the bash
/// decision, so it pairs with a plain `DenyAsk` resolver; when `approve_edits` is true, an
/// `ApprovalModeGate` wraps the `PolicyGate` so an ordinary (rule-`Allow`ed) `fs.write` is
/// upgraded to `Ask` for interactive approval — a rule-driven `deny`/`ask` (and the sensitive
/// floor) still win.
pub fn build_tool_registry_with_permissions(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    permissions: &PermissionRules,
    approve_edits: bool,
) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, approve_edits, Some(permissions))
}
```

- [ ] **Step 5: Replace the hard assert with real composition in `build_tool_registry_inner`**

In `crates/engine/src/lib.rs`, replace the whole body of `build_tool_registry_inner` from the assert through the end of the `match` (the block producing `(gate, ask)`):

```rust
fn build_tool_registry_inner(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
    permissions: Option<&PermissionRules>,
) -> ToolRegistry {
    // Invariant: permissions are wired only on the non-approving run path this slice. The
    // PolicyGate × ApprovalModeGate composition is a deferred serve-path slice. A hard assert
    // (not debug_assert) so a future caller passing both fails loud in release too, rather than
    // silently dropping approval mode (a security regression).
    assert!(
        !(approve_edits && matches!(permissions, Some(r) if !r.is_empty())),
        "PolicyGate × ApprovalModeGate composition is not yet wired",
    );
    let sandboxed = os_sandbox_available();
    let base_gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());

    // When permission rules exist, the PolicyGate owns every verdict (incl. bash), so it pairs
    // with a plain DenyAsk. Otherwise the wiring is exactly as before: the bash allow-list
    // resolver auto-allows the structurally-Asked sandboxed bash, and approval mode (serve) may
    // upgrade fs.write. (PolicyGate × ApprovalModeGate composition is a deferred serve-path slice,
    // so `permissions` is only ever Some on the non-approving run path.)
    let (gate, ask): (Arc<dyn PermissionGate>, Arc<dyn AskResolver>) = match permissions {
        Some(rules) if !rules.is_empty() => (
            Arc::new(PolicyGate::new(base_gate, rules.clone(), sandboxed)),
            Arc::new(DenyAsk),
        ),
        _ => {
            // NB: the ask-resolver only ever auto-allows `bash`. An `Ask` on `fs.write` (approval
            // mode) is resolved by the orchestrator's `Approver`, never here — so writes can't
            // slip through.
            let ask: Arc<dyn AskResolver> = if sandboxed {
                Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
            } else {
                Arc::new(DenyAsk)
            };
            let gate: Arc<dyn PermissionGate> = if approve_edits {
                Arc::new(ApprovalModeGate::new(base_gate))
            } else {
                base_gate
            };
            (gate, ask)
        }
    };
```

with:

```rust
fn build_tool_registry_inner(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
    permissions: Option<&PermissionRules>,
) -> ToolRegistry {
    let sandboxed = os_sandbox_available();
    let base_gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());

    // When permission rules exist, the PolicyGate owns every verdict (incl. bash), so it always
    // pairs with a plain DenyAsk. `approve_edits` then wraps an `ApprovalModeGate` around the
    // PolicyGate: an ordinary (rule-`Allow`ed) `fs.write` is upgraded to `Ask` for interactive
    // approval, while a rule-driven `deny`/`ask` (and the sensitive floor) pass through
    // unchanged. The orchestrator's edit-apply path treats any `Ask` on `fs.write` identically
    // regardless of which gate produced it, so the two compose without special-casing there.
    let (gate, ask): (Arc<dyn PermissionGate>, Arc<dyn AskResolver>) = match permissions {
        Some(rules) if !rules.is_empty() => {
            let policy_gate: Arc<dyn PermissionGate> =
                Arc::new(PolicyGate::new(base_gate, rules.clone(), sandboxed));
            let gate: Arc<dyn PermissionGate> = if approve_edits {
                Arc::new(ApprovalModeGate::new(policy_gate))
            } else {
                policy_gate
            };
            (gate, Arc::new(DenyAsk))
        }
        _ => {
            // NB: the ask-resolver only ever auto-allows `bash`. An `Ask` on `fs.write` (approval
            // mode) is resolved by the orchestrator's `Approver`, never here — so writes can't
            // slip through.
            let ask: Arc<dyn AskResolver> = if sandboxed {
                Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
            } else {
                Arc::new(DenyAsk)
            };
            let gate: Arc<dyn PermissionGate> = if approve_edits {
                Arc::new(ApprovalModeGate::new(base_gate))
            } else {
                base_gate
            };
            (gate, ask)
        }
    };
```

(The rest of the function — registering `fs.read`/`fs.write`/`fs.list` and the sandboxed `bash` tool — is unchanged.)

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p otto-engine registry_with_permissions --lib`
Expected: PASS — 3 tests (`registry_with_permissions_denies_matched_write`,
`registry_with_permissions_and_approval_upgrades_ordinary_write_to_ask`,
`registry_with_permissions_and_approval_preserves_rule_driven_ask`).

- [ ] **Step 7: Run the full `otto-engine` lib test suite to check for regressions**

Run: `cargo test -p otto-engine --lib`
Expected: all tests PASS (the crate doesn't build yet if `main.rs` hasn't been updated — see Task 2 — so if this fails only on `main.rs` compile errors referencing the old 3-arg signature, that's expected until Task 2 is done; if it fails for any other reason, stop and investigate before continuing).

- [ ] **Step 8: Commit**

```bash
git add crates/engine/src/lib.rs
git commit -m "feat(engine): compose PolicyGate with ApprovalModeGate for permissions + approve-edits"
```

---

### Task 2: Wire discovered permissions into `otto serve`

**Files:**
- Modify: `crates/engine/src/main.rs:160-173` (`build_tools_preferring_mcp`)
- Modify: `crates/engine/src/main.rs:1138-1145` (test `run_path_registry_applies_discovered_permissions`)
- Modify: `crates/engine/src/main.rs` (`cmd_serve`, around lines 588-604)
- Test: `crates/engine/src/main.rs` (new test in `mod tests`)

- [ ] **Step 1: Update `build_tools_preferring_mcp` to pass `approve_edits` through to the permissions branch**

In `crates/engine/src/main.rs`, replace:

```rust
    let mut registry = if !permissions.is_empty() {
        // Permission rules override the default gate with a PolicyGate (run path only; not
        // composed with approve_edits this slice).
        otto_engine::build_tool_registry_with_permissions(
            tools_workspace,
            root.clone(),
            permissions,
        )
    } else if approve_edits {
        otto_engine::build_tool_registry_approving(tools_workspace, root.clone())
    } else {
        build_tool_registry(tools_workspace, root.clone())
    };
```

with:

```rust
    let mut registry = if !permissions.is_empty() {
        // Permission rules override the default gate with a PolicyGate, composed with approval
        // mode when the caller requests it (e.g. `otto serve --approve-edits`).
        otto_engine::build_tool_registry_with_permissions(
            tools_workspace,
            root.clone(),
            permissions,
            approve_edits,
        )
    } else if approve_edits {
        otto_engine::build_tool_registry_approving(tools_workspace, root.clone())
    } else {
        build_tool_registry(tools_workspace, root.clone())
    };
```

- [ ] **Step 2: Update the existing run-path test to the new 4-arg signature**

In `crates/engine/src/main.rs`, find `run_path_registry_applies_discovered_permissions` (around line 1117) and change:

```rust
        let reg = if !ext.permissions.is_empty() {
            otto_engine::build_tool_registry_with_permissions(
                ws,
                proj.path().to_path_buf(),
                &ext.permissions,
            )
        } else {
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf())
        };
```

to:

```rust
        let reg = if !ext.permissions.is_empty() {
            // The `otto run` spine never sets approve_edits.
            otto_engine::build_tool_registry_with_permissions(
                ws,
                proj.path().to_path_buf(),
                &ext.permissions,
                false,
            )
        } else {
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf())
        };
```

- [ ] **Step 3: Run the updated test to confirm it still passes**

Run: `cargo test -p otto-engine run_path_registry_applies_discovered_permissions --lib`
Expected: PASS.

- [ ] **Step 4: Wire `cmd_serve` to enforce discovered permissions and drop the stale warning**

In `crates/engine/src/main.rs`, inside `cmd_serve`, replace:

```rust
    {
        let ext = otto_extensions::discover(&root, &home_dir());
        if !ext.hooks.is_empty() {
            eprintln!(
                "warning: settings.json hooks are configured but are NOT enforced on the serve \
                 path (hooks are wired only on the `otto run` spine for now)."
            );
        }
        if !ext.permissions.is_empty() {
            eprintln!(
                "warning: settings.json permissions are configured but are NOT enforced on \
                 this path (permissions are wired only on the `otto run` spine for now)."
            );
        }
    }

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    // NOTE: hooks/skills/plugin MCP servers are wired only in the main `otto run` spine for now; the
    // --agent/--command/serve paths are deferred (extensions hooks slice).
    let (tools, _mcp_conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &otto_extensions::PermissionRules::default(),
    )
    .await;
```

with:

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
```

- [ ] **Step 5: Add a test mirroring the serve-path gate composition**

Immediately after `run_path_registry_applies_discovered_permissions` (still inside `mod tests`, before the closing `}` of the module), add:

```rust
    #[tokio::test]
    async fn serve_path_registry_composes_permissions_with_approval_mode() {
        use otto_engine_core::tool::Decision;
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#,
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.permissions.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let approve_edits = true;
        // Mirrors the gate-selection logic in `build_tools_preferring_mcp` (which `cmd_serve`
        // calls with the discovered permissions + the --approve-edits flag); keep in sync.
        let reg = if !ext.permissions.is_empty() {
            otto_engine::build_tool_registry_with_permissions(
                ws,
                proj.path().to_path_buf(),
                &ext.permissions,
                approve_edits,
            )
        } else if approve_edits {
            otto_engine::build_tool_registry_approving(ws, proj.path().to_path_buf())
        } else {
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf())
        };

        // An ordinary write is upgraded to Ask for interactive approval, not silently applied.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "src/x.rs"})),
            Decision::Ask
        );
        // A rule-driven deny still wins over approval mode.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "dist/x.txt"})),
            Decision::Deny
        );
    }
```

- [ ] **Step 6: Run the new test**

Run: `cargo test -p otto-engine serve_path_registry_composes_permissions_with_approval_mode --lib`
Expected: PASS.

- [ ] **Step 7: Run the full `otto-engine` test suite**

Run: `cargo test -p otto-engine`
Expected: all tests PASS, including every test touched in Task 1 and Task 2.

- [ ] **Step 8: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): enforce settings.json permissions on the otto serve path"
```

---

### Task 3: Update documentation to reflect the shipped composition

**Files:**
- Modify: `CLAUDE.md` (the `extensions` crate table row)

- [ ] **Step 1: Append a "Slice 9" sentence to the `extensions` row in `CLAUDE.md`'s crate table**

Find the end of the long `extensions` row in the crate table (it currently ends with `"...Skills' `model` stays inert (no invocation scope to pin); serve-path wiring and per-sub-agent model remain deferred. |"`). Replace that final sentence:

```
Skills' `model` stays inert (no invocation scope to pin); serve-path wiring and per-sub-agent model remain deferred. |
```

with:

```
Skills' `model` stays inert (no invocation scope to pin); per-sub-agent model routing remains deferred. Slice 9 wires **permissions onto `otto serve`**: `cmd_serve` now passes its discovered `PermissionRules` into the tool registry instead of only warning that they're unenforced, and `build_tool_registry_with_permissions` gained an `approve_edits` parameter so `otto serve --approve-edits` composes correctly with permission rules — an `ApprovalModeGate` wraps the `PolicyGate` (rather than the two being mutually exclusive), upgrading an ordinary rule-`Allow`ed `fs.write` to interactive `Ask` while a rule-driven `deny`/`ask` and the sensitive floor still win. Hooks/skills/plugin MCP servers and the `--agent`/`--command` subpaths remain the other deferred serve-path threads. |
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: record extensions slice 9 (serve-path permissions + approval composition)"
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

- Compose `PolicyGate` with `ApprovalModeGate`, replacing the hard assert → Task 1.
- Wire discovered `.claude/settings.json` permissions into `otto serve` (currently only warned-about) → Task 2.
- Drop the now-stale "permissions not enforced" warning on serve, keep the still-accurate hooks warning → Task 2 Step 4.
- Regression safety: existing permissions tests updated in place, not replaced → Task 1 Step 1, Task 2 Step 2.
- New coverage for the composed behavior (ordinary write → Ask, rule-deny wins, sensitive floor wins, rule-ask preserved) → Task 1 Step 2, Task 2 Step 5.
- Docs reflect the shipped state → Task 3.
- No regression to `cargo build --workspace` / the offline determinism suite → Task 4.
