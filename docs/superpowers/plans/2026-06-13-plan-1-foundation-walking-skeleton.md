# otto Plan 1 — Foundation & Walking Skeleton Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a headless otto engine that runs a complete `Plan → Execute → Verify → Done` turn against a `LocalWorkspace`, driven by a deterministic provider through the `Agent`/`Provider`/`Workspace` trait seams, emitting a typed `Event` stream — proving the spine end-to-end with no network, no real LLM, and no UI.

**Architecture:** A Cargo workspace (edition 2024) of small focused crates. `protocol` holds wire types shared with the future UI. `engine-core` defines the four seam traits and the deterministic orchestrator state machine. `workspace`, `providers`, and `agents` provide the minimal local impls. `engine` wires them and exposes a tiny CLI that runs a canned turn. Everything is exercised by an end-to-end integration test using a deterministic provider, so CI needs no external services.

**Tech Stack:** Rust (edition 2024, toolchain 1.85+), tokio, async-trait, serde + serde_json, uuid, anyhow, thiserror, tempfile (dev).

---

## File Structure

```
otto-next/
├── Cargo.toml                      # workspace manifest
├── rust-toolchain.toml             # pin stable toolchain
└── crates/
    ├── protocol/
    │   ├── Cargo.toml
    │   └── src/lib.rs               # SessionId, Role, Command, Event, EventKind, CapabilitiesManifest
    ├── engine-core/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs               # re-exports
    │       ├── traits.rs            # Provider, Workspace, Agent traits + ctx
    │       ├── types.rs             # CompleteRequest/Response, Edit, Milestone, AgentRequest/Output
    │       ├── registry.rs          # AgentRegistry (Role -> Box<dyn Agent>)
    │       └── orchestrator.rs       # Orchestrator state machine + Emitter
    ├── workspace/
    │   ├── Cargo.toml
    │   └── src/lib.rs               # LocalWorkspace (path-contained read/list/apply_edit)
    ├── providers/
    │   ├── Cargo.toml
    │   └── src/lib.rs               # LocalProvider (deterministic)
    ├── agents/
    │   ├── Cargo.toml
    │   └── src/lib.rs               # StubPlanner, StubContextFinder, EchoCoder, StubVerifier
    └── engine/
        ├── Cargo.toml
        └── src/
            ├── lib.rs               # build_default_engine() wiring helper
            ├── main.rs              # CLI: `otto run "<goal>" [--root <path>]`
            └── tests/turn.rs        # (placed under tests/ — see Task 8) end-to-end turn test
```

**Responsibility boundaries:**
- `protocol` depends only on serde/uuid — it is the *only* crate the future UI will link. No I/O, no engine logic.
- `engine-core` defines traits + orchestrator; it must NOT depend on the concrete impl crates (`workspace`, `providers`, `agents`).
- `workspace`/`providers`/`agents` depend on `engine-core` (for the traits) and `protocol`.
- `engine` depends on all of the above and is the only crate that wires concrete impls together.

---

## Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`

- [ ] **Step 1: Create the workspace manifest**

Create `Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = [
    "crates/protocol",
    "crates/engine-core",
    "crates/workspace",
    "crates/providers",
    "crates/agents",
    "crates/engine",
]

[workspace.package]
edition = "2024"
rust-version = "1.85"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync"] }
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4", "serde"] }
anyhow = "1"
thiserror = "2"
tempfile = "3"
```

- [ ] **Step 2: Pin the toolchain**

Create `rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.85"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 3: Verify the workspace is recognized**

Run: `cargo metadata --no-deps --format-version 1 > /dev/null && echo OK`
Expected: prints `OK` (members don't exist yet, so `cargo build` would fail — `metadata` only validates the manifest is parseable; if it complains about missing members, that is expected until later tasks create them. If so, skip to Task 2 and build then.)

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml rust-toolchain.toml
git commit -m "chore: scaffold cargo workspace"
```

---

## Task 2: `protocol` crate — wire types

**Files:**
- Create: `crates/protocol/Cargo.toml`
- Create: `crates/protocol/src/lib.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/protocol/Cargo.toml`:

```toml
[package]
name = "otto-protocol"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
uuid.workspace = true
```

- [ ] **Step 2: Write the failing test**

Create `crates/protocol/src/lib.rs` with the types and a serde round-trip test:

