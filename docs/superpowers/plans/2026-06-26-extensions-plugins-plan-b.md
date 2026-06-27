# Extensions Plugins — Plan B (bundled MCP servers) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the plugins slice by routing each enabled plugin's bundled MCP servers
(`.mcp.json` file or an inline `mcpServers` object in `plugin.json`) into otto's existing MCP client.
The `extensions` crate parses each server config into a pure-data `PluginMcpServer` spec (with
`${CLAUDE_PLUGIN_ROOT}` expanded and namespaced by plugin name), and the engine spawns + registers
each through the existing stdio `connect` path, behind the permission gate with namespaced tool
names that cannot impersonate a built-in (`fs.*`/`bash`/`git.*`).

**Architecture:** Two crates, no new dependencies.

- `otto-extensions` stays hermetic — it spawns nothing. `plugin_def.rs` gains a `PluginMcpServer`
  pure-data struct, an `McpServersField` enum (path-or-inline), an `mcp_servers` field on
  `PluginManifest`, and a pure `parse_mcp_servers(servers, namespace)` function. `lib.rs` adds
  `mcp_servers: Vec<PluginMcpServer>` to `Extensions`, threads a `&mut Vec<PluginMcpServer>` through
  `fold_plugins`/`fold_one_plugin`, and in `fold_one_plugin` resolves the manifest's
  `mcpServers` field (path → read file; inline → use value; absent → convention `.mcp.json` if
  present), parses it, and expands `${CLAUDE_PLUGIN_ROOT}` in each spec's `command`/`args`/`env`/`cwd`
  via the existing `expand_plugin_root`.
- `otto-engine` gains `connect_plugin_server(spec)` in `mcp.rs`, which builds a
  `tokio::process::Command` from the spec and reuses the existing generic `connect`, but maps each
  advertised tool to a namespaced gate name `plugin__{namespace}__{server_key}__{tool}`. `cmd_run`
  iterates `ext.mcp_servers`, connects each, registers the namespaced tools, and keeps the
  connections alive for the process lifetime; a server that won't spawn is logged and skipped
  (additive, never fatal). Re-exported from `engine/lib.rs` as `mcp_connect_plugin_server`.

With no `.claude/plugins/` present, `ext.mcp_servers` is empty, the `cmd_run` loop does nothing, and
the spine's tool set is byte-for-byte unchanged — the offline determinism suite is untouched.

**Tech Stack:** Rust (edition 2024), `serde_json` for parsing, `tokio::process::Command` +
`rmcp` for the stdio connect, `tempfile` for hermetic discovery tests, `anyhow` for fallible work.

**Source of truth:** `docs/superpowers/specs/2026-06-26-extensions-plugins-design.md` (Approved
design — the "Plan B — bundled MCP servers" scope, the `PluginMcpServer`/`McpServersField` shapes,
and the engine-wiring + security/determinism sections).

---

## File Structure

- **Modify `crates/extensions/src/plugin_def.rs`** — add `McpServersField`, `PluginMcpServer`, the
  `mcp_servers: Option<McpServersField>` field on `PluginManifest`, parse it in `parse_plugin_json`,
  and add the pure `parse_mcp_servers(servers: &Value, namespace: &str) -> Vec<PluginMcpServer>`.
- **Modify `crates/extensions/src/lib.rs`** — re-export the new `plugin_def` items; add
  `mcp_servers` to `Extensions`; thread `&mut Vec<PluginMcpServer>` through
  `fold_plugins`/`fold_one_plugin`; resolve + parse + `${CLAUDE_PLUGIN_ROOT}`-expand each plugin's
  MCP servers in `fold_one_plugin`; populate `Extensions { mcp_servers, .. }` in `discover`.
- **Modify `crates/engine/src/mcp.rs`** — factor the tool-name mapping out of `connect` into a
  private `connect_mapped`; add the pure `plugin_gate_name(ns, key, tool)` and the public
  `connect_plugin_server(spec)`.
- **Modify `crates/engine/src/lib.rs`** — re-export `connect_plugin_server as mcp_connect_plugin_server`.
- **Modify `crates/engine/src/main.rs`** — in `cmd_run`, after `register_hooks`, iterate
  `ext.mcp_servers`, `mcp_connect_plugin_server` each, register the namespaced tools, and push the
  connection onto the kept-alive vec.
