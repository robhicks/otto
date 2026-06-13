//! `DefaultPermissionGate`: otto's built-in guardrail. Denies tool calls that touch
//! sensitive paths (the inviolable floor); allows everything else for now.

use otto_engine_core::tool::{Decision, PermissionGate};
use serde_json::Value;

/// Substrings (lowercase) that mark a path as sensitive. A tool-call argument naming such a
/// path is denied. Matching is case-insensitive (see `is_sensitive`). Symlink-to-secret
/// escapes are owned by `LocalWorkspace` path containment, not this string gate.
const SENSITIVE_MARKERS: &[&str] = &[".env", ".ssh/", ".ssh", ".git/", "id_rsa", ".aws/", ".aws"];

pub struct DefaultPermissionGate;

impl DefaultPermissionGate {
    pub fn new() -> Self {
        Self
    }

    /// True if `s` names a sensitive path. Case-insensitive so `.ENV` / `.AWS/...` can't
    /// slip past on case-insensitive filesystems (macOS/Windows).
    fn is_sensitive(s: &str) -> bool {
        let lower = s.to_ascii_lowercase();
        SENSITIVE_MARKERS.iter().any(|m| lower.contains(m))
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
        out
    }
}

impl Default for DefaultPermissionGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionGate for DefaultPermissionGate {
    fn evaluate(&self, _tool: &str, args: &Value) -> Decision {
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
