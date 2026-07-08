# mcp-lsp Multi-Language Dispatch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generalize the `mcp-lsp` MCP server from a single hardcoded `rust-analyzer` client to a lazy, per-language client registry routing each file to the right language server (Rust/TS/JS/Python/Go) by extension — no new tools, no protocol change.

**Architecture:** A pure language table (`lang.rs`: extension → server + LSP languageId, PATH resolver) drives a lazy per-key client registry in `LspServer` (`main.rs`). Each language server spawns on first use behind a per-key lock (so a cold `gopls` never blocks a warm `rust-analyzer`), with a per-server cold-start diagnostics budget, retry-eligible spawn failures, crashed-client eviction, and a startup PATH gate that keeps the whole `lsp.*` toolset absent when no server is installed. `lsp_client.rs` gains a `spawn_process` args param and a defensive `MethodNotFound` reply that protects the load-bearing default-capabilities invariant.

**Tech Stack:** Rust (edition 2024), tokio, rmcp (stdio MCP server), lsp-types, serde_json, tempfile (tests). Design: `docs/superpowers/specs/2026-07-08-mcp-lsp-multi-language-design.md`.

---

## File structure

- `crates/mcp-lsp/src/lang.rs` — **new.** Pure, no I/O beyond a PATH filesystem probe: `ServerSpec`, the four server consts, `config_for_extension`, `resolved_bin`, `resolve_executable`, `any_server_available`. Fully unit-testable.
- `crates/mcp-lsp/src/lsp_client.rs` — **modify.** `spawn_process(bin, args)`; shared `Arc<Mutex>` writer so the reader loop can reply; defensive `MethodNotFound` reply to unknown server→client requests; an `is_alive()` liveness flag; documented capabilities invariant on `initialize`.
- `crates/mcp-lsp/src/main.rs` — **modify.** `mod lang;`; replace the single `lsp` field with the lazy per-key registry (`ReadyServer`, `slots`, `absent`, `served_diag`); `LspServer::new(root)`; `seed_ready_for_test`; `get_or_spawn`; `evict`; rewrite `open_if_needed` to route by extension and return the client; rewire `do_*`; per-server first-open diagnostics budget; the PATH-gate in `main()`. Rewrite the `duplex_server` helper + the `rust_analyzer_integration` test; add per-language integration tests.
- `crates/engine/src/mcp.rs` — **modify (test only).** A test that a child which exits before the MCP handshake surfaces through `connect_lsp` as `Err`.
- `CLAUDE.md`, `docs/ARCHITECTURE.md`, `docs/superpowers/specs/2026-07-07-mcp-lsp-design.md` — **modify (docs).**

Each task ends green (build + relevant tests) and is committed. Run all commands from the repo root `/home/robhicks/dev/otto-next`.

---

### Task 1: `spawn_process` gains an `args` parameter

**Files:**
- Modify: `crates/mcp-lsp/src/lsp_client.rs` (`spawn_process`)
- Modify: `crates/mcp-lsp/src/main.rs` (the two current call sites)

- [ ] **Step 1: Change the signature and pass args**

In `crates/mcp-lsp/src/lsp_client.rs`, change `spawn_process`:

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

Change `spawn_process_with_bogus_binary_errors` to:

```rust
    #[test]
    fn spawn_process_with_bogus_binary_errors() {
        assert!(spawn_process("definitely-not-a-real-binary-xyz", &[]).is_err());
    }
```

- [ ] **Step 3: Update the two call sites in `main.rs`**

In `main.rs`, `main()` currently has `let (lsp, _child) = lsp_client::spawn_process(&rust_analyzer_bin())?;` and the `rust_analyzer_integration` test has `spawn_process(&rust_analyzer_bin()).unwrap()`. Add `, &[]` to both so the crate compiles: `spawn_process(&rust_analyzer_bin(), &[])`. (Both call sites are rewritten later — Tasks 8 and 6 — this only keeps the build green now.)

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

- [ ] **Step 1: Create `lang.rs` with the table and a failing test**

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
    /// Timeout budget for the FIRST `lsp.diagnostics` call against this server, before its
    /// index is warm. Cold pyright/gopls indexing routinely exceeds the 15s steady-state
    /// default; a too-short budget returns `{diagnostics: [], timed_out: true}`, which reads as
    /// falsely "clean".
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

