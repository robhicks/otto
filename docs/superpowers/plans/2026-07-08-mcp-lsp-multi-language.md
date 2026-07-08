# mcp-lsp Multi-Language Dispatch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generalize the `mcp-lsp` MCP server from a single hardcoded `rust-analyzer` client to a lazy, per-language client registry routing each file to the right language server (Rust/TS/JS/Python/Go) by extension — no new tools, no protocol change.

**Architecture:** A pure language table (`lang.rs`: extension → server + LSP languageId, PATH resolver) drives a lazy per-key client registry in `LspServer` (`main.rs`). Each language server spawns on first use behind a per-key lock (so a cold `gopls` never blocks a warm `rust-analyzer`), with a per-server cold-start diagnostics budget, retry-eligible spawn failures vs a permanent absent cache, and crashed-client eviction. `lsp_client.rs` gains a `spawn_process` args param, a synchronous liveness flag (flipped on write failure), and a defensive `MethodNotFound` reply protecting the load-bearing default-capabilities invariant. A startup PATH gate keeps the whole `lsp.*` toolset absent when no server is installed.

**Tech Stack:** Rust (edition 2024), tokio 1.52, rmcp (stdio MCP server), lsp-types, serde_json, tempfile (tests). Design: `docs/superpowers/specs/2026-07-08-mcp-lsp-multi-language-design.md`.

---

## File structure

- `crates/mcp-lsp/src/lang.rs` — **new.** Pure logic (one PATH filesystem probe): `ServerSpec`, the four server consts, `config_for_extension`, `resolved_bin`/`resolved_bin_with`, `resolve_executable`, `any_server_available`. Fully unit-testable without spawning a server.
- `crates/mcp-lsp/src/lsp_client.rs` — **modify.** `spawn_process(bin, args)`; a shared `Arc<Mutex>` writer so the reader loop can reply; a synchronous `alive` flag (flipped on write error AND on reader EOF) + `is_alive()`; a defensive `MethodNotFound` reply to unknown server→client requests; documented capabilities invariant on `initialize`.
- `crates/mcp-lsp/src/main.rs` — **modify.** `mod lang;`; replace the single `lsp` field with the lazy per-key registry; dispatch, timeouts, eviction, the PATH gate, and all tests.
- `crates/engine/src/mcp.rs` — **modify (test only).**
- `CLAUDE.md`, `docs/ARCHITECTURE.md`, `docs/superpowers/specs/2026-07-07-mcp-lsp-design.md` — **docs.**

Run all commands from the repo root `/home/robhicks/dev/otto-next`. Tasks 1–4 each end green and committed independently. **Task 5 is one atomic refactor** (struct-field change + all its usages + `main()` + tests) — it is the only large task and has a single build/test/commit at its end; it cannot be split into compiling sub-checkpoints.

---

### Task 1: `spawn_process` gains an `args` parameter

**Files:**
- Modify: `crates/mcp-lsp/src/lsp_client.rs` (`spawn_process`)
- Modify: `crates/mcp-lsp/src/main.rs` (the two current call sites)

- [ ] **Step 1: Change the signature and pass args**

In `crates/mcp-lsp/src/lsp_client.rs`, rewrite `spawn_process`:

```rust
/// Spawn `bin args…` as a child process and wire an `LspClient` to its stdio.
pub fn spawn_process(
    bin: &str,
    args: &[&str],
) -> anyhow::Result<(LspClient, tokio::process::Child)> {
    let mut child = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("child process has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child process has no stdout"))?;
    let client = LspClient::spawn(tokio::io::BufReader::new(stdout), stdin);
    Ok((client, child))
}
```

- [ ] **Step 2: Update the `spawn_process` test in `lsp_client.rs`**

```rust
    #[test]
    fn spawn_process_with_bogus_binary_errors() {
        assert!(spawn_process("definitely-not-a-real-binary-xyz", &[]).is_err());
    }
```

- [ ] **Step 3: Update the two call sites in `main.rs`**

`main.rs` currently has (in `main()`) `let (lsp, _child) = lsp_client::spawn_process(&rust_analyzer_bin())?;` and (in the `rust_analyzer_integration` test) `spawn_process(&rust_analyzer_bin()).unwrap()`. Add `, &[]` to both so the crate compiles now: `spawn_process(&rust_analyzer_bin(), &[])`. (Both call sites are fully rewritten in Task 5.)

- [ ] **Step 4: Build and test**

Run: `cargo test -p otto-mcp-lsp`
Expected: PASS (all existing tests, unchanged behavior).

- [ ] **Step 5: Commit**

```bash
git add crates/mcp-lsp/src/lsp_client.rs crates/mcp-lsp/src/main.rs
git commit -m "refactor(mcp-lsp): spawn_process takes an args slice"
```

---

### Task 2: `lang.rs` — the language table

**Files:**
- Create: `crates/mcp-lsp/src/lang.rs`
- Modify: `crates/mcp-lsp/src/main.rs` (add `mod lang;`)

- [ ] **Step 1: Create `lang.rs`**

Create `crates/mcp-lsp/src/lang.rs`:

