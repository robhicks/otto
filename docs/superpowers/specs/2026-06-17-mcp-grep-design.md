# otto Design — mcp-grep

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-17

## Goal

Add `mcp-grep`: a standalone rmcp stdio server providing ripgrep-style regex search over a
path-contained root, exposed to the engine via the existing MCP client as a gated `grep` tool.
Second MCP-axis sub-project — a new capability (the engine has no in-process search today).

## Context

The MCP pipeline is proven (PR #37): `crates/mcp-fs` is an rmcp stdio server, and the engine's
`mcp.rs` client (`connect`/`connect_fs`) spawns a server and registers its tools as gated
`Tool`s. `mcp-grep` mirrors `mcp-fs`. The `DefaultPermissionGate` (`crates/tools/src/gate.rs`)
is generic over tool name: it denies any call whose `path`/`paths[]` arg names a sensitive
marker (`.env`/`.ssh`/`.aws`/`.git`/`id_rsa`), and the engine's `ToolRegistry::call` runs it
before dispatch.

## Decisions (locked during brainstorming)

1. **Search engine = the `grep`/`ignore` crates** (`grep-searcher` + `grep-regex` + `ignore` —
   ripgrep's own building blocks), in-process. No external `rg` binary dependency. The
   `ignore::WalkBuilder` respects `.gitignore` and **skips hidden files by default**, so
   `.env`/`.ssh`/`.aws`/`.git` are never walked.
2. **Tool name `grep`** (the gate is generic, so the name need not be `fs.*`).
3. **No in-process fallback** — `grep` is a new capability; if `mcp-grep` is absent the tool is
   simply not registered (logged), not broken (unlike `mcp-fs`, which falls back to in-process
   fs tools).
4. **Results are capped** (a match limit) with a `truncated` flag — bounding output (the
   unbounded-output gap the `bash` tool still has is closed here).

## Architecture

### `crates/mcp-grep` (new binary)

`mcp-grep <root>`. One rmcp tool:

```
grep { pattern: String, glob: Option<String> }
  -> { matches: [ { path: String, line_number: u64, line: String } ], truncated: bool }
```

- `pattern` is a regex compiled with `grep-regex` (`RegexMatcher`).
- Search walks `<root>` with `ignore::WalkBuilder`: defaults keep `hidden(true)` (skip dotfiles/
  dotdirs) and `git_ignore(true)`; `follow_links(false)` so symlinks can't escape the root. An
  optional `glob` (an `ignore`/`globset` pattern, or a simple substring/relative-prefix filter —
  implementer picks the simplest that works) limits which files are searched.
- Each file is searched with `grep-searcher`'s `Searcher` + a sink that records
  `{ relative path, line_number, matched line }`.
- Matches are collected up to `MAX_MATCHES` (a const, e.g. 1000). On hitting the cap, search
  stops and `truncated = true`; otherwise `false`.
- Binary files (non-UTF-8) are skipped by the searcher's default binary detection.
- The result is returned as MCP structured content carrying exactly the `{ matches, truncated }`
  JSON object.

`main` reads `<root>` from `argv[1]` and serves over stdio (mirrors `mcp-fs`). Deps: `rmcp`
(server/transport-io/macros), `grep-searcher`, `grep-regex`, `ignore`, `tokio`, `serde`,
`serde_json`, `anyhow`. (The search itself is sync/CPU-bound; run it on a blocking task or
directly within the async handler as the implementer finds cleanest — searches are bounded by
`MAX_MATCHES`.)

### Engine wiring (`crates/engine/src/main.rs`)

Mirror the `mcp-fs` step in `build_tools_preferring_mcp`: after the fs step, try
`mcp_connect_grep(&mcp_grep_bin(), &root)` (a thin wrapper over the existing generic `connect`,
building the `mcp-grep <root>` command); on success register the `grep` tool and hold the
connection; on failure log "mcp-grep unavailable; search disabled" and continue (no fallback).
`mcp_grep_bin()` = `OTTO_MCP_GREP_BIN` or `"mcp-grep"`. The returned `McpConnection` is held for
the process lifetime alongside the fs one. The existing generic `connect` and `McpTool` are
reused unchanged; only a `connect_grep` convenience + the binary wiring are added.

### Safety (two layers)

1. **Gate in front** (unchanged): `ToolRegistry::call` denies a `grep` call whose args name a
   sensitive `path` (the gate inspects `path`/`paths[]`) before dispatch.
2. **Server-side hidden-skip + containment**: `mcp-grep` never walks hidden files (so a tree-wide
   `grep` cannot read `.env`/`id_rsa`/`.ssh` contents), stays rooted at `<root>`, and does not
   follow symlinks out.

## Error handling & determinism

- An invalid regex `pattern` → the tool returns an error (mapped to an MCP error result the
  client surfaces as `anyhow::Error`), not a panic.
- A `connect_grep` failure leaves the engine without a `grep` tool (logged); everything else
  works.
- Determinism: matches are returned in a stable order (walk order is sorted/deterministic via
  `ignore`'s sorted walk, or the implementer sorts results by `(path, line_number)` before
  returning) so tests are reproducible. The integration test spawns the `mcp-grep` child over
  stdio on the local machine (no network).

## Testing

- **`mcp-grep` unit tests** (against a tempdir, calling the rmcp-independent core search method
  directly — mirroring `mcp-fs`'s `do_*` pattern):
  - seed `a.txt`/`src/b.rs` with known lines; `grep("TODO")` returns the expected
    `{path, line_number, line}` matches (relative paths, correct line numbers).
  - a `glob` filter narrows results to matching files.
  - an invalid regex errors.
  - the match cap: seed > `MAX_MATCHES` matches → `truncated == true` and the result is capped.
  - **secret-not-leaked:** a `.env` containing `SECRET=hunter2` is NOT returned by
    `grep("SECRET")` (hidden-skip) — the key safety test.
- **Engine integration test** (`escargot`-built `mcp-grep`): through a gated `ToolRegistry`,
  register the `grep` tool via `mcp_connect_grep`; `grep {pattern}` returns matches with the
  right shape; and a `grep` call with a sensitive `path` arg (e.g. `{ pattern: "x", path: ".env" }`)
  is **gate-denied before reaching the server**.

**Implementation latitude (rmcp + grep-crate APIs):** the rmcp tool/serve wiring (as in `mcp-fs`)
and the exact `grep-searcher`/`grep-regex`/`ignore` API (matcher construction, the `Sink`
callback, `WalkBuilder` options, glob filtering) are pinned to the resolved crate versions and
adjusted to their real surfaces; keep the tool shape, the cap, and the safety behavior fixed.
Consult current docs (e.g. context7) as needed.

## Out of scope (named, not silently dropped)

- **`mcp-git`, `mcp-bash`** — subsequent sub-projects.
- **Advanced ripgrep features** — context lines (`-A`/`-B`/`-C`), case-insensitive/smart-case
  modes, multiline, fixed-strings vs regex toggles, replacement — addable later; v1 is
  `pattern` (regex) + optional `glob`.
- **Searching hidden/gitignored files on request** — intentionally not offered (the hidden-skip
  is a safety feature, not a limitation to toggle off).
- **Generalizing the binary's MCP-server list** — fs + grep are wired explicitly for now.
