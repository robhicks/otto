# otto Extensions Slice 2 Design — `commands/*.md` (command registry + spine dispatch)

**Status:** Approved design.
**Date:** 2026-06-25.

## Why this document

`ARCHITECTURE.md` ("Claude Code compatibility") describes one `extensions` crate that discovers
`.claude/` (project) and `~/.claude/` (user-global) and registers each artifact — agents,
commands, skills, hooks, permissions, plugins — into an existing otto primitive. That is a
multi-sub-project effort, decomposed the way the UI roadmap was. **Slice 1** shipped the
`extensions` crate scaffold plus **custom agents** (`agents/*.md` → `Role::Custom` + a `TaskTool`).
This is **slice 2**: the **commands** artifact — Claude Code's `commands/*.md` prompt templates,
discovered, namespaced, expanded (arguments + `!bash`/`@file` injection), and dispatched as a
normal otto spine turn.

## Scope

Build, end to end:

1. `extensions` additions: recursive discovery of `~/.claude/commands/**.md` and
   `<project>/.claude/commands/**.md` (project overrides user by namespaced name) +
   Claude-Code-compatible `commands/*.md` parsing (optional frontmatter + template body).
2. A `CustomCommandDef` and `parse_command_md(name, text)`.
3. Template expansion in two stages: a pure `expand_args` (`$ARGUMENTS`, `$1..$9`) followed by an
   async `resolve_injections` that resolves `` !`cmd` `` (gated `bash`) and `@path` (gated
   `fs.read`) through the existing permission gate.
4. A CLI entry — `otto run --command <name> [args...]` — that expands the named command and runs the
   result as the goal of a normal spine turn (`run_goal`). This is the user-visible, verifiable
   demonstration.

## Fixed decisions (from brainstorming)

- **Execution = run through the spine.** An expanded command becomes the *goal* of a normal
  Plan→Code→Verify turn, exactly like `otto run "<goal>"`. A command is a saved, parameterized goal.
- **Discovery = recursive with namespaces.** Walk `commands/` recursively; `commands/git/commit.md`
  → command name `git:commit` (full Claude Code parity). Project overrides user by namespaced name.
- **Injection = included this slice.** `` !`cmd` `` and `@path` are resolved at expansion time
  through otto's **existing** gated `bash`/`fs.read` tools — no new gate logic.
- **Schema = Claude-Code-compatible.** Optional frontmatter (`description`, `argument-hint`,
  `model`, `allowed-tools`); a bare prompt file (no frontmatter) is a valid command.

## Non-goals (explicitly out of scope — later slices)

- **`model`-hint routing.** A command's `model` is parsed and preserved on `CustomCommandDef` but
  does not influence provider selection — identical to slice 1's treatment of an agent's `model`. A
  command's spine turn uses the engine's configured router slots.
- **`allowed-tools` enforcement.** Parsed and preserved, but **not** used to authorize injection.
  In otto, injection authorization comes from the permission gate/resolver (the authority), not the
  per-command list. Composing `allowed-tools` into the gate is a later slice. (See Security.)
- **Per-command spine-turn tool restriction.** The spine runs its fixed agents over the full gated
  registry; restricting the *turn's* tools per command is deferred (it needs threading an allowlist
  through `run_goal`, which the spine does not take today).
- **UI command palette.** The ARCHITECTURE's "palette" is a UI surface that does not exist yet; the
  CLI `--command` entry is this slice's invocation path. The registry it builds is palette-ready.
- **The other artifact types** — skills, hooks, permissions, plugins — each its own later slice.
- **Retrieval/RemoteWorkspace concerns** — unchanged by this slice.

## Architecture

Dependencies flow strictly inward, unchanged from slice 1:

```
extensions  ──depends on──►  engine-core  (ToolRegistry, WorkspaceRead, Router, types)
   ▲
   │ wired only by
engine (otto binary: run)
```

`extensions` stays a leaf: it depends on `engine-core` and serde; it is **never** linked into
`engine-core` and is invoked **only** from the `engine` binary. The orchestrator core never calls
discovery or expansion, so the offline determinism suite is untouched. **No `engine-core` changes
are required** — expansion reuses the existing `ToolRegistry::call` (which routes through the gate);
the spine turn reuses the existing `run_goal`.

### Components

#### `extensions` crate

- **`CustomCommandDef`** — a parsed command:
  - `name: String` — the **namespaced** command name, derived from the file path (see discovery),
    *not* from frontmatter.
  - `description: Option<String>`
  - `argument_hint: Option<String>` — from the `argument-hint` key; informational (palette hint).
  - `model: Option<String>` — preserved, not routed (later slice).
  - `allowed_tools: Option<Vec<String>>` — from the `allowed-tools` key; preserved, not enforced
    (later slice).
  - `template: String` — the markdown body (the prompt template).

