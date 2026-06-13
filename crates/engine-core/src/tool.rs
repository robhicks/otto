//! The tool seam. Agents call tools through a `ToolRegistry` that runs a deterministic
//! `PermissionGate` (otto's guardrail) before dispatching. Tools are MCP-shaped — a name
//! plus JSON args in / JSON result out — so an MCP-stdio tool (rmcp client) can register
//! behind this same `Tool` trait later.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

/// A callable tool: a stable name and a JSON-in / JSON-out call.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    async fn call(&self, args: Value) -> anyhow::Result<Value>;
}

/// The verdict a permission gate returns for a proposed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// Deterministic, non-LLM evaluation of a proposed tool call — otto's guardrail for tools.
pub trait PermissionGate: Send + Sync {
    fn evaluate(&self, tool: &str, args: &Value) -> Decision;
}

/// Resolves an `Ask` verdict to allow (true) or deny (false). Headless mode uses `DenyAsk`
/// (safe default); the interactive UI supplies a prompting resolver in a later plan.
pub trait AskResolver: Send + Sync {
    fn resolve(&self, tool: &str, args: &Value) -> bool;
}

/// Headless default: deny anything that requires asking.
pub struct DenyAsk;

impl AskResolver for DenyAsk {
    fn resolve(&self, _tool: &str, _args: &Value) -> bool {
        false
    }
}

/// Resolves `Ask` to allow only for an explicit allow-list of tool names. Used by the engine
/// to permit a tool that is `Ask`-gated but otherwise confined (e.g. a sandboxed `bash`).
pub struct AllowListAskResolver {
    allowed: Vec<String>,
}

impl AllowListAskResolver {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

impl AskResolver for AllowListAskResolver {
    fn resolve(&self, tool: &str, _args: &Value) -> bool {
        self.allowed.iter().any(|t| t == tool)
    }
}

/// Holds the available tools plus the gate/resolver. Every `call` is gated before dispatch.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    gate: Arc<dyn PermissionGate>,
    ask: Arc<dyn AskResolver>,
}

impl ToolRegistry {
    pub fn new(gate: Arc<dyn PermissionGate>, ask: Arc<dyn AskResolver>) -> Self {
        Self {
            tools: HashMap::new(),
            gate,
            ask,
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Gate then dispatch. Denied (or ask-denied) calls error before the tool runs.
    pub async fn call(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        match self.gate.evaluate(name, &args) {
            Decision::Deny => anyhow::bail!("tool '{name}' denied by permission gate"),
            Decision::Ask => {
                if !self.ask.resolve(name, &args) {
                    anyhow::bail!("tool '{name}' not permitted (ask denied)");
                }
            }
            Decision::Allow => {}
        }
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("no tool registered named '{name}'"))?;
        tool.call(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        async fn call(&self, args: Value) -> anyhow::Result<Value> {
            Ok(json!({ "echoed": args }))
        }
    }

    struct AllowAll;
    impl PermissionGate for AllowAll {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Allow
        }
    }

    struct DenyAll;
    impl PermissionGate for DenyAll {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Deny
        }
    }

    struct AskGate;
    impl PermissionGate for AskGate {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Ask
        }
    }

    fn registry(gate: Arc<dyn PermissionGate>, ask: Arc<dyn AskResolver>) -> ToolRegistry {
        let mut r = ToolRegistry::new(gate, ask);
        r.register(Arc::new(EchoTool));
        r
    }

    #[tokio::test]
    async fn allowed_call_dispatches_to_tool() {
        let r = registry(Arc::new(AllowAll), Arc::new(DenyAsk));
        let out = r.call("echo", json!({"x": 1})).await.unwrap();
        assert_eq!(out, json!({ "echoed": { "x": 1 } }));
    }

    #[tokio::test]
    async fn denied_call_never_dispatches() {
        let r = registry(Arc::new(DenyAll), Arc::new(DenyAsk));
        let err = r.call("echo", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("denied by permission gate"));
    }

    #[tokio::test]
    async fn ask_resolved_to_deny_blocks_call() {
        let r = registry(Arc::new(AskGate), Arc::new(DenyAsk));
        let err = r.call("echo", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("ask denied"));
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let r = registry(Arc::new(AllowAll), Arc::new(DenyAsk));
        let err = r.call("nope", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("no tool registered"));
    }

    #[test]
    fn allow_list_resolver_allows_listed_tool_only() {
        let r = AllowListAskResolver::new(vec!["bash".to_string()]);
        assert!(r.resolve("bash", &json!({})));
        assert!(!r.resolve("fs.write", &json!({})));
    }

    #[test]
    fn allow_list_resolver_denies_all_when_empty() {
        let r = AllowListAskResolver::new(vec![]);
        assert!(!r.resolve("bash", &json!({})));
        assert!(!r.resolve("fs.write", &json!({})));
    }
}
