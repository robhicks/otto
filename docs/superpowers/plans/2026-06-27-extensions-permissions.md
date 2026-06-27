# Extensions Permissions Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read the `permissions` block (allow/deny/ask) from `.claude/settings.json` and compose it into otto's permission gate so Claude-Code `Tool(specifier)` rules govern tool calls in the `otto run` spine, with the sensitive-path floor staying inviolable.

**Architecture:** A pure `permission_def.rs` in the `extensions` crate parses the rules and answers `decision(tool, args) -> Option<Decision>`. A thin `PolicyGate` decorator in the `engine` crate wraps `DefaultPermissionGate`, applying those rules with `deny > ask > allow` precedence above the floor, and owns the "sandboxed bash is allowed" default so the gate becomes the single authority (the hardcoded bash allow-list resolver is retired whenever the PolicyGate is active). Wiring is inserted into `cmd_run` only when at least one rule exists, so a workspace with no `permissions` is byte-for-byte unchanged.

**Tech Stack:** Rust (edition 2024), `serde_json` for parsing, `globset` 0.4 for path-glob matching, the existing `PermissionGate`/`Decision` seam in `engine-core`.

**Spec:** `docs/superpowers/specs/2026-06-27-extensions-permissions-design.md`

---

## File Structure

- **Create** `crates/extensions/src/permission_def.rs` — the rule types (`PermissionRules`, private `Rule`/`Specifier`), the Claude-Code `Tool(specifier)` parser + tool-name alias map, and the pure `decision`/`is_empty`/`extend` matcher. One responsibility: turn `settings.json` permission text into a verdict.
- **Modify** `crates/extensions/Cargo.toml` — add the `globset` dependency.
- **Modify** `crates/extensions/src/lib.rs` — declare/re-export the module; add `permissions: PermissionRules` to `Extensions`; read+union the rules in `discover()`.
- **Create** `crates/engine/src/policy_gate.rs` — the `PolicyGate` `PermissionGate` decorator. One responsibility: apply parsed rules over the base gate with the right precedence.
- **Modify** `crates/engine/src/lib.rs` — declare/re-export `PolicyGate`; add `build_tool_registry_with_permissions`; thread `permissions: Option<&PermissionRules>` through `build_tool_registry_inner`.
- **Modify** `crates/engine/src/main.rs` — thread `permissions` into `build_tools_preferring_mcp`; reorder `discover()` before the registry build in `cmd_run` and pass `&ext.permissions`.

---

## Task 1: Rule types, alias map, and `parse_permissions`

**Files:**
- Modify: `crates/extensions/Cargo.toml`
- Create: `crates/extensions/src/permission_def.rs`

- [ ] **Step 1: Add the `globset` dependency**

In `crates/extensions/Cargo.toml`, under `[dependencies]` (after the `serde_json.workspace = true` line), add:

```toml
globset = "0.4"
```

- [ ] **Step 2: Write the failing test for parsing + alias mapping**

Create `crates/extensions/src/permission_def.rs` with the types and an empty parser, plus the tests. (The matcher is added in Task 2; this task only proves parsing populates the buckets and normalizes tool names. We assert through `decision()` so we never touch private fields.)

```rust
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
    /// `:*` form (prefix match); without it the command must match exactly.
    CmdPrefix { prefix: String, wildcard: bool },
}

/// Map a Claude Code tool name to its otto equivalent. otto-native and unknown names pass through
/// verbatim, so a future otto tool name works without a code change.
fn normalize_tool(name: &str) -> String {
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
        let pat = inner.strip_prefix("./").unwrap_or(inner).to_string();
        globset::Glob::new(&pat).ok()?; // reject an uncompilable glob
        Some(Specifier::PathGlob(pat))
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

    /// The verdict for a proposed call, or `None` when no rule matches. Added in Task 2.
    pub fn decision(&self, _tool: &str, _args: &Value) -> Option<Decision> {
        None
    }
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

    // Placeholder: decision() returns None until Task 2 wires matching.
    #[test]
    fn decision_is_none_before_matcher() {
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Read(src/**)"] } }"#);
        assert_eq!(rules.decision("fs.read", &json!({"path": "src/a.rs"})), None);
    }
}
```