- **`parse_command_md(name: &str, text: &str) -> Result<CustomCommandDef>`** — the `name` is
  supplied by the caller (discovery computes it from the path). If `text` begins with `---`, the
  YAML-ish frontmatter is split and parsed (same line-by-line `key: value` style as
  `parse_agent_md`, accepting a comma-separated string or inline `[a, b]` list for `allowed-tools`);
  the remainder is the template. **No leading `---` ⇒ the whole text is the template** and all
  frontmatter fields are `None` (a bare prompt file is valid — unlike agents, commands have no
  required fields). An unterminated frontmatter block (`---` with no closing `---`) is an error for
  that file (skipped, logged), never fatal to discovery.

- **`discover_commands(project_root: &Path, home: &Path) -> Vec<CustomCommandDef>`** — walks
  `<home>/.claude/commands/` then `<project_root>/.claude/commands/` **recursively**. For each
  `*.md`, the command name is its path **relative to that `commands/` dir**, with the `.md`
  extension stripped and path separators replaced by `:` (`git/commit.md` → `git:commit`,
  `review.md` → `review`). User-global is read first, then project, so a project command overrides a
  user command of the same namespaced name. Missing dirs yield nothing; unreadable/malformed files
  are skipped with a warning. `home` is an explicit parameter (never read ambiently) — hermetic
  tests, deterministic default suite.

  (Discovery lives alongside the existing agent discovery; the slice-1 `Extensions` struct gains a
  `commands: Vec<CustomCommandDef>` field, and `discover` populates both. Callers that only want
  one artifact use the typed field.)

#### Expansion (in `extensions`)

Two stages, kept separate so the substitution logic is a pure, exhaustively-testable function and
the gated I/O is isolated:

1. **`expand_args(template: &str, args: &[String]) -> String`** — pure. Replaces `$ARGUMENTS` with
   all args joined by a single space; `$1`..`$9` with the corresponding positional arg (1-based;
   a missing positional ⇒ empty string). Runs first, so substituted args can appear inside an
   injection target (e.g. `@$1` or `` !`grep $1 .` ``).

2. **`resolve_injections(text: &str, tools: &ToolRegistry) -> Result<String>`** — async, runs after
   `expand_args`. Scans the text and replaces:
   - `` !`cmd` `` (backtick-delimited) → calls the gated **`bash`** tool with `{ "command": cmd }`,
     inlining the trimmed stdout.
   - `@path` → calls the gated **`fs.read`** tool with `{ "path": path }`, inlining the file
     contents.

   Every injection routes through `ToolRegistry::call`, i.e. through the permission gate. **Fail
   closed:** a missing `bash` tool (no sandbox backend), a gate `Deny`/unresolved `Ask`, a
   sensitive-path floor denial (`@.env`, `@.ssh/...`), or any tool error makes `resolve_injections`
   return `Err`, and the command run aborts with a clear message. Injection can never inline a secret
   or run an unsandboxed shell — it has exactly the capabilities the engine's gate already grants.

The CLI calls `expand_args` then `resolve_injections`; the `Result<String>` is the goal handed to
`run_goal`.

### Wiring (in `engine`)

