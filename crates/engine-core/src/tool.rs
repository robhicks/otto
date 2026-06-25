//! The tool seam. Agents call tools through a `ToolRegistry` that runs a deterministic
//! `PermissionGate` (otto's guardrail) before dispatching. Tools are MCP-shaped — a name
//! plus JSON args in / JSON result out — so an MCP-stdio tool (rmcp client) can register
//! behind this same `Tool` trait later.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

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

/// Resolves an interactive approval for a proposed edit (the `Ask`-on-`fs.write` path).
/// Async because the verdict is a round-trip to a human/UI. Implementations MUST fail closed
/// (return `false`) when they cannot obtain an answer (e.g. a closed channel on disconnect).
#[async_trait]
pub trait Approver: Send + Sync {
    /// `true` = apply the edit, `false` = skip it. `old` is the file's current contents
    /// (`None` if the file does not exist yet); `new` is the proposed contents.
    async fn request(&self, id: Uuid, path: &Path, old: Option<&str>, new: &str) -> bool;
}

/// Headless default: never approve (≙ the orchestrator's prior `Ask → skip` behavior).
pub struct DenyApprover;

#[async_trait]
impl Approver for DenyApprover {
    async fn request(&self, _id: Uuid, _path: &Path, _old: Option<&str>, _new: &str) -> bool {
        false
    }
}

/// Cooperative pause for an in-flight turn. The orchestrator calls this at each phase
/// boundary: if a pause is requested it parks the turn until resumed. The default never
/// pauses, so CLI/headless/offline runs are unaffected.
#[async_trait]
pub trait PauseController: Send + Sync {
    /// A sync peek at a phase boundary: is a pause currently requested?
    fn should_pause(&self) -> bool;
    /// Park until resumed (or released on disconnect/abort). Returns promptly if not paused.
    async fn wait_for_resume(&self);
}

/// Default: never pauses.
pub struct NeverPause;

#[async_trait]
impl PauseController for NeverPause {
    fn should_pause(&self) -> bool {
        false
    }
    async fn wait_for_resume(&self) {}
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

    /// Returns a new registry containing only the tools whose names appear in `allowed`,
    /// sharing this registry's permission gate and ask-resolver unchanged (via `Arc::clone`).
    /// The sensitive-path floor and every gate decision are identical to the parent. Names in
    /// `allowed` that do not exist in this registry are silently ignored, so the returned subset
    /// can only be equal to or smaller than the parent — it can never widen capability.
    pub fn subset(&self, allowed: &[String]) -> ToolRegistry {
        let tools = allowed
            .iter()
            .filter_map(|name| self.tools.get(name).map(|t| (name.clone(), Arc::clone(t))))
            .collect();
        ToolRegistry {
            tools,
            gate: Arc::clone(&self.gate),
            ask: Arc::clone(&self.ask),
        }
    }

    /// The names of every registered tool. Lets a dispatcher request "all tools" as a subset.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Return the gate's `Decision` for a proposed call WITHOUT dispatching. Lets the
    /// orchestrator gate edits it applies directly (via the workspace) through the same
    /// policy that governs tool calls.
    pub fn check(&self, name: &str, args: &Value) -> Decision {
        self.gate.evaluate(name, args)
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
    use std::path::Path;
    use uuid::Uuid;

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

    struct PingTool;
    #[async_trait]
    impl Tool for PingTool {
        fn name(&self) -> &str {
            "ping"
        }
        async fn call(&self, _args: Value) -> anyhow::Result<Value> {
            Ok(json!("pong"))
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

    #[tokio::test]
    async fn deny_approver_always_rejects() {
        let a = DenyApprover;
        assert!(
            !a.request(Uuid::from_u128(0), Path::new("x.rs"), None, "new")
                .await
        );
        assert!(
            !a.request(Uuid::from_u128(1), Path::new("y.rs"), Some("old"), "new")
                .await
        );
    }

    #[tokio::test]
    async fn never_pause_does_not_pause() {
        let p = NeverPause;
        assert!(!p.should_pause());
        p.wait_for_resume().await; // returns immediately
    }

    #[test]
    fn check_returns_gate_decision_without_dispatch() {
        let allow = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        assert_eq!(
            allow.check("fs.write", &json!({"path": "a.txt"})),
            Decision::Allow
        );

        let deny = ToolRegistry::new(Arc::new(DenyAll), Arc::new(DenyAsk));
        assert_eq!(
            deny.check("fs.write", &json!({"path": "a.txt"})),
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn subset_keeps_only_named_tools() {
        let mut r = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        r.register(Arc::new(EchoTool));
        r.register(Arc::new(PingTool));

        let sub = r.subset(&["ping".to_string()]);
        assert!(sub.call("ping", json!({})).await.is_ok());
        // "echo" was excluded by the allowlist → no such tool in the subset.
        assert!(sub.call("echo", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn subset_preserves_gate_denials() {
        let mut r = ToolRegistry::new(Arc::new(DenyAll), Arc::new(DenyAsk));
        r.register(Arc::new(PingTool));
        let sub = r.subset(&["ping".to_string()]);
        // Tool is present in the allowlist, but the shared gate still denies it.
        assert!(sub.call("ping", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn subset_with_unknown_name_yields_empty_registry() {
        let mut r = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        r.register(Arc::new(PingTool));
        // A name not registered in the parent is silently dropped — no panic, no widening.
        let sub = r.subset(&["nonexistent".to_string()]);
        assert!(sub.call("nonexistent", json!({})).await.is_err());
        assert!(sub.call("ping", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn subset_with_empty_allowlist_exposes_no_tools() {
        let mut r = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        r.register(Arc::new(PingTool));
        let sub = r.subset(&[]);
        assert!(sub.call("ping", json!({})).await.is_err());
    }
}
