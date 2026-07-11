# Plugin MCP tool enforcement (hook-wrapping + `mcp__` addressing) — Design

**Date:** 2026-07-10
**Status:** Approved (brainstorm), pending implementation plan

## Problem

Bundled plugin MCP servers register their tools behind the gate under the internal name
`plugin__<plugin>__<serverkey>__<tool>` (see `crates/engine/src/mcp.rs:plugin_gate_name`). Two
enforcement gaps remain from the Slice-5 Plan B rollout, recorded in CLAUDE.md / ARCHITECTURE.md as
"`model`/`allowed-tools` enforcement and hook-wrapping of plugin MCP tools remain deferred":

1. **Hooks never fire on plugin MCP tools.** In `build_composed_tools`
   (`crates/engine/src/main.rs`), plugin MCP servers register *after* `register_hooks`, so
   `register_hooks`' `wrap_each` never covers them. A configured `PreToolUse`/`PostToolUse` hook
   fires on `fs.*`/`bash`/`git.*` but is silently absent on any `plugin__…` call.

2. **Permission rules and hook matchers can't idiomatically name plugin tools.** Both surfaces
   match by the runtime tool name via exact string comparison (`Rule::matches` in
   `permission_def.rs`; `matcher_selects` in `hook_exec.rs`). A rule/matcher *can* today spell out
   otto's internal `plugin__acme__srv__search`, but nobody writes that — Claude Code users write
   `mcp__server__tool`. There is no bridge, so idiomatic rules/matchers silently fail to match.

The third item in the deferral sentence — **`model`** — does not apply: MCP *servers* have no model.
Model routing applies to a plugin's folded *agents/commands*, which Slice 14 already enforces via
`build_router_with_model`. This slice records that "model" was stale wording and removes it.

## Scope

In scope:

- Hook-wrap plugin MCP tools so `PreToolUse`/`PostToolUse` hooks fire on them (#1).
- Add Claude-Code-idiom `mcp__` addressing so `permissions` rules and hook `matcher`s can target
  plugin tools (#2), consumed identically by both surfaces.
- Update CLAUDE.md / ARCHITECTURE.md to drop the deferral and describe the shipped behavior.

Out of scope (unchanged, still deferred where noted elsewhere):

- Interactive `/plugin` UX; project-level (non-user-global) marketplace installs.
- Hook regex matchers, lifecycle hooks, JSON-stdout control, `settings.local.json` (the general
  hooks-slice deferrals — this slice only adds `mcp__` token handling to the existing exact matcher).
- Renaming the internal `plugin__…` gate names (they stay the stable identity).

## Naming model

Operators address plugin MCP tools the Claude Code way. The internal gate name stays
`plugin__<plugin>__<serverkey>__<tool>`; `mcp__…` is purely an addressing alias resolved inside the
two matchers.

| Specifier          | Meaning                                       | Matches internal names             |
|--------------------|-----------------------------------------------|------------------------------------|
| `mcp__<plugin>`    | whole plugin (all its servers, all tools)     | `plugin__<plugin>__*__*`           |
| `mcp__<plugin>__<tool>` | that tool name across any of the plugin's servers | `plugin__<plugin>__*__<tool>` |

The **server key is always wildcarded** — it never appears in an `mcp__` specifier. Documented
consequence: if one plugin bundles two servers that both expose a `search` tool,
`mcp__acme__search` matches both. That is the intended "deny search from acme" semantics.

Rationale: otto namespaces plugin tools by *plugin* name (the trust unit in `enabledPlugins`), and a
plugin may bundle multiple MCP servers. Collapsing Claude Code's flat `mcp__server__tool` onto the
plugin identity (wildcarding the server key) keeps the operator-facing name aligned with the trust
boundary and avoids leaking otto's internal two-level structure.

## Design

### The shared matcher (core of the slice)

Add one helper in the `extensions` crate, in a new small module `mcp_name.rs` (isolated, so both
`permission_def` and `hook_exec` depend on it without either owning it):

```rust
/// True if a settings-side specifier addresses the given runtime tool name.
/// Fires ONLY when `specifier` is an `mcp__…` form AND `tool_name` is a `plugin__…`
/// form; returns false otherwise, so ordinary exact-match handles everything else.
pub fn mcp_specifier_matches(specifier: &str, tool_name: &str) -> bool;
```

Behavior:

- Parse `tool_name`: require the `plugin__` prefix and exactly the shape
  `plugin__<plugin>__<serverkey>__<tool>` → `(plugin, serverkey, tool)`. The `<tool>` segment may
  itself contain the tool's own dots/underscores (e.g. `fs.read`); split only on the first three
  `__` boundaries so the remainder is the tool name verbatim. Not this shape → return `false`.
- Parse `specifier`: require the `mcp__` prefix → `mcp__<plugin>` (plugin-level) or
  `mcp__<plugin>__<tool>` (tool-level), splitting only on the first two `__` boundaries so a
  tool-level name keeps a dotted tail verbatim. Not this shape → return `false`.
- Match: `plugin` equal AND (specifier is plugin-level OR `tool` equal).

Empty segments (e.g. `mcp__`, `mcp__acme__`) parse to `false` rather than a wildcard, so a malformed
specifier never silently widens access.

### Consumer 1 — permission rules

`Rule::matches` (`crates/extensions/src/permission_def.rs`): before the existing
`self.tool != tool` exact check, if `self.tool` starts with `mcp__`, return
`mcp_specifier_matches(&self.tool, tool)`. An `mcp__` rule carries no path-glob specifier (MCP tools
are not path-vetted): `parse_rule`/`build_specifier` must not attempt to build a `Specifier` for an
`mcp__…` tool — its `spec` stays `None`. (A parenthesized `mcp__foo(bar)` rule is treated as
malformed and dropped, consistent with existing bad-rule handling.)

`normalize_tool` leaves `mcp__…` untouched (it already passes unknown names through verbatim), so no
alias entry is needed there.

Precedence (deny > ask > allow) and the inviolable sensitive-path floor are unchanged: `PolicyGate`
still consults the base gate first and a base `Deny` short-circuits before any rule is considered.

### Consumer 2 — hook matchers

`matcher_selects` (`crates/extensions/src/hook_exec.rs`): for each `|`-split, trimmed token, if the
token starts with `mcp__`, use `mcp_specifier_matches(token, tool_name)`; otherwise the existing
exact equality. `None`/`""`/`"*"` still match everything. This composes with alternation, e.g.
`"mcp__acme|bash"`.

### Hook-wrapping order (#1)

In `build_composed_tools` (`crates/engine/src/main.rs`), move the plugin-MCP connect/register loop
to run **before** `register_hooks`, so `register_hooks`' `wrap_each` wraps the plugin tools too. New
composition order:

```
gate (build_tools_preferring_mcp) → register_skills → plugin MCP servers → register_hooks (wrap all)
```

The stale comment ("Bundled plugin MCP servers register AFTER register_hooks, mirroring cmd_run
exactly … not hook-wrapped this slice") is removed. Preserved unchanged:

- Unreachable plugin server → logged and skipped (additive, never fatal).
- Hooks configured without an OS sandbox backend → skipped with the existing loud warning; the
  guarded tools (now including plugin tools) still run.
- `--approve-edits` composition and the plain-gate branch.

Because every entrypoint (`otto run`, `otto serve`, `--command`, `--agent`) composes through this one
function, the reorder fixes all of them at once.

## Data flow

1. Discovery emits `PluginMcpServer` specs (unchanged) and parses `permissions` +
   `hooks` from `settings.json` (unchanged parsers; `mcp__` names flow through verbatim).
2. `build_composed_tools` connects plugin servers → registers their tools under `plugin__…` names →
   `register_hooks` wraps every tool, plugin tools included.
3. At call time the gate/`PolicyGate` evaluates the `plugin__…` name; an `mcp__…` rule matches via
   `mcp_specifier_matches`. A wrapped plugin tool's `PreToolUse`/`PostToolUse` hooks fire; a matcher
   written as `mcp__…` selects it via the same helper.

## Error handling

- Malformed `mcp__` specifier (empty segment, stray parens) → no match (fail-closed on widening; a
  deny that fails to parse simply doesn't deny, matching today's drop-bad-rule behavior — and the
  base gate floor is unaffected).
- Runtime name not in `plugin__…` shape → helper returns `false`, exact-match path handles it.
- Plugin server unreachable / no sandbox for hooks → existing skip+warn paths, unchanged.

## Testing

`extensions` crate (all offline, deterministic):

- `mcp_specifier_matches` unit tests: plugin-level match, tool-level match, wrong plugin, wrong tool,
  non-`mcp__` specifier passthrough (`false`), non-`plugin__` runtime name (`false`), the
  two-servers-same-tool case (both match `mcp__acme__search`), malformed specifiers
  (`mcp__`, `mcp__acme__`) → `false`, and a dotted tool tail (`plugin__acme__srv__fs.read` vs
  `mcp__acme__fs.read`).
- Permission tests: `deny`/`allow`/`ask` of `mcp__acme` and `mcp__acme__search` resolve correctly
  against a `plugin__acme__srv__search` call, with deny>ask>allow precedence and the sensitive floor
  still winning.
- Hook `matcher_selects` tests: bare `mcp__` token, tool-level token, alternation
  (`mcp__acme|bash`), non-matching plugin.

`engine` crate:

- Flip `main.rs`'s `build_composed_tools_plugin_tools_are_gate_guarded_but_not_hook_wrapped` to
  assert the plugin tool IS hook-wrapped.
- New test: a `PreToolUse` hook with `"matcher": "mcp__…"` fires around a wrapped plugin tool and can
  block it (exit 2), gated on `os_sandbox_available()` like the existing hook tests; reuse the
  in-process `mcp-fs`-as-plugin fixture pattern already used by the plugin-MCP tests (no network).

Determinism invariant preserved: no new env reads, no network; the default offline path is unchanged
(with no `settings.json` and no plugins, composition is byte-for-byte identical).

## Files touched

- `crates/extensions/src/mcp_name.rs` (new): `mcp_specifier_matches` + unit tests.
- `crates/extensions/src/lib.rs`: register/export the new module.
- `crates/extensions/src/permission_def.rs`: `Rule::matches` `mcp__` branch;
  `parse_rule`/`build_specifier` skip specifier-building for `mcp__…` tools; tests.
- `crates/extensions/src/hook_exec.rs`: `matcher_selects` `mcp__` token branch; tests.
- `crates/engine/src/main.rs`: reorder plugin-MCP loop before `register_hooks`; update comment; flip
  one test; add the hook-fires-on-plugin-tool test.
- `docs/ARCHITECTURE.md`, `CLAUDE.md`: drop the deferral wording; describe shipped `mcp__` addressing
  + hook-wrapping; note `model` was N/A for MCP servers.

## Dependencies flow

New logic lives in `extensions` (depends inward on `engine-core`/`protocol`) and the binary
(`engine`). No new crate dependencies; no protocol/wire changes; semver-minor.