```rust
//! Wire types shared between the engine and any frontend.
//! This crate has no I/O and no engine logic.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies a single agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// The role an atomic agent plays in the orchestrator spine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Planner,
    ContextFinder,
    Coder,
    Verifier,
    Custom(String),
}

/// Commands sent from a frontend to the engine (request/response channel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    CreateSession,
    SendPrompt { session: SessionId, text: String },
    Abort { session: SessionId },
}

/// The body of an event emitted by the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    AgentStarted { role: Role },
    AgentFinished { role: Role },
    FileEdit { path: PathBuf, bytes_written: u64 },
    VerifyResult { ok: bool, detail: String },
    Log { message: String },
    TurnComplete { ok: bool },
}

/// A sequenced, session-scoped event in the engine -> frontend stream.
/// `seq` is monotonic per session so reconnecting clients can replay via Last-Event-ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub session: SessionId,
    pub kind: EventKind,
}

/// What the running engine environment can do. The frontend composes its behavior
/// from the intersection of this manifest and its own form factor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilitiesManifest {
    pub engine_remote: bool,
    pub local_llm: bool,
    pub sandbox: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trips_through_json() {
        let event = Event {
            seq: 7,
            session: SessionId::new(),
            kind: EventKind::FileEdit {
                path: PathBuf::from("otto_output.txt"),
                bytes_written: 42,
            },
        };

        let json = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(event, back);
    }

    #[test]
    fn command_round_trips_through_json() {
        let cmd = Command::SendPrompt {
            session: SessionId::new(),
            text: "add a greeting".to_string(),
        };

        let json = serde_json::to_string(&cmd).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(cmd, back);
    }
}
```

Note: `serde_json` is only needed for the test. Add it as a dev-dependency.

Append to `crates/protocol/Cargo.toml`:

```toml
[dev-dependencies]
serde_json.workspace = true
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p otto-protocol`
Expected: PASS — `event_round_trips_through_json` and `command_round_trips_through_json` both pass.

- [ ] **Step 4: Commit**

```bash
git add crates/protocol
git commit -m "feat(protocol): wire types for sessions, commands, and events"
```

---

## Task 3: `engine-core` — seam traits and core types

**Files:**
- Create: `crates/engine-core/Cargo.toml`
- Create: `crates/engine-core/src/types.rs`
- Create: `crates/engine-core/src/traits.rs`
- Create: `crates/engine-core/src/lib.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/engine-core/Cargo.toml`:

```toml
[package]
name = "otto-engine-core"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-protocol = { path = "../protocol" }
async-trait.workspace = true
anyhow.workspace = true
thiserror.workspace = true
serde.workspace = true
```

- [ ] **Step 2: Define the core data types**

Create `crates/engine-core/src/types.rs`:

```rust
//! Plain data passed across the trait seams. No behavior.

use std::path::PathBuf;

/// A request to an LLM provider.
#[derive(Debug, Clone, PartialEq)]
pub struct CompleteRequest {
    pub prompt: String,
}

/// A provider's completion.
#[derive(Debug, Clone, PartialEq)]
pub struct CompleteResponse {
    pub text: String,
}

/// A single file edit. For the walking skeleton an edit is a full-file write;
/// real unified-diff application arrives with the real Coder agent in a later plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub path: PathBuf,
    pub new_contents: String,
}

/// One unit of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Milestone {
    pub description: String,
}

/// The uniform request passed to any atomic agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRequest {
    Plan { goal: String },
    FindContext { goal: String },
    Code { goal: String, context: Vec<PathBuf> },
    Verify,
}

/// The uniform structured output returned by any atomic agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutput {
    Plan { milestones: Vec<Milestone> },
    Context { files: Vec<PathBuf> },
    Code { edits: Vec<Edit> },
    Verify { ok: bool, detail: String },
}
```

- [ ] **Step 3: Define the seam traits**

Create `crates/engine-core/src/traits.rs`:

```rust
//! The four seams that keep otto's axes decoupled. Only `Provider`, `Workspace`,
//! and `Agent` are exercised in the walking skeleton; `RemoteTarget` arrives later.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::types::{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit};

/// An LLM provider (local Ollama, remote Claude, etc.). In-process by default.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse>;
}

/// The repository the engine operates on. `LocalWorkspace` edits a real folder in
/// place (no clone); `RemoteWorkspace` operates on a remote checkout (later plan).
#[async_trait]
pub trait Workspace: Send + Sync {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>>;
    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>>;
    /// Apply a full-file edit, returning the number of bytes written.
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64>;
}

/// A small, single-purpose atomic agent. Native in v1; the trait is the seam where
/// a wasm32-wasip2 agent backend slots in later.
#[async_trait]
pub trait Agent: Send + Sync {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput>;
}

/// Scoped resources an agent may use during a turn.
pub struct AgentCtx<'a> {
    pub provider: &'a dyn Provider,
    pub workspace: &'a dyn Workspace,
}
```