- **Modify `docs/ARCHITECTURE.md` and `CLAUDE.md`** — record Plan B's shipped behavior in the
  plugins compatibility row / extensions paragraph.

---

## Task 1: `plugin_def` — `PluginMcpServer`, `McpServersField`, manifest field, `parse_mcp_servers`

**Files:**
- Modify: `crates/extensions/src/plugin_def.rs`
- Modify: `crates/extensions/src/lib.rs` (re-exports only)

- [ ] **Step 1: Write the failing tests**

In `plugin_def.rs` `#[cfg(test)] mod tests`, add (and update the existing
`mcp_servers_field_is_ignored_this_plan` test — it must now assert the field *is* parsed):

```rust
#[test]
fn parses_mcp_servers_path_field() {
    let m = parse_plugin_json(r#"{"name":"foo","mcpServers":"./.mcp.json"}"#).unwrap();
    assert_eq!(m.mcp_servers, Some(McpServersField::Path("./.mcp.json".to_string())));
}

#[test]
fn parses_mcp_servers_inline_field() {
    let m = parse_plugin_json(
        r#"{"name":"foo","mcpServers":{"s":{"command":"node"}}}"#,
    )
    .unwrap();
    assert!(matches!(m.mcp_servers, Some(McpServersField::Inline(_))));
}

#[test]
fn absent_mcp_servers_is_none() {
    let m = parse_plugin_json(r#"{"name":"foo"}"#).unwrap();
    assert_eq!(m.mcp_servers, None);
}

#[test]
fn parse_mcp_servers_maps_each_server() {
    // The map of server_key -> config (the value under "mcpServers").
    let v: serde_json::Value = serde_json::from_str(
        r#"{"my-server":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/s.js","--x"],
             "env":{"FOO":"bar"},"cwd":"${CLAUDE_PLUGIN_ROOT}"}}"#,
    )
    .unwrap();
    let specs = parse_mcp_servers(&v, "foo");
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    assert_eq!(s.namespace, "foo");
    assert_eq!(s.server_key, "my-server");
    assert_eq!(s.command, "node");
    assert_eq!(s.args, vec!["${CLAUDE_PLUGIN_ROOT}/s.js", "--x"]); // un-expanded here; fold expands
    assert_eq!(s.env.get("FOO").map(String::as_str), Some("bar"));
    assert_eq!(s.cwd.as_deref(), Some("${CLAUDE_PLUGIN_ROOT}"));
}

#[test]
fn parse_mcp_servers_skips_server_without_command() {
    let v: serde_json::Value =
        serde_json::from_str(r#"{"good":{"command":"node"},"bad":{"args":["x"]}}"#).unwrap();
    let specs = parse_mcp_servers(&v, "foo");
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].server_key, "good");
}

#[test]
fn parse_mcp_servers_defaults_args_env_cwd() {
    let v: serde_json::Value = serde_json::from_str(r#"{"s":{"command":"x"}}"#).unwrap();
    let s = &parse_mcp_servers(&v, "ns")[0];
    assert!(s.args.is_empty());
    assert!(s.env.is_empty());
    assert_eq!(s.cwd, None);
}
```

