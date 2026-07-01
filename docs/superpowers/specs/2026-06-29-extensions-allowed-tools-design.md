# otto Extensions Design — per-artifact `allowed-tools` enforcement (commands)

**Status:** Approved design.
**Date:** 2026-06-29.

## Why this document

`ARCHITECTURE.md` ("Claude Code compatibility") describes one `extensions` crate that discovers
`.claude/` (project) and `~/.claude/` (user-global) and registers each artifact — agents, commands,
skills, hooks, plugins, permissions — into an existing otto primitive. Slices 1–6 shipped agents,
commands, skills, hooks, plugins, and the global `settings.json` `permissions` → gate layer. Across
those slices, three threads were named and left open: **per-artifact `allowed-tools` enforcement**,
**`model` routing**, and **serve-path wiring**.

This document is the first of those: **per-artifact `allowed-tools` enforcement for commands**. The
permissions slice (`2026-06-27-extensions-permissions-design.md`) named it precisely — "making a
command's or skill's declared `allowed-tools` narrow the gate for the duration of that invocation,
composing on top of this layer the way agents already narrow via `subset`."

Agents already do this: `TaskTool` runs a dispatched agent's sub-turn against
`base_tools.subset(&allowlist)` (`crates/extensions/src/task_tool.rs`). This slice extends that exact
pattern to commands. Skills are deliberately deferred (see Scope); `model` routing and serve-path
remain the other two open threads.

## Scope

Make a discovered command's `allowed_tools` narrow the tool registry for the duration of an
`otto run --command <name>` invocation, reusing the agent narrowing convention.

1. `crates/engine/src/main.rs` — `run_command_in`: after building the command's tool registry, apply
   the command's `allowed_tools` by `subset`-ing the registry before it is used for **both** the
   `!bash`/`@file` injection resolution and the spine turn.
2. No new types or parsing: `CustomCommandDef.allowed_tools` is already parsed (slice 2); this slice
   only stops it being inert on the command run path.

### The narrowing convention (identical to agents)

| `allowed_tools` value      | effect                                                        |
|----------------------------|---------------------------------------------------------------|
| absent (`None`)            | **all** tools (Claude-Code-compatible; registry used as-is)   |
| present, non-empty `Some`  | registry narrowed to exactly that intersection of tool names  |
| present, empty `Some([])`  | **no** tools                                                   |

Narrowing is `ToolRegistry::subset`, which keeps the same gate/ask resolver — so it can only *remove*
tools, never widen access. A listed name that does not exist in the base registry (a typo, or `bash`
when no sandbox backend is present) is simply absent from the subset (an intersection), never an
error — matching `subset`'s existing semantics and Claude Code's tolerance.

### Out of scope this slice (deferred, consistent with prior slices)

- **Skills.** A skill has no invocation scope to narrow. The `skill` tool returns a skill's
  instructions into the *ongoing* turn (`crates/extensions/src/skill_tool.rs`); the agent then keeps
  using the **shared** registry, so a skill's tool calls are indistinguishable from the rest of the
  turn's. Faithfully enforcing a skill's `allowed-tools` would require modeling "skill is active" as a
  scope — a stateful gate or re-driving the turn as a sub-turn — a real architecture change, and
  arguably contrary to Claude Code's "skills augment the current context" model. So
  `CustomSkillDef.allowed_tools` stays parsed-but-inert until a dedicated skill-activation-scope
  design. Recorded here as the explicit deferral.
- **Permissions / `PolicyGate` on the `--command` path.** Permissions wire into the `otto run` spine
  only (slice 6); the `--command`/`--agent`/serve paths are deferred to the cross-cutting wiring
  slice. This slice does not change that — `allowed-tools` narrowing operates over whatever gate the
  `--command` path builds today (the `DefaultPermissionGate`). The two layers are orthogonal and will
  compose unchanged when `PolicyGate` is later wired into the command path.
- **`model` routing** and **serve-path wiring** — the other two open extensions threads.

## Design

### `run_command_in` narrowing (`crates/engine/src/main.rs`)