/// The executable to spawn for `spec`: the `env_override` value if set, else `default_bin`.
pub fn resolved_bin(spec: &ServerSpec) -> String {
    std::env::var(spec.env_override).unwrap_or_else(|_| spec.default_bin.to_string())
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
    fn resolved_bin_defaults_without_an_override() {
        // GOPLS's env var is very unlikely to be set in the test environment.
        assert_eq!(resolved_bin(&GOPLS), "gopls");
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/mcp-lsp/src/main.rs`, add near the top with the other `mod` declaration (`mod lsp_client;`):

```rust
mod lang;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-mcp-lsp lang::`
Expected: PASS (all four `lang::tests`).

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-lsp/src/lang.rs crates/mcp-lsp/src/main.rs
git commit -m "feat(mcp-lsp): language dispatch table (ext -> server, languageId)"
```

---

### Task 3: `lang.rs` — PATH executable resolver + availability gate helper

**Files:**
- Modify: `crates/mcp-lsp/src/lang.rs`

- [ ] **Step 1: Add the resolver and its tests (Unix)**

Append to `lang.rs` (before the `#[cfg(test)] mod tests`):

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
        let path_var = dir.path().to_str().unwrap();
        assert_eq!(resolve_executable("myserver", path_var), Some(bin));
    }

    #[test]
    fn resolve_executable_rejects_a_non_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("myserver");
        std::fs::write(&bin, "not executable").unwrap(); // default 0o644
        let path_var = dir.path().to_str().unwrap();
        assert!(resolve_executable("myserver", path_var).is_none());
    }

    #[test]
    fn resolve_executable_honors_a_path_separator_override() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("custom-ra");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);
        // A value with a '/' is checked directly, ignoring PATH.
        assert_eq!(
            resolve_executable(bin.to_str().unwrap(), ""),
            Some(bin.clone())
        );
        // ...and fails if that exact file isn't executable.
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

Note: `tempfile` is already a dev-dependency of `otto-mcp-lsp`.

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-mcp-lsp lang::`
Expected: PASS (table tests + four resolver tests).

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-lsp/src/lang.rs
git commit -m "feat(mcp-lsp): PATH executable resolver + availability gate helper"
```

---

### Task 4: `lsp_client.rs` — liveness flag + defensive `MethodNotFound` reply

**Files:**
- Modify: `crates/mcp-lsp/src/lsp_client.rs`

The reader loop must be able to write a reply, so the writer becomes a shared `Arc<Mutex>`. A liveness flag flips false when the read stream closes (server died), so callers can evict a dead client.

- [ ] **Step 1: Share the writer and add the liveness flag**

In `LspClient`, change the fields:

```rust
pub struct LspClient {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    diagnostics: Arc<Mutex<HashMap<String, DiagEntry>>>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
    alive: Arc<std::sync::atomic::AtomicBool>,
}
```

In `spawn`, build the shared writer and the flag, clone them into the reader task, and reply to unknown server→client requests:

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
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));

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
                // gate these on capability and don't send them (see `initialize`), but reply
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

- [ ] **Step 2: Update `write` and add `is_alive`**

```rust
    async fn write(&self, value: &Value) -> anyhow::Result<()> {
        let mut w = self.writer.lock().await;
        write_message(&mut *w, value).await
    }

    /// False once the server's stream has closed (process exited). Callers use this to evict a
    /// dead client and re-spawn on the next call instead of hanging on every request.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
```

- [ ] **Step 3: Document the capabilities invariant on `initialize`**

Prepend to `initialize`'s doc comment:

```rust
    /// Send `initialize` then `initialized`. `root` becomes the `rootUri`.
    ///
    /// INVARIANT: this client answers no server→client *requests* (the reader loop only replies
    /// `MethodNotFound`). `capabilities` must therefore stay minimal — advertising a richer
    /// capability (pull diagnostics, `workspace/configuration`, dynamic registration) makes a
    /// server send requests the client can't satisfy, stalling it. This minimal-capabilities
    /// choice is exactly why rust-analyzer/tsserver/pyright/gopls all work with no
    /// `initializationOptions`.
```

- [ ] **Step 4: Add the defensive-reply + liveness tests**

Add to `lsp_client.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn reader_replies_method_not_found_to_unknown_server_requests() {
        let (client, server_end) = duplex_client();
        let (sr, mut sw) = tokio::io::split(server_end);
        let mut sr = BufReader::new(sr);

        // Server → client request we don't handle.
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
    async fn is_alive_flips_false_after_the_stream_closes() {
        let (client, server_end) = duplex_client();
        assert!(client.is_alive());
        drop(server_end); // server side gone → reader hits EOF
        // Give the reader task a moment to observe EOF.
        for _ in 0..50 {
            if !client.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!client.is_alive());
    }
```

