# otto Plugin Marketplace Install Action — Design

**Status:** Approved design.
**Date:** 2026-07-06.

## Why this document

`ARCHITECTURE.md` ("Claude Code compatibility") and the Slice 5 plugins design
(`docs/superpowers/specs/2026-06-26-extensions-plugins-design.md`) both call out the same
deferral: plugin **discovery** (Plan A) and bundled **MCP server** wiring (Plan B) operate only
over marketplaces already materialized on disk under `.claude/plugins/marketplaces/`. Getting a
marketplace onto disk in the first place — "the network install action (marketplace `git clone`,
lockfile)" — was explicitly out of scope for that slice. This document designs that action.

Unlike `marketplace.json`/`plugin.json` parsing, which must match Claude Code's on-disk schema so
real marketplaces work with otto, the **install mechanics** (CLI verbs, lockfile shape) are otto's
own design — Claude Code's actual `/plugin` implementation is a private detail we are not
replicating, only its file formats and enable-gate semantics.

## Scope

A CLI-operator-only, pre-session action: a developer runs `otto plugin ...` from a shell to manage
marketplaces and enabled plugins under `~/.claude/`. It never runs mid-turn, is never agent-facing,
and does not route through `ToolRegistry`/`PermissionGate` — those gate *agent-initiated* tool
calls during a turn; this is an operator running a command, exactly like `git clone` itself.

**In scope this slice:**

1. `otto plugin marketplace add|remove|update|list` — clone/manage marketplace repos under
   `~/.claude/plugins/marketplaces/<name>/`, tracked in a lockfile.
2. `otto plugin install|uninstall <plugin>@<marketplace>` — flip the `enabledPlugins` allowlist in
   `~/.claude/settings.json`, merge-preserving every other top-level key.
3. `otto plugin list` — show discovered plugins across installed marketplaces plus enabled state.

**Out of scope (deferred to a later slice, consistent with how prior slices deferred mechanics):**

- Materializing `PluginSource::Remote` entries (an individual plugin whose code lives in a repo
  separate from its marketplace). `install` errors out on these today, same posture as discovery's
  existing skip-with-warning.
- Project-level marketplace installs (`.claude/plugins/marketplaces/` relative to a project root).
  Discovery already reads project-level marketplaces; the CLI just never writes there. A project
  can still vendor a marketplace manually.
- An interactive `/plugin` TUI/REPL.
- Any serve-path exposure. This is a CLI-only action.

*(All three install-side deferrals shipped since: remote-source materialization 2026-07-10; the
interactive `/plugin` TUI and project-level installs 2026-07-30.)*

## Code layout

Following the existing split — pure discovery/parsing in `extensions`, OS/process-touching code at
"the CLI edge" in `engine` (mirroring `mcp.rs` for spawning and `microvm_config_from_env` for
env-reading):

- **`crates/extensions/src/marketplace_install.rs`** (new): pure logic, no filesystem/process I/O.
  - `MarketplaceLock { url, ref_: String, commit: String, updated_at: String }` +
    `MarketplaceLockfile(BTreeMap<String, MarketplaceLock>)` with `parse`/`to_json` (stable
    `BTreeMap` key order for git-diff-friendly output).
  - `set_enabled_plugin(settings_json: &str, key: &str, enabled: Option<bool>) -> String` — given
    existing `settings.json` content (or `"{}"`), an `"<plugin>@<marketplace>"` key, and
    `Some(true)`/`Some(false)`/`None` (insert-true, insert-false, or remove the key), returns the
    rewritten JSON with every other top-level key untouched and `enabledPlugins` keys sorted.
  - Resolves a marketplace `--name`/basename from a URL (pure string logic).
- **`crates/engine/src/plugin_cli.rs`** (new): `cmd_plugin(args: Vec<String>)` dispatched from
  `main()` alongside `cmd_run`/`cmd_serve`; the actual `git clone`/`git fetch`+reset subprocess
  calls (with duplicated hardening — see below); reads/writes the real
  `~/.claude/plugins/marketplaces.lock.json` and `~/.claude/settings.json` files via
  `marketplace_install`'s pure functions.

## Git hardening (duplicated, not shared)

`mcp-git`'s `do_clone` already validates URLs (scheme allowlist blocking `ext::`/`fd::`/bare
relative paths, plus scp-like SSH syntax) and rejects leading-dash argv-injection attempts before
shelling to `git clone -- <url> <dir>`. `mcp-git` is a `[[bin]]`-only crate — `CLAUDE.md`'s
architecture rule is explicit that MCP tool crates are standalone binaries the engine only talks to
over stdio, never by linking. Adding a lib target to `mcp-git` just to share ~30 lines would break
that invariant, so `plugin_cli.rs` duplicates `validate_clone_url`/`reject_leading_dash` (both
covered by the same style of unit tests `mcp-git` already has: bad-scheme URLs, leading-dash
url/dir, path escape).

## Marketplace commands

### `otto plugin marketplace add <url> [--name <alias>] [--ref <branch|tag|sha>]`