```rust
//! The language dispatch table: file extension → (language server, LSP languageId), plus
//! binary resolution and a PATH executable probe. Pure logic (the PATH probe is the only
//! filesystem touch), so it is exhaustively unit-testable without spawning a server.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// One language server process. Several extensions may map to the same `ServerSpec` (e.g. all
/// of `.ts`/`.tsx`/`.js`/`.jsx` share one `typescript-language-server`), so the client registry
/// is keyed by `key`, not by extension or languageId.
pub struct ServerSpec {
    /// Registry key — dedups extensions that share one process.
    pub key: &'static str,
    /// Default executable name (overridable via `env_override`).
    pub default_bin: &'static str,
    /// Fixed argv passed after the binary (e.g. `--stdio`).
    pub args: &'static [&'static str],
    /// Env var whose value, if set, replaces `default_bin` (a bare executable path — no argv).
    pub env_override: &'static str,
    /// Timeout budget for the FIRST `lsp.diagnostics` call against this server, before its index
    /// is warm. Cold pyright/gopls indexing routinely exceeds the 15s steady-state default; a
    /// too-short budget returns `{diagnostics: [], timed_out: true}`, which reads as falsely
    /// "clean".
    pub first_open_diag_timeout: Duration,
}

pub static RUST_ANALYZER: ServerSpec = ServerSpec {
    key: "rust-analyzer",
    default_bin: "rust-analyzer",
    args: &[],
    env_override: "OTTO_RUST_ANALYZER_BIN",
    first_open_diag_timeout: Duration::from_secs(60),
};

pub static TYPESCRIPT: ServerSpec = ServerSpec {
    key: "typescript-language-server",
    default_bin: "typescript-language-server",
    args: &["--stdio"],
    env_override: "OTTO_TYPESCRIPT_LANGUAGE_SERVER_BIN",
    first_open_diag_timeout: Duration::from_secs(30),
};

pub static PYRIGHT: ServerSpec = ServerSpec {
    key: "pyright-langserver",
    default_bin: "pyright-langserver",
    args: &["--stdio"],
    env_override: "OTTO_PYRIGHT_LANGSERVER_BIN",
    first_open_diag_timeout: Duration::from_secs(60),
};

pub static GOPLS: ServerSpec = ServerSpec {
    key: "gopls",
    default_bin: "gopls",
    args: &[],
    env_override: "OTTO_GOPLS_BIN",
    first_open_diag_timeout: Duration::from_secs(60),
};

/// Every distinct server, for the startup availability gate.
pub static ALL_SERVERS: &[&ServerSpec] = &[&RUST_ANALYZER, &TYPESCRIPT, &PYRIGHT, &GOPLS];

/// Map a file extension (already lowercased, no leading dot) to its server + LSP languageId.
/// `None` ⇒ no language server configured for that extension.
pub fn config_for_extension(ext: &str) -> Option<(&'static ServerSpec, &'static str)> {
    match ext {
        "rs" => Some((&RUST_ANALYZER, "rust")),
        "ts" => Some((&TYPESCRIPT, "typescript")),
        "tsx" => Some((&TYPESCRIPT, "typescriptreact")),
        "js" | "mjs" | "cjs" => Some((&TYPESCRIPT, "javascript")),
        "jsx" => Some((&TYPESCRIPT, "javascriptreact")),
        "py" | "pyi" => Some((&PYRIGHT, "python")),
        "go" => Some((&GOPLS, "go")),
        _ => None,
    }
}

/// The executable for `spec`, given an optional override value (the `env_override`'s value).
/// Pure — the env read lives in `resolved_bin`, so this is directly testable.
pub fn resolved_bin_with(spec: &ServerSpec, override_val: Option<String>) -> String {
    override_val.unwrap_or_else(|| spec.default_bin.to_string())
}

/// The executable to spawn for `spec`: the `env_override` value if set, else `default_bin`.
pub fn resolved_bin(spec: &ServerSpec) -> String {
    resolved_bin_with(spec, std::env::var(spec.env_override).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_to_the_right_server_and_language_id() {
        assert_eq!(config_for_extension("rs").unwrap().0.key, "rust-analyzer");
        assert_eq!(config_for_extension("rs").unwrap().1, "rust");
        assert_eq!(config_for_extension("go").unwrap().1, "go");
        assert_eq!(config_for_extension("py").unwrap().1, "python");
        assert_eq!(config_for_extension("pyi").unwrap().1, "python");
    }

    #[test]
    fn ts_js_family_shares_one_server_with_distinct_language_ids() {
        let key = "typescript-language-server";
        assert_eq!(config_for_extension("ts").unwrap().0.key, key);
        assert_eq!(config_for_extension("tsx").unwrap().0.key, key);
        assert_eq!(config_for_extension("js").unwrap().0.key, key);
        assert_eq!(config_for_extension("jsx").unwrap().0.key, key);
        assert_eq!(config_for_extension("ts").unwrap().1, "typescript");
        assert_eq!(config_for_extension("tsx").unwrap().1, "typescriptreact");
        assert_eq!(config_for_extension("js").unwrap().1, "javascript");
        assert_eq!(config_for_extension("mjs").unwrap().1, "javascript");
        assert_eq!(config_for_extension("cjs").unwrap().1, "javascript");
        assert_eq!(config_for_extension("jsx").unwrap().1, "javascriptreact");
    }

    #[test]
    fn unknown_and_empty_extensions_are_unsupported() {
        assert!(config_for_extension("txt").is_none());
        assert!(config_for_extension("").is_none());
        assert!(config_for_extension("md").is_none());
    }

    #[test]
    fn resolved_bin_defaults_without_an_override_and_honors_one() {
        assert_eq!(resolved_bin_with(&GOPLS, None), "gopls");
        assert_eq!(
            resolved_bin_with(&PYRIGHT, Some("/opt/custom-pyright".to_string())),
            "/opt/custom-pyright"
        );
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/mcp-lsp/src/main.rs`, next to the existing `mod lsp_client;`, add:

```rust
mod lang;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-mcp-lsp lang::`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-lsp/src/lang.rs crates/mcp-lsp/src/main.rs
git commit -m "feat(mcp-lsp): language dispatch table (ext -> server, languageId)"
```

---

### Task 3: `lang.rs` — PATH executable resolver + availability gate

**Files:**
- Modify: `crates/mcp-lsp/src/lang.rs`

- [ ] **Step 1: Add the resolver and gate helper (Unix)**

Append to `lang.rs`, before `#[cfg(test)] mod tests`:

```rust
/// Resolve `bin` to an executable file using a minimal PATH search. If `bin` contains a path
/// separator it is checked directly; otherwise each colon-separated entry of `path_var` is
/// tried. Returns the path only when the file exists AND has an executable bit set — a
/// present-but-non-executable file does not resolve. Unix-only, matching the OS sandbox's
/// Linux/macOS targeting (Windows PATHEXT/.cmd shims are out of scope).
pub fn resolve_executable(bin: &str, path_var: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = Path::new(bin);
        return is_executable(p).then(|| p.to_path_buf());
    }
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(bin);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// True when at least one configured language server's resolved binary is on PATH. The startup
/// gate uses this: with no server present, `mcp-lsp` exits and the engine registers no `lsp.*`
/// tools (the additive-absence pattern).
pub fn any_server_available() -> bool {
    let path_var = std::env::var("PATH").unwrap_or_default();
    ALL_SERVERS
        .iter()
        .any(|spec| resolve_executable(&resolved_bin(spec), &path_var).is_some())
}
```

