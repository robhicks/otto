//! Parses the `permissions` block of `.claude/settings.json` (Claude Code's allow/deny/ask
//! rules) into a pure verdict source. The gate decorator that *applies* these verdicts lives in
//! the `engine` crate (`policy_gate.rs`); this module is I/O-free and hermetic.

use otto_engine_core::tool::Decision;
use serde_json::Value;

/// The parsed allow/deny/ask rule sets. Verdict precedence is deny > ask > allow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionRules {
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    ask: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// The matched tool, normalized to an otto tool name (see `normalize_tool`).
    tool: String,
    /// `None` ⇒ the rule matches the tool regardless of arguments.
    spec: Option<Specifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Specifier {
    /// A gitignore-style path glob (raw pattern; compiled at match time), matched against the
    /// call's path argument(s).
    PathGlob(String),
    /// A bash command-prefix match against the `command` argument. `wildcard` is the trailing
    /// `:*` form, which matches at a whitespace boundary (e.g. `git:*` matches `git`/`git push`,
    /// not `gitfoo`); without it the command must match exactly.
    CmdPrefix { prefix: String, wildcard: bool },
}

/// Map a Claude Code tool name to its otto equivalent. otto-native and unknown names pass through
/// verbatim, so a future otto tool name works without a code change. Shared with `command_def`'s
/// `allowed-tools` parsing so both surfaces recognize the same Claude-Code-native names.
pub(crate) fn normalize_tool(name: &str) -> String {
    match name {
        "Read" => "fs.read",
        "Edit" | "Write" | "MultiEdit" => "fs.write",
        "Bash" => "bash",
        "Grep" => "grep",
        "Glob" | "LS" => "fs.list",
        other => other,
    }
    .to_string()
}

/// Parse one `"Tool"` or `"Tool(specifier)"` rule string. Returns `None` for an empty/malformed
/// rule (unbalanced parens, empty tool, or an uncompilable path glob) so a single bad rule is
/// dropped rather than poisoning the set.
fn parse_rule(s: &str) -> Option<Rule> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (tool_raw, spec_raw) = match s.split_once('(') {
        Some((t, rest)) => {
            let inner = rest.strip_suffix(')')?; // must end with ')'
            (t.trim(), Some(inner.trim()))
        }
        None => (s, None),
    };
    if tool_raw.is_empty() {
        return None;
    }
    let tool = normalize_tool(tool_raw);
    let spec = match spec_raw {
        None | Some("") => None,
        Some(inner) => Some(build_specifier(&tool, inner)?),
    };
    Some(Rule { tool, spec })
}

/// Build a specifier from the parenthesized text. `bash` → a command-prefix; every other tool →
/// a path glob (validated at parse time, stored raw).
fn build_specifier(tool: &str, inner: &str) -> Option<Specifier> {
    if tool == "bash" {
        let (prefix, wildcard) = match inner.strip_suffix(":*") {
            Some(p) => (p.trim().to_string(), true),
            None => (inner.to_string(), false),
        };
        Some(Specifier::CmdPrefix { prefix, wildcard })
    } else {
        build_glob(inner).ok()?; // reject an uncompilable glob
        Some(Specifier::PathGlob(inner.to_string()))
    }
}

/// Parse the `permissions` object of a `settings.json` document. A missing/invalid block, or a
/// non-array bucket, yields the empty set; individual malformed rules are skipped.
pub fn parse_permissions(settings_json: &str) -> PermissionRules {
    let Ok(v) = serde_json::from_str::<Value>(settings_json) else {
        return PermissionRules::default();
    };
    let Some(perms) = v.get("permissions") else {
        return PermissionRules::default();
    };
    let bucket = |key: &str| -> Vec<Rule> {
        perms
            .get(key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .filter_map(parse_rule)
                    .collect()
            })
            .unwrap_or_default()
    };
    PermissionRules {
        allow: bucket("allow"),
        deny: bucket("deny"),
        ask: bucket("ask"),
    }
}

impl PermissionRules {
    /// True when no allow/deny/ask rule is present.
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.ask.is_empty()
    }

    /// Append another rule set (union). Used to merge user then project `settings.json`.
    pub fn extend(&mut self, other: PermissionRules) {
        self.allow.extend(other.allow);
        self.deny.extend(other.deny);
        self.ask.extend(other.ask);
    }

    /// The verdict for a proposed call, or `None` when no rule matches. Precedence: deny > ask >
    /// allow (the first matching rule in that order wins).
    pub fn decision(&self, tool: &str, args: &Value) -> Option<Decision> {
        if self.deny.iter().any(|r| r.matches(tool, args)) {
            return Some(Decision::Deny);
        }
        if self.ask.iter().any(|r| r.matches(tool, args)) {
            return Some(Decision::Ask);
        }
        if self.allow.iter().any(|r| r.matches(tool, args)) {
            return Some(Decision::Allow);
        }
        None
    }
}