(`duplex_client`, `BufReader`, and `Duration` are already imported in that test module.)

- [ ] **Step 5: Run the tests**

Run: `cargo test -p otto-mcp-lsp lsp_client::`
Expected: PASS (existing client tests + the two new ones). The pre-existing `initialize`/`request`/diagnostics tests must stay green — the writer is now shared but behavior is unchanged.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp-lsp/src/lsp_client.rs
git commit -m "feat(mcp-lsp): defensive MethodNotFound reply + client liveness flag"
```

---

### Task 5: `main.rs` — lazy per-key client registry

**Files:**
- Modify: `crates/mcp-lsp/src/main.rs` (`LspServer` struct, `new`, `get_or_spawn`, `evict`, `seed_ready_for_test`)

This task introduces the registry and its spawn/seed/evict machinery but does **not** yet rewire `open_if_needed`/`do_*` (Task 6) — so `open_if_needed` still references `self.lsp` and won't compile until Task 6. To keep this task independently green, implement the registry **alongside** the rewire in one commit if your workflow requires a compiling checkpoint; otherwise treat Tasks 5+6 as a single compile unit and run the build at the end of Task 6. (Subagent note: implement Task 5 and Task 6 back-to-back; the crate compiles only after Task 6.)

- [ ] **Step 1: Replace the struct fields and constructor**

Replace the `LspServer` struct and `impl LspServer::new`:

```rust
use std::collections::{HashMap, HashSet};
use tokio::sync::Mutex;

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
    /// server keys that have already returned a non-timed-out diagnostics result — after which
    /// the steady-state (short) diagnostics budget applies instead of the cold-start budget.
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

- [ ] **Step 2: Add `get_or_spawn`, `evict`, and the test seam**

Add these methods to `impl LspServer` (keep `uri_for`, `path_for`, etc.):

```rust
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
        if let Some(slot) = self.slots.lock().await.get(key).cloned() {
            *slot.lock().await = None;
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
    async fn slot_is_ready(&self, key: &'static str) -> bool {
        match self.slots.lock().await.get(key) {
            Some(slot) => slot.lock().await.is_some(),
            None => false,
        }
    }
```

Proceed directly to Task 6 (the crate does not compile until `open_if_needed`/`do_*` are rewired).

---

### Task 6: `main.rs` — rewire dispatch, timeouts, eviction; fix the tests

**Files:**
- Modify: `crates/mcp-lsp/src/main.rs` (`open_if_needed`, `do_*`, `duplex_server`, tests, `rust_analyzer_integration`)

- [ ] **Step 1: Rewrite `open_if_needed` to route by extension and return the client**

```rust
    /// Ensure `path` is open (or up to date) in its language's server. Resolves the extension to
    /// a (server, languageId), spawns the server on first use, sends `didOpen`/`didChange` with
    /// that languageId, and returns the client so the caller issues its request against it.
    async fn open_if_needed(&self, path: &str) -> anyhow::Result<(Arc<LspClient>, String, u64)> {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let (spec, language_id) = lang::config_for_extension(&ext)
            .ok_or_else(|| anyhow::anyhow!("no language server configured for .{ext}"))?;
        let client = self.get_or_spawn(spec).await?;

        let content = String::from_utf8(self.workspace.read(Path::new(path)).await?)?;
        let uri = self.uri_for(path)?;
        let uri_str = uri.as_str().to_string();
        let generation = client.bump_generation(&uri_str).await;
        let mut open = self.open_docs.lock().await;
        match open.get(&uri_str) {
            None => {
                open.insert(uri_str.clone(), 1);
                client
                    .notify(
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
                    .await?;
            }
            Some(&version) => {
                let next = version + 1;
                open.insert(uri_str.clone(), next);
                client
                    .notify(
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
                    .await?;
            }
        }
        Ok((client, uri_str, generation))
    }

    /// Resolve `path`'s server spec (for timeout budgets / eviction) without opening it.
    fn spec_for(&self, path: &str) -> Option<&'static lang::ServerSpec> {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        lang::config_for_extension(&ext).map(|(spec, _)| spec)
    }
```