- [ ] **Step 2: Add resolver tests inside `mod tests`**

```rust
    use std::os::unix::fs::PermissionsExt;

    fn make_executable(path: &Path) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn resolve_executable_finds_a_bare_name_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("myserver");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);
        assert_eq!(
            resolve_executable("myserver", dir.path().to_str().unwrap()),
            Some(bin)
        );
    }

    #[test]
    fn resolve_executable_rejects_a_non_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("myserver");
        std::fs::write(&bin, "not executable").unwrap(); // default 0o644
        assert!(resolve_executable("myserver", dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn resolve_executable_honors_a_path_separator_override() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("custom-ra");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);
        assert_eq!(
            resolve_executable(bin.to_str().unwrap(), ""),
            Some(bin.clone())
        );
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert!(resolve_executable(plain.to_str().unwrap(), "").is_none());
    }

    #[test]
    fn resolve_executable_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_executable("definitely-not-here", dir.path().to_str().unwrap()).is_none());
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-mcp-lsp lang::`
Expected: PASS (table tests + four resolver tests).

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-lsp/src/lang.rs
git commit -m "feat(mcp-lsp): PATH executable resolver + availability gate helper"
```

Note: `any_server_available` is unused until Task 5, so Tasks 3–4 build with a `dead_code` warning on it (tests still pass). Task 5 wires it; the Task 9 clippy sweep confirms clean.

---

### Task 4: `lsp_client.rs` — synchronous liveness + defensive `MethodNotFound` reply

**Files:**
- Modify: `crates/mcp-lsp/src/lsp_client.rs`

The reader loop must reply to server→client requests (so a capability-mismatched server never stalls), which requires a shared writer. A liveness flag flips **synchronously on any write failure** (so eviction callers get an unambiguous death signal without waiting on the async reader) and also on reader EOF.

- [ ] **Step 1: Import `AtomicBool` and change the fields**

Change the existing atomic import line to include `AtomicBool`:

```rust
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
```

Change `LspClient`'s fields:

```rust
pub struct LspClient {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    diagnostics: Arc<Mutex<HashMap<String, DiagEntry>>>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
    alive: Arc<AtomicBool>,
}
```

- [ ] **Step 2: Write a failing test for the defensive reply**

Add to `lsp_client.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn reader_replies_method_not_found_to_unknown_server_requests() {
        let (client, server_end) = duplex_client();
        let (sr, mut sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);

        write_message(
            &mut sw,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "workspace/configuration",
                "params": {}
            }),
        )
        .await
        .unwrap();

        let reply = read_message(&mut sr).await.unwrap();
        assert_eq!(reply["id"], 99);
        assert_eq!(reply["error"]["code"], -32601);
        drop(client);
    }

    #[tokio::test]
    async fn write_failure_marks_the_client_dead() {
        let (client, server_end) = duplex_client();
        assert!(client.is_alive());
        drop(server_end); // pipe closed synchronously
        let _ = client
            .notify("textDocument/didOpen", serde_json::json!({}))
            .await;
        assert!(!client.is_alive());
    }
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p otto-mcp-lsp lsp_client:: -- reader_replies write_failure`
Expected: FAIL to compile (`is_alive` not defined) / fail assertions — the reply and flag don't exist yet.

- [ ] **Step 4: Implement the shared writer, liveness flag, and defensive reply in `spawn`**

Rewrite `LspClient::spawn`:

```rust
    pub fn spawn<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Arc<Mutex<HashMap<String, DiagEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let generations: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(Mutex::new(Box::new(writer)));
        let alive = Arc::new(AtomicBool::new(true));

        let reader_pending = Arc::clone(&pending);
        let reader_diagnostics = Arc::clone(&diagnostics);
        let reader_generations = Arc::clone(&generations);
        let reader_writer = Arc::clone(&writer);
        let reader_alive = Arc::clone(&alive);
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                let msg = match read_message(&mut reader).await {
                    Ok(m) => m,
                    Err(_) => {
                        // Stream closed / server exited: mark dead so callers can evict + re-spawn.
                        reader_alive.store(false, Ordering::SeqCst);
                        break;
                    }
                };
                let method = msg.get("method").and_then(Value::as_str);
                // Response: has `id`, no `method`.
                if method.is_none() {
                    if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                        if let Some(tx) = reader_pending.lock().await.remove(&id) {
                            let _ = tx.send(msg);
                        }
                    }
                    continue;
                }
                let method = method.unwrap();
                if method == "textDocument/publishDiagnostics" {
                    if let Some(params) = msg.get("params") {
                        if let Ok(p) = serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(
                            params.clone(),
                        ) {
                            let uri = p.uri.as_str().to_string();
                            let generation =
                                *reader_generations.lock().await.get(&uri).unwrap_or(&0);
                            reader_diagnostics.lock().await.insert(
                                uri,
                                DiagEntry {
                                    generation,
                                    diagnostics: p.diagnostics,
                                    updated_at: tokio::time::Instant::now(),
                                },
                            );
                        }
                    }
                    continue;
                }
                // A server->client *request* (has both `id` and `method`) we don't handle. The
                // client advertises minimal capabilities specifically so well-behaved servers
                // gate these on capability and don't send them (see `initialize`); reply
                // defensively so a capability-mismatched server never blocks on an unanswered
                // request. Notifications (method, no id) other than publishDiagnostics are ignored.
                if let Some(id) = msg.get("id") {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": id.clone(),
                        "error": {"code": -32601, "message": "method not supported by otto lsp bridge"},
                    });
                    let mut w = reader_writer.lock().await;
                    let _ = write_message(&mut *w, &reply).await;
                }
            }
        });

        Self {
            writer,
            next_id: AtomicI64::new(1),
            pending,
            diagnostics,
            generations,
            alive,
        }
    }
```

- [ ] **Step 5: Flip liveness on write failure; add `is_alive`**

Rewrite `write` and add `is_alive`:

```rust
    async fn write(&self, value: &Value) -> anyhow::Result<()> {
        let mut w = self.writer.lock().await;
        let result = write_message(&mut *w, value).await;
        if result.is_err() {
            // A failed write means the server's pipe is closed — mark dead synchronously so
            // callers can evict without racing the async reader loop.
            self.alive.store(false, Ordering::SeqCst);
        }
        result
    }

    /// False once the server's stream has closed (process exited) or a write to it failed.
    /// Callers evict a dead client and re-spawn on the next call instead of hanging.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
