# otto Extensions Slice 4 Design — `settings.json` hooks (PreToolUse / PostToolUse)

**Status:** Approved design.
**Date:** 2026-06-25.

## Why this document

`ARCHITECTURE.md` ("Claude Code compatibility") describes one `extensions` crate that discovers
`.claude/` (project) and `~/.claude/` (user-global) and registers each artifact — agents, commands,
skills, **hooks**, permissions, plugins — into an existing otto primitive. That is a multi-sub-project
effort, decomposed like the UI roadmap. **Slice 1** shipped the crate scaffold + **custom agents**
(`agents/*.md` → `Role::Custom` + a `TaskTool`); **slice 2** shipped **commands** (`commands/**.md` →
a namespaced command registry, expanded and dispatched as a spine turn); **slice 3** shipped **skills**
(`skills/<name>/SKILL.md` → a gated built-in `skill` tool). This is **slice 4**: the **hooks** artifact
— Claude Code's `settings.json` `hooks`, fired at otto's tool-dispatch lifecycle points, exactly as the
architecture says ("`settings.json` hooks → `HookRegistry`, fired at the same lifecycle points").

## Scope

Build, end to end, the **`PreToolUse` and `PostToolUse`** hook events:

1. `extensions` additions: `parse_hooks(settings_json)` → a typed `HookSet`; discovery of
   `<base>/.claude/settings.json` from both `home` and `project_root`.
2. A `HookExecutor` seam (so `extensions` stays hermetic) + matcher logic.
3. A `HookedTool` decorator that wraps any `Tool`, firing matched `PreToolUse` hooks before the inner
   call (a hook may **block** the call) and `PostToolUse` hooks after (observe-only).
4. Engine wiring: a stdin-capable variant of the shared sandbox core, a `SandboxedHookExecutor`, a small
   generic `ToolRegistry::wrap_each` helper, and the `cmd_run` call that wraps every registered tool when
   hooks are discovered and a sandbox backend exists.

**Out of scope this slice** (deferred to later slices, consistent with how prior slices deferred work):

- **Lifecycle hooks** (`SessionStart` / `UserPromptSubmit` / `Stop` / `SubagentStop` / `PreCompact` /
  `Notification` / `SessionEnd`). They fire at turn boundaries in `EngineService`, a different mechanism;
  slice 5.
- **JSON-stdout advanced control** (`decision`/`continue`/`systemMessage`/`hookSpecificOutput`). This
  slice honors the **exit-code contract only** (exit `2` = block on `PreToolUse`; other nonzero =
  non-blocking warning; `0` = proceed).
- **Regex matchers.** Matchers support `None`/`""`/`"*"` = all and `|`-alternation of exact tool names.
  Full regex (and a Claude-Code→otto tool-name alias map, since otto's tools are `fs.write`/`bash`/… not
  `Write`/`Bash`) are future work.
- **`settings.local.json`** and **serve-path wiring.** Slice 4 wires the `otto run` path only —
  consistent with skills/commands/agents being CLI-wired so far. `settings.json` only this slice.
- **`permissions` composition.** A hook composes *below* the gate (see Security); composing
  `settings.json` permissions *into* the gate is a separate slice.

## Design

### `HookSet` + `parse_hooks` (`crates/extensions/src/hook_def.rs`)

```rust
pub struct HookCommand {
    pub command: String,
    pub timeout: Option<u64>,   // seconds; a default is applied at run time
}
pub struct HookMatcher {
    pub matcher: Option<String>,  // None / "" / "*" = match every tool
    pub hooks: Vec<HookCommand>,
}
#[derive(Default)]
pub struct HookSet {
    pub pre_tool_use: Vec<HookMatcher>,
    pub post_tool_use: Vec<HookMatcher>,
}

pub fn parse_hooks(settings_json: &str) -> anyhow::Result<HookSet>
```

`parse_hooks` parses the JSON and reads the top-level `hooks` object. The Claude-Code shape:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "bash", "hooks": [ { "type": "command", "command": "…", "timeout": 60 } ] }
    ],
    "PostToolUse": [ … ]
  }
}
```

Rules:

- **Missing `hooks`** (or a `settings.json` with no hooks) → an empty `HookSet`, returned `Ok` (a
  settings file is valid without hooks).
- **Malformed JSON** → `Err` (discovery turns this into a skip-with-warning).
- Each matcher entry contributes its optional `matcher` plus its `hooks` array. A hook entry whose
  `type` is absent or not `"command"`, or which is missing a non-empty `command`, is **skipped** (this
  slice runs only `type: "command"` hooks). Unknown hook keys are ignored.
- **Unknown event keys** (`SessionStart`, `Stop`, …) are ignored this slice — they parse without error
  but are not collected.

### Discovery (`crates/extensions/src/lib.rs`)

`Extensions` gains `pub hooks: HookSet`. `discover()` reads, for each base (`home` then
`project_root`):

```
<base>/.claude/settings.json
```

Unlike `agents`/`commands`/`skills` (which collapse name collisions, project overriding user), **hooks
from both bases are concatenated**: `discover` extends `pre_tool_use`/`post_tool_use` with the user's
entries first, then the project's — so a hook configured user-globally **and** one configured per-project
both fire. This matches Claude Code, where hook arrays are additive across settings sources.

Missing `settings.json` → no hooks. An unreadable file, or one whose JSON fails `parse_hooks`, is
skipped with a warning (never fatal). `home` stays an explicit parameter (never read ambiently), so
discovery is hermetic and tests never touch a developer's real `~/.claude`.

### Matching + the executor seam (`crates/extensions/src/hook_exec.rs`)

```rust
pub enum HookEvent { PreToolUse, PostToolUse }

