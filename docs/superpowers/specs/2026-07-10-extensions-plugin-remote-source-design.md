# Remote plugin source materialization

**Date:** 2026-07-10
**Status:** design approved, pre-plan

## Context

`otto plugin install|uninstall|list` and `otto plugin marketplace add|remove|update|list`
(shipped in the 2026-07-06 plugin-install slice) let a CLI operator clone Claude-Code-compatible
plugin marketplaces under `~/.claude/plugins/marketplaces/` and flip the `enabledPlugins`
allowlist in `~/.claude/settings.json`. That slice explicitly deferred **materializing
`PluginSource::Remote`** — a marketplace entry whose plugin code lives in a repo *separate* from
the marketplace repo, described by an object `source` rather than a local `./path` string.

Today `plugin_install` hard-errors on such an entry:

```
'<key>' is remote-sourced (its code lives outside its marketplace repo);
installing remote-sourced plugins isn't supported yet
```

Most real Claude Code marketplaces list their plugins as remote repos (`{"source":"github",
"repo":"owner/name"}` / `{"source":"git","url":"…"}`), so install currently works only for the
local-path minority. This slice makes remote-sourced install work end to end.

This is **slice 1 of 3** in the plugin-install follow-up decomposition. The other two —
project-level marketplace installs (writing under a project `.claude/` instead of `~/.claude/`)
and an interactive `/plugin` UX — are independent subsystems and get their own spec → plan →
implementation cycles later. *(Both shipped 2026-07-30: `--project`/`--root` scoping and the
interactive TUI, per the install slice's deferral.)*

## Goal

`otto plugin install <plugin>@<marketplace>` succeeds when the plugin's marketplace entry is a
github- or git-sourced remote: it clones the remote repo to a local cache, tracks it in the
lockfile, flips the enable bit, and `discover()` folds the materialized plugin's
agents/commands/skills/hooks/MCP-servers exactly as it does a local-path plugin.

## Non-goals (explicitly deferred)

- **Refreshing materialized plugin clones.** A materialized clone stays pinned at its install
  commit; `marketplace update` refreshes marketplaces only, not plugin repos. Re-materialize by
  `marketplace remove` + re-`install`. (Symmetric plugin-repo refresh is a clean follow-up.)
- **Remote `source` kinds beyond `github` and `git`.** Any other `source` value produces a clear
  error naming the unsupported kind.
- **Project-level and `/plugin` UX** — separate slices (see Context).
- **Any serve-path exposure.** `otto plugin …` remains a CLI-operator-only action, never
  agent-facing.

## Design

### A. Parse the remote descriptor — `extensions`, pure, no I/O

`PluginSource::Remote(Value)` keeps the raw JSON verbatim. Add to
`crates/extensions/src/marketplace_def.rs`:

```rust
/// A remote plugin source resolved to something `git clone` can consume.
pub struct RemoteClone {
    pub url: String,
    pub git_ref: Option<String>,
}

/// Resolve a `PluginSource::Remote` descriptor to a clone target, or error with a message
/// naming the unsupported shape. Pure — no I/O, no process spawn.
pub fn resolve_remote_source(src: &Value) -> anyhow::Result<RemoteClone>;
```

Mapping:

- `{"source":"github","repo":"<owner>/<name>", …}` → `url = "https://github.com/<owner>/<name>"`.
  `<owner>/<name>` must be exactly two non-empty, path-safe segments (no leading `-`, no `/`
  beyond the single separator, no `.`/`..` segment). A malformed `repo` errors.
- `{"source":"git","url":"<url>", …}` → `url` verbatim.
- Optional pin, first present wins in this order: `commit`, `tag`, `branch`, `ref` → `git_ref`.
  (Precedence documented on the function; a `commit` is the most specific, so it wins.)
- Any other `source` string (`"gitlab"`, …) or a shape missing its required field → `Err`
  naming what was unsupported. This replaces today's blanket "not supported yet".

Kept pure and in `extensions` to match the crate convention (all parsing takes strings/Values
and returns data; every disk/process touch lives at the CLI edge in `plugin_cli.rs`).

### B. On-disk layout

Materialized plugins clone into a new cache dir parallel to `marketplaces/`:

```
~/.claude/plugins/repos/<marketplace>/<plugin>/
```

Kept separate from `marketplaces/<name>/` (themselves git working trees) so a plugin clone never
pollutes a marketplace's tree. `<marketplace>` and `<plugin>` are validated path-safe single
components before use (reusing the `validate_marketplace_name` shape).

### C. Lockfile tracking — nested, back-compatible

The lockfile (`~/.claude/plugins/marketplaces.lock.json`) currently serializes a **flat** object
of `marketplace-name → {url,ref,commit,updated_at_unix}`. Extend `MarketplaceLockfile` to carry a
second map for materialized plugins, keyed by the `"<plugin>@<marketplace>"` enable-key:

```json
{
  "marketplaces": { "acme": {url,ref,commit,updated_at_unix} },
  "plugins":      { "foo@acme": {url,ref,commit,updated_at_unix} }
}
```

- `MarketplaceLockfile` gains a `plugins: BTreeMap<String, MarketplaceLock>` field (reusing the
  existing `MarketplaceLock` struct — same four fields).
- `to_json` writes the nested `{marketplaces, plugins}` shape (sorted keys in each map, still
  git-diff-friendly).
- `parse` stays tolerant and **back-compatible**: a top-level object containing `marketplaces`
  and/or `plugins` keys is read as the nested format; a flat top-level object with *neither* key
  is read as marketplaces-only (today's format), so any existing on-disk lockfile still loads.
  Malformed input still yields an empty lockfile.

Discovery does **not** read the lockfile (it only does an existence check on the repos dir — see
E); the plugins map is consumed solely by the CLI for cleanup and to record provenance.

### D. CLI wiring — `crates/engine/src/plugin_cli.rs`

**`plugin_install <plugin>@<marketplace>` on a `Remote` source:**

1. `resolve_remote_source(&entry.source)` → `RemoteClone { url, git_ref }`.
2. Harden: `validate_clone_url(&url)` + `reject_leading_dash` on any ref.
3. If `repos/<mp>/<plugin>/` already exists, reuse it (no re-clone). Otherwise clone via the same
   staging-dir → atomic `rename` → cleanup-on-failure pattern `marketplace_add` uses: clone into
   `repos/<mp>/.staging-<pid>-<uuid>/`, `checkout` the pin if any, then rename into place; any
   failure removes the staging dir and returns.
4. Record `"<plugin>@<marketplace>"` in the lockfile's `plugins` map (resolved ref + `HEAD`
   commit + `now_unix()`), same resolution logic `marketplace_add` uses.
5. Flip the enable bit via `set_enabled_plugin(…, Some(true))`.

A local-path (`LocalPath`) source install is unchanged — it never clones, just flips the bit.

**`marketplace_remove <name>`** additionally removes `repos/<name>/` (whole tree) and drops every
`plugins` lock entry whose key ends with `@<name>` — so removing a marketplace leaves no orphaned
plugin clones or stale lock rows. (Its existing behavior — remove the marketplace dir + its
`marketplaces` lock entry, leave `enabledPlugins` keys inert — is preserved.)

**`plugin_uninstall`** is unchanged: it flips the enable bit off and leaves the clone cached,
consistent with the existing posture that `marketplace remove` is the explicit disk-cleanup path.

**`plugin_list`** is unchanged (it lists offered plugins + enabled state from the manifest).

### E. Discovery — `crates/extensions/src/lib.rs::fold_plugins`

For an **enabled** plugin whose `source` is `Remote`, resolve its root to
`base/.claude/plugins/repos/<mp>/<plugin>/` (derived from the same `base` the marketplace was
found under) instead of the current warn-and-skip:

- Directory present → `fold_one_plugin` on it, identical to a local-path plugin.
- Directory absent → warn `"<key>: enabled but not materialized; run 'otto plugin install <key>'"`
  and skip (never fatal).

Discovery stays lockfile-free — a pure existence check on the repos path. Local-path resolution is
untouched.

## Security

Reuses the plugin-install slice's hardening; no new sandbox surface (CLI-operator-only, never
agent-facing):