```

- [ ] **Step 6: Document the capabilities invariant on `initialize`**

Prepend to `initialize`'s doc comment (above the existing `/// Send `initialize`…` line):

```rust
    /// INVARIANT: this client answers no server→client *requests* (the reader loop only replies
    /// `MethodNotFound`). `capabilities` must therefore stay minimal — advertising a richer
    /// capability (pull diagnostics, `workspace/configuration`, dynamic registration) makes a
    /// server send requests the client can't satisfy, stalling it. This minimal-capabilities
    /// choice is exactly why rust-analyzer/tsserver/pyright/gopls all work with no
    /// `initializationOptions`.
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p otto-mcp-lsp lsp_client::`
Expected: PASS — the two new tests plus every pre-existing client test (writer sharing is behavior-preserving).

- [ ] **Step 8: Commit**

```bash
git add crates/mcp-lsp/src/lsp_client.rs
git commit -m "feat(mcp-lsp): synchronous liveness flag + defensive MethodNotFound reply"
```

---

### Task 5: `main.rs` — lazy per-language registry, dispatch, gate (one atomic refactor)

This replaces the single `lsp` field with the per-key registry and rewires everything that used it, patches `main()`, and lands all tests. It compiles only at the end — implement it in full before building.

**Files:**
- Modify: `crates/mcp-lsp/src/main.rs`

- [ ] **Step 1: Fix the top-of-file imports (avoid E0252)**

`main.rs` already has `use std::collections::HashMap;` and `use tokio::sync::Mutex;`. Change the collections import in place to add `HashSet` — do **not** add a second `use` for `HashMap`/`Mutex` anywhere:

```rust
use std::collections::{HashMap, HashSet};
```

- [ ] **Step 2: Replace the `LspServer` struct and `new`**

Replace the existing `LspServer` struct and its `impl LspServer { pub fn new(...) }` with:

```rust
/// A spawned, initialized language server. `_child` is `Option` so a test can seed a
/// duplex-backed client with no OS process (a pipe has nothing for kill_on_drop to protect).
struct ReadyServer {
    client: Arc<LspClient>,
    _child: Option<tokio::process::Child>,
}

#[derive(Clone)]
pub struct LspServer {
    /// server key → its lazily-spawned slot. The outer lock is held only to get-or-insert a
    /// slot; the slow spawn+initialize happens under the per-key inner lock, so a cold `gopls`
    /// never blocks a warm `rust-analyzer` call.
    slots: Arc<Mutex<HashMap<&'static str, Arc<Mutex<Option<ReadyServer>>>>>>,
    /// server keys whose binary is definitely not on PATH — a permanent negative cache (avoids
    /// spawn hammering). Spawn/init errors are NOT cached here; they stay retry-eligible.
    absent: Arc<Mutex<HashSet<&'static str>>>,
    /// server keys that have returned a non-timed-out diagnostics result — after which the
    /// steady-state (short) diagnostics budget applies instead of the cold-start budget.
    served_diag: Arc<Mutex<HashSet<&'static str>>>,
    workspace: Arc<LocalWorkspace>,
    root: PathBuf,
    open_docs: Arc<Mutex<HashMap<String, i32>>>,
}

impl LspServer {
    pub fn new(root: PathBuf) -> Self {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Self {
            slots: Arc::new(Mutex::new(HashMap::new())),
            absent: Arc::new(Mutex::new(HashSet::new())),
            served_diag: Arc::new(Mutex::new(HashSet::new())),
            workspace: Arc::new(LocalWorkspace::new(root.clone())),
            root,
            open_docs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
```

- [ ] **Step 3: Add registry, dispatch, timeout, and eviction methods**

Add these to `impl LspServer` (keep the existing `uri_for`, `path_for`, `goto_response_to_locations`, `render_hover_contents`):

```rust
    /// Extension of `path`, lowercased, no leading dot (`""` if none).
    fn extension_of(path: &str) -> String {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default()
    }

    fn spec_for(&self, path: &str) -> Option<&'static lang::ServerSpec> {
        lang::config_for_extension(&Self::extension_of(path)).map(|(spec, _)| spec)
    }

    /// Get the client for `spec`, spawning + initializing it on first use. Per-key locked, so
    /// concurrent first-calls for the same server don't double-spawn and different languages
    /// proceed in parallel. A definitely-absent binary is cached in `absent` (never retried);
    /// spawn/init failures leave the slot empty (retry-eligible next call).
    async fn get_or_spawn(&self, spec: &'static lang::ServerSpec) -> anyhow::Result<Arc<LspClient>> {
        if self.absent.lock().await.contains(spec.key) {
            anyhow::bail!("no language server `{}` on PATH", lang::resolved_bin(spec));
        }
        let slot = {
            let mut slots = self.slots.lock().await;
            slots
                .entry(spec.key)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        let mut guard = slot.lock().await;
        if let Some(ready) = guard.as_ref() {
            return Ok(ready.client.clone());
        }
        let bin = lang::resolved_bin(spec);
        let path_var = std::env::var("PATH").unwrap_or_default();
        if lang::resolve_executable(&bin, &path_var).is_none() {
            self.absent.lock().await.insert(spec.key);
            anyhow::bail!("no language server `{bin}` on PATH");
        }
        let (client, child) = lsp_client::spawn_process(&bin, spec.args)?;
        client.initialize(&self.root).await?;
        let client = Arc::new(client);
        *guard = Some(ReadyServer {
            client: client.clone(),
            _child: Some(child),
        });
        Ok(client)
    }

    /// Drop a cached client (its server process died) so the next call re-spawns.
    async fn evict(&self, key: &'static str) {
        let slot = self.slots.lock().await.get(key).cloned();
        if let Some(slot) = slot {
            *slot.lock().await = None;
        }
    }

    /// The diagnostics wait budget for `spec`: the cold-start budget until the server has
    /// returned one non-timed-out result, then the steady-state default.
    async fn diag_wait_for(&self, spec: &lang::ServerSpec) -> std::time::Duration {
        if self.served_diag.lock().await.contains(spec.key) {
            diagnostics_timeout()
        } else {
            spec.first_open_diag_timeout
        }
    }

    /// Record that `spec` returned diagnostics; only a non-timed-out result flips it to
    /// steady-state (a cold server keeps the long budget until it actually responds).
    async fn mark_served(&self, spec: &lang::ServerSpec, timed_out: bool) {
        if !timed_out {
            self.served_diag.lock().await.insert(spec.key);
        }
    }

    #[cfg(test)]
    async fn seed_ready_for_test(&self, key: &'static str, client: LspClient) {
        let slot = {
            let mut slots = self.slots.lock().await;
            slots
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        *slot.lock().await = Some(ReadyServer {
            client: Arc::new(client),
            _child: None,
        });
    }

    #[cfg(test)]
    async fn slot_handle_for_test(&self, key: &'static str) -> Arc<Mutex<Option<ReadyServer>>> {
        let mut slots = self.slots.lock().await;
        slots
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    #[cfg(test)]
    async fn slot_is_ready(&self, key: &'static str) -> bool {
        let slot = self.slots.lock().await.get(key).cloned();
        match slot {
            Some(s) => s.lock().await.is_some(),
            None => false,
        }
    }

    #[cfg(test)]
    async fn mark_absent_for_test(&self, key: &'static str) {
        self.absent.lock().await.insert(key);
    }

    #[cfg(test)]
    async fn absent_contains(&self, key: &'static str) -> bool {
        self.absent.lock().await.contains(key)
    }
```