impl Rule {
    /// True if this rule governs `(tool, args)`.
    fn matches(&self, tool: &str, args: &Value) -> bool {
        if self.tool != tool {
            return false;
        }
        match &self.spec {
            None => true,
            Some(Specifier::PathGlob(pat)) => {
                // Recompiled per call by design: storing the raw pattern lets `Specifier` /
                // `PermissionRules` derive `Clone`/`Eq` (a `GlobMatcher` is neither). Call volume
                // is low (a handful of rules × tool calls); an `Arc<GlobMatcher>` is the future
                // optimization if it ever matters.
                let Ok(glob) = build_glob(pat) else {
                    return false;
                };
                let matcher = glob.compile_matcher();
                candidate_paths(args).iter().any(|p| {
                    let p = p.strip_prefix("./").unwrap_or(p);
                    matcher.is_match(p)
                })
            }
            Some(Specifier::CmdPrefix { prefix, wildcard }) => {
                match args.get("command").and_then(Value::as_str) {
                    // A wildcard prefix matches the command itself, or a longer command that
                    // continues at a whitespace boundary — so `git:*` matches `git`/`git status`
                    // but NOT `gitfoo` (an over-allow). An empty prefix (`Bash(:*)`) matches every
                    // command.
                    Some(cmd) if *wildcard => {
                        prefix.is_empty()
                            || cmd == prefix
                            || (cmd.starts_with(prefix.as_str())
                                && cmd[prefix.len()..].starts_with(char::is_whitespace))
                    }
                    Some(cmd) => cmd == prefix,
                    None => false,
                }
            }
        }
    }
}

/// Compile a path-glob pattern with gitignore-style semantics (`*` does not cross `/`, `**`
/// does). A leading `./` is stripped so workspace-relative patterns and args line up. Used at
/// both parse time (validation) and match time so the two never diverge.
fn build_glob(pat: &str) -> Result<globset::Glob, globset::Error> {
    let pat = pat.strip_prefix("./").unwrap_or(pat);
    globset::GlobBuilder::new(pat)
        .literal_separator(true)
        .build()
}