- `validate_clone_url` (https/http/ssh/file/scp-like scheme allowlist) on every resolved URL.
- `reject_leading_dash` on the ref and, inside `resolve_remote_source`, on the `owner`/`name`
  `github` segments — blocking argv-injection.
- Path-safe validation of `<owner>`, `<name>`, `<plugin>`, `<marketplace>` before they become URL
  or directory components (no `..`, no stray `/`), so a malicious manifest can't escape
  `~/.claude/plugins/repos/`.
- The clone URL is stored verbatim in the lockfile — same "avoid embedding credentials in the URL"
  caveat as `marketplace_add`.

## Testing

**`extensions` unit tests (`marketplace_def.rs`, `marketplace_install.rs`):**

- `resolve_remote_source`: github → correct https URL; git → verbatim URL; ref/tag/branch/commit
  precedence; unknown `source` kind errors; malformed `github` `repo` (one segment, `..`,
  leading-dash) errors.
- Nested lockfile round-trip (`marketplaces` + `plugins`); flat-format back-compat parse still
  loads as marketplaces-only; malformed → empty.

**`plugin_cli.rs` integration tests** (extend the existing `bare_marketplace_remote` `file://`
harness so no network is touched):

- Install a git-sourced plugin (the marketplace manifest lists a `{"source":"git","url":"file://…"}`
  plugin backed by a second bare repo) end to end: clone lands under `repos/<mp>/<plugin>/`, the
  `plugins` lock records it, the enable bit flips.
- Re-install reuses the existing clone (no error, no duplicate).
- `marketplace remove` deletes the `repos/<mp>/` tree and drops the plugin's lock entry.
- A `discover()` over the fixture folds a materialized remote plugin's artifacts; an
  enabled-but-unmaterialized remote plugin warns and is skipped.

All tests offline/deterministic (`file://` remotes only), preserving the workspace determinism
invariant.