- [ ] **Step 4: Rewrite `open_if_needed` (routes by extension, evicts on notify failure)**

Replace `open_if_needed`:

```rust
    /// Ensure `path` is open (or up to date) in its language's server. Resolves the extension to
    /// a (server, languageId), spawns the server on first use, sends `didOpen`/`didChange` with
    /// that languageId, and returns the client so the caller issues its request against it. A
    /// notify write failure means the server's pipe is closed (it died) — evict so the next call
    /// re-spawns rather than leaving a dead client cached.
    async fn open_if_needed(&self, path: &str) -> anyhow::Result<(Arc<LspClient>, String, u64)> {
        let ext = Self::extension_of(path);
        let (spec, language_id) = lang::config_for_extension(&ext)
            .ok_or_else(|| anyhow::anyhow!("no language server configured for .{ext}"))?;
        let client = self.get_or_spawn(spec).await?;

        let content = String::from_utf8(self.workspace.read(Path::new(path)).await?)?;
        let uri = self.uri_for(path)?;
        let uri_str = uri.as_str().to_string();
        let generation = client.bump_generation(&uri_str).await;

        let (method, params) = {
            let mut open = self.open_docs.lock().await;
            match open.get(&uri_str).copied() {
                None => {
                    open.insert(uri_str.clone(), 1);
                    (
                        "textDocument/didOpen",
                        serde_json::to_value(lsp_types::DidOpenTextDocumentParams {
                            text_document: lsp_types::TextDocumentItem::new(
                                uri,
                                language_id.to_string(),
                                1,
                                content,
                            ),
                        })?,
                    )
                }
                Some(version) => {
                    let next = version + 1;
                    open.insert(uri_str.clone(), next);
                    (
                        "textDocument/didChange",
                        serde_json::to_value(lsp_types::DidChangeTextDocumentParams {
                            text_document: lsp_types::VersionedTextDocumentIdentifier::new(
                                uri, next,
                            ),
                            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                                range: None,
                                range_length: None,
                                text: content,
                            }],
                        })?,
                    )
                }
            }
        };

        if let Err(e) = client.notify(method, params).await {
            self.evict(spec.key).await;
            return Err(e);
        }
        Ok((client, uri_str, generation))
    }
```

- [ ] **Step 5: Rewrite the four `do_*` methods (diagnostics budget + nav eviction)**

Replace `do_diagnostics`, `do_diagnostics_with_timeout`, `do_definition`, `do_references`, `do_hover` with:

```rust
    pub async fn do_diagnostics(&self, path: String) -> anyhow::Result<(Vec<DiagnosticOut>, bool)> {
        let spec = self
            .spec_for(&path)
            .ok_or_else(|| anyhow::anyhow!("no language server configured for `{path}`"))?;
        let wait = self.diag_wait_for(spec).await;
        let (out, timed_out) = self.do_diagnostics_with_timeout(path, wait).await?;
        self.mark_served(spec, timed_out).await;
        Ok((out, timed_out))
    }

    async fn do_diagnostics_with_timeout(
        &self,
        path: String,
        wait: std::time::Duration,
    ) -> anyhow::Result<(Vec<DiagnosticOut>, bool)> {
        let (client, uri, generation) = self.open_if_needed(&path).await?;
        let (diags, timed_out) = client.wait_for_diagnostics(&uri, generation, wait).await;
        if timed_out && !client.is_alive() {
            if let Some(spec) = self.spec_for(&path) {
                self.evict(spec.key).await;
            }
        }
        let out = diags
            .into_iter()
            .map(|d| DiagnosticOut {
                line: d.range.start.line + 1,
                character: d.range.start.character + 1,
                severity: d.severity.map(severity_name),
                message: d.message,
                code: d.code.map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s,
                }),
            })
            .collect();
        Ok((out, timed_out))
    }

    /// Evict `path`'s server if a request failed against a now-dead client.
    async fn evict_if_dead(&self, path: &str, client: &LspClient, failed: bool) {
        if failed && !client.is_alive() {
            if let Some(spec) = self.spec_for(path) {
                self.evict(spec.key).await;
            }
        }
    }

    pub async fn do_definition(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<LocationOut>> {
        let (client, uri, _generation) = self.open_if_needed(&path).await?;
        let params = lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.parse()? },
                position: lsp_types::Position::new(
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                ),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let result = client
            .request(
                "textDocument/definition",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await;
        self.evict_if_dead(&path, &client, result.is_err()).await;
        self.goto_response_to_locations(result?)
    }

    pub async fn do_references(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<LocationOut>> {
        let (client, uri, _generation) = self.open_if_needed(&path).await?;
        let params = lsp_types::ReferenceParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.parse()? },
                position: lsp_types::Position::new(
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                ),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_types::ReferenceContext {
                include_declaration: true,
            },
        };
        let result = client
            .request(
                "textDocument/references",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await;
        self.evict_if_dead(&path, &client, result.is_err()).await;
        let result = result?;
        if result.is_null() {
            return Ok(vec![]);
        }
        let locs: Vec<lsp_types::Location> = serde_json::from_value(result)?;
        // Out-of-root locations are skipped, not errors — see `goto_response_to_locations`.
        Ok(locs
            .into_iter()
            .filter_map(|l| {
                self.path_for(&l.uri).ok().map(|path| LocationOut {
                    path,
                    line: l.range.start.line + 1,
                    character: l.range.start.character + 1,
                })
            })
            .collect())
    }

    pub async fn do_hover(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Option<String>> {
        let (client, uri, _generation) = self.open_if_needed(&path).await?;
        let params = lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.parse()? },
                position: lsp_types::Position::new(
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                ),
            },
            work_done_progress_params: Default::default(),
        };
        let result = client
            .request(
                "textDocument/hover",
                serde_json::to_value(params)?,
                NAVIGATION_TIMEOUT,
            )
            .await;
        self.evict_if_dead(&path, &client, result.is_err()).await;
        let result = result?;
        if result.is_null() {
            return Ok(None);
        }
        let hover: lsp_types::Hover = serde_json::from_value(result)?;
        Ok(Some(render_hover_contents(hover.contents)))
    }
```

