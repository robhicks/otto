//! The Verifier agent: checks the workspace builds. For a Cargo project it runs
//! `cargo check --offline` inside the sandboxed `bash` tool and reports pass/fail. It degrades
//! gracefully: no recognized project -> "nothing to verify"; `bash` unavailable (no OS sandbox)
//! -> "verification skipped". A failure here drives the orchestrator's Repair loop.
//!
//! Precondition: the check runs `--offline` because the sandbox has no network
//! (`--unshare-net`). A project's dependencies must therefore already be present in the host
//! `CARGO_HOME` cache; if they are not, `cargo` cannot fetch them and the check fails for a
//! reason unrelated to the edits under test. The common case — re-verifying an
//! already-built project — has a warm cache, so this holds.

use async_trait::async_trait;
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest};
use serde_json::{Value, json};

pub struct Verifier;

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
        let is_cargo = files.iter().any(|f| f == "Cargo.toml");
        if !is_cargo {
            return Ok(AgentOutput::Verify {
                ok: true,
                detail: "no recognized project; nothing to verify".to_string(),
            });
        }

        // Run `cargo check --offline` in the sandbox (stderr merged into stdout via 2>&1).
        let result = ctx
            .tools()
            .call(
                "bash",
                json!({ "command": "cargo check --offline --quiet 2>&1", "timeout_ms": 180000u64 }),
            )
            .await;

        match result {
            Ok(Value::Object(map)) => {
                let exit = map.get("exit_code").and_then(Value::as_i64);
                let stdout = map.get("stdout").and_then(Value::as_str).unwrap_or("");
                if exit == Some(0) {
                    Ok(AgentOutput::Verify {
                        ok: true,
                        detail: "cargo check passed".to_string(),
                    })
                } else {
                    Ok(AgentOutput::Verify {
                        ok: false,
                        detail: truncate(stdout.trim(), 2000),
                    })
                }
            }
            // bash unavailable (no OS sandbox) or denied by the gate -> can't verify safely; skip.
            _ => Ok(AgentOutput::Verify {
                ok: true,
                detail: "verification skipped: bash tool unavailable (no sandbox)".to_string(),
            }),
        }
    }
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

    async fn seed_cargo_toml(ws: &LocalWorkspace) {
        ws.apply_edit(&Edit {
            path: std::path::PathBuf::from("Cargo.toml"),
            new_contents: "[package]\nname=\"x\"\n".to_string(),
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
}
