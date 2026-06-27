//! `PolicyGate`: applies the `.claude/settings.json` permission rules over the base gate. It
//! preserves the inviolable sensitive-path floor (a base `Deny` short-circuits), then applies
//! deny > ask > allow rules, and finally upgrades a structural `bash` `Ask` to `Allow` when a
//! sandbox backend exists — so it owns the bash decision and pairs with a plain `DenyAsk`
//! resolver (the hardcoded bash allow-list is retired whenever this gate is active).

use std::sync::Arc;

use otto_engine_core::tool::{Decision, PermissionGate};
use otto_extensions::PermissionRules;
use serde_json::Value;

pub struct PolicyGate {
    inner: Arc<dyn PermissionGate>,
    rules: PermissionRules,
    bash_allowed: bool,
}

impl PolicyGate {
    pub fn new(inner: Arc<dyn PermissionGate>, rules: PermissionRules, bash_allowed: bool) -> Self {
        Self {
            inner,
            rules,
            bash_allowed,
        }
    }
}

impl PermissionGate for PolicyGate {
    fn evaluate(&self, tool: &str, args: &Value) -> Decision {
        // 1. The sensitive-path floor is inviolable — no allow rule can pierce it.
        let base = self.inner.evaluate(tool, args);
        if base == Decision::Deny {
            return Decision::Deny;
        }
        // 2–4. Rules, deny > ask > allow.
        if let Some(d) = self.rules.decision(tool, args) {
            return d;
        }
        // 5. No rule matched → base, except sandboxed bash's structural Ask becomes Allow.
        if tool == "bash" && base == Decision::Ask && self.bash_allowed {
            return Decision::Allow;
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_extensions::parse_permissions;
    use otto_tools::DefaultPermissionGate;
    use serde_json::json;

    fn gate(settings: &str, bash_allowed: bool) -> PolicyGate {
        PolicyGate::new(
            Arc::new(DefaultPermissionGate::new()),
            parse_permissions(settings),
            bash_allowed,
        )
    }

    #[test]
    fn floor_beats_allow_rule() {
        let g = gate(r#"{ "permissions": { "allow": ["Read(.env)"] } }"#, true);
        assert_eq!(
            g.evaluate("fs.read", &json!({"path": ".env"})),
            Decision::Deny
        );
    }

    #[test]
    fn bash_with_no_rule_follows_sandbox_flag() {
        let g = gate("{}", true);
        assert_eq!(
            g.evaluate("bash", &json!({"command": "ls"})),
            Decision::Allow
        );
        let g = gate("{}", false);
        assert_eq!(g.evaluate("bash", &json!({"command": "ls"})), Decision::Ask);
    }

    #[test]
    fn deny_rule_blocks_otherwise_allowed_call() {
        let g = gate(r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#, true);
        assert_eq!(
            g.evaluate("fs.write", &json!({"path": "dist/x"})),
            Decision::Deny
        );
        assert_eq!(
            g.evaluate("fs.write", &json!({"path": "src/x"})),
            Decision::Allow
        );
    }

    #[test]
    fn ask_rule_on_bash_returns_ask() {
        let g = gate(
            r#"{ "permissions": { "ask": ["Bash(git push:*)"] } }"#,
            true,
        );
        assert_eq!(
            g.evaluate("bash", &json!({"command": "git push origin"})),
            Decision::Ask
        );
    }

    #[test]
    fn allow_rule_upgrades_bash_when_sandbox_absent() {
        let g = gate(
            r#"{ "permissions": { "allow": ["Bash(cargo test:*)"] } }"#,
            false,
        );
        assert_eq!(
            g.evaluate("bash", &json!({"command": "cargo test --all"})),
            Decision::Allow
        );
    }
}