- [ ] **Step 4: Wire up the lib root**

Create `crates/engine-core/src/lib.rs`:

```rust
//! otto engine core: the orchestrator state machine and the trait seams it drives.

pub mod orchestrator;
pub mod registry;
pub mod traits;
pub mod types;

pub use orchestrator::{Emitter, Orchestrator, TurnOutcome};
pub use registry::AgentRegistry;
pub use traits::{Agent, AgentCtx, Provider, Workspace};
pub use types::{
    AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, Milestone,
};
```

Note: this references `orchestrator` and `registry` modules created in Tasks 6 and 7. The crate will not compile until those exist; that is expected. Do NOT run `cargo build` for this crate until Task 7.

- [ ] **Step 5: Commit**

```bash
git add crates/engine-core
git commit -m "feat(engine-core): seam traits and core data types"
```

---

## Task 4: `workspace` crate — LocalWorkspace with path containment

**Files:**
- Create: `crates/workspace/Cargo.toml`
- Create: `crates/workspace/src/lib.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/workspace/Cargo.toml`:

```toml
[package]
name = "otto-workspace"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-engine-core = { path = "../engine-core" }
async-trait.workspace = true
anyhow.workspace = true
tokio = { workspace = true, features = ["fs"] }

[dev-dependencies]
tempfile.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "fs"] }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/workspace/src/lib.rs`:

```rust
//! `LocalWorkspace`: edits a real on-disk folder in place, with path containment.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use otto_engine_core::types::Edit;
use otto_engine_core::traits::Workspace;

/// A workspace rooted at a real directory on disk. All paths are resolved relative
/// to `root` and may never escape it.
pub struct LocalWorkspace {
    root: PathBuf,
}

impl LocalWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve a workspace-relative path against the root, rejecting any path that
    /// escapes the root via `..` or absolute components.
    fn contain(&self, path: &Path) -> anyhow::Result<PathBuf> {
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    anyhow::bail!("path escapes workspace root: {}", path.display())
                }
                Component::Prefix(_) | Component::RootDir => {
                    anyhow::bail!("absolute paths are not allowed: {}", path.display())
                }
                _ => {}
            }
        }
        Ok(self.root.join(path))
    }
}

#[async_trait]
impl Workspace for LocalWorkspace {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        let full = self.contain(path)?;
        Ok(tokio::fs::read(full).await?)
    }

    async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
        // Skeleton: shallow listing of the root, relative paths. Globbing arrives
        // with the retrieval/ContextFinder work in a later plan.
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut out = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if let Ok(rel) = entry.path().strip_prefix(&self.root) {
                out.push(rel.to_path_buf());
            }
        }
        out.sort();
        Ok(out)
    }

    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
        let full = self.contain(&edit.path)?;
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full, edit.new_contents.as_bytes()).await?;
        Ok(edit.new_contents.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_edit_writes_file_and_read_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());

        let edit = Edit {
            path: PathBuf::from("greeting.txt"),
            new_contents: "hello otto".to_string(),
        };
        let written = ws.apply_edit(&edit).await.unwrap();
        assert_eq!(written, 10);

        let bytes = ws.read(Path::new("greeting.txt")).await.unwrap();
        assert_eq!(bytes, b"hello otto");
    }

    #[tokio::test]
    async fn apply_edit_rejects_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());

        let edit = Edit {
            path: PathBuf::from("../escape.txt"),
            new_contents: "nope".to_string(),
        };
        let err = ws.apply_edit(&edit).await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace root"));
    }

    #[tokio::test]
    async fn list_returns_relative_entries() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        ws.apply_edit(&Edit {
            path: PathBuf::from("a.txt"),
            new_contents: "a".to_string(),
        })
        .await
        .unwrap();

        let listing = ws.list("*").await.unwrap();
        assert_eq!(listing, vec![PathBuf::from("a.txt")]);
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p otto-workspace`
Expected: PASS — all three tests pass (`apply_edit_writes_file_and_read_returns_it`, `apply_edit_rejects_parent_dir_escape`, `list_returns_relative_entries`).

- [ ] **Step 4: Commit**

```bash
git add crates/workspace
git commit -m "feat(workspace): LocalWorkspace with path containment"
```

---

## Task 5: `providers` crate — deterministic LocalProvider