- [ ] **Step 2: Rewrite `do_diagnostics` with the cold-start budget**

```rust
    pub async fn do_diagnostics(&self, path: String) -> anyhow::Result<(Vec<DiagnosticOut>, bool)> {
        let spec = self
            .spec_for(&path)
            .ok_or_else(|| anyhow::anyhow!("no language server configured for `{path}`"))?;
        let first_open = !self.served_diag.lock().await.contains(spec.key);
        let wait = if first_open {
            spec.first_open_diag_timeout
        } else {
            diagnostics_timeout()
        };
        let (out, timed_out) = self.do_diagnostics_with_timeout(path, wait).await?;
        if !timed_out {
            self.served_diag.lock().await.insert(spec.key);
        }
        Ok((out, timed_out))
    }
```

- [ ] **Step 3: Rewire `do_diagnostics_with_timeout` and the navigation methods to the returned client + evict on death**

In `do_diagnostics_with_timeout`, replace `let (uri, generation) = self.open_if_needed(&path).await?;` and the `self.lsp.wait_for_diagnostics(...)` line:

```rust
    async fn do_diagnostics_with_timeout(
        &self,
        path: String,
        wait: std::time::Duration,
    ) -> anyhow::Result<(Vec<DiagnosticOut>, bool)> {
        let (client, uri, generation) = self.open_if_needed(&path).await?;
        let (diags, timed_out) = client.wait_for_diagnostics(&uri, generation, wait).await;
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
```

For `do_definition`, `do_references`, `do_hover`: bind the client from `open_if_needed`, run the request, and evict on a dead client. Apply this shape (shown for `do_definition`; mirror it for the other two, keeping each one's existing params/response parsing):

```rust
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
        if result.is_err() && !client.is_alive() {
            if let Some(spec) = self.spec_for(&path) {
                self.evict(spec.key).await;
            }
        }
        self.goto_response_to_locations(result?)
    }
```

For `do_references` and `do_hover`, wrap their `client.request(...).await` the same way: capture the `Result`, run the `is_err() && !client.is_alive()` eviction, then `?` the result and proceed with the existing parsing. Every former `self.lsp.<method>` becomes `client.<method>`.

- [ ] **Step 4: Rewrite the `duplex_server` test helper (now async, seeds a client)**

Replace the `duplex_server` helper in `#[cfg(test)] mod tests`:

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

Every existing test that calls `duplex_server(dir.path())` now must `.await` it: `let (server, server_end) = duplex_server(dir.path()).await;`. Update all of them (`open_if_needed_sends_did_open_then_did_change`, `do_diagnostics_returns_fresh_results`, `do_diagnostics_times_out_without_a_response`, `do_definition_parses_a_scalar_location_response`, `do_definition_skips_out_of_root_locations`, `do_definition_returns_empty_for_a_null_response`, `do_references_parses_an_array_of_locations`, `do_hover_renders_markup_contents`, `do_hover_returns_none_for_a_null_response`). The `path_for_round_trips_with_a_non_canonical_root` test builds its server by hand — update it to `LspServer::new(messy)` and seed nothing (it only calls `uri_for`/`path_for`, which don't touch the registry).

- [ ] **Step 5: Add the dispatch + eviction unit tests**

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
    async fn open_if_needed_rejects_an_unsupported_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hi\n").unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        let err = server.open_if_needed("a.txt").await.unwrap_err();
        assert!(err.to_string().contains("no language server configured"));
    }

    #[tokio::test]
    async fn a_dead_client_is_evicted_so_the_next_call_respawns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let (server, server_end) = duplex_server(dir.path()).await;
        assert!(server.slot_is_ready("rust-analyzer").await);
        drop(server_end); // server process "dies"
        // A navigation call now fails and, seeing the dead client, evicts the slot.
        let _ = server.do_definition("a.rs".to_string(), 1, 1).await;
        assert!(!server.slot_is_ready("rust-analyzer").await);
    }
```

- [ ] **Step 6: Rewrite the `rust_analyzer_integration` test to drive the real dispatch path**

Replace the body of `full_round_trip_against_a_real_rust_analyzer` (keep the `rust_analyzer_available()` self-skip helper):

```rust
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

        // Drive the public dispatch path: no hand-wired client. get_or_spawn spawns + initializes
        // rust-analyzer lazily on the first `.rs` open.
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

