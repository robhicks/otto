# Generalized Verifier (multi-ecosystem) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generalize the Verifier from Cargo-only to a data-driven recipe table that detects Rust, Go, Node, Python, and Make projects and runs each ecosystem's test command in the sandboxed `bash` tool.

**Architecture:** Replace the hard-coded `Cargo.toml` check with an ordered `RECIPES` table (`{markers, command, label}`); the first recipe whose marker is present at the workspace root wins (language-native systems before the generic `Makefile`). The existing run→parse→degrade tail is reused unchanged, plus one new rule: exit code 127 ("command not found" — toolchain not on the sandbox PATH) degrades to a skip rather than a failure.

**Tech Stack:** Rust (edition 2024), async-trait, anyhow, serde_json, tempfile (dev). Runtime: the sandboxed `bash` tool.

---

## Context for the implementer (read once)

- Single file changes: `crates/agents/src/verifier.rs` (logic + tests) and `docs/ARCHITECTURE.md` (docs).
- The Verifier is an `Agent`: `run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput>`. Use `ctx: &AgentCtx` (never `<'_>`).
- It detects the project by listing the workspace root via the `fs.list` tool (`ctx.tools().call("fs.list", json!({}))` → `{"paths":[..]}`), then runs a command via the `bash` tool (`ctx.tools().call("bash", json!({"command": ..., "timeout_ms": ...}))` → `{"stdout","stderr","exit_code"}`).
- Output is `AgentOutput::Verify { ok: bool, detail: String }`. `ok: false` drives the orchestrator's Repair loop.
- The existing tests use a `FakeBash` stand-in tool (returns a canned `exit_code`/`stdout`) so the parse logic is tested without a real sandbox. Keep that pattern.
- Conventions: branch `feat/verifier-multi-ecosystem`; never detach HEAD; `git add`+`commit` only (no `--amend`); `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt` clean; TDD; **no AI/Claude self-attribution anywhere**.

## Execution setup (before Task 1)

Create the branch off `main`:
```bash
git checkout main && git checkout -b feat/verifier-multi-ecosystem
```

---

## File Structure

```
crates/agents/src/verifier.rs   # MODIFY: recipe table + generalized detection/command + 127-skip; module doc; tests
docs/ARCHITECTURE.md            # MODIFY: document the multi-ecosystem Verifier
```

---

## Task 1: Recipe-driven, multi-ecosystem Verifier

**Files:**
- Modify: `crates/agents/src/verifier.rs`

- [ ] **Step 1: Add the new tests**

Add these tests to the existing `#[cfg(test)] mod tests` block in `crates/agents/src/verifier.rs` (alongside the current ones). They reference a free function `detect` and a generic `seed_file` helper that don't exist yet, so this step won't compile until Step 3.

First add a generic seeding helper next to `seed_cargo_toml`:

```rust
    async fn seed_file(ws: &LocalWorkspace, name: &str) {
        ws.apply_edit(&Edit {
            path: std::path::PathBuf::from(name),
            new_contents: "x".to_string(),
        })
        .await
        .unwrap();
    }
```

Then the new tests:

```rust
    #[test]
    fn detect_selects_recipe_by_marker() {
        // Each ecosystem's marker maps to the right command label.
        let cases = [
            ("Cargo.toml", "cargo test"),
            ("go.mod", "go test"),
            ("package.json", "npm test"),
            ("pyproject.toml", "pytest"),
            ("setup.py", "pytest"),
            ("Makefile", "make test"),
        ];
        for (marker, label) in cases {
            let files = vec![marker.to_string()];
            let recipe = detect(&files).unwrap_or_else(|| panic!("no recipe for {marker}"));
            assert_eq!(recipe.label, label, "marker {marker} should map to {label}");
        }
        // An unrecognized workspace yields no recipe.
        assert!(detect(&["README.md".to_string()]).is_none());
    }

    #[test]
    fn detect_prefers_language_recipe_over_makefile() {
        // A repo with both a language marker and a Makefile runs the language recipe.
        let files = vec!["Makefile".to_string(), "Cargo.toml".to_string()];
        assert_eq!(detect(&files).unwrap().label, "cargo test");
        let files = vec!["Makefile".to_string(), "go.mod".to_string()];
        assert_eq!(detect(&files).unwrap().label, "go test");
    }

    #[tokio::test]
    async fn verifies_a_non_cargo_project() {
        // A Go project whose `go test` passes.
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_file(&ws, "go.mod").await;
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash {
                exit_code: 0,
                output: "ok  \tmod\t0.1s".into(),
            })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(ok);
                assert!(detail.contains("go test"), "detail names the command: {detail}");
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fails_when_non_cargo_check_errors() {
        // A Node project whose `npm test` fails surfaces the output.
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_file(&ws, "package.json").await;
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash {
                exit_code: 1,
                output: "FAIL src/x.test.js".into(),
            })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(!ok);
                assert!(detail.contains("FAIL"), "detail carries the test output: {detail}");
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_when_toolchain_not_found() {
        // Exit 127 = command not found (toolchain not on the sandbox PATH) -> skip, not fail.
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_file(&ws, "pyproject.toml").await;
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash {
                exit_code: 127,
                output: "bash: pytest: command not found".into(),
            })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(ok, "a missing toolchain must not fail the turn: {detail}");
                assert!(detail.contains("skipped"), "detail says skipped: {detail}");
                assert!(detail.contains("not found"), "detail explains why: {detail}");
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail (don't compile yet)**

Run: `cargo test -p otto-agents verifier::`
Expected: FAIL to compile — `detect` is not defined.

- [ ] **Step 3: Implement the recipe-driven Verifier**

Replace the module doc comment and the body of `verifier.rs` from the top of the file through the end of the `impl Agent for Verifier` block (i.e. everything above the `fn is_tool_unavailable` definition) with the following. Leave `fn is_tool_unavailable`, `fn truncate`, and the entire `#[cfg(test)] mod tests` block in place (only add the new tests from Step 1 to that module).