1. Validate `<url>` (hardening above).
2. Resolve the target name: `--name` if given, else the URL's basename (strip a trailing `.git`).
3. Error if `~/.claude/plugins/marketplaces/<name>/` already exists ("already installed; use
   `update` instead").
4. `git clone -- <url> <ref-checkout-if-given> <target-dir>` (a `--ref` clones default branch then
   checks out the ref, since `git clone -b` doesn't accept arbitrary commit SHAs).
5. Verify `<target-dir>/.claude-plugin/marketplace.json` exists and parses via
   `parse_marketplace_json` — on failure, **remove the partial clone** and error (never leave a
   half-installed marketplace directory behind).
6. Resolve HEAD's commit sha, write a `MarketplaceLock` entry (`url`, resolved `ref` — the given
   `--ref` or `"HEAD"`/the default branch name, `commit`, `updated_at` = now), merge into the
   lockfile.

### `otto plugin marketplace remove <name>`

Delete `~/.claude/plugins/marketplaces/<name>/` and its lockfile entry. Does **not** scrub any
`enabledPlugins` keys referencing that marketplace — they become inert (discovery finds no matching
directory and folds nothing), a documented limitation rather than a cross-cutting settings.json
cleanup.

### `otto plugin marketplace update [<name>]`

For the named marketplace (or every locked marketplace if omitted): `git fetch`, then reset to the
lockfile's recorded `ref` at its remote tip (`git reset --hard origin/<ref>` for a branch name, or
re-checkout for a pinned tag/sha — a moving branch ref re-resolves, a pinned sha/tag is a no-op
fetch-and-confirm). Refresh `commit`/`updated_at` in the lockfile. A marketplace directory that's
been manually deleted out from under the lockfile is reported and skipped (never fatal to the rest
of the batch).

### `otto plugin marketplace list`

Print each locked marketplace: name, url, ref, commit (short sha), updated_at.

## Install / uninstall

### `otto plugin install <plugin>@<marketplace>`

1. Look up `<marketplace>` in the lockfile (error if unknown).
2. Parse its `marketplace.json`, find the `<plugin>` entry (error if unknown).
3. If the entry's `source` is `PluginSource::Remote`, error: "remote-sourced plugin install isn't
   supported yet — this plugin's code lives outside its marketplace repo." (Matches the deferred
   scope above.)
4. Otherwise (`LocalPath`, resolvable on disk after the marketplace clone): read
   `~/.claude/settings.json` (or `"{}"`), call
   `set_enabled_plugin(json, "<plugin>@<marketplace>", Some(true))`, write it back.

### `otto plugin uninstall <plugin>@<marketplace>`

Same lookup (marketplace/plugin existence checked so typos are caught), then
`set_enabled_plugin(json, key, None)` to remove the key entirely (rather than writing `false` —
keeps `settings.json` from accumulating dead entries for uninstalled plugins).

### `otto plugin list`

For every locked marketplace, list its plugins with an "enabled"/"available" marker sourced from
the merged `enabledPlugins` map (reusing the existing `parse_enabled_plugins` reader from Slice 5).

## Error handling

Consistent with the rest of the CLI (`cmd_run`/`cmd_serve`): print a clear `error: ...` message to
stderr and exit non-zero. No partial state is left behind on failure — a failed clone removes its
target directory; a failed lockfile/settings write is all-or-nothing (write to a temp file in the
same directory, then atomic rename).

## Security & determinism properties

- **No implicit execution.** `install` only ever flips an allowlist entry; it never runs plugin
  code, mirroring the existing "enable-gated, no implicit execution" property from Slice 5.
- **Operator-only, no gate bypass.** This path never touches `ToolRegistry`/`PermissionGate` — it
  runs before any session/turn exists, so there is nothing to gate. It does not create a new way
  for an agent to reach the network or spawn `git`.
- **Hardened clone.** Same URL-scheme allowlist + argv-injection rejections as `mcp-git`, applied
  independently at this call site.
- **Hermetic core, I/O at the edge.** `marketplace_install`'s lockfile/settings-merge functions are
  pure (take/return strings, no filesystem or clock reads) and unit-testable without touching disk;
  `plugin_cli.rs` in `engine` owns real file I/O, process spawning, and wall-clock reads
  (`updated_at`). This path is never exercised by `cargo test --workspace`'s offline determinism
  suite (no plugin install commands run there today), so it introduces no new nondeterminism risk
  to CI.

## Testing

- **`marketplace_install`** (pure, `extensions`): lockfile JSON round-trips, sorted key order;
  `set_enabled_plugin` inserts/removes/updates a key while preserving unrelated top-level keys
  (hooks, permissions) and other `enabledPlugins` entries; empty/missing input treated as `"{}"`.
- **`plugin_cli`** (`engine`, hermetic against a local bare git repo, same pattern as `mcp-git`'s
  `clone_from_local_bare_remote`):
  - `add` clones a local bare repo, writes a correct lockfile entry, and a subsequent `add` of the
    same name errors.
  - `add` against a repo lacking `marketplace.json` cleans up the partial clone and errors.
  - Bad-scheme URLs and leading-dash url/dir args are rejected (mirrors `mcp-git`'s
    `clone_rejects_flag_and_bad_scheme_urls`/`clone_rejects_escaping_dir`).
  - `update` fetches new commits from the bare remote and refreshes the lockfile's `commit`.
  - `remove` deletes the directory and lockfile entry; a stale `enabledPlugins` key is left as-is
    (asserted, not treated as a bug).
  - `install`/`uninstall` round-trip through a real `settings.json`, preserving unrelated keys;
    `install` of a `Remote`-sourced entry errors with the expected message; `install` of an unknown
    plugin/marketplace errors.
  - `list` commands render the expected discovered/locked state.

## What this unblocks

With marketplaces installable end to end, the plugins artifact (Slice 5) is fully closed —
discovery, bundled MCP servers, and now acquisition. Remaining `extensions`/Claude-Code-compat
threads (per `ARCHITECTURE.md`) are: `PluginSource::Remote` per-plugin materialization, hooks'
lifecycle/JSON-stdout control/regex matchers/`settings.local.json`, and the standalone `cli` crate
split called out in the crate layout table.
