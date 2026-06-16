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

use async_trait::async_trait;
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest};
use serde_json::{Value, json};

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

/// Truncate to at most `max` chars on a char boundary (for bounded failure detail).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("… (truncated)");
        out
    }
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
                    // than fail the turn. Tradeoff: a genuine in-test 127 (a missing binary inside
                    // a test/make target) is also skipped rather than failed — accepted for v1, as
                    // failing every project whose toolchain isn't on the PATH would be worse.
                    Some(127) => Ok(AgentOutput::Verify {
                        ok: true,
                        detail: format!("verification skipped: {} tooling not found", recipe.label),
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
            // bash ran but failed (e.g. the command timed out, or the process couldn't spawn),
            // or returned an unexpected shape. Surface it as a verification failure rather than
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

/// Whether a `ToolRegistry::call` error means the `bash` tool was unavailable (unregistered or
/// permission-denied before dispatch) rather than a failure of the command itself. These
/// substrings mirror the pre-dispatch `bail!` messages in `crates/engine-core/src/tool.rs`.
fn is_tool_unavailable(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("no tool registered")
        || msg.contains("ask denied")
        || msg.contains("denied by permission gate")
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::tool::{
        AllowListAskResolver, AskResolver, DenyAsk, PermissionGate, Tool, ToolRegistry,
    };
    use otto_engine_core::traits::Workspace;
    use otto_engine_core::types::Edit;
    use otto_providers::LocalProvider;
    use otto_router::SingleProviderRouter;
    use otto_tools::{DefaultPermissionGate, FsListTool};
    use otto_workspace::LocalWorkspace;
    use std::sync::Arc;

    /// A stand-in `bash` tool returning a canned exit code + output, so the Verifier's parse
    /// logic is tested without a real sandbox/cargo.
    struct FakeBash {
        exit_code: i64,
        output: String,
    }
    #[async_trait]
    impl Tool for FakeBash {
        fn name(&self) -> &str {
            "bash"
        }
        async fn call(&self, _args: Value) -> anyhow::Result<Value> {
            Ok(json!({ "stdout": self.output, "stderr": "", "exit_code": self.exit_code }))
        }
    }

    /// A `bash` stand-in whose call fails the way a real execution error does (e.g. a timeout),
    /// to prove the Verifier surfaces it instead of silently skipping.
    struct ErroringBash;
    #[async_trait]
    impl Tool for ErroringBash {
        fn name(&self) -> &str {
            "bash"
        }
        async fn call(&self, _args: Value) -> anyhow::Result<Value> {
            anyhow::bail!("bash command timed out after 180000 ms")
        }
    }

    async fn seed_cargo_toml(ws: &LocalWorkspace) {
        ws.apply_edit(&Edit {
            path: std::path::PathBuf::from("Cargo.toml"),
            new_contents: "[package]\nname=\"x\"\n".to_string(),
        })
        .await
        .unwrap();
    }

    async fn seed_file(ws: &LocalWorkspace, name: &str) {
        ws.apply_edit(&Edit {
            path: std::path::PathBuf::from(name),
            new_contents: "x".to_string(),
        })
        .await
        .unwrap();
    }

    fn router() -> SingleProviderRouter {
        SingleProviderRouter::new(Arc::new(LocalProvider::new()))
    }

    /// Build a registry with fs.list over `ws_path`, an optional fake bash, and a resolver that
    /// permits bash when one is registered.
    fn registry(ws_path: &std::path::Path, bash: Option<Arc<dyn Tool>>) -> ToolRegistry {
        let gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());
        let ask: Arc<dyn AskResolver> = if bash.is_some() {
            Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
        } else {
            Arc::new(DenyAsk)
        };
        let mut reg = ToolRegistry::new(gate, ask);
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_path));
        reg.register(Arc::new(FsListTool::new(ws)));
        if let Some(b) = bash {
            reg.register(b);
        }
        reg
    }

    async fn run_verifier(ws: &LocalWorkspace, tools: &ToolRegistry) -> AgentOutput {
        let router = router();
        let ctx = AgentCtx::new(&router, ws, tools);
        Verifier.run(AgentRequest::Verify, &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn passes_when_cargo_check_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_cargo_toml(&ws).await;
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash {
                exit_code: 0,
                output: "Finished".into(),
            })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, .. } => assert!(ok),
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fails_when_cargo_check_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_cargo_toml(&ws).await;
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash {
                exit_code: 1,
                output: "error[E0277]: the trait bound is not satisfied".into(),
            })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(!ok);
                assert!(
                    detail.contains("E0277"),
                    "detail should carry the error: {detail}"
                );
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_when_no_cargo_project() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        // No Cargo.toml seeded. A fake bash is present but must NOT be called.
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash {
                exit_code: 99,
                output: "should not run".into(),
            })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(ok);
                assert!(detail.contains("nothing to verify"));
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_when_bash_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_cargo_toml(&ws).await;
        // Cargo project, but NO bash tool registered (and DenyAsk) -> bash call fails -> skip.
        let tools = registry(dir.path(), None);
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(ok);
                assert!(detail.contains("skipped"));
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fails_when_bash_execution_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_cargo_toml(&ws).await;
        // Cargo project; bash IS permitted and registered, but the command errors (e.g. a
        // timeout). This must surface as a verification failure, never a silent skip-as-ok.
        let tools = registry(dir.path(), Some(Arc::new(ErroringBash)));
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(
                    !ok,
                    "an execution error must not pass verification: {detail}"
                );
                assert!(
                    detail.contains("timed out"),
                    "detail should carry the execution error: {detail}"
                );
                assert!(
                    !detail.contains("skipped"),
                    "an execution error must not be reported as skipped: {detail}"
                );
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[test]
    fn detect_selects_recipe_by_marker() {
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
        assert!(detect(&["README.md".to_string()]).is_none());
    }

    #[test]
    fn detect_prefers_language_recipe_over_makefile() {
        let files = vec!["Makefile".to_string(), "Cargo.toml".to_string()];
        assert_eq!(detect(&files).unwrap().label, "cargo test");
        let files = vec!["Makefile".to_string(), "go.mod".to_string()];
        assert_eq!(detect(&files).unwrap().label, "go test");
    }

    #[tokio::test]
    async fn verifies_a_non_cargo_project() {
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
                assert!(
                    detail.contains("go test"),
                    "detail names the command: {detail}"
                );
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fails_when_non_cargo_check_errors() {
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
                assert!(
                    detail.contains("FAIL"),
                    "detail carries the test output: {detail}"
                );
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_when_toolchain_not_found() {
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
                assert!(
                    detail.contains("not found"),
                    "detail explains why: {detail}"
                );
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }
}