**Files:**
- Create: `crates/providers/Cargo.toml`
- Create: `crates/providers/src/lib.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/providers/Cargo.toml`:

```toml
[package]
name = "otto-providers"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-engine-core = { path = "../engine-core" }
async-trait.workspace = true
anyhow.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 2: Write the failing test**

Create `crates/providers/src/lib.rs`:

```rust
//! `LocalProvider`: a deterministic provider used for tests and offline runs.
//! It performs no network I/O and returns a fixed transform of the prompt.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};

/// A provider whose output is a pure function of its input — used to drive the
/// spine deterministically in CI. Replaced by real providers in a later plan.
pub struct LocalProvider;

impl LocalProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn id(&self) -> &str {
        "local"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        Ok(CompleteResponse {
            text: format!("// generated by otto local provider\n{}\n", req.prompt),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn complete_is_deterministic() {
        let provider = LocalProvider::new();
        let req = CompleteRequest {
            prompt: "add a greeting".to_string(),
        };

        let a = provider.complete(req.clone()).await.unwrap();
        let b = provider.complete(req).await.unwrap();

        assert_eq!(a, b);
        assert!(a.text.contains("add a greeting"));
        assert_eq!(provider.id(), "local");
    }
}
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p otto-providers`
Expected: PASS — `complete_is_deterministic` passes.

- [ ] **Step 4: Commit**

```bash
git add crates/providers
git commit -m "feat(providers): deterministic LocalProvider for offline runs and CI"
```

---

## Task 6: `agents` crate — stub atomic agents

**Files:**
- Create: `crates/agents/Cargo.toml`
- Create: `crates/agents/src/lib.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/agents/Cargo.toml`:

```toml
[package]
name = "otto-agents"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-engine-core = { path = "../engine-core" }
async-trait.workspace = true
anyhow.workspace = true

[dev-dependencies]
otto-providers = { path = "../providers" }
otto-workspace = { path = "../workspace" }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "fs"] }
tempfile.workspace = true
```

- [ ] **Step 2: Write the failing tests**

Create `crates/agents/src/lib.rs`. Four minimal agents that honor the uniform `Agent` trait. `EchoCoder` exercises the `Provider` seam end-to-end by turning a completion into a file edit.

```rust
//! Walking-skeleton atomic agents. These return canned/structured output so the
//! orchestrator spine can be proven before real LLM-backed agents arrive.

use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{
    AgentOutput, AgentRequest, CompleteRequest, Edit, Milestone,
};

/// Turns a goal into a single milestone.
pub struct StubPlanner;

#[async_trait]
impl Agent for StubPlanner {
    async fn run(&self, req: AgentRequest, _ctx: &AgentCtx<'_>) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Plan { goal } = req else {
            anyhow::bail!("StubPlanner received a non-Plan request");
        };
        Ok(AgentOutput::Plan {
            milestones: vec![Milestone { description: goal }],
        })
    }
}

/// Returns an empty context set in the skeleton.
pub struct StubContextFinder;

#[async_trait]
impl Agent for StubContextFinder {
    async fn run(&self, req: AgentRequest, _ctx: &AgentCtx<'_>) -> anyhow::Result<AgentOutput> {
        let AgentRequest::FindContext { .. } = req else {
            anyhow::bail!("StubContextFinder received a non-FindContext request");
        };
        Ok(AgentOutput::Context { files: Vec::new() })
    }
}

/// Calls the provider with the goal and writes the completion to `otto_output.txt`.
/// This is the agent that exercises the Provider seam end-to-end.
pub struct EchoCoder;

#[async_trait]
impl Agent for EchoCoder {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx<'_>) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Code { goal, .. } = req else {
            anyhow::bail!("EchoCoder received a non-Code request");
        };
        let completion = ctx
            .provider
            .complete(CompleteRequest { prompt: goal })
            .await?;
        Ok(AgentOutput::Code {
            edits: vec![Edit {
                path: PathBuf::from("otto_output.txt"),
                new_contents: completion.text,
            }],
        })
    }
}

/// Always reports success in the skeleton.
pub struct StubVerifier;

#[async_trait]
impl Agent for StubVerifier {
    async fn run(&self, req: AgentRequest, _ctx: &AgentCtx<'_>) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Verify = req else {
            anyhow::bail!("StubVerifier received a non-Verify request");
        };
        Ok(AgentOutput::Verify {
            ok: true,
            detail: "skeleton verifier: no checks run".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_providers::LocalProvider;
    use otto_workspace::LocalWorkspace;

    fn ctx<'a>(
        provider: &'a LocalProvider,
        workspace: &'a LocalWorkspace,
    ) -> AgentCtx<'a> {
        AgentCtx { provider, workspace }
    }