Delete the now-unused top-level `rust_analyzer_bin()` fn from `main.rs` if the compiler flags it dead (the `rust_analyzer_integration` module's own `rust_analyzer_available()` should use `lang::resolved_bin(&lang::RUST_ANALYZER)` for the binary name).

- [ ] **Step 7: Build and test the whole crate**

Run: `cargo test -p otto-mcp-lsp`
Expected: PASS. The `rust_analyzer_integration` test either runs (if rust-analyzer is installed) or prints the skip message and passes.

- [ ] **Step 8: Commit**

```bash
git add crates/mcp-lsp/src/main.rs
git commit -m "feat(mcp-lsp): lazy per-language client registry with per-key spawn, budgets, eviction"
```

---

### Task 7: `main.rs` — startup PATH availability gate

**Files:**
- Modify: `crates/mcp-lsp/src/main.rs` (`main`)

- [ ] **Step 1: Replace the eager rust-analyzer spawn with the PATH gate**

Rewrite `main()` (removing the eager `spawn_process`/`initialize`/`LspServer::new(lsp, root)`):

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

- [ ] **Step 2: Build**

Run: `cargo build -p otto-mcp-lsp && cargo test -p otto-mcp-lsp`
Expected: PASS. (If no language server is installed, the binary now exits with the gate message when run manually — verified via the engine test in Task 8.)

- [ ] **Step 3: Commit**

```bash
git add crates/mcp-lsp/src/main.rs
git commit -m "feat(mcp-lsp): PATH availability gate — absent when no language server installed"
```

---

### Task 8: engine — verify a pre-handshake exit surfaces as a connect error

**Files:**
- Modify: `crates/engine/src/mcp.rs` (test only)

- [ ] **Step 1: Add the test**

In `crates/engine/src/mcp.rs`'s `#[cfg(test)] mod tests`, add:

```rust
    #[tokio::test]
    async fn connect_lsp_surfaces_a_pre_handshake_exit_as_err() {
        // `false` spawns successfully then exits nonzero immediately — before speaking MCP,
        // exactly as `mcp-lsp` does when its PATH availability gate finds no language server.
        // `connect` must surface this as an Err (so no lsp tools get registered), not hang.
        assert!(connect_lsp("false", Path::new(".")).await.is_err());
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p otto-engine connect_lsp`
Expected: PASS (both `connect_lsp_with_bogus_binary_errors` and the new test).

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/mcp.rs
git commit -m "test(engine): connect_lsp surfaces a pre-handshake exit as Err"
```

---

### Task 9: per-language integration tests (self-skipping)

**Files:**
- Modify: `crates/mcp-lsp/src/main.rs` (add integration test modules)

These run only where the server binary exists; otherwise they print a skip and pass. Each asserts a diagnostics *shape* (a known-broken fixture yields a non-empty diagnostic, not timed out), exercising the timeout/quiescence path, not just navigation.

- [ ] **Step 1: Add the Go integration test**

Append to `main.rs`:

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
        // References an undefined symbol → gopls reports an error.
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
        assert!(
            !diags.is_empty(),
            "expected a diagnostic for the undefined symbol, got none"
        );
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
        // A clear type error: adding a str and an int.
        std::fs::write(dir.path().join("a.py"), "x: int = \"not an int\"\n").unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        let (diags, timed_out) = server
            .do_diagnostics_with_timeout("a.py".to_string(), std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!timed_out, "pyright did not respond within 60s");
        assert!(!diags.is_empty(), "expected a type diagnostic, got none");
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
        std::fs::write(dir.path().join("tsconfig.json"), "{\"compilerOptions\":{\"strict\":true}}\n").unwrap();
        // Type error: assigning a string to a number.
        std::fs::write(dir.path().join("a.ts"), "const x: number = \"nope\";\n").unwrap();
        let server = LspServer::new(dir.path().to_path_buf());
        let (diags, timed_out) = server
            .do_diagnostics_with_timeout("a.ts".to_string(), std::time::Duration::from_secs(45))
            .await
            .unwrap();
        assert!(!timed_out, "typescript-language-server did not respond within 45s");
        assert!(!diags.is_empty(), "expected a type diagnostic, got none");
    }
}
```

- [ ] **Step 4: Run (self-skips where the toolchain is absent)**

Run: `cargo test -p otto-mcp-lsp integration`
Expected: PASS — each test runs or prints its skip line. On a bare CI image only the Rust one runs.

- [ ] **Step 5: Commit**

```bash
git add crates/mcp-lsp/src/main.rs
git commit -m "test(mcp-lsp): self-skipping go/python/typescript integration round-trips"
```

---

### Task 10: documentation

**Files:**
- Modify: `CLAUDE.md`, `docs/ARCHITECTURE.md`, `docs/superpowers/specs/2026-07-07-mcp-lsp-design.md`

- [ ] **Step 1: Update `CLAUDE.md`**

In the intro paragraph describing the MCP tier, change the `mcp-lsp` description so it no longer says "additive and Rust-only in v1, multi-language dispatch deferred." Replace with language noting multi-language dispatch (Rust/TS/JS/Python/Go) via `rust-analyzer`/`typescript-language-server`/`pyright-langserver`/`gopls`, lazy per-language spawn, and PATH-gated presence.

In the crate table `mcp-lsp` row, update the trailing clause similarly: bridges `lsp.*` to the per-extension language server (Rust/TS/JS/Python/Go), lazily spawned per language behind a per-key lock, with per-server first-open diagnostics budgets and env overrides (`OTTO_RUST_ANALYZER_BIN`, `OTTO_TYPESCRIPT_LANGUAGE_SERVER_BIN`, `OTTO_PYRIGHT_LANGSERVER_BIN`, `OTTO_GOPLS_BIN`); registered additively only when ≥1 server is on PATH.

- [ ] **Step 2: Update `docs/ARCHITECTURE.md`**

Find the `mcp-lsp` mention (grep `mcp-lsp` — it appears in the crate-tree comment and the MCP-tier prose) and drop "deferred to v2" / "Rust-only v1", replacing with the shipped multi-language description.

- [ ] **Step 3: Add a status note to the v1 design**

At the top of `docs/superpowers/specs/2026-07-07-mcp-lsp-design.md`, under its Status line, add:

```markdown
> **Update (2026-07-08):** the "Future generalization" section below is now built —
> multi-language dispatch (Rust/TS/JS/Python/Go) shipped per
> [2026-07-08-mcp-lsp-multi-language-design.md](2026-07-08-mcp-lsp-multi-language-design.md).
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md docs/superpowers/specs/2026-07-07-mcp-lsp-design.md
git commit -m "docs: mcp-lsp is now multi-language (Rust/TS/JS/Python/Go)"
```

---

### Task 11: full verification sweep

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `git diff --stat` — if fmt changed anything, review and `git commit -am "style: cargo fmt"`.

- [ ] **Step 2: Clippy (workspace + the standalone mcp-lsp crate)**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings in changed crates. Fix any clippy findings in the touched files (e.g. needless clones, `filter().next()`), commit as `style(mcp-lsp): clippy`.

- [ ] **Step 3: Full offline test suite (determinism invariant)**

Run: `cargo test --workspace`
Expected: PASS with no network/keys. Confirms the multi-language change did not perturb the offline-deterministic engine path (`mcp-lsp` is spawned only by the binary; no unit test execs a real server).

- [ ] **Step 4: Targeted mcp-lsp + engine runs**

Run: `cargo test -p otto-mcp-lsp && cargo test -p otto-engine connect_lsp`
Expected: PASS. Integration tests self-skip where a server binary is absent.

- [ ] **Step 5: Final commit if anything remained**

```bash
git status   # should be clean
```

---

## Spec coverage check

- Language table + languageIds + env overrides → Task 2. PATH resolver + executable bit → Task 3. Per-key lazy registry + `Option<Child>` + retry-eligible-vs-permanent failures → Tasks 5–6. Cold-start diagnostics budget → Task 6. Crashed-client eviction + `is_alive` → Tasks 4, 6. Default-capabilities invariant + defensive `MethodNotFound` reply → Task 4. Startup PATH gate + behavior change → Task 7, verified Task 8. `spawn_process` args → Task 1. Test seam (`seed_ready_for_test`), rewritten `duplex_server`/integration test, PATH-resolver tests, dispatch/eviction/defensive-reply tests → Tasks 3, 4, 6. Per-language self-skipping integration tests asserting diagnostics shape → Task 9. Docs → Task 10. Determinism + clippy + fmt sweep → Task 11.
- Documented non-goals (per-sub-project rootUri, supervision loop, incremental didChange, runtime probing, richer capabilities, Windows, `.js` semantic-only, tsserver staged publish) require no code and are recorded in the spec's "Known limitations / accepted trades".