- **`otto run --command <name> [args...]`:**
  - `extensions::discover(root, HOME)`; select the command whose namespaced `name` matches (clear
    error listing both search dirs if absent).
  - Build the tool registry (`build_tools_preferring_mcp`) exactly as `otto run` does, so injection
    can reach the gated `fs.read`/`bash` (bash present only when `os_sandbox_available()`).
  - `expand_args(template, &args)` → `resolve_injections(.., &tools)` → the expanded goal string.
  - Run the goal through the normal spine: `run_goal(&goal, store, router, workspace, tools,
    retriever)`, printing the event log and outcome exactly like `cmd_run` (the dispatch reuses
    `cmd_run`'s tail; the only addition is the expand step in front of it).
  - A `--command` invocation injects `home` explicitly via an inner `*_in` function (mirroring
    `run_custom_agent_in`) so engine tests stay hermetic.
- Absent `--command`, `otto run` is unchanged (the fixed spine turn over the raw goal).
- `--command` and `--agent` are distinct entry points; supplying both is a usage error.

## Gate classification

No new tool is introduced. Expansion's `!`/`@` injections call the **already-classified** `bash`
(`Ask`, registered only when a sandbox backend exists, paired with the CLI's
`AllowListAskResolver`) and `fs.read` (`Allow`, sensitive-floor `Deny` first) tools. There is
nothing new to classify and no change to `DefaultPermissionGate`.

## Data flow

```
otto run --command git:commit "fix the parser"
  └─► extensions::discover(root, HOME) ──► [CustomCommandDef{name:"git:commit", template, ...}, ...]
        └─► expand_args(template, ["fix the parser"])            // $ARGUMENTS/$1.. substituted
        └─► resolve_injections(text, &tools)                     // !`git diff` via gated bash,
        │       └─► ToolRegistry::call("bash"|"fs.read", ..)     //   @file via gated fs.read;
        │             (gate + sandbox + sensitive floor apply)   //   fail-closed on deny/error
        └─► goal = expanded text
              └─► run_goal(goal, store, router, workspace, tools, retriever)   // normal spine turn
                    └─► Plan → ContextFinder → Coder (gated fs.write) → Verify
```

## Error handling

- No leading `---` → the whole file is the template (not an error).
- Unterminated frontmatter (`---` with no closing `---`) → that file is skipped, logged; discovery
  continues.
- Missing `.claude/commands/` (either root) → empty contribution, no error.
- `--command` for an unknown namespaced name → clear error naming both search dirs (no panic).
- Injection failure (`bash` absent / gate `Deny` / sensitive `@path` / tool error) →
  `resolve_injections` returns `Err`; the command run aborts before the spine turn, with a message
  identifying the failed injection.
- Name collision across user/project → project wins (defined precedence, not an error).

## Security

- **Injection reuses the gate — no new surface.** `!`/`@` go through `ToolRegistry::call`, so the
  sensitive-path floor (`.env*`, `.ssh/`, `.git/`, `.aws/`, ssh keys) still denies; `@.env` can
  never be inlined. `bash` is reachable only when a sandbox backend exists and only via the
  established `Ask`→`AllowListAskResolver` path — `!`cmd`` runs in the same confinement as any agent
  shell call.
- **Fail closed.** Any denied or erroring injection aborts the run rather than silently inlining an
  empty string — a hostile or mistaken `@secret`/`!danger` yields a hard error, not a partial prompt.
- **`allowed-tools` is inert this slice.** It is parsed and preserved but does not authorize
  injection; otto's gate is the sole authority, so a command cannot grant itself a capability the
  gate withholds. When `allowed-tools` is later composed into the gate it can only **narrow**.
- **`model` is inert this slice.** Parsed and stored; does not influence routing.
- **Attacker-controlled template** runs under the same sandbox/gate as the spine — a hostile
  `commands/*.md` gains no capability beyond the (gated) `fs.read`/`bash` and the normal spine turn.
- **Hermetic discovery.** `home` is an explicit parameter; tests never read the developer's real
  `~/.claude`, and the orchestrator core never calls discovery or expansion, so the determinism
  suite is unchanged.

## Testing

- **`expand_args`** (pure): `$ARGUMENTS` joins all args; `$1`/`$9` positional; missing positional →
  empty; no-placeholder template is returned verbatim; args substituted before injection targets.
- **`parse_command_md`:** frontmatter present (all of `description`/`argument-hint`/`model`/
  `allowed-tools`, CSV and inline-list forms) → fields populated, body is the template; **no
  frontmatter → whole text is the template**, all optional fields `None`; unterminated frontmatter
  → `Err`.
- **`discover_commands`:** recursive walk names `git/commit.md` → `git:commit` and `review.md` →
  `review`; project overrides user by namespaced name; absent dirs → empty; malformed file skipped,
  others kept.
- **`resolve_injections`** (against a stub `ToolRegistry`): `@file` inlines file contents via
  `fs.read`; `` !`cmd` `` inlines trimmed bash stdout; a sensitive `@.env` (gate `Deny`) → `Err`;
  a `!`cmd`` with `bash` absent from the registry → `Err`; plain text with no markers is unchanged.
- **`engine`:** `otto run --command <name>` over a tempdir `.claude/commands/` (hermetic `home`)
  expands and runs a spine turn, emitting the event log; an unknown command name errors; the
  existing offline determinism suite stays green (no `.claude/` ⇒ no commands, `otto run` unchanged).

## What this unblocks

With commands discovered, namespaced, expanded (args + gated injection), and dispatched through the
spine, the remaining `extensions` artifacts slot in against the same seam:

- **skills** (`SKILL.md` + resources → a built-in `Skill` tool),
- **hooks** (`settings.json` hooks → a new `HookRegistry`),
- **permissions** (`settings.json` permissions, and command `allowed-tools`, → composed into the gate),
- **plugins** (`.claude-plugin/plugin.json` → fan out to the above; bundled MCP servers → the MCP client),
- and the **UI command palette** (consumes the namespaced command registry this slice builds).
