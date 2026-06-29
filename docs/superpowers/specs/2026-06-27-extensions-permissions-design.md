# otto Extensions Slice 6 Design — permissions (`settings.json` `permissions` → the gate)

**Status:** Approved design.
**Date:** 2026-06-27.

## Why this document

`ARCHITECTURE.md` ("Claude Code compatibility") describes one `extensions` crate that discovers
`.claude/` (project) and `~/.claude/` (user-global) and registers each artifact — agents, commands,
skills, hooks, plugins, **permissions** — into an existing otto primitive. Slices 1–5 shipped agents,
commands, skills, hooks, and plugins (marketplace discovery + bundled MCP servers). Across every one of
those slices the same two fields were **parsed but left inert**: a command/skill/agent `allowed-tools`
list, and the `settings.json` `permissions` block. Each slice's "what this unblocks" section named the
same remaining work: *"`settings.json` permissions composed into the Layer-2 gate, where
command/skill/agent `allowed-tools` stops being inert."*

This is **slice 6**: the `settings.json` `permissions` block, composed into otto's permission gate. It
is the foundational half — a global, user/project-level allow/deny/ask policy layered over the inviolable
sensitive-path floor. Per-artifact `allowed-tools` enforcement (the narrowing a *specific* command or
skill invocation applies) is a smaller follow-on that composes on top of this layer; it is deferred to a
later slice so this one stays a single reviewable plan.

## Scope

Build, end to end, the **global permission policy** from `settings.json`:

1. `extensions` additions:
   - `permission_def.rs`: `parse_permissions(settings_json) -> PermissionRules` — the typed
     `allow`/`deny`/`ask` rule sets, the Claude-Code `Tool(specifier)` parser, the tool-name alias map,
     and a pure `PermissionRules::decision(tool, args) -> Option<Decision>` matcher.
   - `Extensions` gains a `permissions: PermissionRules` field; `discover()` reads and unions the rules
     across the user then project `settings.json` (the same file it already reads for hooks /
     `enabledPlugins`).
2. Engine wiring:
   - `engine/src/policy_gate.rs`: `PolicyGate` — a `PermissionGate` decorator over
     `DefaultPermissionGate` that applies the parsed rules with the correct precedence and preserves the
     inviolable floor.
   - `cmd_run` inserts the `PolicyGate` (paired with `DenyAsk`) **only when at least one rule exists**;
     with no `permissions` configured the tool registry is built exactly as today.

### Out of scope this slice (deferred, consistent with prior slices)

- **Per-artifact `allowed-tools` enforcement** for commands/skills. Agents already narrow the shared gate
  via `ToolRegistry::subset` (slice 1); making a command's/skill's `allowed-tools` narrow the gate for the
  duration of that invocation is a follow-on slice that composes on top of this global layer. The fields
  stay parsed-but-inert for commands/skills until then.
- **`model` routing.** A command/agent `model` field is a *routing* concern (provider selection), not a
  gate one. It remains parsed-but-inert; routing it lives on the router axis, not here.
- **Serve-path wiring.** Like every prior extensions slice, permissions wire into `otto run` only.
  Composing `PolicyGate` with the serve-only `ApprovalModeGate` (and the `--approve-edits` interactive
  approver) is part of the deferred cross-cutting serve-path slice.
- **`settings.local.json`**, `permissions.defaultMode`, and `permissions.additionalDirectories`. The
  first follows slice 4's `settings.json`-only posture; `defaultMode` overlaps the edit-approval default
  (serve / `--approve-edits`); `additionalDirectories` is workspace-containment, owned by
  `LocalWorkspace`, not the gate.
- **Plugin-contributed permissions.** Only user/project `settings.json` is read — a bundled plugin cannot
  grant itself permissions.

## Design

### `PermissionRules` + `parse_permissions` (`crates/extensions/src/permission_def.rs`)

```rust
pub struct PermissionRules {
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    ask: Vec<Rule>,
}

struct Rule {
    /// The matched tool, normalized to an otto tool name (see the alias map).
    tool: String,
    /// `None` ⇒ the rule matches the tool regardless of arguments.
    spec: Option<Specifier>,
}

enum Specifier {
    /// A gitignore-style path glob (stored raw and compiled at match time, so `Specifier`
    /// can derive `Clone`/`Eq` — `globset::GlobMatcher` is neither), matched against the
    /// call's path argument(s).
    PathGlob(String),
    /// A bash command-prefix match against the `command` argument. `wildcard` is the trailing
    /// `:*` form (prefix match); without it the command must match exactly.
    CmdPrefix { prefix: String, wildcard: bool },
}

pub fn parse_permissions(settings_json: &str) -> PermissionRules;
```

The Claude Code shape in `settings.json`:

```jsonc
{
  "permissions": {
    "allow": ["Bash(cargo test:*)", "Read(src/**)"],
    "deny":  ["Bash(curl:*)", "Write(dist/**)"],
    "ask":   ["Bash(git push:*)"]
  }
}
```

Parsing rules:

- A rule string is `Tool` or `Tool(specifier)`. `"Read"` → tool only (matches any `fs.read`);
  `"Read(src/**)"` → tool + path-glob specifier; `"Bash(cargo test:*)"` → tool + command-prefix.
- **Malformed JSON**, a missing `permissions` object, or unparseable individual rules → those pieces are
  dropped; an entirely-missing/invalid block yields an **empty** `PermissionRules`. A single bad rule is
  skipped, never fatal (the prior slices' posture).
- Unknown keys inside `permissions` (`defaultMode`, `additionalDirectories`, …) are ignored.

### Tool-name alias map (accept both Claude Code and otto names)

Claude Code rules name Claude's tools; otto's tools have different names. `parse_permissions` normalizes
every rule's tool to its otto name so real `.claude/` configs match, while otto-native names pass through:

| Claude Code name        | otto tool   |
|-------------------------|-------------|
| `Read`                  | `fs.read`   |
| `Edit` / `Write` / `MultiEdit` | `fs.write` |
| `Bash`                  | `bash`      |
| `Grep`                  | `grep`      |
| `Glob` / `LS`           | `fs.list`   |
| `fs.read` / `fs.write` / `bash` / `grep` / `git.*` (otto-native) | unchanged |

Because `Edit`, `Write`, and `MultiEdit` all map to otto's single `fs.write`, one such rule governs every
otto write. A name with no mapping is kept verbatim (so a future otto tool name works without a code
change); it simply won't match Claude Code's spelling.

### Specifier matching (`PermissionRules::decision`)

`decision(tool, args) -> Option<Decision>` returns `Some(Decision)` for the highest-precedence matching
rule and `None` when no rule matches the call. Matching:

- **Path tools** (`fs.read` / `fs.write` / `fs.list` / `grep`): a `PathGlob` specifier is matched against
  the candidate path argument(s) — `path`, each of `paths[]`, and `glob` (the same shapes
  `DefaultPermissionGate` already inspects). Leading `./` is tolerated; `*` / `**` follow gitignore-glob
  semantics via `globset` (already a workspace dependency). A bare tool rule (no specifier) matches any
  call to that tool.
- **`bash`**: a `CmdPrefix` specifier is matched against the `command` argument. `cargo test:*` matches
  any command beginning with `cargo test`; `cargo test` (no `:*`) matches that exact command. A bare
  `Bash` rule matches every bash call.

A rule whose specifier shape doesn't fit the tool (e.g. a `PathGlob` on `bash`) simply never matches.

### `PolicyGate` — composition & precedence (`crates/engine/src/policy_gate.rs`)

A decorator over the base gate, mirroring the existing `ApprovalModeGate` pattern so `engine-core` stays
free of any concrete policy type:

```rust
pub struct PolicyGate {
    inner: Arc<dyn PermissionGate>,   // DefaultPermissionGate
    rules: PermissionRules,
    bash_allowed: bool,               // = os_sandbox_available()
}

impl PermissionGate for PolicyGate {
    fn evaluate(&self, tool: &str, args: &Value) -> Decision {
        // 1. Sensitive-path floor is inviolable — no allow rule can pierce it.
        let base = self.inner.evaluate(tool, args);
        if base == Decision::Deny {
            return Decision::Deny;
        }
        // 2–4. Rules, in Claude Code precedence: deny > ask > allow.
        if let Some(d) = self.rules.decision(tool, args) {
            return d;
        }
        // 5. No rule matched → base, except bash's structural Ask is upgraded to Allow when a
        //    sandbox backend exists (preserving today's "all sandboxed bash runs" default).
        if tool == "bash" && base == Decision::Ask && self.bash_allowed {
            return Decision::Allow;
        }
        base
    }
}
```

`rules.decision` itself encodes deny > ask > allow: it checks the `deny` set first, then `ask`, then
`allow`, returning the first match.

Two consequences worth calling out:

- **The gate becomes the single authority for bash.** Step 5 folds in the structural "all sandboxed bash
  is allowed" behavior that the hardcoded `AllowListAskResolver(vec!["bash"])` provides today. So whenever
  the `PolicyGate` is active it is paired with a plain `DenyAsk` resolver — the allow-list resolver is
  *replaced*, not stacked. A rule-driven `Ask` (step 3) on bash therefore returns `Ask` → `DenyAsk` →
  denied in headless `otto run` (which has no human to ask), the correct fail-closed result, instead of
  being silently auto-allowed by the old bash allow-list.
- **No-rules is byte-for-byte unchanged.** With an empty `PermissionRules`, `decision` always returns
  `None`; step 5 reproduces today's bash handling and step 1/base reproduce everything else. So inserting
  the `PolicyGate` only when rules exist is purely additive — but even when inserted with zero effective
  rules it would behave identically.

### Engine `cmd_run` wiring (`crates/engine/src/main.rs`, `crates/extensions/src/lib.rs`)

- `Extensions` gains `pub permissions: PermissionRules`. `discover()` reads `parse_permissions` from
  `<home>/.claude/settings.json` then `<project>/.claude/settings.json` and **unions** the three rule
  sets. Because deny is checked first and wins, union order is safe regardless of base; `home` stays an
  explicit parameter so discovery is hermetic.
- `cmd_run`: after building the workspace/tools, if `!ext.permissions.is_empty()`, construct the registry
  with `PolicyGate::new(Arc::new(DefaultPermissionGate::new()), ext.permissions, os_sandbox_available())`
  and a `DenyAsk` resolver; otherwise build it exactly as today (`build_tool_registry`). The fs/bash tool
  registration itself is unchanged — `bash` is still registered only when `os_sandbox_available()`, and
  `bash_allowed` is wired from the same call so the two can't drift.

With no `.claude/settings.json` `permissions`, `ext.permissions.is_empty()` is true and the registry is
the one shipped today — the offline determinism suite is untouched.

## Security & determinism properties

- **Floor inviolable.** Step 1 returns the base `Deny` for any sensitive path before any rule is consulted
  — an `allow` rule can never reach `.env*` / `.ssh/` / `.git/` / `.aws/` / ssh keys.
- **Deny > ask > allow.** Matches Claude Code; a `deny` rule beats an overlapping `allow`.
- **Rule-driven `Ask` fails closed in headless.** Paired with `DenyAsk`, a forced `Ask` denies in
  `otto run` rather than auto-allowing — the existing headless safety posture, now extended to bash.
- **Additive / no implicit widening.** With no `permissions` block the tool set and every verdict are
  byte-for-byte unchanged. The `PolicyGate` can only be *inserted*; it never relaxes the base gate beyond
  the bash-when-sandboxed upgrade that already exists today.
- **Hermetic + deterministic.** `home` is an explicit parameter; `extensions` spawns nothing (rules are
  pure data); rule matching does no I/O. The offline determinism suite stays green.
- **No self-granting plugins.** Only user/project `settings.json` is read; bundled-plugin permissions are
  out of scope.
- **`Bash(...)` rules do not constrain otto's native `git.*` tools.** A ported Claude Code config
  typically limits git via `Bash(git push:*)`, which maps to otto's `bash`. otto *also* exposes
  native `git.*` MCP tools (structured calls, not bash commands) that a `Bash(...)` rule never
  matches — to constrain those, write explicit `git.*` rules (otto-native names pass through the
  alias map unchanged). This is a consequence of the alias model, surfaced here so a user writing
  Claude-Code-style rules isn't lulled into a false sense of coverage.
- **Rule path-globs match literally, not path-normalized.** A `deny` like `Write(dist/**)` matches
  the candidate path as given; a non-normalized path such as `foo/../dist/x` is not canonicalized
  before matching, so rule-level path denies are best-effort (the same semantics as Claude Code).
  The inviolable sensitive-path floor is unaffected — it is component-based (`is_sensitive`), not
  glob-based — so this is a precision limitation of user rules, never a floor bypass.

## Testing

- **`parse_permissions`** (pure): full `permissions` block → populated allow/deny/ask; `Tool` vs
  `Tool(specifier)` parsing; the alias map (`Read→fs.read`, `Edit`/`Write`/`MultiEdit→fs.write`,
  `Bash→bash`, `Grep→grep`, `Glob`/`LS→fs.list`, otto-native pass-through); missing/invalid block →
  empty; a single malformed rule skipped while siblings survive; unknown keys ignored.
- **`PermissionRules::decision`** (pure): path-glob match against `path`/`paths[]`/`glob` (incl. `**`,
  leading `./`); bash `CmdPrefix` exact vs `:*` wildcard; bare tool rule matches any args; deny > ask >
  allow precedence on overlapping rules; no match → `None`.
- **`PolicyGate`** (engine): floor `Deny` beats an `allow` rule (sensitive path still denied); bash with
  no rule + `bash_allowed=true` → `Allow`, with `bash_allowed=false` → `Ask`; an `ask` rule on bash →
  `Ask` (→ `DenyAsk` denies); a `deny` rule → `Deny`; an `allow` rule upgrades an otherwise-`Ask`.
- **discovery / union** (hermetic `home`+`project` tempdirs): rules from both `settings.json` files are
  unioned; a project `deny` is honored alongside a user `allow`; missing files → empty.
- **engine `cmd_run`** (hermetic `home`): with a `permissions` block under a tempdir `.claude/`, the
  registry's verdicts reflect the rules; with no `permissions`, the registry is identical to today and the
  offline determinism suite stays green.

## What this unblocks

With the global policy layer in place, the remaining `extensions` permission surface is **per-artifact
`allowed-tools` enforcement** — making a command's or skill's declared `allowed-tools` narrow the gate for
the duration of that invocation, composing on top of this layer the way agents already narrow via
`subset`. The cross-cutting **serve-path wiring** (this gate, plus all prior slices' artifacts, threaded
through `otto serve`, including `PolicyGate × ApprovalModeGate` composition) and **`model` routing**
remain the other two open extensions threads.
