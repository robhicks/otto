# otto Design — mcp-git

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-17

## Goal

Add `mcp-git`: a standalone rmcp stdio server that performs git operations on the repo at
`<root>` by shelling out to the `git` (and `gh`) CLI, exposed to the engine as gated tools.
Full scope — local ops (status/diff/log/add/commit/branch/checkout), `clone`, `push`, and
PR-open — with the network/PR happy-paths manual-only and everything else CI-tested. Third
MCP-axis sub-project. The permission gate denies raw `fs` access to `.git/`, so `mcp-git` is
the *sanctioned* way an agent touches git.

## Decisions (locked during brainstorming)

1. **Uniform backend = shell out to `git`/`gh`.** `git -C <root> <subcommand>` (parse
   porcelain/`--format`); `gh pr create` for PR. One backend, exact git semantics (config,
   `.gitignore`, hooks, credential helpers) for free. Requires `git` (and `gh` for PR) on PATH.
   (gix was considered but cannot open PRs and has immature `push`; git2 adds a C dep.)
2. **Full scope in this sub-project**, but the un-CI-able parts (`push`/`pr_open` *success*)
   are manual-only; their graceful-failure paths are tested.
3. **`git.add` refuses to stage gate-sensitive paths** (mirrors the gate floor) so an agent
   can't commit a secret.
4. **Git tools register as `Allow`** (like fs tools) for v1; `Ask`-gating git mutations is a
   deferred policy hook.

## Architecture

### `crates/mcp-git` (new binary)

`mcp-git <root>`, mirrors `mcp-fs`/`mcp-grep`. A `GitServer { root: Arc<PathBuf> }` with core
helpers:

```rust
/// Run `git -C <root> <args>`; Ok(stdout) on exit 0, else Err carrying stderr.
async fn run_git(root: &Path, args: &[&str]) -> anyhow::Result<String>;
/// Run `gh <args>` with cwd = root (for PR). Same Ok/Err contract.
async fn run_gh(root: &Path, args: &[&str]) -> anyhow::Result<String>;
```

Each tool is a thin async core method (e.g. `do_status`, `do_commit`) callable directly in unit
tests, plus an rmcp `#[tool]` wrapper returning structured content (the `mcp-fs` pattern). The
rmcp wiring (imports, `#[tool_router(server_handler)]`, `Parameters<T>`, `CallToolResult::structured`,
`serve(stdio())`) is copied from `mcp-fs`. Deps: `rmcp` (same version/features as `mcp-fs`),
`tokio` (process), `serde`/`serde_json`/`schemars`/`anyhow`.

### Tool surface

Local (CI-tested):
- `git.status {}` → `{ branch: String, changes: [{ path, status }] }` (from `status --porcelain=v1 -b`).
- `git.diff { staged?: bool, path?: String }` → `{ diff: String }` (`diff [--cached] [-- <path>]`).
- `git.log { max?: u32 }` → `{ commits: [{ hash, summary, author, date }] }`
  (`log -n <max> --format=<delimited>`; `max` default e.g. 20).
- `git.add { paths: [String] }` → `{ added: [String] }` — rejects any path matching a sensitive
  marker BEFORE running `git add -- <paths>` (returns an error naming the offending path).
- `git.commit { message: String }` → `{ hash: String }` (`commit -m <message>`; identity from
  git config — the unit tests set repo-local `user.name`/`user.email`).
- `git.branch {}` → `{ current: String, branches: [String] }` (`branch --format`).
- `git.checkout { name: String, create?: bool }` → `{ branch: String }`
  (`checkout [-b] <name>`).

Network / external (manual-only happy-path, graceful-failure tested):
- `git.clone { url: String, dir?: String }` → `{ path: String }` — clones into `dir` (default a
  sane name) UNDER root; the resolved target is validated to stay within root (reject `..`/abs).
  CI-tested against a local `file://` bare remote (no network/creds).
- `git.push { remote?: String, branch?: String }` → `{ output: String }` (`push [remote] [branch]`).
- `git.pr_open { title: String, body?: String, base?: String }` → `{ url: String }`
  (`gh pr create --title --body [--base]`, parse the URL from stdout).

