//! `DefaultPermissionGate`: otto's built-in guardrail. Denies tool calls that touch
//! sensitive paths (the inviolable floor); allows everything else for now.
//!
//! NOTE: The canonical sensitive-path marker list now lives in `otto_engine_core::SENSITIVE_MARKERS`
//! (see `crates/engine-core/src/sensitive.rs`). Add new markers there — not here. `crates/mcp-grep`
//! keeps an independent copy (`SENSITIVE_SKIP`) because it is a standalone binary that cannot
//! depend on `engine-core`; keep it in sync manually when adding markers. NOTE: symlink-to-secret
//! escapes are a KNOWN OPEN ITEM — `LocalWorkspace` containment is lexical and does not resolve
//! symlinks; they are addressed by the sandboxed mcp-fs/mcp-bash layer in a later plan, not this
//! string gate.

use otto_engine_core::tool::{Decision, PermissionGate};
use serde_json::Value;

pub struct DefaultPermissionGate;

impl DefaultPermissionGate {
    pub fn new() -> Self {
        Self
    }

    /// True if `s` names a sensitive path. Delegates to the canonical floor in `engine-core`.
    fn is_sensitive(s: &str) -> bool {
        otto_engine_core::is_sensitive(s)
    }

    /// Collect candidate path strings from common arg shapes: `path`, `paths[]`.
    fn candidate_paths(args: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(p) = args.get("path").and_then(Value::as_str) {
            out.push(p.to_string());
        }
        if let Some(arr) = args.get("paths").and_then(Value::as_array) {
            for v in arr {
                if let Some(p) = v.as_str() {
                    out.push(p.to_string());
                }
            }
        }
        if let Some(g) = args.get("glob").and_then(Value::as_str) {
            out.push(g.to_string());
        }
        out
    }
}

impl Default for DefaultPermissionGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionGate for DefaultPermissionGate {
    fn evaluate(&self, tool: &str, args: &Value) -> Decision {
        // Shell exec can't be statically vetted by path — it always requires explicit approval.
        if tool == "bash" {
            return Decision::Ask;
        }
        for p in Self::candidate_paths(args) {
            if Self::is_sensitive(&p) {
                return Decision::Deny;
            }
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn denies_dotenv_path() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": ".env"})),
            Decision::Deny
        );
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": "config/.env.local"})),
            Decision::Deny
        );
    }

    #[test]
    fn denies_ssh_and_git_internal() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": ".ssh/id_rsa"})),
            Decision::Deny
        );
        assert_eq!(
            gate.evaluate("fs.write", &json!({"path": ".git/config"})),
            Decision::Deny
        );
    }

    #[test]
    fn allows_ordinary_paths() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": "src/main.rs"})),
            Decision::Allow
        );
        assert_eq!(gate.evaluate("fs.list", &json!({})), Decision::Allow);
    }

    #[test]
    fn denies_when_any_path_in_list_is_sensitive() {
        let gate = DefaultPermissionGate::new();
        let args = json!({"paths": ["src/a.rs", ".ssh/known_hosts"]});
        assert_eq!(gate.evaluate("fs.search", &args), Decision::Deny);
    }

    #[test]
    fn denies_bare_git_path() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": ".git"})),
            Decision::Deny
        );
    }

    #[test]
    fn denies_sensitive_glob() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(
            gate.evaluate("fs.list", &json!({"glob": ".ssh/**"})),
            Decision::Deny
        );
        assert_eq!(
            gate.evaluate("fs.list", &json!({"glob": "src/**/*.rs"})),
            Decision::Allow
        );
    }

    #[test]
    fn bash_requires_ask() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(
            gate.evaluate("bash", &json!({"command": "ls"})),
            Decision::Ask
        );
    }

    #[test]
    fn denies_case_variant_sensitive_paths() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": ".ENV"})),
            Decision::Deny
        );
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": ".SSH/config"})),
            Decision::Deny
        );
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": ".AWS/credentials"})),
            Decision::Deny
        );
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": "Id_Rsa"})),
            Decision::Deny
        );
    }
}