pub struct HookOutcome {
    pub exit_code: Option<i32>,   // None if the process was killed (e.g. timeout)
    pub stdout: String,
    pub stderr: String,
}

#[async_trait]
pub trait HookExecutor: Send + Sync {
    /// Run `command`, piping `stdin_json` to stdin, killed after `timeout`.
    async fn run(&self, command: &str, stdin_json: &str, timeout: Duration)
        -> anyhow::Result<HookOutcome>;
}

impl HookSet {
    /// The HookCommands whose matcher selects `tool_name` for `event`, in declaration order.
    pub fn matched(&self, event: HookEvent, tool_name: &str) -> Vec<HookCommand>;
}
```

Matcher semantics (`matcher_selects(matcher, tool_name)`):

- `None`, `""`, or `"*"` → matches every tool.
- otherwise split on `|`, trim each, and match if any token **equals** `tool_name`.

Defining `HookExecutor` in `extensions` (not `engine-core`) keeps the seam where it is used: the
orchestrator never runs hooks, so the core needs no new trait. Tests inject a fake executor that records
the command + stdin and returns a scripted `HookOutcome`.

### `HookedTool` decorator (`crates/extensions/src/hooked_tool.rs`)

```rust
pub struct HookedTool { /* inner, pre, post, executor, default_timeout */ }