/// Candidate path strings from common tool-arg shapes — mirrors `DefaultPermissionGate`'s arg
/// inspection (`path`, `paths[]`, `glob`) so rules and the floor see the same surface.
fn candidate_paths(args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = args.get("path").and_then(Value::as_str) {
        out.push(p.to_string());
    }
    if let Some(arr) = args.get("paths").and_then(Value::as_array) {
        out.extend(arr.iter().filter_map(Value::as_str).map(str::to_string));
    }
    if let Some(g) = args.get("glob").and_then(Value::as_str) {
        out.push(g.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_buckets_and_reports_non_empty() {
        let rules = parse_permissions(
            r#"{ "permissions": { "allow": ["Read(src/**)"], "deny": ["Write(dist/**)"],
                                  "ask": ["Bash(git push:*)"] } }"#,
        );
        assert!(!rules.is_empty());
    }

    #[test]
    fn missing_or_invalid_block_is_empty() {
        assert!(parse_permissions("{}").is_empty());
        assert!(parse_permissions("not json").is_empty());
        assert!(parse_permissions(r#"{ "permissions": {} }"#).is_empty());
        assert!(parse_permissions(r#"{ "permissions": { "allow": "nope" } }"#).is_empty());
    }

    #[test]
    fn parse_rule_normalizes_aliases() {
        // Edit/Write/MultiEdit all map to otto's single fs.write; a bare Read maps to fs.read.
        assert_eq!(parse_rule("Edit(src/**)").unwrap().tool, "fs.write");
        assert_eq!(parse_rule("Write").unwrap().tool, "fs.write");
        assert_eq!(parse_rule("MultiEdit(a/**)").unwrap().tool, "fs.write");
        assert_eq!(parse_rule("Read").unwrap().tool, "fs.read");
        assert_eq!(parse_rule("Bash(ls:*)").unwrap().tool, "bash");
        assert_eq!(parse_rule("Grep").unwrap().tool, "grep");
        assert_eq!(parse_rule("Glob(x)").unwrap().tool, "fs.list");
        // otto-native + unknown names pass through verbatim.
        assert_eq!(parse_rule("fs.write").unwrap().tool, "fs.write");
        assert_eq!(parse_rule("git.commit").unwrap().tool, "git.commit");
    }

    #[test]
    fn parse_rule_rejects_malformed() {
        assert!(parse_rule("").is_none());
        assert!(parse_rule("   ").is_none());
        assert!(parse_rule("Read(src/**").is_none()); // unbalanced paren
        assert!(parse_rule("(x)").is_none()); // empty tool
    }

    #[test]
    fn bare_rule_has_no_specifier() {
        // A bare tool rule matches any args — proven behaviorally in Task 2; here just smoke-test
        // that parsing a non-bash bare rule succeeds and an inline-empty-spec is treated as bare.
        assert!(parse_rule("Read").is_some());
        assert!(parse_rule("Read()").is_some());
    }

    #[test]
    fn path_glob_matches_path_args() {
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Read(src/**)"] } }"#);
        assert_eq!(
            rules.decision("fs.read", &json!({"path": "src/a.rs"})),
            Some(Decision::Deny)
        );
        // Non-matching path → no rule applies.
        assert_eq!(
            rules.decision("fs.read", &json!({"path": "docs/a.md"})),
            None
        );
        // Leading "./" on the arg is tolerated.
        assert_eq!(
            rules.decision("fs.read", &json!({"path": "./src/a.rs"})),
            Some(Decision::Deny)
        );
        // paths[] form: any element matching triggers.
        assert_eq!(
            rules.decision("fs.read", &json!({"paths": ["docs/a.md", "src/b.rs"]})),
            Some(Decision::Deny)
        );
        // glob arg key (mcp-grep's file-filter arg shape) is also a candidate path.
        assert_eq!(
            rules.decision("fs.read", &json!({"glob": "src/foo.rs"})),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn bare_tool_rule_matches_any_args() {
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Read"] } }"#);
        assert_eq!(
            rules.decision("fs.read", &json!({"path": "anything.rs"})),
            Some(Decision::Deny)
        );
        // Different tool → no match.
        assert_eq!(
            rules.decision("fs.write", &json!({"path": "anything.rs"})),
            None
        );
    }

    #[test]
    fn bash_prefix_exact_vs_wildcard() {
        let wild = parse_permissions(r#"{ "permissions": { "allow": ["Bash(cargo test:*)"] } }"#);
        assert_eq!(
            wild.decision("bash", &json!({"command": "cargo test --all"})),
            Some(Decision::Allow)
        );
        assert_eq!(
            wild.decision("bash", &json!({"command": "cargo build"})),
            None
        );

        let exact = parse_permissions(r#"{ "permissions": { "allow": ["Bash(cargo test)"] } }"#);
        assert_eq!(
            exact.decision("bash", &json!({"command": "cargo test"})),
            Some(Decision::Allow)
        );
        // Exact rule does not match a longer command.
        assert_eq!(
            exact.decision("bash", &json!({"command": "cargo test --all"})),
            None
        );

        // `Bash(:*)` → empty prefix + wildcard ⇒ matches every bash command.
        let all = parse_permissions(r#"{ "permissions": { "deny": ["Bash(:*)"] } }"#);
        assert_eq!(
            all.decision("bash", &json!({"command": "anything at all"})),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn precedence_is_deny_over_ask_over_allow() {
        // Same call matched by all three buckets → deny wins.
        let rules = parse_permissions(
            r#"{ "permissions": { "allow": ["Bash(git:*)"], "ask": ["Bash(git push:*)"],
                                  "deny": ["Bash(git push --force:*)"] } }"#,
        );
        assert_eq!(
            rules.decision("bash", &json!({"command": "git push --force origin"})),
            Some(Decision::Deny)
        );
        // ask beats allow when deny doesn't match.
        assert_eq!(
            rules.decision("bash", &json!({"command": "git push origin"})),
            Some(Decision::Ask)
        );
        // only allow matches.
        assert_eq!(
            rules.decision("bash", &json!({"command": "git status"})),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn bash_wildcard_respects_word_boundary() {
        let rules = parse_permissions(r#"{ "permissions": { "allow": ["Bash(git:*)"] } }"#);
        // matches the bare command and a whitespace-separated continuation...
        assert_eq!(
            rules.decision("bash", &json!({"command": "git"})),
            Some(Decision::Allow)
        );
        assert_eq!(
            rules.decision("bash", &json!({"command": "git status"})),
            Some(Decision::Allow)
        );
        // ...but NOT a longer token that merely starts with the prefix.
        assert_eq!(
            rules.decision("bash", &json!({"command": "gitfoo bar"})),
            None
        );
    }

    #[test]
    fn single_star_does_not_cross_slash() {
        let star = parse_permissions(r#"{ "permissions": { "deny": ["Read(src/*)"] } }"#);
        assert_eq!(
            star.decision("fs.read", &json!({"path": "src/a.rs"})),
            Some(Decision::Deny)
        );
        // `*` stops at `/`, so a nested path is NOT matched.
        assert_eq!(
            star.decision("fs.read", &json!({"path": "src/sub/a.rs"})),
            None
        );

        let dstar = parse_permissions(r#"{ "permissions": { "deny": ["Read(src/**)"] } }"#);
        // `**` crosses `/`.
        assert_eq!(
            dstar.decision("fs.read", &json!({"path": "src/sub/a.rs"})),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn extend_unions_rule_sets() {
        let mut a = parse_permissions(r#"{ "permissions": { "allow": ["Read(src/**)"] } }"#);
        let b = parse_permissions(r#"{ "permissions": { "deny": ["Read(secret/**)"] } }"#);
        a.extend(b);
        assert_eq!(
            a.decision("fs.read", &json!({"path": "secret/x"})),
            Some(Decision::Deny)
        );
        assert_eq!(
            a.decision("fs.read", &json!({"path": "src/x"})),
            Some(Decision::Allow)
        );
    }
}