Run `cargo test -p otto-extensions plugin_def::` — fails to compile (`McpServersField`,
`PluginMcpServer`, `parse_mcp_servers`, the `mcp_servers` field don't exist).

- [ ] **Step 2: Implement the types + parsing**

In `plugin_def.rs`:

```rust
use std::collections::BTreeMap;

/// A bundled MCP server config, resolved to pure data (no process spawned here). `command`/`args`/
/// `env`/`cwd` are stored verbatim from the manifest; `${CLAUDE_PLUGIN_ROOT}` expansion happens at
/// fold time (in `lib.rs`, where the plugin root is known).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMcpServer {
    pub namespace: String,   // the plugin name, for tool-name prefixing
    pub server_key: String,  // the key under "mcpServers"
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

/// How a plugin declares its MCP servers: a path to a JSON file (relative to the plugin root) or an
/// inline object (the value of the `mcpServers` key in `plugin.json`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServersField {
    Path(String),
    Inline(Value),
}
```

Add `pub mcp_servers: Option<McpServersField>` to `PluginManifest` (after `hooks`). In
`parse_plugin_json`, after the component fields:

```rust
let mcp_servers = match v.get("mcpServers") {
    Some(Value::String(s)) if !s.is_empty() => Some(McpServersField::Path(s.clone())),
    Some(Value::Object(o)) => Some(McpServersField::Inline(Value::Object(o.clone()))),
    _ => None,
};
```

(set it in the returned `PluginManifest`).

Add the pure parser. `servers` is the map of `server_key → config`:

```rust
/// Parse a map of `server_key -> config` into `PluginMcpServer` specs, namespaced by `namespace`.
/// A server missing a non-empty `command` is skipped. `args` defaults to empty, `env` to empty,
/// `cwd` to `None`. Values are stored verbatim — `${CLAUDE_PLUGIN_ROOT}` is expanded by the caller.
pub fn parse_mcp_servers(servers: &Value, namespace: &str) -> Vec<PluginMcpServer> {
    let Some(map) = servers.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (server_key, cfg) in map {
        let Some(command) = cfg
            .get("command")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let args = cfg
            .get("args")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let env = cfg
            .get("env")
            .and_then(|e| e.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let cwd = cfg
            .get("cwd")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        out.push(PluginMcpServer {
            namespace: namespace.to_string(),
            server_key: server_key.clone(),
            command: command.to_string(),
            args,
            env,
            cwd,
        });
    }
    out
}
```

`BTreeMap` (not `HashMap`) keeps server/env ordering deterministic — a test invariant for this repo.

Also update the now-stale doc comments in `plugin_def.rs` that say `mcpServers` is "Plan B / not
parsed yet / ignored" (the module header, lines 1–4, and the `parse_plugin_json` doc, lines 22–23) —
they describe Plan-A behavior that this task supersedes.

Run `cargo test -p otto-extensions plugin_def::` — passes.

- [ ] **Step 3: Re-export from `lib.rs`**

Change the `plugin_def` re-export line to:

```rust
pub use plugin_def::{McpServersField, PluginManifest, PluginMcpServer, parse_mcp_servers, parse_plugin_json};
```

Run `cargo build -p otto-extensions`. Commit: `feat(extensions): parse bundled MCP server specs from plugin manifests (plugins slice B)`.

---

## Task 2: fold bundled MCP servers into `Extensions`

**Files:**
- Modify: `crates/extensions/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

In `lib.rs` `#[cfg(test)] mod tests`, add hermetic discovery tests. Reuse the existing helpers that
set up a marketplace + enabled plugin (see the Plan A tests around the
`${CLAUDE_PLUGIN_ROOT}` hook test, ~line 691–810, for the exact tempdir scaffolding to copy). Add:

```rust
#[test]
fn enabled_plugin_mcp_server_from_dot_mcp_json_is_folded_namespaced_and_expanded() {
    // Scaffold: home/.claude/plugins/marketplaces/acme/ with marketplace.json listing plugin "foo"
    // (source "./plugins/foo"), enabledPlugins {"foo@acme": true}, plugin.json {"name":"foo"} (no
    // mcpServers override → convention), and plugins/foo/.mcp.json with one server whose args use
    // ${CLAUDE_PLUGIN_ROOT}.
    // ... (build with the same fs helpers Plan A's plugin tests use) ...
    let ext = discover(proj.path(), home.path());
    assert_eq!(ext.mcp_servers.len(), 1);
    let s = &ext.mcp_servers[0];
    assert_eq!(s.namespace, "foo");
    assert_eq!(s.server_key, "my-server");
    // ${CLAUDE_PLUGIN_ROOT} expanded to the absolute plugin root in args; no token remains.
    assert!(s.args.iter().all(|a| !a.contains("${CLAUDE_PLUGIN_ROOT}")));
    assert!(s.args.iter().any(|a| a.contains("plugins") && a.ends_with("s.js")));
}

#[test]
fn inline_mcp_servers_in_plugin_json_are_folded() {
    // plugin.json carries "mcpServers": { "s": { "command": "node", ... } } directly (no .mcp.json).
    let ext = discover(proj.path(), home.path());
    assert_eq!(ext.mcp_servers.len(), 1);
    assert_eq!(ext.mcp_servers[0].server_key, "s");
}

#[test]
fn no_plugins_yields_no_mcp_servers() {
    let home = tempdir().unwrap();
    let proj = tempdir().unwrap();
    assert!(discover(proj.path(), home.path()).mcp_servers.is_empty());
}

#[test]
fn disabled_plugin_contributes_no_mcp_servers() {
    // Same scaffold as the first test but enabledPlugins {"foo@acme": false}.
    let ext = discover(proj.path(), home.path());
    assert!(ext.mcp_servers.is_empty());
}
```

Run `cargo test -p otto-extensions` — fails (`Extensions` has no `mcp_servers` field).

- [ ] **Step 2: Add the field + thread it through the fold**

- Add `pub mcp_servers: Vec<PluginMcpServer>` to `Extensions` (after `hooks`).
- In `discover`, declare `let mut mcp_servers: Vec<PluginMcpServer> = Vec::new();`, pass
  `&mut mcp_servers` into `fold_plugins`, and set `mcp_servers` in the returned `Extensions`.
- Add the `&mut Vec<PluginMcpServer>` parameter to `fold_plugins` and `fold_one_plugin` signatures
  and the `fold_one_plugin(...)` call site.
- In `fold_one_plugin`, after the hooks block, resolve and fold MCP servers:

```rust
// Bundled MCP servers (Plan B): path override, inline object, or the convention `.mcp.json`.
let raw: Option<serde_json::Value> = match &manifest.mcp_servers {
    Some(McpServersField::Inline(v)) => Some(v.clone()),
    Some(McpServersField::Path(p)) => {
        read_json_file(&plugin_root.join(p.trim_start_matches("./")))
    }
    None => read_json_file(&plugin_root.join(".mcp.json")),
};
if let Some(v) = raw {
    // Tolerate both the `.mcp.json` wrapper ({"mcpServers": {...}}) and a bare server map.
    let servers = v.get("mcpServers").unwrap_or(&v);
    for mut spec in parse_mcp_servers(servers, ns) {
        spec.command = expand_plugin_root(&spec.command, plugin_root);
        spec.args = spec
            .args
            .iter()
            .map(|a| expand_plugin_root(a, plugin_root))
            .collect();
        spec.env = spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), expand_plugin_root(v, plugin_root)))
            .collect();
        spec.cwd = spec.cwd.map(|c| expand_plugin_root(&c, plugin_root));
        mcp_servers.push(spec);
    }
}
```

Add the private reader (mirrors the silent-on-missing posture of `read_plugin_hooks`):

```rust
/// Read + parse a JSON file. Missing file → `None` (silent, convention default may be absent);
/// unreadable-but-present or malformed → `None` with a warning. Never fatal.
fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("warning: skipping malformed {}: {e}", path.display());
            None
        }
    }
}
```

(If a `read_json_file`-equivalent already exists, reuse it.) MCP servers are **additive** (pushed,
not deduped) — consistent with how hooks fold; the engine namespaces tool names so distinct servers
never collide at the gate.

Run `cargo test -p otto-extensions` — all pass. Commit:
`feat(extensions): fold enabled-plugin MCP server specs into Extensions (plugins slice B)`.

---

## Task 3: engine `connect_plugin_server` + namespaced gate names

**Files:**
- Modify: `crates/engine/src/mcp.rs`
- Modify: `crates/engine/src/lib.rs` (re-export)

- [ ] **Step 1: Write the failing tests**

In `mcp.rs` `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn plugin_gate_name_is_namespaced() {
    assert_eq!(
        super::plugin_gate_name("foo", "my-server", "search"),
        "plugin__foo__my-server__search"
    );
}

#[tokio::test]
async fn connect_plugin_server_with_bogus_command_errors() {
    use otto_extensions::PluginMcpServer;
    let spec = PluginMcpServer {
        namespace: "foo".into(),
        server_key: "s".into(),
        command: "definitely-not-a-real-binary-xyz".into(),
        args: vec![],
        env: Default::default(),
        cwd: None,
    };
    assert!(connect_plugin_server(&spec).await.is_err());
}
```

Run `cargo test -p otto-engine mcp::` — fails to compile.

- [ ] **Step 2: Implement**

Factor the existing `connect` so the tool-name mapping is injectable, then add the plugin variant:

```rust
/// Core connect: spawn `command`, list tools, map each server tool name to a gate name via `map`.
async fn connect_mapped(
    command: tokio::process::Command,
    map: impl Fn(&str) -> String,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let transport = TokioChildProcess::new(command)?;
    let service = Arc::new(().serve(transport).await?);
    let tools = service.peer().list_all_tools().await?;
    let mcp_tools: Vec<Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| {
            let server_name = t.name.to_string();
            let gate_name = map(&server_name);
            Arc::new(McpTool {
                service: Arc::clone(&service),
                server_name,
                gate_name,
            }) as Arc<dyn Tool>
        })
        .collect();
    Ok((McpConnection { service }, mcp_tools))
}

pub async fn connect(
    command: tokio::process::Command,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    connect_mapped(command, |n| to_gate_name(n)).await
}

/// Namespaced gate name for a plugin-bundled MCP tool. Distinct from otto's `fs.*`/`bash`/`git.*`
/// so a plugin server can never shadow or be confused with a built-in tool by the gate.
fn plugin_gate_name(namespace: &str, server_key: &str, tool: &str) -> String {
    format!("plugin__{namespace}__{server_key}__{tool}")
}

/// Spawn a plugin-bundled MCP server from its spec and register each advertised tool under a
/// namespaced gate name. The spec's `command`/`args`/`env`/`cwd` are already
/// `${CLAUDE_PLUGIN_ROOT}`-expanded by discovery.
pub async fn connect_plugin_server(
    spec: &otto_extensions::PluginMcpServer,
) -> anyhow::Result<(McpConnection, Vec<Arc<dyn Tool>>)> {
    let mut command = tokio::process::Command::new(&spec.command);
    command.args(&spec.args);
    for (k, v) in &spec.env {
        command.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let ns = spec.namespace.clone();
    let key = spec.server_key.clone();
    connect_mapped(command, move |tool| plugin_gate_name(&ns, &key, tool)).await
}
```

`otto-engine` already depends on `otto-extensions` (confirmed in `engine/Cargo.toml`), so the
`PluginMcpServer` import needs no new dependency.

Run `cargo test -p otto-engine mcp::` — passes.

- [ ] **Step 3: Re-export**

In `engine/src/lib.rs`, add `connect_plugin_server as mcp_connect_plugin_server` to the
`pub use mcp::{...}` block. Run `cargo build -p otto-engine`. Commit:
`feat(engine): connect_plugin_server spawns bundled plugin MCP servers with namespaced tools (plugins slice B)`.

---

## Task 4: wire bundled MCP servers into `cmd_run`

**Files:**
- Modify: `crates/engine/src/main.rs`

- [ ] **Step 1: Implement the wiring**

In `cmd_run`, change the binding to keep the connection vec mutable:

```rust
let (mut tools, mut mcp_conns) =
    build_tools_preferring_mcp(tools_workspace, root.clone(), false).await;
```

(rename the `_mcp_conns` binding to `mcp_conns` in `cmd_run` only — it is now used). After
`register_hooks(&mut tools, &ext.hooks, &root);` and before `let tools = Arc::new(tools);`:

```rust
// Bundled plugin MCP servers (Plan B): spawn each enabled plugin's servers through the same
// stdio connect path otto uses for its own MCP servers; register the namespaced tools behind the
// gate. A server that won't spawn is logged and skipped (additive, never fatal). With no
// `.claude/plugins/`, `ext.mcp_servers` is empty and the tool set is byte-for-byte unchanged.
for spec in &ext.mcp_servers {
    match mcp_connect_plugin_server(spec).await {
        Ok((conn, mcp_tools)) => {
            for t in mcp_tools {
                tools.register(t);
            }
            mcp_conns.push(conn);
        }
        Err(e) => eprintln!(
            "plugin mcp server {}:{} unavailable ({e}); skipping",
            spec.namespace, spec.server_key
        ),
    }
}
```

Add `mcp_connect_plugin_server` to the `use otto_engine::{...}` import at the top of `main.rs`.
Update the existing `// _mcp_conns is held until end of function ...` comment to refer to `mcp_conns`.

Plugin MCP tools register **after** `register_hooks`, so they are gate-guarded (the primary
boundary) but not hook-wrapped this slice — matching the design's stated `cmd_run` ordering and the
slice-by-slice deferral pattern (hook matchers target built-in tool names like `bash`/`fs.write`,
not the namespaced `plugin__…` names). Only the main `otto run` spine is wired; the
`--command`/`--agent`/serve subpaths stay deferred, exactly as skills and hooks are.

- [ ] **Step 2: Verify the no-plugins path is unchanged**

Run `cargo test -p otto-engine` and `cargo test --workspace`. The offline determinism suite must
stay green (no `.claude/plugins/` in the test workspaces → empty `ext.mcp_servers` → no spawning).

Commit: `feat(engine): wire bundled plugin MCP servers into the otto run spine (plugins slice B)`.

> **Testing scope note (matches prior slices):** `cmd_run` and `build_tools_preferring_mcp` are
> spawn-heavy glue and are not unit-tested in this repo (the MCP `connect_*` functions are tested
> only via their bogus-binary error path; there are no stub stdio MCP servers in the test suite).
> Plan B follows that precedent: the discovery→spec path is fully covered in `otto-extensions`
> (pure + hermetic fs), the spawn/namespacing logic is covered in `mcp.rs`
> (`plugin_gate_name` pure test + `connect_plugin_server` bogus-command error), and the `cmd_run`
> loop is the same trivially-correct connect-register-or-skip glue the fs/grep/git/bash blocks
> already use.

---

## Task 5: docs

**Files:**
- Modify: `docs/ARCHITECTURE.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update both docs**

- `CLAUDE.md`: extend the `extensions` crate row's slice-5 sentence — Plan A folds static
  components; **Plan B** parses each enabled plugin's `.mcp.json`/inline `mcpServers` into
  `PluginMcpServer` specs (`${CLAUDE_PLUGIN_ROOT}` expanded, namespaced) and the engine spawns +
  registers them via `connect_plugin_server` (namespaced gate names `plugin__{ns}__{key}__{tool}`,
  gate-guarded, additive/fail-open-to-skip). Note the network install action remains deferred.
- `docs/ARCHITECTURE.md`: update the plugins compatibility row to reflect that bundled MCP servers
  now route into otto's MCP client (remove/adjust any "pending"/"Plan B" qualifier on that line).

No code; docs only. Commit: `docs(extensions): record plugins slice B (bundled MCP servers)`.

---

## Definition of Done

- [ ] `cargo test --workspace` green; `cargo test -p otto-extensions` and `cargo test -p otto-engine` green.
- [ ] `cargo fmt --all` clean; `cargo clippy --workspace --all-targets` clean.
- [ ] `parse_mcp_servers` (pure), the manifest `mcpServers` parse, discovery folding (path + inline +
      absent + disabled), `plugin_gate_name`, and `connect_plugin_server` bogus-command error are
      all covered by tests.
- [ ] With no `.claude/plugins/`, `Extensions::mcp_servers` is empty and the offline determinism
      suite is byte-for-byte unchanged.
- [ ] Namespaced gate names (`plugin__{ns}__{key}__{tool}`) cannot collide with `fs.*`/`bash`/`git.*`.
- [ ] Docs (`CLAUDE.md`, `ARCHITECTURE.md`) record the shipped behavior.

## Out of scope (deferred, consistent with the design + prior slices)

- The network **install action** (marketplace/plugin `git clone`, lockfile, `/plugin` UX).
- `model`/`allowed-tools` enforcement (gate stays the sole authority).
- Serve-path wiring and the `--command`/`--agent` subpaths for plugin MCP servers.
- Hook-wrapping of plugin MCP tools (they are gate-guarded; hook composition over them is a later
  slice).