Replace the module doc (`//!` lines at the very top) with:

```rust
//! The Verifier agent: checks the workspace builds/tests. It detects the project type from the
//! root file listing (`Cargo.toml`, `go.mod`, `package.json`, `pyproject.toml`/`setup.py`,
//! `Makefile`) and runs that ecosystem's test command inside the sandboxed `bash` tool,
//! reporting pass/fail. Detection is first-match over an ordered recipe table — language-native
//! build systems take precedence over the generic `Makefile` escape hatch.
//!
//! It degrades gracefully: no recognized project -> "nothing to verify"; `bash` unavailable
//! (no OS sandbox) -> "verification skipped"; the command's toolchain not on the sandbox PATH
//! (exit 127, "command not found") -> "verification skipped: <tool> tooling not found". A
//! non-zero exit drives the orchestrator's Repair loop; a `bash` *execution* error (e.g. the
//! command timed out, or the process couldn't spawn) is reported as a verification failure, not
//! silently skipped.
//!
//! Offline posture: the sandbox has no network (`--unshare-net`), so commands run offline —
//! `cargo test --offline` uses the warm cache; `go test`/`npm test`/`pytest` assume the
//! project's dependencies are already installed. A check needing the network fails, the same
//! accepted v1 posture as Cargo.
```

Replace the `pub struct Verifier;` line and the entire `#[async_trait] impl Agent for Verifier { ... }` block (but NOT `fn truncate`, which stays where it is) with:

```rust
pub struct Verifier;

/// A verification recipe: if any of `markers` is present at the workspace root, run `command`
/// (in the sandboxed `bash` tool) to verify the project; `label` names it in the result detail.
struct Recipe {
    markers: &'static [&'static str],
    command: &'static str,
    label: &'static str,
}

/// Ordered verification recipes. The first whose marker is present at the workspace root wins;
/// language-native build systems precede the generic `Makefile` escape hatch. Each command runs
/// offline (the sandbox has no network) and merges stderr into stdout (`2>&1`).
const RECIPES: &[Recipe] = &[
    Recipe {
        markers: &["Cargo.toml"],
        command: "cargo test --offline --quiet 2>&1",
        label: "cargo test",
    },
    Recipe {
        markers: &["go.mod"],
        command: "go test ./... 2>&1",
        label: "go test",
    },
    Recipe {
        markers: &["package.json"],
        command: "npm test 2>&1",
        label: "npm test",
    },
    Recipe {
        markers: &["pyproject.toml", "setup.py"],
        command: "pytest -q 2>&1",
        label: "pytest",
    },
    Recipe {
        markers: &["Makefile"],
        command: "make test 2>&1",
        label: "make test",
    },
];

/// The first recipe whose any marker file appears in the root listing.
fn detect(files: &[String]) -> Option<&'static Recipe> {
    RECIPES
        .iter()
        .find(|r| r.markers.iter().any(|m| files.iter().any(|f| f == m)))
}

#[async_trait]
impl Agent for Verifier {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Verify = req else {
            anyhow::bail!("Verifier received a non-Verify request");
        };

        // Detect the project type by listing the workspace root.
        let files: Vec<String> = match ctx.tools().call("fs.list", json!({})).await {
            Ok(Value::Object(map)) => map
                .get("paths")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let Some(recipe) = detect(&files) else {
            return Ok(AgentOutput::Verify {
                ok: true,
                detail: "no recognized project; nothing to verify".to_string(),
            });
        };

        // Run the recipe's command in the sandbox (stderr merged into stdout via 2>&1).
        let result = ctx
            .tools()
            .call(
                "bash",
                json!({ "command": recipe.command, "timeout_ms": 180000u64 }),
            )
            .await;

        match result {
            Ok(Value::Object(map)) => {
                let exit = map.get("exit_code").and_then(Value::as_i64);
                let stdout = map.get("stdout").and_then(Value::as_str).unwrap_or("");
                match exit {
                    Some(0) => Ok(AgentOutput::Verify {
                        ok: true,
                        detail: format!("{} passed", recipe.label),
                    }),
                    // Exit 127 = command not found: the toolchain isn't on the sandbox PATH (the
                    // curated env only guarantees cargo). We can't verify safely, so skip rather
                    // than fail the turn.
                    Some(127) => Ok(AgentOutput::Verify {
                        ok: true,
                        detail: format!(
                            "verification skipped: {} tooling not found",
                            recipe.label
                        ),
                    }),
                    _ => Ok(AgentOutput::Verify {
                        ok: false,
                        detail: truncate(stdout.trim(), 2000),
                    }),
                }
            }
            // bash is genuinely unavailable: no OS sandbox backend, so the tool is unregistered
            // or its `Ask` verdict is denied. `ToolRegistry::call` reports these before dispatch
            // (see `crates/engine-core/src/tool.rs`). We can't verify safely, so skip without
            // failing the turn. The substrings mirror that crate's pre-dispatch error messages.
            Err(e) if is_tool_unavailable(&e) => Ok(AgentOutput::Verify {
                ok: true,
                detail: "verification skipped: bash tool unavailable (no sandbox)".to_string(),
            }),
            // bash ran but failed (e.g. the command timed out, or the process couldn't spawn), or
            // returned an unexpected shape. Surface it as a verification failure rather than
            // silently passing — a real problem must drive the Repair loop, not read as success.
            Err(e) => Ok(AgentOutput::Verify {
                ok: false,
                detail: truncate(&format!("verification error: {e}"), 2000),
            }),
            Ok(_) => Ok(AgentOutput::Verify {
                ok: false,
                detail: "verification error: bash returned an unexpected result shape".to_string(),
            }),
        }
    }
}
```