- [ ] **Step 6: Rewrite `main()` with the PATH gate; remove `rust_analyzer_bin()`**

Replace `main()` and delete the now-unused top-level `fn rust_analyzer_bin()`:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-lsp <root>"))?;
    let root = std::fs::canonicalize(&root)?;

    // Additive-absence gate: if no supported language server is on PATH, exit before the MCP
    // handshake so the engine's `connect_lsp` fails and registers no `lsp.*` tools. If ≥1 is
    // present, serve — individual servers spawn lazily on first use of their language.
    if !lang::any_server_available() {
        anyhow::bail!(
            "no supported language server on PATH \
             (rust-analyzer / typescript-language-server / pyright-langserver / gopls); \
             lsp tools will be unavailable"
        );
    }

    let server = LspServer::new(root);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

- [ ] **Step 7: Rewrite the `duplex_server` test helper (now async, seeds a client)**

In `#[cfg(test)] mod tests`, replace `duplex_server`:

```rust
    async fn duplex_server(root: &Path) -> (LspServer, tokio::io::DuplexStream) {
        seeded_duplex_server(root, "rust-analyzer").await
    }

    /// A server with one duplex-backed client seeded under `key` (no real process spawned).
    async fn seeded_duplex_server(
        root: &Path,
        key: &'static str,
    ) -> (LspServer, tokio::io::DuplexStream) {
        let (client_end, server_end) = tokio::io::duplex(16384);
        let (cr, cw) = tokio::io::split(client_end);
        let lsp = LspClient::spawn(BufReader::new(cr), cw);
        let server = LspServer::new(root.to_path_buf());
        server.seed_ready_for_test(key, lsp).await;
        (server, server_end)
    }
```

Then update every existing test that binds `duplex_server(...)` to `.await` it — these tests: `open_if_needed_sends_did_open_then_did_change`, `do_diagnostics_returns_fresh_results`, `do_diagnostics_times_out_without_a_response`, `do_definition_parses_a_scalar_location_response`, `do_definition_skips_out_of_root_locations`, `do_definition_returns_empty_for_a_null_response`, `do_references_parses_an_array_of_locations`, `do_hover_renders_markup_contents`, `do_hover_returns_none_for_a_null_response`. Change each `let (server, server_end) = duplex_server(dir.path());` to `... = duplex_server(dir.path()).await;`.

`path_for_round_trips_with_a_non_canonical_root` builds its server inline — replace its body's server construction with `let server = LspServer::new(messy);` (it only calls `uri_for`/`path_for`; drop the `LspClient`/duplex setup entirely).

- [ ] **Step 8: Add the new dispatch / per-key / budget / absent / eviction tests**

Add to `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn open_if_needed_sends_python_language_id_for_py_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let (server, server_end) = seeded_duplex_server(dir.path(), "pyright-langserver").await;
        let (sr, _sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        server.open_if_needed("a.py").await.unwrap();
        let msg = lsp_client::read_message(&mut sr).await.unwrap();
        assert_eq!(msg["method"], "textDocument/didOpen");
        assert_eq!(msg["params"]["textDocument"]["languageId"], "python");
    }

    #[tokio::test]
    async fn open_if_needed_sends_go_language_id_for_go_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.go"), "package main\n").unwrap();
        let (server, server_end) = seeded_duplex_server(dir.path(), "gopls").await;
        let (sr, _sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        server.open_if_needed("a.go").await.unwrap();
        let msg = lsp_client::read_message(&mut sr).await.unwrap();
        assert_eq!(msg["params"]["textDocument"]["languageId"], "go");
    }

    #[tokio::test]
    async fn open_if_needed_lowercases_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.PY"), "x = 1\n").unwrap();
        let (server, server_end) = seeded_duplex_server(dir.path(), "pyright-langserver").await;
        let (sr, _sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);
        server.open_if_needed("A.PY").await.unwrap();
        let msg = lsp_client::read_message(&mut sr).await.unwrap();
        assert_eq!(msg["params"]["textDocument"]["languageId"], "python");
    }

    #[tokio::test]
    async fn open_if_needed_rejects_an_unsupported_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hi\n").unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        let err = server.open_if_needed("a.txt").await.unwrap_err();
        assert!(err.to_string().contains("no language server configured"));
    }

    // R1: a cold server holding its own slot lock must not block a warm call to another server —
    // proves the outer map lock is never held across a busy per-key slot.
    #[tokio::test]
    async fn a_busy_server_slot_does_not_block_a_warm_call_to_another() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, _server_end) = duplex_server(dir.path()).await; // rust-analyzer seeded ready
        let gopls_slot = server.slot_handle_for_test("gopls").await;
        let _held = gopls_slot.lock().await; // simulate a cold gopls spawn in progress
        let opened =
            tokio::time::timeout(Duration::from_secs(2), server.open_if_needed("a.rs")).await;
        assert!(
            opened.is_ok(),
            "a warm rust-analyzer call blocked behind gopls's slot lock"
        );
    }

    // R3: cold-start budget selection + the bug-prone `if !timed_out` state transition.
    #[tokio::test]
    async fn diagnostics_budget_uses_first_open_then_steady_state() {
        let dir = tempfile::tempdir().unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        assert_eq!(
            server.diag_wait_for(&lang::RUST_ANALYZER).await,
            lang::RUST_ANALYZER.first_open_diag_timeout
        );
        // A timed-out result must NOT flip to steady state.
        server.mark_served(&lang::RUST_ANALYZER, true).await;
        assert_eq!(
            server.diag_wait_for(&lang::RUST_ANALYZER).await,
            lang::RUST_ANALYZER.first_open_diag_timeout
        );
        // A successful result flips to steady state.
        server.mark_served(&lang::RUST_ANALYZER, false).await;
        assert_eq!(
            server.diag_wait_for(&lang::RUST_ANALYZER).await,
            diagnostics_timeout()
        );
    }

    // R4: a definitely-absent server is cached permanently and short-circuits.
    #[tokio::test]
    async fn an_absent_server_is_cached_and_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        server.mark_absent_for_test("gopls").await;
        assert!(server.get_or_spawn(&lang::GOPLS).await.is_err());
        assert!(server.get_or_spawn(&lang::GOPLS).await.is_err());
        assert!(server.absent_contains("gopls").await);
    }

    // R4: a dead client is evicted (via the notify-failure path in open_if_needed) so the next
    // call re-spawns.
    #[tokio::test]
    async fn a_dead_client_is_evicted_so_the_next_call_respawns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        assert!(server.slot_is_ready("rust-analyzer").await);
        drop(server_end); // server process "dies" — client's write pipe closes
        let res = server.do_definition("a.rs".to_string(), 1, 1).await;
        assert!(res.is_err());
        assert!(
            !server.slot_is_ready("rust-analyzer").await,
            "a dead client should have been evicted"
        );
    }
```