impl HookedTool {
    /// Wrap `inner` with the hooks matching its name. If NO pre/post hook matches, returns `inner`
    /// unchanged (identity — zero overhead for un-hooked tools).
    pub fn wrap(
        inner: Arc<dyn Tool>,
        hooks: &HookSet,
        executor: Arc<dyn HookExecutor>,
    ) -> Arc<dyn Tool>;
}
```

`wrap` computes `pre = hooks.matched(PreToolUse, inner.name())` and `post = matched(PostToolUse, …)`.
When both are empty it returns the original `Arc` — so tools with no applicable hooks are untouched.

`Tool::name()` delegates to `inner.name()`. `call(args)`:

1. Build the PreToolUse input:
   `{ "hook_event_name": "PreToolUse", "tool_name": <inner.name()>, "tool_input": <args> }`.
2. For each `pre` hook (declaration order): `executor.run(cmd, &input, timeout)`.
   - `exit_code == Some(2)` → **block**: return `Err` (the inner tool never runs); the hook's `stderr`
     becomes the error message.
   - `exit_code == Some(0)` → proceed.
   - any other exit code, **or an executor error / timeout** → **non-blocking**: log a warning to
     `stderr` and proceed (infrastructure problems must not silently block the spine).
3. `let result = inner.call(args).await?`.
4. Build the PostToolUse input (same fields plus `"tool_response": <result>`) and run each `post` hook.
   PostToolUse is observe-only: the result already exists, so any nonzero exit (including `2`) is logged
   as a warning and the result is returned regardless.
5. Return `result`.

`default_timeout` (e.g. 60 s) applies when a `HookCommand` omits `timeout`.

### Engine wiring (`crates/engine`, `crates/tools`)

1. **Stdin-capable sandbox core (`crates/tools/src/sandbox.rs`).** Add
   `run_sandboxed_with_stdin(policy, root, command, timeout, stdin: Option<&str>)`. The existing
   `run_sandboxed` becomes a thin delegate passing `None`, so `bash.rs` / `mcp-bash` / existing tests are
   untouched. The core swaps the hardcoded `Stdio::null()` for `Stdio::piped()` when `stdin` is `Some`,
   writes the payload, and **drops the stdin handle before `wait_with_output`** (deadlock-safe). This is
   the single security-critical change and is unit-tested directly.

2. **`SandboxedHookExecutor` (`crates/engine/src/hooks.rs`).** Implements `HookExecutor` by calling
   `run_sandboxed_with_stdin` under `SandboxPolicy::Os` with the workspace root — so hooks inherit the
   shared sandbox (read-only filesystem except the workspace root, no network/pid/ipc, minimal env,
   killed on timeout). Constructed only when `os_sandbox_available()`.

3. **`ToolRegistry::wrap_each` (`crates/engine-core/src/tool.rs`).** A small generic helper:
   `pub fn wrap_each(&mut self, f: impl FnMut(Arc<dyn Tool>) -> Arc<dyn Tool>)` rebuilds the tool map by
   mapping `f` over each `Arc<dyn Tool>`, leaving the gate/resolver unchanged. It takes a **closure**, so
   `engine-core` gains no dependency on `extensions` and the orchestrator is untouched. This is the one
   `engine-core` addition — a generic registry capability, not hook logic.

4. **`cmd_run` (`crates/engine/src/main.rs`).** After registering skills, if `ext.hooks` is non-empty
   **and** `os_sandbox_available()`, build a `SandboxedHookExecutor` and call
   `tools.wrap_each(|t| HookedTool::wrap(t, &ext.hooks, exec.clone()))` — mirroring where `register_skills`
   wires in. With no `settings.json`, or no sandbox backend, no wrapping occurs and the spine's tool set
   is byte-for-byte unchanged.

## Security & determinism properties

- **Gate-first composition.** Hooks fire *inside* the wrapped tool's `call`, which `ToolRegistry::call`
  only reaches **after** the gate returns `Allow` (or `Ask` resolved to allow). Therefore a hook can
  **deny an allowed call** (exit `2`) but can **never allow a gate-denied one** — the sensitive-path
  floor denies before any hook runs. Composing permissions *into* the gate is a separate slice; here a
  hook is strictly a further restriction.
- **Sandboxed execution, no unsandboxed path.** Hook commands run only through the shared
  `run_sandboxed` core under `SandboxPolicy::Os`. The slice never wires `SandboxPolicy::None`.
- **Fail-closed availability.** No OS sandbox backend → no `SandboxedHookExecutor` → tools are not
  wrapped → hooks simply don't fire and the spine is unchanged. This mirrors how `bash` is absent
  without a sandbox. (Infrastructure unavailability never *blocks* a tool; it just means no hook.)
- **Non-blocking on hook failure.** A hook that errors or times out is a warning, not a block — only an
  explicit `PreToolUse` exit `2` blocks. A misconfigured or crashing hook can't wedge the spine.
- **Hermetic + deterministic.** `home` is an explicit parameter; the orchestrator core never constructs
  hooks; `HookedTool::wrap` returns the inner tool unchanged when nothing matches. With no `.claude/`,
  the tool set is byte-for-byte unchanged and the offline determinism suite is untouched.
- **Documented firing boundary.** The orchestrator's own Coder edits go through `tools.check("fs.write")`
  + `workspace.apply_edit`, **not** `tools.call`, so `PreToolUse` does **not** fire on those edits this
  slice. The firing surface is agent-invoked tool calls (`bash`, `grep`, `git.*`, `fs.read`, an
  agent-invoked `fs.write`, `skill`, `task`).

## Testing

- **`parse_hooks`** (pure): full `settings.json` with `PreToolUse`/`PostToolUse`, `matcher`, `hooks`,
  `timeout` → fields populated; missing `hooks` → empty `Ok`; malformed JSON → `Err`; non-`command` and
  command-less hook entries skipped; unknown event keys ignored.
- **matching**: `None`/`""`/`"*"` → all; `|`-alternation; exact equality; `HookSet::matched` returns the
  right commands in declaration order.
- **discovery**: `settings.json` from both bases concatenated (user then project); missing file → empty;
  malformed file → skipped with a warning while a valid sibling base is kept; `home` explicit.
- **`HookedTool`** (fake executor): no matching hooks → `wrap` returns the identity `Arc`; `PreToolUse`
  exit `2` → `call` errors and the inner tool is **not** invoked; exit `0` → inner runs; other nonzero /
  executor error / timeout → inner still runs (non-blocking); `PostToolUse` runs after with
  `tool_response` present; assert the stdin JSON shape (`hook_event_name`/`tool_name`/`tool_input`) the
  executor received.
- **`run_sandboxed_with_stdin`** (sandbox tests, gated on `os_sandbox_available()` like the existing
  ones): a command reading stdin echoes the payload; the no-stdin delegate preserves the prior
  null-stdin behavior; timeout still kills the process.
- **engine**: over a tempdir `.claude/settings.json` (hermetic `home`) with a sandbox available, the
  built registry wraps tools so a `PreToolUse` hook blocks a tool call; with no `.claude/` the registry
  is unchanged and the offline determinism suite stays green.

## What this unblocks

With tool-dispatch hooks fired through a gated, sandboxed executor, the remaining hook surface and the
sibling `extensions` artifacts slot in against the same seams:

- **lifecycle hooks** (`SessionStart`/`UserPromptSubmit`/`Stop`/…) at `EngineService` turn boundaries,
- **JSON-stdout advanced control** (`decision`/`continue`/`systemMessage`),
- **serve-path wiring**, `settings.local.json`, and **regex / Claude-Code tool-name-alias** matchers,
- **permissions** (`settings.json` permissions → composed into the gate; where command/skill
  `allowed-tools` stops being inert),
- **plugins** (`.claude-plugin/plugin.json` → fan out to all of the above).