NOTE: keep the existing `fn truncate(...)` and `fn is_tool_unavailable(...)` exactly as they are. The existing tests (`passes_when_cargo_check_succeeds`, `fails_when_cargo_check_errors`, `skips_when_no_cargo_project`, `skips_when_bash_unavailable`, `fails_when_bash_execution_errors`) stay and still pass: Cargo.toml is recipe #1, exit 0 → pass, exit 1 → fail with output, no marker → nothing to verify, bash-unavailable → skip, execution error → fail.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-agents verifier::`
Expected: PASS — the 5 existing tests + the 5 new ones (`detect_selects_recipe_by_marker`, `detect_prefers_language_recipe_over_makefile`, `verifies_a_non_cargo_project`, `fails_when_non_cargo_check_errors`, `skips_when_toolchain_not_found`).

Also run `cargo test -p otto-agents` to confirm nothing else broke.

- [ ] **Step 5: Lint, format, commit**

Run: `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean) and `cargo fmt -p otto-agents`.

```bash
git add crates/agents/src/verifier.rs
git commit -m "feat(agents): Verifier detects Go/Node/Python/Make projects via a recipe table"
```

---

## Task 2: Docs + final quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Update the Verifier description**

In `docs/ARCHITECTURE.md`, find the sentence(s) in the `### \`Agent\`` subsection describing the Verifier as running `cargo check`/`cargo check --offline` for "a Cargo project". Replace that Verifier description with:

```markdown
The `Verifier` is real and multi-ecosystem: it detects the project type from the workspace root
(`Cargo.toml`, `go.mod`, `package.json`, `pyproject.toml`/`setup.py`, or `Makefile`) via an
ordered recipe table — first match wins, language-native build systems before the generic
`Makefile` — and runs that ecosystem's test command (`cargo test --offline`, `go test ./...`,
`npm test`, `pytest -q`, `make test`) inside the sandboxed `bash` tool. A non-zero exit becomes
`Verify { ok: false }` with the truncated output as detail, which drives the orchestrator's
Repair loop. It degrades safely: no recognized project → "nothing to verify"; `bash` unavailable
(no OS sandbox) → "verification skipped"; the toolchain not on the sandbox PATH (exit 127) →
"verification skipped: <tool> tooling not found". Commands run offline (the sandbox has no
network), so dependencies must already be installed/cached — the accepted v1 posture.
```

Adapt the surrounding wording so it reads cleanly (e.g. if a neighboring sentence still says "for a Cargo project", reconcile it). Do not make unrelated edits.

- [ ] **Step 2: Final gate**

Run and capture output:
- `cargo fmt --all -- --check` (clean)
- `cargo clippy --workspace --all-targets -- -D warnings` (clean)
- `cargo test --workspace` — capture the per-crate `test result:` lines + summed total.

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document the multi-ecosystem Verifier"
```

---

## Done — what this delivers

The Verifier works beyond Rust: it recognizes Go, Node/TypeScript, Python, and Make projects and
runs each one's test command in the sandbox, driving the Repair loop on real failures. A missing
toolchain (exit 127) degrades to a skip instead of a spurious failure, and the recipe table is the
single seam where future ecosystems or per-project configuration land.

**Carried forward / deferred:**
- One command per ecosystem (no per-project override / config file yet).
- No monorepo/multi-language running of several checks (first match only).
- No `.tool-versions`/nvm/pyenv resolution — toolchains must be on the sandbox PATH (else the
  127-skip applies).
- The fixed 180 s timeout (test suites can be slower than a compile check).