    #[tokio::test]
    async fn planner_produces_one_milestone_from_goal() {
        let provider = LocalProvider::new();
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let out = StubPlanner
            .run(
                AgentRequest::Plan { goal: "add a greeting".to_string() },
                &ctx(&provider, &ws),
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Plan { milestones } => {
                assert_eq!(milestones.len(), 1);
                assert_eq!(milestones[0].description, "add a greeting");
            }
            other => panic!("expected Plan output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn coder_turns_completion_into_an_edit() {
        let provider = LocalProvider::new();
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let out = EchoCoder
            .run(
                AgentRequest::Code {
                    goal: "add a greeting".to_string(),
                    context: Vec::new(),
                },
                &ctx(&provider, &ws),
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Code { edits } => {
                assert_eq!(edits.len(), 1);
                assert_eq!(edits[0].path, PathBuf::from("otto_output.txt"));
                assert!(edits[0].new_contents.contains("add a greeting"));
            }
            other => panic!("expected Code output, got {other:?}"),
        }
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p otto-agents`
Expected: PASS — `planner_produces_one_milestone_from_goal` and `coder_turns_completion_into_an_edit` pass.

- [ ] **Step 4: Commit**

```bash
git add crates/agents
git commit -m "feat(agents): walking-skeleton atomic agents"
```

---

## Task 7: `engine-core` — AgentRegistry and Orchestrator state machine

**Files:**
- Create: `crates/engine-core/src/registry.rs`
- Create: `crates/engine-core/src/orchestrator.rs`

- [ ] **Step 1: Write the registry**

Create `crates/engine-core/src/registry.rs`:

```rust
//! Maps each `Role` to the agent that fulfills it. Built-in agents are registered
//! by the engine; user-defined agents register here too in a later plan.

use std::collections::HashMap;
use std::sync::Arc;

use otto_protocol::Role;

use crate::traits::Agent;

#[derive(Default)]
pub struct AgentRegistry {
    agents: HashMap<Role, Arc<dyn Agent>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self { agents: HashMap::new() }
    }

    pub fn register(&mut self, role: Role, agent: Arc<dyn Agent>) {
        self.agents.insert(role, agent);
    }

    pub fn get(&self, role: &Role) -> anyhow::Result<Arc<dyn Agent>> {
        self.agents
            .get(role)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no agent registered for role {role:?}"))
    }
}
```

- [ ] **Step 2: Write the failing orchestrator test**

Create `crates/engine-core/src/orchestrator.rs`. The orchestrator drives `Plan → Execute → Verify → Done`, emitting events through an `Emitter` closure. Tests use inline fake agents so the orchestrator is tested in isolation from the concrete impl crates.

```rust
//! The deterministic orchestrator spine: Plan -> Execute -> Verify -> Done.
//! It owns control flow and event emission; capabilities live in the agents.

use otto_protocol::{Event, EventKind, Role, SessionId};

use crate::registry::AgentRegistry;
use crate::traits::{AgentCtx, Provider, Workspace};
use crate::types::{AgentOutput, AgentRequest};

/// Sink for engine -> frontend events. The engine supplies a real implementation;
/// tests supply a collecting closure.
pub trait Emitter: Send + Sync {
    fn emit(&self, kind: EventKind);
}

impl<F: Fn(EventKind) + Send + Sync> Emitter for F {
    fn emit(&self, kind: EventKind) {
        self(kind)
    }
}

/// The result of running a single turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub ok: bool,
}

pub struct Orchestrator<'a> {
    pub registry: &'a AgentRegistry,
    pub provider: &'a dyn Provider,
    pub workspace: &'a dyn Workspace,
}

impl<'a> Orchestrator<'a> {
    /// Build the per-session monotonic event sequence. Each emitted event takes the
    /// next seq value. Returns the events as a side effect via `emit`.
    pub async fn run_turn(
        &self,
        session: SessionId,
        goal: &str,
        emit: &dyn Emitter,
    ) -> anyhow::Result<TurnOutcome> {
        let ctx = AgentCtx {
            provider: self.provider,
            workspace: self.workspace,
        };

        // --- Plan ---
        emit.emit(EventKind::AgentStarted { role: Role::Planner });
        let plan = self
            .registry
            .get(&Role::Planner)?
            .run(AgentRequest::Plan { goal: goal.to_string() }, &ctx)
            .await?;
        let AgentOutput::Plan { milestones } = plan else {
            anyhow::bail!("planner returned unexpected output");
        };
        emit.emit(EventKind::Log {
            message: format!("planned {} milestone(s)", milestones.len()),
        });
        emit.emit(EventKind::AgentFinished { role: Role::Planner });

        // --- Execute ---
        emit.emit(EventKind::AgentStarted { role: Role::ContextFinder });
        let context = self
            .registry
            .get(&Role::ContextFinder)?
            .run(AgentRequest::FindContext { goal: goal.to_string() }, &ctx)
            .await?;
        let AgentOutput::Context { files } = context else {
            anyhow::bail!("context finder returned unexpected output");
        };
        emit.emit(EventKind::AgentFinished { role: Role::ContextFinder });

        emit.emit(EventKind::AgentStarted { role: Role::Coder });
        let coded = self
            .registry
            .get(&Role::Coder)?
            .run(AgentRequest::Code { goal: goal.to_string(), context: files }, &ctx)
            .await?;
        let AgentOutput::Code { edits } = coded else {
            anyhow::bail!("coder returned unexpected output");
        };
        for edit in &edits {
            let bytes_written = self.workspace.apply_edit(edit).await?;
            emit.emit(EventKind::FileEdit {
                path: edit.path.clone(),
                bytes_written,
            });
        }
        emit.emit(EventKind::AgentFinished { role: Role::Coder });

        // --- Verify ---
        emit.emit(EventKind::AgentStarted { role: Role::Verifier });
        let verified = self
            .registry
            .get(&Role::Verifier)?
            .run(AgentRequest::Verify, &ctx)
            .await?;
        let AgentOutput::Verify { ok, detail } = verified else {
            anyhow::bail!("verifier returned unexpected output");
        };
        emit.emit(EventKind::VerifyResult { ok, detail });
        emit.emit(EventKind::AgentFinished { role: Role::Verifier });

        // --- Done ---
        emit.emit(EventKind::TurnComplete { ok });
        let _ = session; // sequencing/seq assignment is the engine's job in a later plan
        Ok(TurnOutcome { ok })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Agent, Provider, Workspace};
    use crate::types::{CompleteRequest, CompleteResponse, Edit, Milestone};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    struct FakeProvider;
    #[async_trait]
    impl Provider for FakeProvider {
        fn id(&self) -> &str {
            "fake"
        }
        async fn complete(&self, _req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse { text: "fake".to_string() })
        }
    }