`run_command_in` already: discovers extensions, builds the gated tool registry
(`build_tools_preferring_mcp` over `LocalWorkspace`), expands `$ARGUMENTS`/`$1..$9`, resolves
`!bash`/`@file` injections through that registry's gate, then runs the expanded text as a spine turn.

The single change: between building the registry and using it, apply the command's `allowed_tools`.

```rust
let (tools, _mcp_conns) = build_tools_preferring_mcp(/* … */).await;
let tools: Arc<ToolRegistry> = match &def.allowed_tools {
    Some(list) => Arc::new(tools.subset(list)),  // present → narrow (empty = no tools)
    None       => Arc::new(tools),               // omitted → all tools
};
```

The subset `tools` is then used unchanged for **both** `resolve_injections(&expanded, tools.as_ref())`
and the `run_goal(..., tools, ...)` spine turn. Consequences, by design and consistent with agents:

- A command declaring `allowed-tools: fs.read` (no `bash`) has its `` !`cmd` `` injection denied
  fail-closed (no `bash` tool in the subset → `resolve_injections` errors before the turn), and its
  spine turn's Coder cannot `fs.write`. This is the literal "narrow for the duration of the
  invocation" semantics, and the same self-inflicted footgun a too-narrow agent `tools` already has.
- A command with **no** `allowed-tools` frontmatter runs byte-for-byte as today (registry used as-is).
- `subset` shares the underlying gate, so the inviolable sensitive-path floor (and any future
  command-path `PolicyGate`) is preserved within the narrowed set.

`_mcp_conns` from `build_tools_preferring_mcp` is still held to keep the MCP child processes alive for
the invocation; `subset` does not affect process lifetime (it filters tool handles, which are `Arc`s
over the same connections).

### What does not change

- The plain `otto run "<goal>"` spine carries no artifact, so there is no `allowed_tools` to apply —
  its registry construction is untouched and the offline determinism suite is unaffected.
- `run_custom_agent_in` already narrows via `TaskTool`'s allowlist; it is not touched.
- No protocol, no `engine-core`, no new public types. `ToolRegistry::subset` and `tool_names` already
  exist and are exercised by the agent path.

## Security & determinism properties

- **Narrowing only removes.** `subset` keeps the same gate and ask resolver and can only drop tools,
  so a command can never widen its access beyond the base registry — the sensitive-path floor and the
  bash-only-when-sandboxed rule are untouched.
- **Fail-closed.** A command that lists a tool absent from the base registry simply cannot use it;
  an empty `allowed-tools` yields a no-tools registry; both degrade safely, never widen.
- **Additive / byte-for-byte unchanged when absent.** A command with no `allowed-tools` frontmatter
  (and the whole non-`--command` spine) is unchanged. With no `.claude/commands/` there is no command
  to run.
- **Deterministic & hermetic.** Discovery already takes an explicit `home`; `subset` is pure
  (no I/O). The change adds no env reads and no new spawned process.

## Testing

- **Registry narrowing (unit).** Build a registry via `build_tool_registry`; assert that
  `tools.subset(&["fs.read"]).tool_names()` is exactly `["fs.read"]`; that `subset(&[])` is empty;
  and that an omitted allowlist leaves the full `tool_names()`. (Confirms the exact expression
  `run_command_in` uses, independent of the spine.)
- **Fail-closed injection (behavioral, `run_command_in`).** A command whose template contains a
  `` !`echo hi` `` injection and `allowed-tools: fs.read` errors at injection resolution (no `bash`
  in the subset); the same command with no `allowed-tools` resolves the injection and runs (offline,
  deterministic). A `bash`-requiring assertion guards on `os_sandbox_available()` like the existing
  hook tests.
- **Omitted allowlist parity.** A command with no `allowed-tools` runs identically to the existing
  `run_command_expands_and_runs_spine` test (which stays green unchanged).
- **No regression.** The full offline `cargo test --workspace` suite stays green.

## What this unblocks

With commands enforcing `allowed-tools`, the remaining `allowed-tools` surface is **skills**, gated on
a skill-activation-scope design (deferred above). The other two open extensions threads — **`model`
routing** (router axis) and **serve-path wiring** (`PolicyGate × ApprovalModeGate`, plus all prior
slices' artifacts threaded through `otto serve`) — are unchanged by this slice.