`Duration` is `std::time::Duration` — add `use std::time::Duration;` inside `mod tests` if not already imported there (the existing tests use fully-qualified `std::time::Duration`; either form is fine, keep it consistent with the file).

- [ ] **Step 9: Rewrite the `rust_analyzer_integration` test to drive the real dispatch path**

In `mod rust_analyzer_integration`, change `rust_analyzer_available()` to use `lang::resolved_bin(&lang::RUST_ANALYZER)` instead of the deleted `rust_analyzer_bin()`, and replace the test body:

```rust
    fn rust_analyzer_available() -> bool {
        std::process::Command::new(lang::resolved_bin(&lang::RUST_ANALYZER))
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn full_round_trip_against_a_real_rust_analyzer() {
        if !rust_analyzer_available() {
            eprintln!("skipping: rust-analyzer not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn greet() -> &'static str {\n    \"hi\"\n}\n\npub fn broken() -> i32 {\n    does_not_exist()\n}\n",
        )
        .unwrap();

        // Drive the public dispatch path: get_or_spawn spawns + initializes rust-analyzer lazily
        // on the first `.rs` open — no hand-wired client.
        let server = LspServer::new(dir.path().to_path_buf());
        let (diags, timed_out) = server
            .do_diagnostics_with_timeout(
                "src/lib.rs".to_string(),
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert!(!timed_out, "rust-analyzer did not respond within 60s");
        assert!(
            diags.iter().any(|d| d.message.contains("does_not_exist")),
            "expected an unresolved-symbol diagnostic, got: {diags:?}"
        );
        let hover = server.do_hover("src/lib.rs".to_string(), 1, 8).await.unwrap();
        assert!(hover.is_some(), "expected hover info for `greet`");
    }
```

- [ ] **Step 10: Build and test the whole crate**

Run: `cargo test -p otto-mcp-lsp`
Expected: PASS. `rust_analyzer_integration` runs (if installed) or prints its skip line.

- [ ] **Step 11: Commit**

```bash
git add crates/mcp-lsp/src/main.rs
git commit -m "feat(mcp-lsp): lazy per-language registry — dispatch, budgets, eviction, PATH gate"
```

---

### Task 6: engine — verify a pre-handshake exit surfaces as a connect error

**Files:**
- Modify: `crates/engine/src/mcp.rs` (test only)

- [ ] **Step 1: Add the test**

In `crates/engine/src/mcp.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn connect_lsp_surfaces_a_pre_handshake_exit_as_err() {
        // `false` spawns successfully then exits nonzero immediately — before speaking MCP,
        // exactly as `mcp-lsp` does when its PATH availability gate finds no language server.
        // `connect` must surface this as an Err (so no lsp tools get registered), not hang.
        assert!(connect_lsp("false", Path::new(".")).await.is_err());
    }
```

- [ ] **Step 2: Run**

Run: `cargo test -p otto-engine connect_lsp`
Expected: PASS (existing `connect_lsp_with_bogus_binary_errors` + the new test).

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/mcp.rs
git commit -m "test(engine): connect_lsp surfaces a pre-handshake exit as Err"
```

---

### Task 7: per-language integration tests (self-skipping)

**Files:**
- Modify: `crates/mcp-lsp/src/main.rs` (append integration test modules)

Each runs only where the server binary exists; otherwise it prints a skip and passes. Each asserts a diagnostics *shape* (a known-broken fixture yields a non-empty diagnostic, not timed out), exercising the timeout/quiescence path.

- [ ] **Step 1: Add the Go integration test**

```rust
#[cfg(test)]
mod gopls_integration {
    use super::*;