    #[derive(Default)]
    struct RecordingWorkspace {
        edits: Mutex<Vec<Edit>>,
    }
    #[async_trait]
    impl Workspace for RecordingWorkspace {
        async fn read(&self, _path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
        async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
            self.edits.lock().unwrap().push(edit.clone());
            Ok(edit.new_contents.len() as u64)
        }
    }

    struct FixedPlanner;
    #[async_trait]
    impl Agent for FixedPlanner {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx<'_>) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Plan { milestones: vec![Milestone { description: "m".into() }] })
        }
    }
    struct EmptyContextFinder;
    #[async_trait]
    impl Agent for EmptyContextFinder {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx<'_>) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Context { files: Vec::new() })
        }
    }
    struct OneEditCoder;
    #[async_trait]
    impl Agent for OneEditCoder {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx<'_>) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Code {
                edits: vec![Edit { path: PathBuf::from("out.txt"), new_contents: "hi".into() }],
            })
        }
    }
    struct OkVerifier;
    #[async_trait]
    impl Agent for OkVerifier {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx<'_>) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Verify { ok: true, detail: "ok".into() })
        }
    }

    fn registry() -> AgentRegistry {
        let mut r = AgentRegistry::new();
        r.register(Role::Planner, Arc::new(FixedPlanner));
        r.register(Role::ContextFinder, Arc::new(EmptyContextFinder));
        r.register(Role::Coder, Arc::new(OneEditCoder));
        r.register(Role::Verifier, Arc::new(OkVerifier));
        r
    }

    #[tokio::test]
    async fn run_turn_drives_full_spine_and_emits_ordered_events() {
        let reg = registry();
        let provider = FakeProvider;
        let workspace = RecordingWorkspace::default();
        let orch = Orchestrator { registry: &reg, provider: &provider, workspace: &workspace };

        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };

        let outcome = orch
            .run_turn(SessionId::new(), "do the thing", &sink)
            .await
            .unwrap();

        assert_eq!(outcome, TurnOutcome { ok: true });
        assert_eq!(workspace.edits.lock().unwrap().len(), 1);

        let recorded = events.lock().unwrap().clone();
        // First agent to start is the Planner; last event is TurnComplete{ok:true}.
        assert_eq!(recorded.first(), Some(&EventKind::AgentStarted { role: Role::Planner }));
        assert_eq!(recorded.last(), Some(&EventKind::TurnComplete { ok: true }));
        assert!(recorded.contains(&EventKind::FileEdit {
            path: PathBuf::from("out.txt"),
            bytes_written: 2,
        }));
    }

    #[tokio::test]
    async fn run_turn_errors_when_a_role_is_missing() {
        let mut reg = AgentRegistry::new();
        reg.register(Role::Planner, Arc::new(FixedPlanner));
        // ContextFinder/Coder/Verifier deliberately not registered.
        let provider = FakeProvider;
        let workspace = RecordingWorkspace::default();
        let orch = Orchestrator { registry: &reg, provider: &provider, workspace: &workspace };

        let err = orch
            .run_turn(SessionId::new(), "x", &(|_k: EventKind| {}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no agent registered"));
    }
}
```

- [ ] **Step 3: Run the engine-core tests to verify they pass**

Run: `cargo test -p otto-engine-core`
Expected: PASS — `run_turn_drives_full_spine_and_emits_ordered_events` and `run_turn_errors_when_a_role_is_missing` pass. The crate now compiles (the `lib.rs` from Task 3 resolves its `orchestrator` and `registry` modules).

- [ ] **Step 4: Commit**

```bash
git add crates/engine-core
git commit -m "feat(engine-core): agent registry and deterministic orchestrator spine"
```

---

## Task 8: `engine` crate — wiring, CLI, and end-to-end turn test

**Files:**
- Create: `crates/engine/Cargo.toml`
- Create: `crates/engine/src/lib.rs`
- Create: `crates/engine/src/main.rs`
- Create: `crates/engine/tests/turn.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/engine/Cargo.toml`:

```toml
[package]
name = "otto-engine"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "otto"
path = "src/main.rs"

[dependencies]
otto-protocol = { path = "../protocol" }
otto-engine-core = { path = "../engine-core" }
otto-workspace = { path = "../workspace" }
otto-providers = { path = "../providers" }
otto-agents = { path = "../agents" }
async-trait.workspace = true
anyhow.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "fs"] }

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 2: Write the wiring helper**