### Safety

1. **`git.add` sensitive-refuse**: a small `SENSITIVE_SKIP` const mirrors the gate's
   `SENSITIVE_MARKERS` (`crates/tools/src/gate.rs`), cross-referenced both ways (as in
   `mcp-grep`). `do_add` returns an error if any requested path matches, before staging.
2. **Containment**: every op runs `git -C <root>`; `git.clone`'s target dir is resolved and
   rejected if it escapes root (no `..`, no absolute).
3. **Gate in front** (unchanged): the engine gate denies `git.add`/`git.diff` calls whose
   `path`/`paths` args name a sensitive marker, before dispatch — a second layer over (1).
4. **`Allow`, not `Ask`** (v1): git tools dispatch like fs tools; an agent with configured creds
   can push. `Ask`-gating git mutations (so an interactive resolver must approve push/PR) is a
   deferred policy hook, noted not built.

### Engine wiring (`crates/engine`)

A `connect_git(bin, root)` helper (mirrors `connect_fs`/`connect_grep`, reuses the generic
`connect`), re-exported as `mcp_connect_git`. `build_tools_preferring_mcp` adds a third step:
spawn `mcp-git` (`OTTO_MCP_GIT_BIN` / `mcp-git` on PATH), register its tools, hold the
connection; additive — on failure log "mcp-git unavailable; git tools disabled" and continue
(no fallback). Returns the live connections `Vec<McpConnection>` (already the shape after
`mcp-grep`).

## Error handling & determinism

- A failing git subcommand → the tool returns the stderr as an `anyhow::Error` (mapped to an MCP
  error result), not a panic.
- `push` with no remote configured, and `pr_open` with `gh` missing/unauthenticated, both
  surface as clean errors — these graceful-failure paths are CI-tested; the *success* paths are
  manual.
- Determinism: local-op tests build a self-contained repo in a tempdir (`git init`, set
  repo-local identity), avoiding assertions on config/time-dependent output. The `clone` test
  uses a local `file://` bare remote. The integration test spawns the `mcp-git` child over stdio
  (no network).

## Testing

- **`mcp-git` unit tests** (against a tempdir repo, calling `do_*` directly):
  - `git init` + config + a seeded commit; `do_status` reflects a new/modified file;
    `do_log` returns the seeded commit (hash/summary); `do_diff` shows a change;
    `do_add` + `do_commit` produces a new commit (hash) and `do_status` goes clean;
    `do_branch`/`do_checkout {create}` create and switch branches.
  - `do_add([".env"])` is **rejected** (sensitive-refuse) and stages nothing.
  - `do_clone(file://<bare>)` into a subdir clones the repo (CI-tested, local remote);
    a `dir` of `../escape` is rejected (containment).
  - `do_push` with no remote → `Err`; `do_pr_open` when `gh` is absent/unauthenticated → `Err`
    (graceful-failure; the success path is documented manual-only).
- **Engine integration test** (`escargot`-built `mcp-git`): through a gated `ToolRegistry`,
  `git.status`/`git.commit` work over the MCP round-trip; and `git.add {paths:[".env"]}` is
  **gate-denied before reaching the server**.

**Implementation latitude:** rmcp wiring is copied from `mcp-fs` (known-good). The git porcelain/
`--format` parsing and `gh` output parsing are adjusted to the installed `git`/`gh` versions;
keep the tool shapes and safety behavior fixed. The push/pr success paths are not asserted in CI.

## Out of scope (named, not silently dropped)

- **Commit signing, `merge`/`rebase`/`stash`/`reset`/`cherry-pick`, submodules** — later.
- **Advanced PR options** (reviewers, labels, draft, head-branch selection beyond current) —
  later; v1 `pr_open` is title/body/base.
- **`fetch`/`pull`** — v1 has `clone`/`push`; fetch/pull can follow.
- **`Ask`-gating git mutations** — a policy hook for when an interactive `AskResolver` exists.