    fn gopls_available() -> bool {
        std::process::Command::new(lang::resolved_bin(&lang::GOPLS))
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn go_diagnostics_round_trip() {
        if !gopls_available() {
            eprintln!("skipping: gopls not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module fixture\n\ngo 1.21\n").unwrap();
        std::fs::write(
            dir.path().join("main.go"),
            "package main\n\nfunc main() {\n\tdoesNotExist()\n}\n",
        )
        .unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        let (diags, timed_out) = server
            .do_diagnostics_with_timeout("main.go".to_string(), std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!timed_out, "gopls did not respond within 60s");
        assert!(!diags.is_empty(), "expected a diagnostic for the undefined symbol");
    }
}
```

- [ ] **Step 2: Add the pyright integration test**

```rust
#[cfg(test)]
mod pyright_integration {
    use super::*;

    fn pyright_available() -> bool {
        std::process::Command::new(lang::resolved_bin(&lang::PYRIGHT))
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn python_diagnostics_round_trip() {
        if !pyright_available() {
            eprintln!("skipping: pyright-langserver not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x: int = \"not an int\"\n").unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        let (diags, timed_out) = server
            .do_diagnostics_with_timeout("a.py".to_string(), std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!timed_out, "pyright did not respond within 60s");
        assert!(!diags.is_empty(), "expected a type diagnostic");
    }
}
```

- [ ] **Step 3: Add the typescript-language-server integration test**

```rust
#[cfg(test)]
mod typescript_integration {
    use super::*;

    fn tsserver_available() -> bool {
        std::process::Command::new(lang::resolved_bin(&lang::TYPESCRIPT))
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn typescript_diagnostics_round_trip() {
        if !tsserver_available() {
            eprintln!("skipping: typescript-language-server not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\":{\"strict\":true}}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("a.ts"), "const x: number = \"nope\";\n").unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        let (diags, timed_out) = server
            .do_diagnostics_with_timeout("a.ts".to_string(), std::time::Duration::from_secs(45))
            .await
            .unwrap();
        assert!(!timed_out, "typescript-language-server did not respond within 45s");
        assert!(!diags.is_empty(), "expected a type diagnostic");
    }
}
```

- [ ] **Step 4: Run (self-skips where the toolchain is absent)**

Run: `cargo test -p otto-mcp-lsp integration`
Expected: PASS — each runs or prints its skip line.

- [ ] **Step 5: Commit**

```bash
git add crates/mcp-lsp/src/main.rs
git commit -m "test(mcp-lsp): self-skipping go/python/typescript integration round-trips"
```

---

### Task 8: documentation

**Files:**
- Modify: `CLAUDE.md`, `docs/ARCHITECTURE.md`, `docs/superpowers/specs/2026-07-07-mcp-lsp-design.md`

- [ ] **Step 1: Update `CLAUDE.md`**

In the intro paragraph describing the MCP tier and in the crate-table `mcp-lsp` row, remove "additive and Rust-only in v1, multi-language dispatch deferred" and replace with: multi-language dispatch (Rust/TS/JS/Python/Go) via `rust-analyzer`/`typescript-language-server`/`pyright-langserver`/`gopls`, lazily spawned per language behind a per-key lock, with per-server first-open diagnostics budgets and env overrides (`OTTO_RUST_ANALYZER_BIN`, `OTTO_TYPESCRIPT_LANGUAGE_SERVER_BIN`, `OTTO_PYRIGHT_LANGSERVER_BIN`, `OTTO_GOPLS_BIN`); registered additively only when ≥1 server is on PATH.

- [ ] **Step 2: Update `docs/ARCHITECTURE.md`**

Grep `mcp-lsp` (it appears in the crate-tree comment and the MCP-tier prose) and drop "deferred to v2" / "Rust-only v1", replacing with the shipped multi-language description.

- [ ] **Step 3: Add a status note to the v1 design**

Under the Status line at the top of `docs/superpowers/specs/2026-07-07-mcp-lsp-design.md`, add:

```markdown
> **Update (2026-07-08):** the "Future generalization" section below is now built — multi-language
> dispatch (Rust/TS/JS/Python/Go) shipped per
> [2026-07-08-mcp-lsp-multi-language-design.md](2026-07-08-mcp-lsp-multi-language-design.md).
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md docs/superpowers/specs/2026-07-07-mcp-lsp-design.md
git commit -m "docs: mcp-lsp is now multi-language (Rust/TS/JS/Python/Go)"
```

---

### Task 9: full verification sweep

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then `git diff --stat`; if fmt changed anything, `git commit -am "style: cargo fmt"`.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings in changed crates (confirms `any_server_available`/helpers are all wired — no residual `dead_code`). Fix any findings in the touched files and commit as `style(mcp-lsp): clippy`.

- [ ] **Step 3: Full offline test suite (determinism invariant)**

Run: `cargo test --workspace`
Expected: PASS with no network/keys — confirms the multi-language change did not perturb the offline-deterministic engine path (`mcp-lsp` is spawned only by the binary; no unit test execs a real server).

- [ ] **Step 4: Targeted runs**

Run: `cargo test -p otto-mcp-lsp && cargo test -p otto-engine connect_lsp`
Expected: PASS. Integration tests self-skip where a server binary is absent.

- [ ] **Step 5: Confirm clean**

```bash
git status   # should be clean
```

---

## Spec coverage check

- Language table + languageIds + env overrides → Task 2. PATH resolver + executable bit + gate helper → Task 3. Synchronous liveness + defensive `MethodNotFound` reply + capabilities invariant → Task 4. Per-key lazy registry + `Option<Child>` + dispatch + cold-start budget + eviction + PATH gate → Task 5. `spawn_process` args → Task 1. Engine pre-handshake-exit verification → Task 6. Per-language self-skipping integration tests → Task 7. Docs → Task 8. Determinism + clippy + fmt sweep → Task 9.
- **Review-resolution test coverage:** R1 per-key non-blocking → `a_busy_server_slot_does_not_block_a_warm_call_to_another`. R3 cold-start budget state transition → `diagnostics_budget_uses_first_open_then_steady_state` (deterministic, no real server). R4 permanent-absent cache → `an_absent_server_is_cached_and_short_circuits`; crashed-client eviction → `a_dead_client_is_evicted_so_the_next_call_respawns` (works because `write()` flips liveness synchronously and `open_if_needed` evicts on notify failure). R2b defensive reply + capabilities invariant → `reader_replies_method_not_found_to_unknown_server_requests` + `write_failure_marks_the_client_dead`. Case-folding → `open_if_needed_lowercases_the_extension`. `resolved_bin` override → `resolved_bin_defaults_without_an_override_and_honors_one`.
- Documented non-goals (per-sub-project rootUri, supervision loop, incremental didChange, runtime probing, richer capabilities, Windows, `.js` semantic-only, tsserver staged publish) need no code and live in the spec's "Known limitations / accepted trades".

## Known execution notes

- **Task 5 is intentionally one atomic task** — replacing the `lsp` field touches the struct, `new`, `open_if_needed`, all `do_*`, `main()`, and every test at once; there is no compiling intermediate. All other tasks are independently green.
- The `evict` vs in-flight-`get_or_spawn` interaction (evict could null a slot a concurrent spawn just populated) is accepted under the design's "no supervision loop" non-goal; the orchestrator spine issues tool calls sequentially, so it is not exercised.