Create `crates/engine/src/lib.rs`. `build_default_registry()` registers the four skeleton agents; `run_goal()` ties registry + provider + workspace together and runs one turn, returning the collected events plus the outcome.

```rust
//! Engine wiring: assemble the default agent registry and run a turn end-to-end.

use std::sync::{Arc, Mutex};

use otto_agents::{EchoCoder, StubContextFinder, StubPlanner, StubVerifier};
use otto_engine_core::{AgentRegistry, Orchestrator, TurnOutcome};
use otto_engine_core::traits::{Provider, Workspace};
use otto_protocol::{Event, EventKind, Role, SessionId};

/// Build the registry of built-in walking-skeleton agents.
pub fn build_default_registry() -> AgentRegistry {
    let mut registry = AgentRegistry::new();
    registry.register(Role::Planner, Arc::new(StubPlanner));
    registry.register(Role::ContextFinder, Arc::new(StubContextFinder));
    registry.register(Role::Coder, Arc::new(EchoCoder));
    registry.register(Role::Verifier, Arc::new(StubVerifier));
    registry
}

/// Run one turn for `goal` against `workspace` using `provider`, returning the
/// sequenced events emitted and the final outcome. The engine assigns the per-session
/// monotonic `seq` to each event here (the orchestrator emits bare `EventKind`s).
pub async fn run_goal(
    goal: &str,
    provider: &dyn Provider,
    workspace: &dyn Workspace,
) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
    let registry = build_default_registry();
    let session = SessionId::new();

    let collected: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let next_seq = Arc::new(Mutex::new(0u64));
    let sink = {
        let collected = Arc::clone(&collected);
        let next_seq = Arc::clone(&next_seq);
        move |kind: EventKind| {
            let mut seq = next_seq.lock().unwrap();
            collected.lock().unwrap().push(Event {
                seq: *seq,
                session,
                kind,
            });
            *seq += 1;
        }
    };

    let orchestrator = Orchestrator { registry: &registry, provider, workspace };
    let outcome = orchestrator.run_turn(session, goal, &sink).await?;

    let events = collected.lock().unwrap().clone();
    Ok((events, outcome))
}
```

