//! Approval-mode gate: an opt-in wrapper that turns ordinary `fs.write` edits into `Ask`
//! verdicts so they require interactive approval. The interactive resolution itself lives in
//! the serve layer (`InteractiveApprover`); this is only the policy decorator.

use std::sync::Arc;

use otto_engine_core::tool::{Decision, PermissionGate};
use serde_json::Value;

/// Wraps an inner gate, upgrading a *permitted* `fs.write` from `Allow` to `Ask`. A sensitive
/// `Deny` and every other classification (incl. `bash → Ask`) pass through unchanged, so the
/// inviolable sensitive-path floor is preserved.
pub struct ApprovalModeGate {
    inner: Arc<dyn PermissionGate>,
}

impl ApprovalModeGate {
    pub fn new(inner: Arc<dyn PermissionGate>) -> Self {
        Self { inner }
    }
}

impl PermissionGate for ApprovalModeGate {
    fn evaluate(&self, tool: &str, args: &Value) -> Decision {
        let inner = self.inner.evaluate(tool, args);
        if tool == "fs.write" && inner == Decision::Allow {
            Decision::Ask
        } else {
            inner
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_tools::DefaultPermissionGate;
    use serde_json::json;

    fn gate() -> ApprovalModeGate {
        ApprovalModeGate::new(Arc::new(DefaultPermissionGate::new()))
    }

    #[test]
    fn upgrades_ordinary_write_allow_to_ask() {
        assert_eq!(
            gate().evaluate("fs.write", &json!({"path": "src/a.rs"})),
            Decision::Ask
        );
    }

    #[test]
    fn sensitive_write_still_denied() {
        assert_eq!(
            gate().evaluate("fs.write", &json!({"path": ".env"})),
            Decision::Deny
        );
    }

    #[test]
    fn reads_and_bash_pass_through() {
        assert_eq!(
            gate().evaluate("fs.read", &json!({"path": "src/a.rs"})),
            Decision::Allow
        );
        assert_eq!(
            gate().evaluate("bash", &json!({"command": "ls"})),
            Decision::Ask
        );
    }
}