- [ ] **Step 3: Declare the module so it compiles**

In `crates/extensions/src/lib.rs`, add to the `mod` block (alphabetically, after `mod marketplace_def;` / wherever fits the existing ordering near line 8-19):

```rust
mod permission_def;
```

And add to the `pub use` block (near line 21-34):

```rust
pub use permission_def::{PermissionRules, parse_permissions};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions permission_def`
Expected: PASS (6 tests). The `decision_is_none_before_matcher` test documents the temporary stub.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/Cargo.toml crates/extensions/src/permission_def.rs crates/extensions/src/lib.rs
git commit -m "feat(extensions): parse settings.json permissions rules (alias map + Tool(specifier))"
```

---

## Task 2: The `decision` matcher (path globs, bash prefixes, precedence)

**Files:**
- Modify: `crates/extensions/src/permission_def.rs`

- [ ] **Step 1: Write the failing tests for matching + precedence**

In `crates/extensions/src/permission_def.rs`, replace the two placeholder tests (`decision_is_none_before_matcher` and `bare_rule_has_no_specifier`'s smoke note can stay) by appending these tests to the `tests` module:

```rust
    #[test]
    fn path_glob_matches_path_args() {
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Read(src/**)"] } }"#);
        assert_eq!(
            rules.decision("fs.read", &json!({"path": "src/a.rs"})),
            Some(Decision::Deny)
        );
        // Non-matching path → no rule applies.
        assert_eq!(rules.decision("fs.read", &json!({"path": "docs/a.md"})), None);
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
    }

    #[test]
    fn bare_tool_rule_matches_any_args() {
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Read"] } }"#);
        assert_eq!(
            rules.decision("fs.read", &json!({"path": "anything.rs"})),
            Some(Decision::Deny)
        );
        // Different tool → no match.
        assert_eq!(rules.decision("fs.write", &json!({"path": "anything.rs"})), None);
    }

    #[test]
    fn bash_prefix_exact_vs_wildcard() {
        let wild = parse_permissions(r#"{ "permissions": { "allow": ["Bash(cargo test:*)"] } }"#);
        assert_eq!(
            wild.decision("bash", &json!({"command": "cargo test --all"})),
            Some(Decision::Allow)
        );
        assert_eq!(wild.decision("bash", &json!({"command": "cargo build"})), None);

        let exact = parse_permissions(r#"{ "permissions": { "allow": ["Bash(cargo test)"] } }"#);
        assert_eq!(
            exact.decision("bash", &json!({"command": "cargo test"})),
            Some(Decision::Allow)
        );
        // Exact rule does not match a longer command.
        assert_eq!(exact.decision("bash", &json!({"command": "cargo test --all"})), None);
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
```

Also delete the now-obsolete `decision_is_none_before_matcher` test.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions permission_def`
Expected: FAIL — `decision` still returns `None`, so the new assertions fail.

- [ ] **Step 3: Implement the matcher**

In `crates/extensions/src/permission_def.rs`, replace the stub `decision` method body and add the helpers. Replace:

```rust
    /// The verdict for a proposed call, or `None` when no rule matches. Added in Task 2.
    pub fn decision(&self, _tool: &str, _args: &Value) -> Option<Decision> {
        None
    }
```

with:

```rust
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
```

Add an `impl Rule` block (place it after the `Rule` struct definition, above `enum Specifier` or anywhere at module scope):

```rust
impl Rule {
    /// True if this rule governs `(tool, args)`.
    fn matches(&self, tool: &str, args: &Value) -> bool {
        if self.tool != tool {
            return false;
        }
        match &self.spec {
            None => true,
            Some(Specifier::PathGlob(pat)) => {
                let Ok(glob) = globset::Glob::new(pat) else {
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
                    Some(cmd) if *wildcard => cmd.starts_with(prefix),
                    Some(cmd) => cmd == prefix,
                    None => false,
                }
            }
        }
    }
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions permission_def`
Expected: PASS (all parsing + matching tests).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/permission_def.rs
git commit -m "feat(extensions): permission rule matcher (path globs, bash prefixes, deny>ask>allow)"
```

---

## Task 3: Thread permissions into `Extensions` + `discover`

**Files:**
- Modify: `crates/extensions/src/lib.rs`

- [ ] **Step 1: Write the failing hermetic discovery test**

In `crates/extensions/src/lib.rs`'s `#[cfg(test)] mod tests`, add (the existing tests use `tempfile` and write `.claude/settings.json`; follow that pattern):

```rust
    #[test]
    fn discovers_and_unions_permissions_across_bases() {
        use std::fs;
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let home_claude = home.path().join(".claude");
        let proj_claude = proj.path().join(".claude");
        fs::create_dir_all(&home_claude).unwrap();
        fs::create_dir_all(&proj_claude).unwrap();
        fs::write(
            home_claude.join("settings.json"),
            r#"{ "permissions": { "allow": ["Read(src/**)"] } }"#,
        )
        .unwrap();
        fs::write(
            proj_claude.join("settings.json"),
            r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#,
        )
        .unwrap();

        let ext = discover(proj.path(), home.path());
        assert!(!ext.permissions.is_empty());
        // user allow + project deny are both present (unioned).
        assert_eq!(
            ext.permissions
                .decision("fs.read", &serde_json::json!({"path": "src/a.rs"})),
            Some(otto_engine_core::tool::Decision::Allow)
        );
        assert_eq!(
            ext.permissions
                .decision("fs.write", &serde_json::json!({"path": "dist/x"})),
            Some(otto_engine_core::tool::Decision::Deny)
        );
    }

    #[test]
    fn no_permissions_block_yields_empty() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let ext = discover(proj.path(), home.path());
        assert!(ext.permissions.is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions discovers_and_unions_permissions`
Expected: FAIL with "no field `permissions` on type `Extensions`".

- [ ] **Step 3: Add the field, the reader, and the union in `discover`**

In `crates/extensions/src/lib.rs`, add the field to `Extensions` (after `mcp_servers`, near line 46):

```rust
    pub permissions: PermissionRules,
```

In `discover()`, add the accumulator next to the other ones (near line 62-63):

```rust
    let mut permissions = PermissionRules::default();
```

Inside the `for base in [home, project_root]` loop, after the hooks read (near line 78), add:

```rust
        permissions.extend(read_settings_permissions(&claude.join("settings.json")));
```

Add the field to the returned `Extensions { ... }` literal (near line 89-95):

```rust
        permissions,
```

Add the reader helper near `read_settings_hooks`/`read_enabled_plugins` (near line 244-273):

```rust
/// Read `<base>/.claude/settings.json` and parse its `permissions` block. Missing/unreadable →
/// empty (never fatal), matching every other `.claude/` reader.
fn read_settings_permissions(path: &Path) -> PermissionRules {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_permissions(&text),
        Err(_) => PermissionRules::default(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions`
Expected: PASS — the new discovery tests pass and every existing extensions test still passes (the new `permissions` field defaults to empty everywhere, and `Extensions` still derives `Default`/`PartialEq`/`Eq` because `PermissionRules` does).

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/lib.rs
git commit -m "feat(extensions): discover + union settings.json permissions across user/project"
```

---

## Task 4: The `PolicyGate` decorator (engine)

**Files:**
- Create: `crates/engine/src/policy_gate.rs`
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Write the failing tests for the gate**

Create `crates/engine/src/policy_gate.rs`:

```rust
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
        // An allow rule naming a sensitive path must NOT pierce the floor.
        let g = gate(r#"{ "permissions": { "allow": ["Read(.env)"] } }"#, true);
        assert_eq!(g.evaluate("fs.read", &json!({"path": ".env"})), Decision::Deny);
    }

    #[test]
    fn bash_with_no_rule_follows_sandbox_flag() {
        let g = gate("{}", true);
        assert_eq!(g.evaluate("bash", &json!({"command": "ls"})), Decision::Allow);
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
        // A non-matching write is still allowed by the base gate.
        assert_eq!(
            g.evaluate("fs.write", &json!({"path": "src/x"})),
            Decision::Allow
        );
    }

    #[test]
    fn ask_rule_on_bash_returns_ask() {
        // A rule-driven Ask (paired with DenyAsk in the registry) fails closed in headless.
        let g = gate(r#"{ "permissions": { "ask": ["Bash(git push:*)"] } }"#, true);
        assert_eq!(
            g.evaluate("bash", &json!({"command": "git push origin"})),
            Decision::Ask
        );
    }

    #[test]
    fn allow_rule_upgrades_bash_when_sandbox_absent() {
        // Even without a sandbox flag, an explicit allow rule wins (rules are checked before the
        // step-5 sandbox upgrade).
        let g = gate(r#"{ "permissions": { "allow": ["Bash(cargo test:*)"] } }"#, false);
        assert_eq!(
            g.evaluate("bash", &json!({"command": "cargo test --all"})),
            Decision::Allow
        );
    }
}
```

- [ ] **Step 2: Declare the module and run the tests to verify they fail**

In `crates/engine/src/lib.rs`, add to the `mod` block (after `mod mcp;` near line 24):

```rust
mod policy_gate;
```

And to the `pub use` area (after `pub use approval::ApprovalModeGate;` near line 28):

```rust
pub use policy_gate::PolicyGate;
```

Run: `cargo test -p otto-engine policy_gate`
Expected: PASS immediately — `PolicyGate` and its tests are self-contained (no other wiring needed yet). If a compile error about `otto_tools::DefaultPermissionGate` appears, confirm the import path matches `crates/engine/src/approval.rs`'s test (it uses `otto_tools::DefaultPermissionGate`).

(This task is test-first but the implementation and tests are written together because the gate is a single small unit; the assertions encode each precedence rule.)

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/policy_gate.rs crates/engine/src/lib.rs
git commit -m "feat(engine): PolicyGate — apply permission rules over the floor (deny>ask>allow + bash)"
```

---

## Task 5: Registry builder that uses the PolicyGate

**Files:**
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Write the failing test for the new builder**

In `crates/engine/src/lib.rs`'s `#[cfg(test)] mod tests` (near line 272), add. (Uses `LocalWorkspace`; check an existing test in this module for the exact constructor + imports and mirror them.)

```rust
    #[tokio::test]
    async fn registry_with_permissions_denies_matched_write() {
        use otto_extensions::parse_permissions;
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path().to_path_buf()));
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#);
        let reg = build_tool_registry_with_permissions(ws, dir.path().to_path_buf(), &rules);

        // A write to a denied path is rejected by the gate before dispatch.
        let err = reg
            .call("fs.write", json!({"path": "dist/x.txt", "content": "hi"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("denied by permission gate"));

        // An unmatched write is permitted.
        assert!(
            reg.call("fs.write", json!({"path": "src/x.txt", "content": "hi"}))
                .await
                .is_ok()
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine registry_with_permissions`
Expected: FAIL with "cannot find function `build_tool_registry_with_permissions`".

- [ ] **Step 3: Add the builder and thread permissions through `build_tool_registry_inner`**

In `crates/engine/src/lib.rs`, add the import near the top `use` block (with the other `otto_extensions`/`otto_engine_core` uses):

```rust
use otto_extensions::PermissionRules;
```

Add the public builder next to `build_tool_registry_approving` (near line 104-106):

```rust
/// Build the tool registry with a `PolicyGate` applying `permissions` over the default gate.
/// Used by the `otto run` spine when `.claude/settings.json` declares any permission rules; the
/// PolicyGate owns the bash decision, so it pairs with a plain `DenyAsk` resolver.
pub fn build_tool_registry_with_permissions(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    permissions: &PermissionRules,
) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, false, Some(permissions))
}
```

Change the two existing wrappers to pass `None`. Replace:

```rust
pub fn build_tool_registry(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, false)
}
```

with:

```rust
pub fn build_tool_registry(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, false, None)
}
```

and replace:

```rust
pub fn build_tool_registry_approving(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, true)
}
```

with:

```rust
pub fn build_tool_registry_approving(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, true, None)
}
```

Update `build_tool_registry_inner`'s signature and gate/resolver selection. Replace the signature line and the gate/ask block (lines 115-134):

```rust
fn build_tool_registry_inner(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> ToolRegistry {
    let sandboxed = os_sandbox_available();
    // NB: the ask-resolver only ever auto-allows `bash`. An `Ask` on `fs.write` (approval mode)
    // is resolved by the orchestrator's `Approver`, never here — so writes can't slip through.
    let ask: Arc<dyn AskResolver> = if sandboxed {
        Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
    } else {
        Arc::new(DenyAsk)
    };

    let base_gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());
    let gate: Arc<dyn PermissionGate> = if approve_edits {
        Arc::new(ApprovalModeGate::new(base_gate))
    } else {
        base_gate
    };
```

with:

```rust
fn build_tool_registry_inner(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
    permissions: Option<&PermissionRules>,
) -> ToolRegistry {
    let sandboxed = os_sandbox_available();
    let base_gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());

    // When permission rules exist, the PolicyGate owns every verdict (incl. bash), so it pairs
    // with a plain DenyAsk. Otherwise the wiring is exactly as before: the bash allow-list
    // resolver auto-allows the structurally-Asked sandboxed bash, and approval mode (serve) may
    // upgrade fs.write. (PolicyGate × ApprovalModeGate composition is a deferred serve-path slice,
    // so `permissions` is only ever Some on the non-approving run path.)
    let (gate, ask): (Arc<dyn PermissionGate>, Arc<dyn AskResolver>) = match permissions {
        Some(rules) if !rules.is_empty() => (
            Arc::new(PolicyGate::new(base_gate, rules.clone(), sandboxed)),
            Arc::new(DenyAsk),
        ),
        _ => {
            let ask: Arc<dyn AskResolver> = if sandboxed {
                Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
            } else {
                Arc::new(DenyAsk)
            };
            let gate: Arc<dyn PermissionGate> = if approve_edits {
                Arc::new(ApprovalModeGate::new(base_gate))
            } else {
                base_gate
            };
            (gate, ask)
        }
    };
```

(The rest of `build_tool_registry_inner` — `ToolRegistry::new(gate, ask)` and tool registration — is unchanged. `PolicyGate` is in scope via the `pub use policy_gate::PolicyGate;` from Task 4; reference it as `PolicyGate` since this is the same crate.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine registry_with_permissions`
Expected: PASS.

Then confirm nothing regressed: `cargo test -p otto-engine`
Expected: PASS (the two existing wrappers now pass `None` and behave identically).

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/lib.rs
git commit -m "feat(engine): build_tool_registry_with_permissions wires the PolicyGate"
```

---

## Task 6: Wire permissions into the `otto run` spine

**Files:**
- Modify: `crates/engine/src/main.rs`

- [ ] **Step 1: Write the failing integration test**

`crates/engine/src/main.rs` already has hermetic `cmd_run`-style tests building a registry from a tempdir `.claude/` (see the test near line 737 that calls `otto_extensions::discover` + `build_tool_registry`). Add a test in that module that exercises the new path. Mirror the existing test's imports/setup; the key new assertions:

```rust
    #[tokio::test]
    async fn run_path_registry_applies_discovered_permissions() {
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#,
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.permissions.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let reg = if !ext.permissions.is_empty() {
            otto_engine::build_tool_registry_with_permissions(
                ws,
                proj.path().to_path_buf(),
                &ext.permissions,
            )
        } else {
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf())
        };

        let err = reg
            .call("fs.write", json!({"path": "dist/x.txt", "content": "hi"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("denied by permission gate"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine run_path_registry_applies_discovered_permissions`
Expected: FAIL — until Step 3 the `build_tools_preferring_mcp` signature/`cmd_run` ordering isn't updated; this standalone test actually passes already (it inlines the wiring), so first confirm it PASSES, then Step 3 makes the *real* `cmd_run` use that same wiring. If it fails to compile, fix imports to match the sibling test.

(Note: this test verifies the builder selection logic. Step 3 applies the identical selection inside the real `cmd_run`/`build_tools_preferring_mcp` so production matches the test.)

- [ ] **Step 3: Thread `permissions` into `build_tools_preferring_mcp` and reorder `discover` in `cmd_run`**

In `crates/engine/src/main.rs`, change `build_tools_preferring_mcp`'s signature (line 155-159) and its first statement (line 160-164). Replace:

```rust
async fn build_tools_preferring_mcp(
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
    let mut registry = if approve_edits {
        otto_engine::build_tool_registry_approving(tools_workspace, root.clone())
    } else {
        build_tool_registry(tools_workspace, root.clone())
    };
```

with:

```rust
async fn build_tools_preferring_mcp(
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
    permissions: &otto_extensions::PermissionRules,
) -> (ToolRegistry, Vec<McpConnection>) {
    let mut registry = if !permissions.is_empty() {
        // Permission rules override the default gate with a PolicyGate (run path only; not
        // composed with approve_edits this slice).
        otto_engine::build_tool_registry_with_permissions(tools_workspace, root.clone(), permissions)
    } else if approve_edits {
        otto_engine::build_tool_registry_approving(tools_workspace, root.clone())
    } else {
        build_tool_registry(tools_workspace, root.clone())
    };
```

Now update the four call sites:

In `cmd_run` (lines ~282-286), reorder so `discover` runs first and feeds the builder. Replace:

```rust
    let (mut tools, mut mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), false).await;
    // mcp_conns is held until end of function so the mcp children stay alive.
    // Register discovered skills as the gated `skill` tool so spine agents can load them mid-turn.
    let ext = otto_extensions::discover(&root, &home_dir());
```

with:

```rust
    // Discover extensions first: the permission rules are needed at registry-construction time so
    // the gate can be a PolicyGate.
    let ext = otto_extensions::discover(&root, &home_dir());
    let (mut tools, mut mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), false, &ext.permissions).await;
    // mcp_conns is held until end of function so the mcp children stay alive.
    // Register discovered skills as the gated `skill` tool so spine agents can load them mid-turn.
```

For the three deferred call sites (the `--agent` subpath near line 377, the `--command` subpath near line 431, and the serve/approve path near line 537), pass an empty rule set to preserve current behavior. At each, replace the trailing `false).await` / `approve_edits).await` argument list by adding `, &otto_extensions::PermissionRules::default()` before `).await`:

- Line ~377: `build_tools_preferring_mcp(tools_ws, root, false).await;`
  → `build_tools_preferring_mcp(tools_ws, root, false, &otto_extensions::PermissionRules::default()).await;`
- Line ~431: `build_tools_preferring_mcp(tools_workspace, root.clone(), false).await;`
  → `build_tools_preferring_mcp(tools_workspace, root.clone(), false, &otto_extensions::PermissionRules::default()).await;`
- Line ~537: `build_tools_preferring_mcp(tools_workspace, root.clone(), approve_edits).await;`
  → `build_tools_preferring_mcp(tools_workspace, root.clone(), approve_edits, &otto_extensions::PermissionRules::default()).await;`

(These three subpaths — `--agent`, `--command`, and the approve/serve path — defer permission wiring exactly as prior slices deferred their non-spine subpaths.)

- [ ] **Step 4: Run the build and the full suite**

Run: `cargo build --workspace`
Expected: SUCCESS (all four call sites updated; no signature mismatch).

Run: `cargo test -p otto-engine`
Expected: PASS, including `run_path_registry_applies_discovered_permissions`.

Run: `cargo test --workspace`
Expected: PASS — the offline determinism suite is untouched (no `.claude/permissions` in those fixtures, so the registry is built exactly as before).

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): wire discovered permissions into the otto run spine"
```

---

## Task 7: Final verification + docs

**Files:**
- Modify: `CLAUDE.md` (extensions crate row — note the permissions slice)

- [ ] **Step 1: Run fmt, clippy, and the full suite**

Run: `cargo fmt --all`
Run: `cargo clippy --workspace --all-targets`
Expected: no warnings introduced by the new code.

Run: `cargo test --workspace`
Expected: PASS.

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: SUCCESS (the UI shares only `protocol`, which is untouched — this confirms no accidental cross-dependency).

- [ ] **Step 2: Manual smoke test of the run path**

```bash
mkdir -p /tmp/otto-perms-demo/.claude && cd /tmp/otto-perms-demo
printf '{ "permissions": { "deny": ["Write(secret/**)"] } }' > .claude/settings.json
OTTO_TOKEN= cargo run -p otto-engine --manifest-path /home/robhicks/dev/otto-next/Cargo.toml -- run "say hi" --root /tmp/otto-perms-demo
```
Expected: the run completes on the offline deterministic path; no panic. (This confirms the PolicyGate wiring doesn't disturb a normal run. A write to `secret/**` would be denied if the spine attempted one.)

- [ ] **Step 3: Update the `extensions` row in CLAUDE.md**

In `CLAUDE.md`, find the `extensions` crate table row and append a sentence describing slice 6 after the plugins (Plan B) description, in the same style as the existing slice notes. Use this text:

```
Slice 6 adds permissions: discovery reads the `settings.json` `permissions` block (Claude Code `allow`/`deny`/`ask` `Tool(specifier)` rules) across `~/.claude` + project (unioned), parses them (tool-name alias map — `Read`/`Edit`/`Write`→`fs.read`/`fs.write`, `Bash`→`bash`, etc. — accepting otto-native names too; path-glob and bash command-prefix `:*` specifiers) into a pure `PermissionRules`, and composes them into the gate via a new `PolicyGate` (engine) that layers deny>ask>allow over the inviolable sensitive-path floor and owns the sandboxed-`bash` Allow (retiring the hardcoded bash allow-list resolver whenever rules are present). Wired into the `otto run` spine only (inserted only when rules exist, so a workspace with no `permissions` is byte-for-byte unchanged); per-artifact `allowed-tools` enforcement, `model` routing, and serve-path/`--command`/`--agent` wiring remain deferred.
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: note extensions permissions slice in CLAUDE.md"
```

---

## Spec coverage check

- `parse_permissions` + alias map + `Tool(specifier)` grammar (path globs, bash `:*` prefix) — Tasks 1–2.
- `PermissionRules::decision` with deny > ask > allow — Task 2.
- `Extensions.permissions` + `discover` union across bases — Task 3.
- `PolicyGate` precedence (floor inviolable, rule application, bash step-5 upgrade) — Task 4.
- Registry builder pairing PolicyGate with `DenyAsk`; existing wrappers unchanged — Task 5.
- `cmd_run` run-path wiring, deferred subpaths pass empty rules, no-permissions byte-for-byte unchanged — Task 6.
- Determinism suite + fmt/clippy + UI wasm build + doc update — Task 7.

Deferred per spec (no task, intentional): per-artifact `allowed-tools` enforcement, `model` routing, serve-path wiring (incl. `PolicyGate × ApprovalModeGate`), `settings.local.json`, `defaultMode`, `additionalDirectories`, plugin-contributed permissions.