- [ ] **Step 3: Write the CLI binary**

Create `crates/engine/src/main.rs`:

```rust
//! `otto run "<goal>" [--root <path>]` — run a single turn and print the event stream.

use std::path::PathBuf;

use otto_engine::run_goal;
use otto_providers::LocalProvider;
use otto_workspace::LocalWorkspace;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    if command != "run" {
        eprintln!("usage: otto run \"<goal>\" [--root <path>]");
        std::process::exit(2);
    }

    let goal = args.next().unwrap_or_else(|| {
        eprintln!("error: missing goal");
        std::process::exit(2);
    });

    let mut root = PathBuf::from(".");
    if let Some(flag) = args.next() {
        if flag == "--root" {
            if let Some(path) = args.next() {
                root = PathBuf::from(path);
            }
        }
    }

    let provider = LocalProvider::new();
    let workspace = LocalWorkspace::new(root);

    let (events, outcome) = run_goal(&goal, &provider, &workspace).await?;
    for event in &events {
        println!("[{:>3}] {:?}", event.seq, event.kind);
    }
    println!("turn ok = {}", outcome.ok);

    if !outcome.ok {
        std::process::exit(1);
    }
    Ok(())
}
```

- [ ] **Step 4: Write the end-to-end integration test**

Create `crates/engine/tests/turn.rs`:

```rust
//! End-to-end: a full turn writes the generated file into the workspace and emits a
//! sequenced event stream ending in a successful TurnComplete.

use std::path::{Path, PathBuf};

use otto_engine::run_goal;
use otto_protocol::EventKind;
use otto_providers::LocalProvider;
use otto_workspace::LocalWorkspace;
use otto_engine_core::traits::Workspace;

#[tokio::test]
async fn full_turn_writes_output_file_and_completes_ok() {
    let dir = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new();
    let workspace = LocalWorkspace::new(dir.path());

    let (events, outcome) = run_goal("add a greeting", &provider, &workspace)
        .await
        .unwrap();

    // Outcome and final event.
    assert!(outcome.ok);
    assert_eq!(events.last().unwrap().kind, EventKind::TurnComplete { ok: true });

    // Sequence numbers are monotonic from 0.
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64);
    }

    // The Coder's edit actually hit the workspace.
    let written = workspace.read(Path::new("otto_output.txt")).await.unwrap();
    let text = String::from_utf8(written).unwrap();
    assert!(text.contains("add a greeting"));

    // A FileEdit event was emitted for the output file.
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::FileEdit { path, .. } if path == &PathBuf::from("otto_output.txt")
    )));
}
```

- [ ] **Step 5: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: PASS — all crate tests plus the `full_turn_writes_output_file_and_completes_ok` integration test pass.

- [ ] **Step 6: Verify the CLI runs a real turn**

Run:
```bash
mkdir -p /tmp/otto-demo && cargo run -p otto-engine -- run "add a greeting" --root /tmp/otto-demo
cat /tmp/otto-demo/otto_output.txt
```
Expected: the event stream prints (ending `turn ok = true`), and `otto_output.txt` contains a line including `add a greeting`.

- [ ] **Step 7: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): wire the spine, add otto CLI, and prove a turn end-to-end"
```

---

## Task 9: Workspace-wide quality gate

**Files:** none (verification only)

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: no diff (clean). If it reports changes, run `cargo fmt --all` and re-run.

- [ ] **Step 2: Clippy with warnings denied**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings or errors.

- [ ] **Step 3: Full test run**

Run: `cargo test --workspace`
Expected: PASS — all tests green.

- [ ] **Step 4: Commit any formatting/lint fixups**

```bash
git add -A
git commit -m "chore: fmt and clippy clean across the workspace" || echo "nothing to commit"
```

---

## Done — what Plan 1 delivers

A compiling, tested Cargo workspace where `otto run "<goal>"` drives a real `Plan → Execute → Verify → Done` turn against a `LocalWorkspace`, with the `Agent`/`Provider`/`Workspace` seams in place and exercised end-to-end by a deterministic provider. The seams are exactly the registration/extension points the next plans plug into:

- **Plan 2** adds real `Provider` impls (Anthropic, Ollama) + the Brain-Blend router behind the same `Provider` trait.
- **Plan 3** adds the MCP tool fleet + security, surfaced to agents through `AgentCtx`.
- **Plan 4** replaces the stub agents registered in `build_default_registry()` with real LLM-backed Planner/ContextFinder/Coder/Verifier — no orchestrator change required.
