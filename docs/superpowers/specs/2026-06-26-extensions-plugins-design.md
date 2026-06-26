# otto Extensions Slice 5 Design — plugins (`.claude-plugin/plugin.json`)

**Status:** Approved design.
**Date:** 2026-06-26.

## Why this document

`ARCHITECTURE.md` ("Claude Code compatibility") describes one `extensions` crate that discovers
`.claude/` (project) and `~/.claude/` (user-global) and registers each artifact — agents, commands,
skills, hooks, permissions, **plugins** — into an existing otto primitive. That is a multi-sub-project
effort, decomposed like the UI roadmap. **Slice 1** shipped the crate scaffold + custom agents
(`agents/*.md` → `Role::Custom` + a `TaskTool`); **slice 2** shipped commands (`commands/**.md` → a
namespaced command registry); **slice 3** shipped skills (`skills/<name>/SKILL.md` → a gated `skill`
tool); **slice 4** shipped hooks (`settings.json` `PreToolUse`/`PostToolUse` → a `HookedTool`
decorator). This is **slice 5**: the **plugins** artifact — exactly as the architecture says
("plugins (`.claude-plugin/plugin.json`) → manifest parsed; each bundled component registered via the
rows above; **bundled MCP servers route straight into otto's MCP client unmodified.**").

A Claude Code plugin is a directory bundling the *same* artifacts otto already parses — `agents/`,
`commands/`, `skills/`, hooks — plus MCP servers. So plugins are a **fan-out to the rows above**: the
genuinely new work is (1) the marketplace + enable-allowlist resolution layer that decides *which*
plugins activate and *where* their files are, (2) namespacing every contributed artifact by plugin
name, (3) `${CLAUDE_PLUGIN_ROOT}` expansion, and (4) spawning each plugin's bundled MCP servers
through otto's existing MCP client.

## Scope

Build, end to end, the **full marketplace plugin model** over marketplaces already present on disk:

1. `extensions` additions:
   - `marketplace_def.rs`: `parse_marketplace_json` → a typed `Marketplace { name, plugins: Vec<MarketplaceEntry> }`.
   - `plugin_def.rs`: `parse_plugin_json` → a typed `PluginManifest` (name/version/description + optional
     component path overrides).
   - `enabled_plugins`: read the `enabledPlugins` allowlist from `settings.json`.
   - Plugin discovery: walk `<base>/.claude/plugins/marketplaces/*/`, resolve enabled plugins, parse each
     plugin's manifest, and **fold its namespaced agents/commands/skills/hooks into `Extensions`** via the
     existing parsers — plus emit a pure-data `Vec<PluginMcpServer>` (no process spawned in this crate).
2. Namespacing by plugin name across every contributed artifact (commands/agents/skills/MCP tools).
3. `${CLAUDE_PLUGIN_ROOT}` expansion in hook commands and MCP server `command`/`args`/`env`.
4. Engine wiring: `connect_plugin_server(spec)` reusing the existing generic `connect(Command)` to spawn
   each bundled MCP server and register its tools behind the gate with namespaced names; wired into
   `cmd_run` alongside `register_skills`/`register_hooks`.

### Build order (two plans)

The spec covers the full model; it ships as two reviewable plans on the project's plan-by-plan cadence:

- **Plan A — marketplace + static components.** Marketplace/plugin discovery, the `enabledPlugins`
  allowlist gate, manifest parsing, namespacing, `${CLAUDE_PLUGIN_ROOT}` in hooks, and folding bundled
  agents/commands/skills/hooks into `Extensions`. Hermetic-only (no spawning); the offline determinism
  suite is untouched.
- **Plan B — bundled MCP servers.** Parse each plugin's `.mcp.json` (or inline `mcpServers`), emit
  `PluginMcpServer` specs (with `${CLAUDE_PLUGIN_ROOT}` expanded), and spawn + register them in the
  engine via `connect_plugin_server`. Touches engine MCP wiring only; spine determinism unchanged when no
  plugins are present.

### Out of scope this slice (deferred, consistent with prior slices)

- **The network install action.** `git clone` of a marketplace or plugin, lockfile writing, and the
  interactive `/plugin` install/enable UX. This slice operates over marketplaces **already materialized on
  disk** under `.claude/plugins/marketplaces/`. A marketplace entry whose `source` is remote and not
  present on disk is **skipped with a warning** (not installed). This mirrors how prior slices deferred
  the serve-path wiring and network/install mechanics while shipping the on-disk behavior.
- **`model`/`allowed-tools` enforcement.** Plugin-contributed agents/commands carry these fields exactly
  as user/project ones do; they remain parsed-but-inert until the permissions slice (the gate stays the
  sole authority).
- **Serve-path wiring.** Like skills/commands/agents/hooks before it, plugins are wired into the
  `otto run` path this slice. Serve-path wiring is a later slice.
- **Lifecycle / JSON-stdout hook control.** Plugin hooks reuse the slice-4 `HookedTool` mechanism
  unchanged: `PreToolUse`/`PostToolUse`, exit-code contract only. The deferrals from slice 4 still apply.

## Design

### `Marketplace` + `parse_marketplace_json` (`crates/extensions/src/marketplace_def.rs`)

```rust
pub struct MarketplaceEntry {
    pub name: String,
    /// Local path (relative to the marketplace root) or a remote source descriptor.
    pub source: PluginSource,
    pub description: Option<String>,
}
pub enum PluginSource {
    /// A path relative to the marketplace root (e.g. "./plugins/foo"). Resolvable on disk.
    LocalPath(String),
    /// A remote source (github/git/etc.) — not materialized by this slice; skipped with a warning.
    Remote(serde_json::Value),
}
pub struct Marketplace {
    pub name: String,
    pub plugins: Vec<MarketplaceEntry>,
}

pub fn parse_marketplace_json(json: &str) -> anyhow::Result<Marketplace>;
```

The Claude Code shape:

```jsonc
{
  "name": "acme",
  "owner": { "name": "..." },
  "plugins": [
    { "name": "foo", "source": "./plugins/foo", "description": "..." },
    { "name": "bar", "source": { "source": "github", "repo": "acme/bar" } }
  ]
}
```

Rules:

- A `source` that is a **string** → `PluginSource::LocalPath`. A `source` that is an **object** →
  `PluginSource::Remote` (kept as-is; skipped at resolution time). A missing/empty `name` or `source`
  entry is skipped.
- **Malformed JSON** or a missing top-level `name`/`plugins` → `Err` (discovery turns this into a
  skip-with-warning). An empty `plugins` array is valid.
- Unknown keys (`owner`, `metadata`, …) are ignored.

### `PluginManifest` + `parse_plugin_json` (`crates/extensions/src/plugin_def.rs`)

```rust
pub struct PluginManifest {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Optional component path overrides (relative to the plugin root). `None` ⇒ convention default.
    pub commands: Option<String>,   // default: "commands"
    pub agents: Option<String>,     // default: "agents"
    pub skills: Option<String>,     // default: "skills"
    pub hooks: Option<String>,      // default: "hooks/hooks.json"
    pub mcp_servers: Option<McpServersField>,  // default: ".mcp.json" (Plan B)
}
pub enum McpServersField {
    /// A path (relative to plugin root) to a JSON file of mcpServers.
    Path(String),
    /// An inline mcpServers object.
    Inline(serde_json::Value),
}

pub fn parse_plugin_json(json: &str) -> anyhow::Result<PluginManifest>;
```

The Claude Code shape:

```jsonc
{ "name": "foo", "version": "1.0.0", "description": "...",
  "author": { "name": "..." },
  "commands": "./commands", "agents": "./agents", "hooks": "./hooks/hooks.json",
  "mcpServers": "./.mcp.json" }
```

Rules:

- Required: a non-empty `name`. Missing → `Err`.
- Component fields are **optional**. A string value is a path override (leading `./` tolerated); absent →
  the convention default. `mcpServers` may be a path string or an inline object.
- Unknown keys (`author`, `homepage`, `keywords`, …) are ignored.

### The `enabledPlugins` allowlist (`crates/extensions/src/lib.rs`)

`settings.json` (the file we already read for hooks) carries:

```jsonc
{ "enabledPlugins": { "foo@acme": true, "bar@acme": false } }
```

A helper `parse_enabled_plugins(settings_json) -> BTreeMap<String, bool>` reads the top-level
`enabledPlugins` object (missing → empty). The key is `"<plugin>@<marketplace>"`. Across bases the maps
are **merged** with project overriding user for the same key (project `settings.json` can flip a
user-global enable). A plugin is **active** iff its `"<name>@<marketplace>"` key maps to `true`.

This is a hard allowlist gate: a plugin present in a marketplace but not explicitly enabled does **not**
activate — matching Claude Code, and keeping discovery side-effect-free (merely cloning a marketplace
never auto-runs its code).

### Plugin discovery + folding (`crates/extensions/src/lib.rs`)

`Extensions` gains:

```rust
pub mcp_servers: Vec<PluginMcpServer>,   // pure data; spawned by the engine (Plan B)
```

(`agents`/`commands`/`skills`/`hooks` are unchanged in type — plugin contributions fold into the existing
vectors/`HookSet`.)

`discover(project_root, home)` extends its existing per-base loop with a plugins pass, **after** the
user/project `.claude/` artifacts are collected (so plugins are lowest precedence):

```
for base in [home, project_root]:
    enabled := merge(enabled, parse_enabled_plugins(<base>/.claude/settings.json))
for base in [home, project_root]:
    for mp_dir in <base>/.claude/plugins/marketplaces/*/ with .claude-plugin/marketplace.json:
        mp := parse_marketplace_json(...)
        for entry in mp.plugins:
            if not enabled["{entry.name}@{mp.name}"]: continue
            if entry.source is Remote or its LocalPath isn't on disk: warn-and-skip
            plugin_root := mp_dir / entry.source
            manifest := parse_plugin_json(plugin_root/.claude-plugin/plugin.json)
            fold(manifest, plugin_root, namespace = manifest.name)
```

`fold` reuses the existing directory readers against the manifest-resolved (or convention) component
paths, then **namespaces** each result:

- **commands**: `read_commands_dir(plugin_root/<commands>)` → each `def.name` is prefixed `"{ns}:"`
  (`foo:commit`, nested `foo:git:commit`). Merged into `commands` only if the namespaced name is not
  already taken by a user/project command (user/project win).
- **agents**: `read_agents_dir(plugin_root/<agents>)` → `def.name` prefixed `"{ns}:"`. Merged with the
  same user/project-wins rule.
- **skills**: `read_skills_dir(plugin_root/<skills>)` → `def.name` prefixed `"{ns}:"`, `root` left at the
  skill dir (resources still read lazily through gated `fs.read`). Same precedence rule.
- **hooks**: `read_settings_hooks`-style parse of `plugin_root/<hooks>` (a `hooks.json` whose shape is the
  same `{ "PreToolUse": [...], "PostToolUse": [...] }` object `parse_hooks` already reads). Each hook
  command has `${CLAUDE_PLUGIN_ROOT}` expanded to `plugin_root` (absolute) before being appended —
  additive, like user/project hooks.
- **mcp servers** (Plan B): parse the manifest's `mcpServers` (path or inline) into `PluginMcpServer`
  specs (below), `${CLAUDE_PLUGIN_ROOT}` expanded.

Precedence summary: **user/project `.claude/` artifacts always win** over a plugin artifact of the same
final (namespaced) name; among plugins, discovery order (user-base marketplaces, then project-base) with
first-wins on an exact namespaced collision. Hooks and MCP servers are additive (no name collision).

Failure handling matches every prior slice: a missing dir/file is silent; an unreadable or malformed
`marketplace.json`/`plugin.json`/component is **skipped with a warning, never fatal**; `home` stays an
explicit parameter so discovery is hermetic and tests never touch a real `~/.claude`.

### `${CLAUDE_PLUGIN_ROOT}` expansion (`crates/extensions/src/lib.rs`)

A small `expand_plugin_root(s: &str, plugin_root: &Path) -> String` replaces every literal
`${CLAUDE_PLUGIN_ROOT}` with the plugin root's absolute path. Applied to hook `command` strings (Plan A)
and to MCP server `command`/`args`/`env` values (Plan B). It is a textual substitution only — it does not
read the environment, preserving hermetic determinism.

### `PluginMcpServer` + engine wiring (Plan B — `crates/extensions`, `crates/engine`)

```rust
// crates/extensions/src/plugin_def.rs (pure data — no spawning)
pub struct PluginMcpServer {
    pub namespace: String,         // the plugin name, for tool-name prefixing
    pub server_key: String,        // the key under "mcpServers" (server id within the plugin)
    pub command: String,           // ${CLAUDE_PLUGIN_ROOT}-expanded
    pub args: Vec<String>,         // ${CLAUDE_PLUGIN_ROOT}-expanded
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}
```

The `.mcp.json` / inline `mcpServers` shape Claude Code uses:

```jsonc
{ "mcpServers": { "my-server": { "command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/server.js"],
                                 "env": { "FOO": "bar" } } } }
```

Each server becomes one `PluginMcpServer`. The engine adds, in `crates/engine/src/mcp.rs`:

```rust
pub async fn connect_plugin_server(spec: &PluginMcpServer)
    -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)>;
```

It builds a `tokio::process::Command` from `command`/`args`/`env`/`cwd` and calls the existing generic
`connect(command)`. The returned tools are registered with **namespaced gate names**
(`plugin__{namespace}__{server_key}__{tool}` — distinct from otto's `fs.*`/`bash`/`git.*`, so a plugin
server can never shadow a built-in tool or be confused with one by the gate). `cmd_run` calls
`connect_plugin_server` for each `ext.mcp_servers` entry after the fs/grep/git/bash connects, registering
each behind the gate (a plugin MCP tool gets the gate's default classification; the sensitive-path floor
and `Ask` resolution apply exactly as for any other tool). A server that fails to spawn is logged and
skipped (additive, never fatal) — the same posture as `mcp-grep`/`mcp-git` today.

### Engine `cmd_run` wiring (`crates/engine/src/main.rs`)

After `register_skills` / `register_hooks`:

- Plan A needs no extra wiring beyond discovery already producing namespaced agents/commands/skills/hooks
  (skills register through the existing `register_skills`; hooks through `register_hooks`; commands/agents
  flow through the existing `--command`/`--agent` lookups using their namespaced names).
- Plan B: iterate `ext.mcp_servers`, `connect_plugin_server` each, push the live `McpConnection` onto the
  same `_mcp_conns` vec that keeps the otto MCP children alive for the process lifetime, and register the
  namespaced tools.

With no `.claude/plugins/` present, `ext.mcp_servers` is empty and nothing is spawned — the spine's tool
set is byte-for-byte unchanged.

## Security & determinism properties

- **Enable-gated, no implicit execution.** A plugin runs **only** if `enabledPlugins["name@marketplace"]`
  is `true`. Cloning/placing a marketplace on disk never auto-activates its code; discovery is
  side-effect-free (filesystem reads + JSON parsing only).
- **Gate-first, plugins lowest precedence.** Plugin-contributed tools (including bundled MCP tools) route
  through `ToolRegistry` → `PermissionGate` exactly like every other tool; the inviolable sensitive-path
  floor applies. A plugin can never override a user/project artifact of the same name, and bundled MCP
  tool names are namespaced so they cannot impersonate `fs.write`/`bash`/`git.*`.
- **Plugin hooks compose below the gate.** Reusing the slice-4 `HookedTool` mechanism, a plugin hook can
  only further-restrict (exit 2 blocks) an already-`Allow`ed call; it can never widen one.
- **Sandboxed hook execution.** Plugin hook commands run through the same `SandboxedHookExecutor`
  (`SandboxPolicy::Os`) as user/project hooks; no unsandboxed path is introduced. `${CLAUDE_PLUGIN_ROOT}`
  is a textual substitution, not an environment read.
- **Bundled MCP servers are spawned, not linked**, via the same stdio `connect` path as otto's own MCP
  servers — additive and fail-open-to-skip (a server that won't spawn is logged and omitted, never fatal).
- **Hermetic + deterministic.** `home` is an explicit parameter; the `extensions` crate spawns nothing
  (MCP specs are pure data handed to the engine). With no `.claude/plugins/`, `Extensions` is unchanged
  and the offline determinism suite is untouched.

## Testing

- **`parse_marketplace_json`** (pure): full `marketplace.json` → name + entries; string `source` →
  `LocalPath`, object `source` → `Remote`; missing `name`/`plugins` → `Err`; empty `plugins` → `Ok`;
  malformed JSON → `Err`; unknown keys ignored.
- **`parse_plugin_json`** (pure): required `name`; optional component overrides populate; absent fields →
  `None` (convention applies); `mcpServers` as path vs. inline object; missing `name` → `Err`; unknown
  keys ignored.
- **`parse_enabled_plugins`** (pure): present map parsed; missing → empty; project overrides user for the
  same `"name@marketplace"` key.
- **`expand_plugin_root`** (pure): every `${CLAUDE_PLUGIN_ROOT}` replaced by the absolute plugin root;
  strings without the token are unchanged; multiple occurrences all replaced.
- **discovery / folding** (hermetic `home`+`project` tempdirs):
  - An enabled plugin's commands/agents/skills appear **namespaced** (`foo:commit`, `foo:agent`,
    `foo:skill`); a plugin **not** in `enabledPlugins` (or set `false`) contributes nothing.
  - A user/project artifact of the same final name **wins** over a plugin's.
  - Plugin hooks are appended (additive) with `${CLAUDE_PLUGIN_ROOT}` expanded in the command.
  - A remote `source` entry, or a `LocalPath` whose dir is absent, is skipped with a warning while a valid
    sibling plugin is kept.
  - Malformed `marketplace.json`/`plugin.json`/component → skipped, never fatal; missing
    `plugins/marketplaces/` → `Extensions::default()` (no plugins).
  - Component path overrides in `plugin.json` are honored over the convention dirs.
- **`PluginMcpServer` parsing** (Plan B): `.mcp.json` path and inline `mcpServers` both → specs with
  `${CLAUDE_PLUGIN_ROOT}` expanded in `command`/`args`/`env`.
- **`connect_plugin_server`** (engine): a bogus command errors (matches the existing `connect_*` bogus-bin
  tests); over a stub stdio MCP server, its tools register with namespaced gate names.
- **engine `cmd_run`** (hermetic `home`): with an enabled plugin under a tempdir
  `.claude/plugins/marketplaces/`, the built registry/command/agent set includes the namespaced
  artifacts; with no `.claude/plugins/`, the registry is unchanged and the offline determinism suite stays
  green.

## What this unblocks

With plugins fanning out to every existing artifact seam plus bundled MCP, the remaining `extensions`
surface is the **permissions** slice (`settings.json` permissions composed into the Layer-2 gate, where
command/skill/agent `allowed-tools` stops being inert) and the cross-cutting **serve-path wiring** of all
discovered artifacts. The network *install action* (marketplace `git clone`, lockfile, `/plugin` UX) is a
separable mechanics slice layered on the discovery this slice establishes.
