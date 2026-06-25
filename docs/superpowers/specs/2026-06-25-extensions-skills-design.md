# otto Extensions Slice 3 Design — `skills/<name>/SKILL.md` (skill discovery + built-in `skill` tool)

**Status:** Approved design.
**Date:** 2026-06-25.

## Why this document

`ARCHITECTURE.md` ("Claude Code compatibility") describes one `extensions` crate that discovers
`.claude/` (project) and `~/.claude/` (user-global) and registers each artifact — agents,
commands, skills, hooks, permissions, plugins — into an existing otto primitive. That is a
multi-sub-project effort, decomposed like the UI roadmap. **Slice 1** shipped the `extensions`
crate scaffold plus **custom agents** (`agents/*.md` → `Role::Custom` + a `TaskTool`); **slice 2**
shipped **commands** (`commands/*.md` → namespaced command registry, expanded and dispatched as a
spine turn). This is **slice 3**: the **skills** artifact — Claude Code's `skills/<name>/SKILL.md`,
discovered and exposed as loadable instructions through a built-in, gated `skill` tool, exactly as
the architecture says ("`skills` (`SKILL.md` + resources) → loadable skills exposed via a built-in
`Skill` tool").

## Scope

Build, end to end:

1. `extensions` additions: discovery of `~/.claude/skills/*/SKILL.md` and
   `<project>/.claude/skills/*/SKILL.md` (project overrides user by `name`) + Claude-Code-compatible
   `SKILL.md` parsing (frontmatter `name`/`description`/`allowed-tools` + markdown body).
2. A `CustomSkillDef` and `parse_skill_md(name, text)`.
3. A built-in `SkillTool` (`Tool` named `skill`): given `{"skill": "<name>"}`, returns the skill's
   `instructions` (body) plus its `resource_dir` (the skill directory path), so an agent can load a
   skill mid-turn and then read any bundled resource on demand through the gated `fs.read`.
4. Engine wiring: register `SkillTool` into the tool registry (behind the gate) when any skills are
   discovered — the same place the `task` tool is registered when agents exist — so agents reach
   skills during a normal spine turn.

**Out of scope this slice** (consistent with how commands deferred `allowed-tools`/`model`):

- `allowed-tools` is parsed and preserved but **inert** — otto's gate remains the sole authority; a
  skill cannot grant itself a capability the gate withholds. When later composed into the gate it can
  only **narrow**.
- **No eager resource bundling.** The `skill` tool returns a `resource_dir` reference; bundled files
  are read lazily via the gated `fs.read` (sensitive-path floor still denies `.env`, `.ssh/`, etc.).
- **No CLI subcommand.** Skills are mid-turn capabilities, not entrypoints (unlike `--agent` /
  `--command`). No `otto run --skill`.

## Design

### `CustomSkillDef` + `parse_skill_md` (`crates/extensions/src/skill_def.rs`)

```rust
pub struct CustomSkillDef {
    pub name: String,                        // discovery supplies the dir name; frontmatter `name` overrides
    pub description: String,                 // frontmatter `description` (required)
    pub allowed_tools: Option<Vec<String>>,  // parsed + preserved; INERT this slice
    pub instructions: String,                // the markdown body
    pub root: PathBuf,                        // skill directory; assigned by discovery, not parsed
}

pub fn parse_skill_md(name: &str, text: &str) -> anyhow::Result<CustomSkillDef>
```

`name` is supplied by the caller (discovery derives it from the skill directory name). If the
frontmatter carries a `name`, it overrides — Claude-Code-compatible. Frontmatter parsing reuses the
commands convention: a leading `---` fence, `key: value` lines, `allowed-tools` accepting both CSV
(`bash, fs.read`) and inline-list (`[bash, fs.read]`) forms; an unterminated fence is an `Err`.

Unlike commands, **`description` is required** — Claude Code uses it to decide when a skill applies,
so a skill without one is unusable. `parse_skill_md` returns `Err` when `description` is missing or
empty; discovery turns that into a skip-with-warning (never fatal). A `SKILL.md` with no frontmatter
at all is therefore invalid (no description) and skipped. `root` is **not** parsed from text — it is
filled in by discovery from the file's directory.

### Discovery (`crates/extensions/src/lib.rs`)

`Extensions` gains `pub skills: Vec<CustomSkillDef>`. `discover()` walks, for each base
(`home` then `project_root`, so project inserts override by `name`):

```
<base>/.claude/skills/<skill-dir>/SKILL.md
```

One level of skill directories; each must contain a `SKILL.md`. The discovered `name` defaults to
`<skill-dir>`; a frontmatter `name` overrides. `root` is set to the `<skill-dir>` path. Missing
`skills/` → no skills. A skill dir without `SKILL.md`, or an unreadable/malformed/`description`-less
`SKILL.md`, is skipped with a warning — never fatal. Other resource files in the skill dir are left
on disk untouched (read later via `fs.read`).

`home` stays an explicit parameter (never read ambiently), so discovery is hermetic and tests never
touch a developer's real `~/.claude`.

### `SkillTool` (`crates/extensions/src/skill_tool.rs`)

```rust
pub struct SkillTool { /* name -> (instructions, root) */ }
impl SkillTool { pub fn new(skills: &[CustomSkillDef]) -> Self }

// name() -> "skill"
// call({"skill": "<name>"}) -> {"instructions": <body>, "resource_dir": <root>}
```

Behavior:

- `{"skill": "<name>"}` returns `{"instructions": <body string>, "resource_dir": <root path string>}`.
- A missing or non-string `skill` arg → `Err`.
- An unknown skill name → `Err`.

The skill name is a **map key**, never used as a filesystem path, so there is no traversal surface in
the tool itself. The returned `resource_dir` is informational; it is derived from the trusted
discovered `root`, not from agent input. Because the call carries no `path`/`bash`, the
`DefaultPermissionGate` returns `Allow` — correct for a read-only "load instructions" call. Reading
the resources it points at goes through the gated `fs.read`, where the inviolable sensitive-path
floor still applies.

### Engine wiring (`crates/engine`)

Where the binary builds the tool registry and registers `task` when `extensions.agents` is non-empty,
also register `SkillTool::new(&extensions.skills)` when `extensions.skills` is non-empty. The tool
lands behind the same gate as every other tool. No orchestrator-core change; no CLI change. With no
`.claude/skills/`, no `skill` tool is registered and the spine is byte-for-byte unchanged.

## Security & determinism properties

- **Gate is the sole authority.** `allowed-tools` is inert; a skill grants no capability. The `skill`
  tool only returns text + a directory reference — it performs no I/O of its own beyond reading the
  in-memory instructions captured at discovery.
- **No new traversal surface.** Skill selection is by registry key; resource reads reuse the gated
  `fs.read` floor unchanged.
- **Hermetic discovery.** `home` is an explicit parameter; the orchestrator core never calls
  discovery or constructs `SkillTool`, so the offline determinism suite is untouched.
- **Fail-soft discovery, fail-loud use.** A malformed `SKILL.md` is skipped (warning) so one bad
  skill never breaks the others; an explicit `skill` call for an unknown/invalid name errors rather
  than silently returning empty instructions.

## Testing

- **`parse_skill_md`** (pure): full frontmatter (`name`/`description`/`allowed-tools`, CSV and
  inline-list) → fields populated, body is `instructions`; frontmatter `name` overrides the supplied
  name; missing/empty `description` → `Err`; no frontmatter → `Err` (no description); unterminated
  frontmatter fence → `Err`; unknown keys ignored.
- **`discover`** (`skills`): one-level skill dirs each with `SKILL.md` are found; `name` defaults to
  dir name, frontmatter `name` overrides; project overrides user by `name`; missing `skills/` →
  empty; a skill dir without `SKILL.md` and a malformed `SKILL.md` are skipped while siblings are
  kept; `root` points at the skill dir.
- **`SkillTool`**: returns `instructions` + `resource_dir` for a known skill; unknown name → `Err`;
  missing/non-string `skill` arg → `Err`.
- **`engine`**: over a tempdir `.claude/skills/<name>/SKILL.md` (hermetic `home`), the built tool
  registry contains `skill`; with no `.claude/` the registry has no `skill` tool and the offline
  determinism suite stays green.

## What this unblocks

With skills discovered and loadable through a gated built-in tool, the remaining `extensions`
artifacts slot in against the same seam:

- **hooks** (`settings.json` hooks → a new `HookRegistry`),
- **permissions** (`settings.json` permissions, plus command/skill `allowed-tools`, → composed into
  the gate; this is where a skill's `allowed-tools` stops being inert),
- **plugins** (`.claude-plugin/plugin.json` → fan out to all of the above; bundled MCP servers → the
  MCP client),
- and a **UI skill palette** alongside the command palette.
