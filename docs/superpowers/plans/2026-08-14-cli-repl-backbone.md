# CLI REPL Backbone Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give otto an interactive local-development CLI: a `crates/cli` library that speaks the `Command`/`Event` protocol through a `ClientTransport` seam, plus conversation history in the orchestrator spine so a second prompt knows what the first one did.

**Architecture:** Three layers, built bottom-up. (1) `TurnOutcome` grows to carry a turn's summary — milestones, edited files, verify result — which `record_turn` persists into the existing opaque `TurnRecord.outcome` JSON, so history needs no schema migration. (2) `SessionStore` gains an owner-scoped `turns()` read; `EngineService` folds those records into a bounded `SessionHistory` and threads it through `AgentCtx` to the Planner and ContextFinder. (3) A new `cli` crate whose REPL is written against `Command`/`ServerMessage` only, reaching the engine through `EmbeddedTransport` (in-process, no socket, no auth).

**Tech Stack:** Rust edition 2024, pinned toolchain 1.97.0. Existing deps only, plus **one new**: `rustyline` (readline for the REPL). `serde`/`serde_json` (already everywhere), `tokio`, `async-trait`, `anyhow`, `tempfile` (tests).

**Spec:** `docs/superpowers/specs/2026-08-14-cli-repl-backbone-design.md` — read it first. This plan implements it exactly, including the two corrections made during planning: history derives from `TurnRecord` alone with no event-log join (spec §2.1), and `SessionStore` gains a `turns()` read method (spec §2.1.1).

## Global Constraints

- **Dependency flow stays strictly inward.** `cli` → `protocol` + `engine`. Nothing depends on `cli` but the binary. `engine-core` gains no dependency on any impl crate — in particular it must NOT depend on `persistence`, which is why the `TurnRecord` → `SessionHistory` conversion lives in `engine` (Task 4), not `engine-core`.
- **The security spine is untouched.** No change to the sensitive-path floor, to gated fail-closed Coder edits, to `bash`-only-when-sandboxed, or to any auth mode. `EmbeddedTransport` runs as `UserId::local()` like every other single-machine path.
- **Determinism holds.** No `OTTO_*` read outside `build_router`. The whole new suite runs offline: no network, no API keys, no TTY, no PTY.
- **The existing offline suite must pass unedited.** No existing test may be modified to accommodate a changed prompt. Task 5 is the rail that enforces this and must land before Task 6.
- **No database schema change.** `TurnRecord.outcome` is an opaque `serde_json::Value`; new fields go inside it. **No `PRAGMA user_version` bump** — the store refuses to open on a version mismatch, so a bump would force every user to delete their session database. Pre-existing rows must deserialize with the new fields absent (`#[serde(default)]`).
- **The CLI is English-only.** No i18n catalog, no `t`/`tf`. `ui-dioxus`'s localization boundary does not extend to `crates/cli`.
- **No new protocol variants.** `crates/protocol` is untouched by every task in this plan.
- **Semver** (breaking changes bumped in the same PR):
  - `otto-engine-core` 0.1.0 → 0.2.0 — `TurnOutcome` gains fields; `Orchestrator::run_turn` gains a parameter; `AgentCtx` gains a field.
  - `otto-persistence` 0.1.0 → 0.2.0 — `SessionStore` gains `turns()`.
  - `otto-cli` new at 0.1.0. `otto-protocol` unchanged.
- **No Claude/AI self-attribution** in any commit, comment, doc, or PR body.
- Run `cargo fmt --all` before **every** Rust commit; `cargo clippy --workspace --all-targets` before merge (CI runs `-D warnings`).
- **Known pre-existing failure:** `otto-mcp-lsp`'s `full_round_trip_against_a_real_rust_analyzer` fails on `main` already. Not a regression from this work.

## File Structure

| File | Responsibility |
|---|---|
| `crates/engine-core/src/types.rs` | **Modify.** Add `VerifySummary`. Add `TurnSummary`, `SessionHistory`, `HISTORY_TURNS` (pure data + bounding; no store types). |
| `crates/engine-core/src/orchestrator.rs` | **Modify.** `TurnOutcome` gains `milestones`/`files_edited`/`verify`; `run_turn` populates them and gains a `history: &SessionHistory` parameter. |
| `crates/engine-core/src/traits.rs` | **Modify.** `AgentCtx` private `history` field + `with_history` + `history()` accessor. |
| `crates/engine-core/Cargo.toml` | **Modify.** Version 0.1.0 → 0.2.0. |
| `crates/persistence/src/lib.rs` | **Modify.** `SessionStore::turns()` trait method + `SqliteStore` impl. |
| `crates/persistence/Cargo.toml` | **Modify.** Version 0.1.0 → 0.2.0. |
| `crates/engine/src/service.rs` | **Modify.** Serialize the whole `TurnOutcome` in `record_turn`; build `SessionHistory` from `store.turns()` before each turn; pass it to `run_turn`. |
| `crates/agents/src/planner.rs` | **Modify.** `plan_prompt` takes history; appends a history block only when non-empty. |
| `crates/agents/src/context_finder.rs` | **Modify.** `select_prompt` takes history; same non-empty rule. |
| `crates/cli/Cargo.toml` | **Create.** New library crate. |
| `crates/cli/src/lib.rs` | **Create.** Public `repl()` entry; module wiring. |
| `crates/cli/src/transport.rs` | **Create.** The `ClientTransport` trait + `FakeTransport` (test double, always compiled — the `ScriptedProvider` precedent). |
| `crates/cli/src/embedded.rs` | **Create.** `EmbeddedTransport` — owns an `EngineService`, bridges events to `ServerMessage`. |
| `crates/cli/src/render.rs` | **Create.** Pure `render(&EventKind) -> Vec<String>`; ANSI color gated on `NO_COLOR`/isatty. |
| `crates/cli/src/repl.rs` | **Create.** The loop: rustyline, dispatch, interrupt, non-TTY mode. |
| `crates/engine/src/main.rs` | **Modify.** No-subcommand arm → `otto_cli::repl`; `USAGE` updated. |
| `crates/engine/Cargo.toml` | **Modify.** Depend on `otto-cli`. |
| `Cargo.toml` | **Modify.** Add `crates/cli` to workspace members; add `rustyline` to workspace deps. |
| `CLAUDE.md`, `README.md`, `docs/ARCHITECTURE.md` | **Modify.** Record the `cli` crate and the history capability. |

## Task Order & Rationale

Forced by the inward dependency rule and by one safety requirement. `engine-core` data (Task 1) precedes its persistence (Task 2), which precedes the store read (Task 3) and the history builder (Task 4). **Task 5 — the byte-identical prompt invariant — must land before Task 6**, because it is the only thing that proves threading history did not silently change the offline path's prompts; written afterward it would merely bless whatever the change produced. Tasks 8–10 (the `cli` crate) depend only on the protocol and `engine`, so they could be built in parallel with 1–7, but are sequenced last so the REPL's first run exercises real history.

No red window: every task leaves `cargo test --workspace` green.

---

### Task 1: `TurnOutcome` carries the turn's summary

**Files:**
- Modify: `crates/engine-core/src/types.rs`, `crates/engine-core/src/orchestrator.rs`, `crates/engine-core/Cargo.toml`

**Interfaces:**
- Produces, used by Tasks 2 and 4:
  - `pub struct VerifySummary { pub ok: bool, pub detail: String }` — `Debug + Clone + PartialEq + Serialize + Deserialize`.
  - `TurnOutcome { ok: bool, milestones: Vec<String>, files_edited: Vec<PathBuf>, verify: Option<VerifySummary> }` — `Debug + Clone + PartialEq + Serialize + Deserialize`, every new field `#[serde(default)]`.

- [ ] **Step 1: Write the failing test**

In `crates/engine-core/src/orchestrator.rs`'s `#[cfg(test)] mod tests`, alongside the existing `run_turn_drives_full_spine_and_emits_ordered_events`. That test already builds fake agents; reuse its harness exactly (copy its setup lines — do not refactor it).

```rust
#[tokio::test]
async fn run_turn_outcome_carries_the_turn_summary() {
    // Same harness as run_turn_drives_full_spine_and_emits_ordered_events.
    let (orch_deps, _guard) = test_orchestrator_deps();
    let orch = orch_deps.orchestrator();
    let outcome = orch
        .run_turn(SessionId::new(), "add a hello function", &|_k| {})
        .await
        .unwrap();

    assert!(outcome.ok);
    assert_eq!(
        outcome.milestones,
        vec!["add a hello function".to_string()],
        "the planner's milestone text must survive into the outcome"
    );
    assert_eq!(outcome.files_edited, vec![PathBuf::from("hello.rs")]);
    assert_eq!(
        outcome.verify,
        Some(VerifySummary { ok: true, detail: "ok".to_string() })
    );
}
```

If the existing test module has no `test_orchestrator_deps` helper, inline the same construction the existing test uses and adapt the asserted values to whatever the fake Planner/Coder/Verifier in that module actually return. **Read the existing test first and match its fakes' values** — the exact strings above are illustrative of the shape, not of that module's fixtures.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p otto-engine-core run_turn_outcome_carries_the_turn_summary`
Expected: FAIL to compile — `no field milestones on TurnOutcome`, `cannot find type VerifySummary`.

- [ ] **Step 3: Add `VerifySummary` to `types.rs`**

```rust
/// The verifier's result for a turn, retained on `TurnOutcome` so conversation history can
/// report it without re-scanning the event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifySummary {
    pub ok: bool,
    pub detail: String,
}
```

- [ ] **Step 4: Widen `TurnOutcome` in `orchestrator.rs`**

Replace the existing declaration:

```rust
/// The result of running a single turn.
///
/// Serialized whole into `TurnRecord.outcome` (an opaque JSON column), which is what lets
/// conversation history be rebuilt from turn records alone — no event-log join, no schema
/// migration. Every field added after `ok` is `#[serde(default)]` so turn rows written before
/// this shape existed still deserialize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub ok: bool,
    #[serde(default)]
    pub milestones: Vec<String>,
    #[serde(default)]
    pub files_edited: Vec<std::path::PathBuf>,
    #[serde(default)]
    pub verify: Option<crate::types::VerifySummary>,
}
```

Add `use serde::{Deserialize, Serialize};` to `orchestrator.rs` if absent.

- [ ] **Step 5: Populate the fields in `run_turn`**

In `run_turn`, after the Planner produces `milestones`, capture the text:

```rust
let milestone_texts: Vec<String> =
    milestones.iter().map(|m| m.description.clone()).collect();
```

Declare accumulators before the repair loop:

```rust
let mut files_edited: Vec<std::path::PathBuf> = Vec::new();
let mut verify_summary: Option<crate::types::VerifySummary> = None;
```

At the existing `emit.emit(EventKind::FileEdit { path, bytes_written })` site, also record the path — push the same `path` value that is emitted, so the outcome and the event stream cannot disagree.

At the existing `emit.emit(EventKind::VerifyResult { ok, detail })` site, also set:

```rust
verify_summary = Some(crate::types::VerifySummary {
    ok,
    detail: detail.clone(),
});
```

(The verify site runs once per repair attempt; assigning each time leaves the **last** attempt's result, which is the turn's actual outcome.)

Then update every `TurnOutcome { ok }` construction in this function to:

```rust
TurnOutcome {
    ok,
    milestones: milestone_texts.clone(),
    files_edited: files_edited.clone(),
    verify: verify_summary.clone(),
}
```

- [ ] **Step 6: Fix every other construction site**

Run: `cargo build --workspace --tests 2>&1 | grep -n "missing field"`
Add the three new fields (empty `Vec::new()` / `None`) at each reported site. Expect hits in `crates/engine/src/lib.rs`, `crates/engine/src/service.rs`, and test modules.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p otto-engine-core`
Expected: PASS, including the new test and every pre-existing orchestrator test unedited.

- [ ] **Step 8: Bump the version and commit**

Set `version = "0.2.0"` in `crates/engine-core/Cargo.toml`.

```bash
cargo fmt --all
cargo test --workspace
git add crates/engine-core Cargo.lock
git commit -m "engine-core: TurnOutcome carries the turn's milestones, edited files, and verify result"
```

---

### Task 2: `record_turn` persists the whole outcome

**Files:**
- Modify: `crates/engine/src/service.rs:305`
- Test: `crates/engine/src/service.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `TurnOutcome`'s new fields (Task 1).
- Produces, used by Task 4: `TurnRecord.outcome` is now `serde_json::to_value(&outcome)?` — a full serialized `TurnOutcome`, not `{"ok": bool}`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn record_turn_persists_the_full_outcome_not_just_ok() {
    let store: Arc<dyn SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(":memory:").await.unwrap(),
    );
    let owner = otto_protocol::UserId::local();
    let service = test_service(Arc::clone(&store)); // reuse this module's existing helper
    let session = service
        .create_session(&owner, "goal", &serde_json::json!({}))
        .await
        .unwrap();
    let mut sink = CollectingSink::default();
    service
        .run_prompt(&owner, session, "add a hello function", &mut sink)
        .await
        .unwrap();

    let state = store.snapshot(&owner, session).await.unwrap();
    let stored = &state.turns[0].outcome;
    let parsed: otto_engine_core::orchestrator::TurnOutcome =
        serde_json::from_value(stored.clone()).unwrap();

    assert!(
        !parsed.milestones.is_empty(),
        "milestones must reach the store; record_turn built a hand-rolled json!() before this change"
    );
    assert!(stored.get("files_edited").is_some());
    assert!(stored.get("verify").is_some());
}
```

If `test_service` does not exist in that module, build the service the way the module's other tests do.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p otto-engine record_turn_persists_the_full_outcome_not_just_ok`
Expected: FAIL — `milestones must reach the store`, because `outcome` is the hand-built `{"ok": …}`.

- [ ] **Step 3: Serialize the outcome**

In `run_prompt_with_controls`, replace:

```rust
outcome: serde_json::json!({ "ok": outcome.ok }),
```

with:

```rust
// Serialize the whole outcome, not a hand-built object: conversation history is rebuilt
// from these rows, so a field added to TurnOutcome must reach the store without anyone
// having to remember to widen a json!() literal here.
outcome: serde_json::to_value(&outcome)?,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p otto-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/engine/src/service.rs
git commit -m "engine: persist the whole TurnOutcome in the turn record"
```

---

### Task 3: `SessionStore::turns()`

**Files:**
- Modify: `crates/persistence/src/lib.rs`, `crates/persistence/Cargo.toml`

**Interfaces:**
- Produces, used by Task 4:
  ```rust
  async fn turns(
      &self,
      owner: &otto_protocol::UserId,
      session: SessionId,
  ) -> anyhow::Result<Vec<TurnRecord>>;
  ```
  Ascending `turn_index`. Returns an **empty Vec** for an unknown session and, identically, for a session `owner` does not own — matching `replay_since`'s non-oracle contract.

- [ ] **Step 1: Write the failing tests**

In `crates/persistence/src/lib.rs`'s test module:

```rust
#[tokio::test]
async fn turns_returns_records_in_ascending_turn_index() {
    let store = SqliteStore::open(":memory:").await.unwrap();
    let owner = otto_protocol::UserId::local();
    let s = store.create_session(&owner, "g", &serde_json::json!({})).await.unwrap();
    for i in [0u32, 1, 2] {
        store.record_turn(s, &TurnRecord {
            turn_index: i,
            goal: format!("goal {i}"),
            outcome: serde_json::json!({ "ok": true }),
        }).await.unwrap();
    }

    let turns = store.turns(&owner, s).await.unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(
        turns.iter().map(|t| t.turn_index).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(turns[1].goal, "goal 1");
}

#[tokio::test]
async fn turns_is_empty_for_an_unknown_session() {
    let store = SqliteStore::open(":memory:").await.unwrap();
    let owner = otto_protocol::UserId::local();
    assert!(store.turns(&owner, SessionId::new()).await.unwrap().is_empty());
}

#[tokio::test]
async fn turns_is_empty_for_a_session_owned_by_someone_else() {
    let store = SqliteStore::open(":memory:").await.unwrap();
    let mine = otto_protocol::UserId::local();
    let theirs = otto_protocol::UserId::parse("someone-else").unwrap();
    let s = store.create_session(&theirs, "g", &serde_json::json!({})).await.unwrap();
    store.record_turn(s, &TurnRecord {
        turn_index: 0,
        goal: "secret".to_string(),
        outcome: serde_json::json!({ "ok": true }),
    }).await.unwrap();

    // Indistinguishable from "no such session" — never an existence oracle.
    assert!(store.turns(&mine, s).await.unwrap().is_empty());
}
```

Check `UserId::parse`'s exact signature in `crates/protocol` and adapt if it differs.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-persistence turns_`
Expected: FAIL to compile — `no method named turns`.

- [ ] **Step 3: Add the trait method**

In the `SessionStore` trait, after `replay_since`:

```rust
/// The session's completed turn records, ascending by `turn_index`.
///
/// Scoped by owner exactly like `replay_since`: an unknown session and a session `owner`
/// does not own both return an empty Vec, so this can never become an existence oracle for
/// another principal's sessions.
async fn turns(
    &self,
    owner: &otto_protocol::UserId,
    session: SessionId,
) -> anyhow::Result<Vec<TurnRecord>>;
```

- [ ] **Step 4: Implement it on `SqliteStore`**

Mirror `replay_since`'s owner-scoping idiom (read that method and copy its shape — it already joins on the owner):

```rust
async fn turns(
    &self,
    owner: &otto_protocol::UserId,
    session: SessionId,
) -> anyhow::Result<Vec<TurnRecord>> {
    let rows = sqlx::query(
        "SELECT t.turn_index, t.goal, t.outcome \
         FROM turns t JOIN sessions s ON s.id = t.session \
         WHERE t.session = ? AND s.owner = ? \
         ORDER BY t.turn_index ASC",
    )
    .bind(session.to_string())
    .bind(owner.as_str())
    .fetch_all(&self.pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(TurnRecord {
                turn_index: r.try_get::<i64, _>("turn_index")? as u32,
                goal: r.try_get("goal")?,
                outcome: serde_json::from_str(&r.try_get::<String, _>("outcome")?)?,
            })
        })
        .collect()
}
```

Match the real column names, the session-id binding form, and the `UserId` accessor to whatever `replay_since` and `record_turn` already use in this file — **read them first**; the names above follow the schema as documented but the file is the authority.

- [ ] **Step 5: Fix other implementors**

Run: `cargo build --workspace --tests 2>&1 | grep -n "not all trait items implemented"`
Add `turns` to any test fake or alternate `SessionStore` impl the compiler reports.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p otto-persistence`
Expected: PASS, all three new tests included.

- [ ] **Step 7: Bump the version and commit**

Set `version = "0.2.0"` in `crates/persistence/Cargo.toml`.

```bash
cargo fmt --all
cargo test --workspace
git add crates/persistence Cargo.lock
git commit -m "persistence: add an owner-scoped turns() read to SessionStore"
```

---

### Task 4: `SessionHistory` and its builder

**Files:**
- Modify: `crates/engine-core/src/types.rs`, `crates/engine/src/service.rs`

**Interfaces:**
- Produces, used by Tasks 6 and 7:
  - `pub const HISTORY_TURNS: usize = 10;`
  - `pub const HISTORY_FILES_PER_TURN: usize = 20;`
  - `pub struct TurnSummary { pub turn_index: u32, pub goal: String, pub milestones: Vec<String>, pub files_edited: Vec<PathBuf>, pub verify: Option<VerifySummary>, pub ok: bool }`
  - `pub struct SessionHistory { turns: Vec<TurnSummary> }` with `SessionHistory::empty()`, `SessionHistory::new(Vec<TurnSummary>) -> Self` (applies the bounds), `is_empty()`, `turns() -> &[TurnSummary]`.
  - In `engine`: `pub(crate) fn history_from_records(records: Vec<TurnRecord>) -> SessionHistory`.

- [ ] **Step 1: Write the failing tests**

In `crates/engine-core/src/types.rs` tests:

```rust
#[test]
fn session_history_keeps_only_the_last_n_turns() {
    let turns: Vec<TurnSummary> = (0..25)
        .map(|i| TurnSummary {
            turn_index: i,
            goal: format!("goal {i}"),
            milestones: vec![],
            files_edited: vec![],
            verify: None,
            ok: true,
        })
        .collect();

    let h = SessionHistory::new(turns);
    assert_eq!(h.turns().len(), HISTORY_TURNS);
    // The most recent turns are the ones kept.
    assert_eq!(h.turns().first().unwrap().turn_index, 15);
    assert_eq!(h.turns().last().unwrap().turn_index, 24);
}

#[test]
fn session_history_caps_files_per_turn() {
    let turns = vec![TurnSummary {
        turn_index: 0,
        goal: "g".to_string(),
        milestones: vec![],
        files_edited: (0..100).map(|i| PathBuf::from(format!("f{i}.rs"))).collect(),
        verify: None,
        ok: true,
    }];
    let h = SessionHistory::new(turns);
    assert_eq!(h.turns()[0].files_edited.len(), HISTORY_FILES_PER_TURN);
}

#[test]
fn empty_history_is_empty() {
    assert!(SessionHistory::empty().is_empty());
}
```

In `crates/engine/src/service.rs` tests:

```rust
#[test]
fn history_from_records_parses_outcomes_and_skips_unparseable_ones() {
    let records = vec![
        TurnRecord {
            turn_index: 0,
            goal: "first".to_string(),
            outcome: serde_json::json!({
                "ok": true,
                "milestones": ["m1"],
                "files_edited": ["a.rs"],
                "verify": { "ok": true, "detail": "passed" }
            }),
        },
        // A row written before TurnOutcome grew: must still load, with defaults.
        TurnRecord {
            turn_index: 1,
            goal: "legacy".to_string(),
            outcome: serde_json::json!({ "ok": false }),
        },
        // Corrupt: must be skipped, never panic.
        TurnRecord {
            turn_index: 2,
            goal: "bad".to_string(),
            outcome: serde_json::json!("not an object"),
        },
    ];

    let h = history_from_records(records);
    assert_eq!(h.turns().len(), 2);
    assert_eq!(h.turns()[0].milestones, vec!["m1".to_string()]);
    assert_eq!(h.turns()[0].files_edited, vec![PathBuf::from("a.rs")]);
    assert_eq!(h.turns()[1].goal, "legacy");
    assert!(h.turns()[1].milestones.is_empty());
    assert!(!h.turns()[1].ok);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-engine-core session_history && cargo test -p otto-engine history_from_records`
Expected: FAIL to compile — types not found.

- [ ] **Step 3: Add the types to `engine-core/src/types.rs`**

```rust
/// How many prior turns conversation history carries. Bounded so prompt size does not grow
/// with session length — a 200-turn session must not produce a 200-turn prompt.
pub const HISTORY_TURNS: usize = 10;

/// How many edited paths a single remembered turn contributes.
pub const HISTORY_FILES_PER_TURN: usize = 20;

/// One prior turn, as the spine remembers it.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnSummary {
    pub turn_index: u32,
    pub goal: String,
    pub milestones: Vec<String>,
    pub files_edited: Vec<PathBuf>,
    pub verify: Option<VerifySummary>,
    pub ok: bool,
}

/// The bounded conversation history handed to agents through `AgentCtx`. Construct via `new`
/// (which applies the bounds) or `empty`; the field is private so the bounds cannot be bypassed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionHistory {
    turns: Vec<TurnSummary>,
}

impl SessionHistory {
    /// Retain the most recent `HISTORY_TURNS` turns, each truncated to
    /// `HISTORY_FILES_PER_TURN` paths.
    pub fn new(mut turns: Vec<TurnSummary>) -> Self {
        if turns.len() > HISTORY_TURNS {
            turns.drain(..turns.len() - HISTORY_TURNS);
        }
        for t in &mut turns {
            t.files_edited.truncate(HISTORY_FILES_PER_TURN);
        }
        Self { turns }
    }

    pub fn empty() -> Self {
        Self { turns: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    pub fn turns(&self) -> &[TurnSummary] {
        &self.turns
    }
}
```

Ensure `use std::path::PathBuf;` is present in `types.rs`.

- [ ] **Step 4: Add the builder to `engine/src/service.rs`**

`engine-core` must not depend on `persistence`, so the `TurnRecord` → `SessionHistory` conversion lives here.

```rust
/// Fold persisted turn records into the bounded history the spine hands to agents.
///
/// A record whose `outcome` will not deserialize as a `TurnOutcome` is **skipped**, not fatal:
/// history is an optimization, and one unreadable row from an older or corrupted store must
/// never stop a turn from running.
pub(crate) fn history_from_records(records: Vec<TurnRecord>) -> SessionHistory {
    let summaries = records
        .into_iter()
        .filter_map(|r| {
            let outcome: TurnOutcome = serde_json::from_value(r.outcome).ok()?;
            Some(TurnSummary {
                turn_index: r.turn_index,
                goal: r.goal,
                milestones: outcome.milestones,
                files_edited: outcome.files_edited,
                verify: outcome.verify,
                ok: outcome.ok,
            })
        })
        .collect();
    SessionHistory::new(summaries)
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p otto-engine-core session_history && cargo test -p otto-engine history_from_records`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/engine-core/src/types.rs crates/engine/src/service.rs
git commit -m "engine-core,engine: add bounded SessionHistory and build it from turn records"
```

---

### Task 5: Pin the current prompts (the regression rail)

**This task must land before Task 6.** It asserts today's prompt strings *before* history exists, so Task 6 cannot silently change the offline path. Written afterward it would only bless whatever the change produced.

**Files:**
- Modify: `crates/agents/src/planner.rs`, `crates/agents/src/context_finder.rs`

- [ ] **Step 1: Write the pinning tests**

In `crates/agents/src/planner.rs` tests:

```rust
#[test]
fn plan_prompt_is_pinned_for_the_no_history_case() {
    // The offline suite asserts on LocalProvider output, which echoes this prompt. Threading
    // conversation history must leave this string byte-identical when there is no history.
    let expected = "You are otto's planner. Decompose the goal into an ordered list of concrete milestones.\n\
                    Goal: add a hello function\n\
                    Respond ONLY with valid JSON matching this schema:\n\
                    milestones: array of objects, each with a string field named description.";
    assert_eq!(plan_prompt("add a hello function"), expected);
}
```

**Do not hand-copy the expected string.** Generate it: run `plan_prompt("add a hello function")`, print it, and paste the exact output. A hand-transcribed continuation-line indentation will not match.

Add the equivalent for `select_prompt` in `context_finder.rs`, using that function's real signature and a fixed candidate list.

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p otto-agents prompt_is_pinned`
Expected: PASS immediately — these describe current behavior. If either fails, the expected string was transcribed rather than generated; fix the string, not the code.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/agents/src/planner.rs crates/agents/src/context_finder.rs
git commit -m "agents: pin the Planner and ContextFinder prompts before threading history"
```

---

### Task 6: Thread history through `AgentCtx` and `run_turn`

**Files:**
- Modify: `crates/engine-core/src/traits.rs`, `crates/engine-core/src/orchestrator.rs`, `crates/engine/src/service.rs`

**Interfaces:**
- Consumes: `SessionHistory` (Task 4), `history_from_records` (Task 4), `SessionStore::turns` (Task 3).
- Produces, used by Task 7:
  - `AgentCtx::with_history(self, history: &'a SessionHistory) -> Self` (builder, mirroring `with_retriever`).
  - `AgentCtx::history(&self) -> &SessionHistory` — returns an empty history when none was attached, so agents never branch on `Option`.
  - `Orchestrator::run_turn(&self, session: SessionId, goal: &str, history: &SessionHistory, emit: &dyn Emitter)`.

- [ ] **Step 1: Write the failing test**

In `crates/engine-core/src/traits.rs` tests:

```rust
#[test]
fn agent_ctx_history_defaults_to_empty_and_round_trips() {
    let router = StubRouter;
    let ws = StubWorkspace::default();       // reuse this module's existing stubs
    let tools = ToolRegistry::new(Arc::new(DenyPermissionGate), Arc::new(DenyAsk));

    let ctx = AgentCtx::new(&router, &ws, &tools);
    assert!(ctx.history().is_empty(), "absent history reads as empty, never as an Option");

    let history = SessionHistory::new(vec![TurnSummary {
        turn_index: 0,
        goal: "first goal".to_string(),
        milestones: vec!["m".to_string()],
        files_edited: vec![],
        verify: None,
        ok: true,
    }]);
    let ctx = AgentCtx::new(&router, &ws, &tools).with_history(&history);
    assert_eq!(ctx.history().turns()[0].goal, "first goal");
}
```

Match the stub names/constructors to what that test module already defines.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p otto-engine-core agent_ctx_history`
Expected: FAIL to compile — `no method named with_history`.

- [ ] **Step 3: Extend `AgentCtx`**

Per the CLAUDE.md convention: private field, accessor, builder — never a widened public surface.

```rust
pub struct AgentCtx<'a> {
    router: &'a dyn Router,
    workspace: &'a dyn WorkspaceRead,
    tools: &'a ToolRegistry,
    retriever: Option<&'a dyn crate::retrieval::Retriever>,
    history: Option<&'a crate::types::SessionHistory>,
}
```

Set `history: None` in `new`, and add:

```rust
/// Attach the session's bounded conversation history.
pub fn with_history(mut self, history: &'a crate::types::SessionHistory) -> Self {
    self.history = Some(history);
    self
}

/// The session's prior turns. Returns an empty history when none is attached, so agents
/// never branch on an `Option` — an absent history and a first turn are the same thing.
pub fn history(&self) -> &crate::types::SessionHistory {
    const EMPTY: &crate::types::SessionHistory = &crate::types::SessionHistory::EMPTY;
    self.history.unwrap_or(EMPTY)
}
```

This needs a const empty value. Add to `SessionHistory` in `types.rs`:

```rust
/// A shared empty history, so `AgentCtx::history()` can return a reference without allocating.
pub const EMPTY: SessionHistory = SessionHistory { turns: Vec::new() };
```

(`Vec::new()` is a `const fn`, so this compiles.)

- [ ] **Step 4: Add the parameter to `run_turn`**

```rust
pub async fn run_turn(
    &self,
    session: SessionId,
    goal: &str,
    history: &crate::types::SessionHistory,
    emit: &dyn Emitter,
) -> anyhow::Result<TurnOutcome> {
```

Note `_session` becomes `session`. If nothing in the body uses it yet, keep the underscore-free name and add `let _ = session;` with a comment, or leave it prefixed — do not invent a use for it.

Attach the history to the context:

```rust
let ctx = {
    let base = AgentCtx::new(self.router, self.workspace, self.tools).with_history(history);
    match self.retriever {
        Some(r) => base.with_retriever(r),
        None => base,
    }
};
```

- [ ] **Step 5: Update callers**

Run: `cargo build --workspace --tests 2>&1 | grep -n "run_turn"`
- In `crates/engine/src/service.rs`, before spawning the turn task, load and pass real history:

```rust
let history = {
    let records = self.store.turns(owner, session).await.unwrap_or_default();
    history_from_records(records)
};
```

Move it into the spawned task alongside `goal` (it must be owned — `SessionHistory` is `Clone`), and call `orchestrator.run_turn(session, &goal, &history, &sink_fn).await`.

- Every test call site passes `&SessionHistory::empty()`.

- [ ] **Step 6: Run the full suite**

Run: `cargo test --workspace`
Expected: PASS — **including Task 5's pinning tests unchanged**. Prompts are untouched at this point; only plumbing moved.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/engine-core crates/engine/src/service.rs
git commit -m "engine-core,engine: thread bounded session history into AgentCtx and run_turn"
```

---

### Task 7: Planner and ContextFinder consume history

**Files:**
- Modify: `crates/agents/src/planner.rs`, `crates/agents/src/context_finder.rs`

**Interfaces:**
- Consumes: `AgentCtx::history()` (Task 6).
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Write the failing tests**

In `planner.rs` tests:

```rust
#[test]
fn plan_prompt_with_empty_history_is_unchanged() {
    // The rail: the no-history prompt must equal the pinned string from Task 5.
    assert_eq!(
        plan_prompt("add a hello function", &SessionHistory::empty()),
        plan_prompt_pinned_expectation()  // the same literal Task 5 pinned
    );
}

#[test]
fn plan_prompt_with_history_names_prior_goals_and_files() {
    let history = SessionHistory::new(vec![TurnSummary {
        turn_index: 0,
        goal: "add a hello function".to_string(),
        milestones: vec!["create hello.rs".to_string()],
        files_edited: vec![PathBuf::from("hello.rs")],
        verify: Some(VerifySummary { ok: true, detail: "passed".to_string() }),
        ok: true,
    }]);

    let p = plan_prompt("now add tests for that", &history);
    assert!(p.contains("add a hello function"), "prior goal must appear");
    assert!(p.contains("hello.rs"), "prior edited file must appear");
    assert!(p.contains("now add tests for that"), "current goal must still appear");
}
```

Refactor Task 5's pinned literal into a shared `fn plan_prompt_pinned_expectation() -> String` in the test module so both tests use one source of truth. Add the equivalent pair for `select_prompt`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-agents plan_prompt`
Expected: FAIL to compile — `plan_prompt` takes 1 argument.

- [ ] **Step 3: Implement the history block**

```rust
/// Render prior turns for a prompt. Returns an **empty string** when there is no history, so
/// a first turn's prompt is byte-identical to the pre-history prompt — the invariant Task 5
/// pins and the offline suite depends on.
fn history_block(history: &SessionHistory) -> String {
    if history.is_empty() {
        return String::new();
    }
    let mut s = String::from("\nEarlier in this session:\n");
    for t in history.turns() {
        s.push_str(&format!("- Goal: {}\n", t.goal));
        if !t.milestones.is_empty() {
            s.push_str(&format!("  Planned: {}\n", t.milestones.join("; ")));
        }
        if !t.files_edited.is_empty() {
            let files: Vec<String> = t
                .files_edited
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            s.push_str(&format!("  Edited: {}\n", files.join(", ")));
        }
        if let Some(v) = &t.verify {
            s.push_str(&format!(
                "  Verify: {}\n",
                if v.ok { "passed" } else { "failed" }
            ));
        }
    }
    s
}
```

Put `history_block` in `crates/agents/src/parse.rs` (already a shared helper module) as `pub(crate)`, so both agents use one implementation.

Then in `plan_prompt`, append it — after the goal line, before the schema instruction is fine, but it **must** contribute nothing when empty:

```rust
fn plan_prompt(goal: &str, history: &SessionHistory) -> String {
    format!(
        "You are otto's planner. Decompose the goal into an ordered list of concrete milestones.\n\
         Goal: {goal}{history}\n\
         Respond ONLY with valid JSON matching this schema:\n\
         milestones: array of objects, each with a string field named description.",
        history = history_block(history)
    )
}
```

Update the call site to `plan_prompt(&goal, ctx.history())`. Do the same for `select_prompt` in `context_finder.rs`.

- [ ] **Step 4: Run the full suite**

Run: `cargo test --workspace`
Expected: PASS. If any pre-existing offline test fails here, the empty-history path is not byte-identical — **fix `history_block`, never the failing test.**

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/agents
git commit -m "agents: give the Planner and ContextFinder the session's prior turns"
```

---

### Task 8: The `cli` crate — `ClientTransport` and rendering

**Files:**
- Create: `crates/cli/Cargo.toml`, `crates/cli/src/lib.rs`, `crates/cli/src/transport.rs`, `crates/cli/src/render.rs`
- Modify: `Cargo.toml` (workspace members + `rustyline`)

**Interfaces:**
- Produces, used by Tasks 9 and 10:
  - `pub trait ClientTransport: Send { async fn send(&mut self, cmd: Command) -> anyhow::Result<()>; async fn recv(&mut self) -> Option<ServerMessage>; }` (`#[async_trait]`).
  - `pub struct FakeTransport` with `FakeTransport::new(scripted: Vec<ServerMessage>)` and `sent(&self) -> &[Command]`.
  - `pub fn render(kind: &EventKind, color: bool) -> Vec<String>`.

- [ ] **Step 1: Create the crate**

`crates/cli/Cargo.toml`:

```toml
[package]
name = "otto-cli"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-protocol = { path = "../protocol" }
otto-engine = { path = "../engine" }
otto-persistence = { path = "../persistence" }
anyhow = { workspace = true }
async-trait = { workspace = true }
tokio = { workspace = true }
rustyline = "14"
uuid = { workspace = true }
```

Add `"crates/cli"` to `members` in the workspace `Cargo.toml`. Match the workspace-dependency style already used by sibling crates (check `crates/engine/Cargo.toml` for the exact keys).

- [ ] **Step 2: Write the failing tests**

`crates/cli/src/render.rs` tests:

```rust
#[test]
fn renders_every_event_kind_without_debug_formatting() {
    let cases = vec![
        EventKind::AgentStarted { role: Role::Planner },
        EventKind::AgentFinished { role: Role::Planner },
        EventKind::FileEdit { path: PathBuf::from("src/a.rs"), bytes_written: 42 },
        EventKind::VerifyResult { ok: true, detail: "3 passed".to_string() },
        EventKind::Log { message: "planned 2 milestone(s)".to_string() },
        EventKind::TurnComplete { ok: true },
    ];
    for kind in cases {
        let lines = render(&kind, false);
        assert!(!lines.is_empty(), "every EventKind must render something: {kind:?}");
        for l in &lines {
            assert!(!l.contains("EventKind"), "must not fall back to Debug: {l}");
            assert!(!l.contains('\u{1b}'), "color=false must emit no ANSI escapes");
        }
    }
}

#[test]
fn color_true_emits_ansi_and_color_false_does_not() {
    let kind = EventKind::VerifyResult { ok: false, detail: "1 failed".to_string() };
    assert!(render(&kind, true).iter().any(|l| l.contains('\u{1b}')));
    assert!(render(&kind, false).iter().all(|l| !l.contains('\u{1b}')));
}

#[test]
fn server_diagnostics_render_verbatim() {
    // Log and VerifyResult.detail are server-originated; the CLI must not reword them.
    let lines = render(&EventKind::Log { message: "exact server text".to_string() }, false);
    assert!(lines.iter().any(|l| l.contains("exact server text")));
}
```

`crates/cli/src/transport.rs` tests:

```rust
#[tokio::test]
async fn fake_transport_records_sends_and_replays_scripted_messages() {
    let session = SessionId::new();
    let mut t = FakeTransport::new(vec![
        ServerMessage::Event {
            event: Event {
                seq: 0,
                session,
                kind: EventKind::TurnComplete { ok: true },
            },
        },
    ]);

    t.send(Command::SendPrompt { session, text: "hi".to_string() }).await.unwrap();
    assert_eq!(t.sent().len(), 1);
    assert!(matches!(t.recv().await, Some(ServerMessage::Event { .. })));
    assert!(t.recv().await.is_none(), "exhausted script ends the stream");
}
```

**Protocol shapes, verified — use exactly these:** `ServerMessage::Event { event: Event }` (struct-shaped, **not** a tuple variant), `ServerMessage::Error { message: String }`, and `ServerMessage::Ready { session: SessionId, capabilities: CapabilitiesManifest }`. **There is no `SessionCreated` variant** — `Ready` is the session-established frame.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p otto-cli`
Expected: FAIL — crate has no `render`/`FakeTransport`.

- [ ] **Step 4: Implement `transport.rs`**

```rust
use async_trait::async_trait;
use otto_protocol::{Command, ServerMessage};

/// The engine as the REPL sees it: a `Command` sink and a `ServerMessage` stream, nothing more.
///
/// The REPL is written against this trait alone — it never names `EngineService`,
/// `Orchestrator`, or `ToolRegistry`. That keeps the CLI a genuine protocol client rather than
/// a privileged one, and it is what makes the loop testable without a live engine or a TTY.
#[async_trait]
pub trait ClientTransport: Send {
    async fn send(&mut self, cmd: Command) -> anyhow::Result<()>;
    /// The next message, or `None` when the engine is finished/disconnected.
    async fn recv(&mut self) -> Option<ServerMessage>;
}

/// A scripted `ClientTransport` for testing the REPL loop with no engine. Always compiled —
/// the same precedent as `providers::ScriptedProvider`.
pub struct FakeTransport {
    sent: Vec<Command>,
    scripted: std::collections::VecDeque<ServerMessage>,
}

impl FakeTransport {
    pub fn new(scripted: Vec<ServerMessage>) -> Self {
        Self { sent: Vec::new(), scripted: scripted.into() }
    }
    pub fn sent(&self) -> &[Command] {
        &self.sent
    }
}

#[async_trait]
impl ClientTransport for FakeTransport {
    async fn send(&mut self, cmd: Command) -> anyhow::Result<()> {
        self.sent.push(cmd);
        Ok(())
    }
    async fn recv(&mut self) -> Option<ServerMessage> {
        self.scripted.pop_front()
    }
}
```

- [ ] **Step 5: Implement `render.rs`**

```rust
use otto_protocol::{EventKind, Role};

const DIM: &str = "\u{1b}[2m";
const GREEN: &str = "\u{1b}[32m";
const RED: &str = "\u{1b}[31m";
const RESET: &str = "\u{1b}[0m";

fn paint(text: &str, code: &str, color: bool) -> String {
    if color { format!("{code}{text}{RESET}") } else { text.to_string() }
}

fn role_name(role: &Role) -> String {
    match role {
        Role::Planner => "planner".to_string(),
        Role::ContextFinder => "context".to_string(),
        Role::Coder => "coder".to_string(),
        Role::Verifier => "verifier".to_string(),
        Role::Custom(n) => n.clone(),
    }
}

/// Render one event as terminal lines. Pure: no I/O, no global state, no locale — which is what
/// makes every branch unit-testable with no TTY.
///
/// `Log` and `VerifyResult.detail` are server-originated diagnostics and are reproduced
/// verbatim; the CLI does not reword or interpret them.
pub fn render(kind: &EventKind, color: bool) -> Vec<String> {
    match kind {
        EventKind::AgentStarted { role } => {
            vec![paint(&format!("• {}", role_name(role)), DIM, color)]
        }
        EventKind::AgentFinished { .. } => vec![],
        EventKind::FileEdit { path, bytes_written } => vec![format!(
            "  edited {} ({bytes_written} bytes)",
            path.display()
        )],
        EventKind::ApprovalRequest { path, .. } => vec![paint(
            &format!("  edit to {} needs approval — skipped", path.display()),
            DIM,
            color,
        )],
        EventKind::VerifyResult { ok, detail } => {
            let head = if *ok {
                paint("  verify passed", GREEN, color)
            } else {
                paint("  verify failed", RED, color)
            };
            let mut out = vec![head];
            if !detail.is_empty() {
                out.push(format!("  {detail}"));
            }
            out
        }
        EventKind::Log { message } => vec![paint(&format!("  {message}"), DIM, color)],
        EventKind::TokenCostMeter { input_tokens, output_tokens } => vec![paint(
            &format!("  {input_tokens} in / {output_tokens} out"),
            DIM,
            color,
        )],
        EventKind::TurnComplete { ok } => vec![if *ok {
            paint("done", GREEN, color)
        } else {
            paint("turn failed", RED, color)
        }],
    }
}

/// Whether to emit ANSI, honoring `NO_COLOR` and non-TTY stdout.
pub fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::IsTerminal::is_terminal(&std::io::stdout())
}
```

Add any `EventKind` variant the compiler reports as missing — the match must be exhaustive with no `_` arm, so a future variant is a compile error rather than a silently unrendered event.

- [ ] **Step 6: Wire `lib.rs`**

```rust
//! otto's interactive command-line client.
//!
//! The REPL speaks the `Command`/`ServerMessage` protocol through `ClientTransport` and never
//! reaches into the engine directly. English-only: `ui-dioxus`'s i18n boundary stops here.

pub mod embedded;
pub mod render;
pub mod repl;
pub mod transport;

pub use repl::repl;
pub use transport::{ClientTransport, FakeTransport};
```

Create `embedded.rs` and `repl.rs` as empty placeholder modules for now so this compiles; Tasks 9 and 10 fill them.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p otto-cli`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
git add crates/cli Cargo.toml Cargo.lock
git commit -m "cli: add the crate, the ClientTransport seam, and pure event rendering"
```

---

### Task 9: `EmbeddedTransport`

**Files:**
- Modify: `crates/cli/src/embedded.rs`

**Interfaces:**
- Consumes: `ClientTransport` (Task 8).
- Produces, used by Task 10: `EmbeddedTransport::new(root: PathBuf) -> anyhow::Result<Self>`, implementing `ClientTransport`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn embedded_transport_runs_a_turn_and_streams_events() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

    let mut t = EmbeddedTransport::new(dir.path().to_path_buf()).await.unwrap();
    t.send(Command::CreateSession).await.unwrap();
    let session = match t.recv().await {
        Some(ServerMessage::Ready { session, .. }) => session,
        other => panic!("expected Ready, got {other:?}"),
    };

    t.send(Command::SendPrompt { session, text: "add a hello function".to_string() })
        .await
        .unwrap();

    let mut saw_turn_complete = false;
    while let Some(msg) = t.recv().await {
        if let ServerMessage::Event { event } = msg {
            if matches!(event.kind, EventKind::TurnComplete { .. }) {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete, "a turn must stream through to TurnComplete");
}

#[tokio::test]
async fn embedded_transport_carries_history_between_turns() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = EmbeddedTransport::new(dir.path().to_path_buf()).await.unwrap();
    t.send(Command::CreateSession).await.unwrap();
    let session = match t.recv().await {
        Some(ServerMessage::Ready { session, .. }) => session,
        other => panic!("expected Ready, got {other:?}"),
    };

    for text in ["first goal", "second goal"] {
        t.send(Command::SendPrompt { session, text: text.to_string() }).await.unwrap();
        while let Some(ServerMessage::Event { event }) = t.recv().await {
            if matches!(event.kind, EventKind::TurnComplete { .. }) {
                break;
            }
        }
    }

    // The second turn must have seen the first: assert on the store, not on model output,
    // so this stays deterministic offline.
    assert_eq!(t.turn_count(session).await.unwrap(), 2);
}
```

Add `#[cfg(test)] pub(crate) async fn turn_count(&self, session: SessionId) -> anyhow::Result<usize>` on `EmbeddedTransport`, delegating to `store.turns(&UserId::local(), session)`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-cli embedded_transport`
Expected: FAIL — `EmbeddedTransport` does not exist.

- [ ] **Step 3: Implement `EmbeddedTransport`**

Build the dependencies exactly as `cmd_run` does in `crates/engine/src/main.rs` — read that function and mirror it, so extensions, permissions, hooks, skills, and plugin MCP servers compose identically. The MCP connection guards must be **stored on the struct**, not dropped, or the MCP child processes die immediately.

```rust
pub struct EmbeddedTransport {
    service: std::sync::Arc<otto_engine::EngineService>,
    store: std::sync::Arc<dyn otto_persistence::SessionStore>,
    owner: otto_protocol::UserId,
    rx: tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    tx: tokio::sync::mpsc::UnboundedSender<ServerMessage>,
    /// Kept alive for the process lifetime: dropping these kills the MCP server children.
    _mcp: Vec<otto_engine::McpConnection>,
    current: Option<tokio::task::JoinHandle<()>>,
}
```

`send` dispatches:
- `Command::CreateSession` → `service.create_session(&owner, "", &config)`, then push `ServerMessage::Ready { session, capabilities: CapabilitiesManifest::default() }` into `tx`. (`Ready` is the protocol's session-established frame; there is no `SessionCreated`.)
- `Command::SendPrompt { session, text }` → spawn a task running `service.run_prompt_with_controls(...)` with a `TurnControls` whose `approver` denies (`DenyApprover`) and whose sink forwards each `Event` into `tx` as `ServerMessage::Event`. Store the `JoinHandle` in `current`. On completion, if the turn returned `Err`, push `ServerMessage::Error` with the error text.
- `Command::Abort { session }` → `service.abort(&owner, session)`, and abort `current`.
- `Command::ApproveDiff { .. }` → no-op in this slice (the approver denies); do not panic on it.
- Any other command → push `ServerMessage::Error` saying it is unsupported by the embedded transport. Never panic on an unexpected command.

`recv` is `self.rx.recv().await`.

The `EventSink` implementation is a small struct wrapping the `UnboundedSender`; model it on `CollectingSink` in `crates/engine/src/service.rs`.

Export whatever `engine` types this needs (`EngineService`, `McpConnection`, `build_composed_tools`) from `crates/engine/src/lib.rs` if they are not already `pub`. If `build_composed_tools` lives in `main.rs` and is therefore unreachable, **move it to `crates/engine/src/lib.rs`** and have `main.rs` call it there — do not duplicate it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p otto-cli embedded_transport`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/cli crates/engine
git commit -m "cli: add EmbeddedTransport, an in-process engine behind the protocol seam"
```

---

### Task 10: The REPL loop and the binary entry

**Files:**
- Modify: `crates/cli/src/repl.rs`, `crates/engine/src/main.rs`, `crates/engine/Cargo.toml`

**Interfaces:**
- Consumes: `ClientTransport`, `FakeTransport`, `render` (Task 8); `EmbeddedTransport` (Task 9).
- Produces: `pub async fn repl(root: PathBuf) -> anyhow::Result<()>`; `pub(crate) async fn run_loop<T: ClientTransport>(transport: &mut T, input: impl Iterator<Item = String>, out: &mut impl Write) -> anyhow::Result<()>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn loop_sends_each_input_line_as_a_prompt() {
    let session = SessionId::new();
    let mut t = FakeTransport::new(vec![
        ServerMessage::Ready { session, capabilities: Default::default() },
        ServerMessage::Event { event: Event { seq: 0, session, kind: EventKind::TurnComplete { ok: true } } },
        ServerMessage::Event { event: Event { seq: 1, session, kind: EventKind::TurnComplete { ok: true } } },
    ]);
    let mut out = Vec::new();

    run_loop(&mut t, vec!["first".to_string(), "second".to_string()].into_iter(), &mut out)
        .await
        .unwrap();

    let prompts: Vec<String> = t
        .sent()
        .iter()
        .filter_map(|c| match c {
            Command::SendPrompt { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, vec!["first".to_string(), "second".to_string()]);
    assert!(t.sent().iter().any(|c| matches!(c, Command::CreateSession)));
}

#[tokio::test]
async fn loop_renders_events_not_debug_output() {
    let session = SessionId::new();
    let mut t = FakeTransport::new(vec![
        ServerMessage::Ready { session, capabilities: Default::default() },
        ServerMessage::Event {
            event: Event {
                seq: 0,
                session,
                kind: EventKind::FileEdit { path: PathBuf::from("a.rs"), bytes_written: 7 },
            },
        },
        ServerMessage::Event { event: Event { seq: 1, session, kind: EventKind::TurnComplete { ok: true } } },
    ]);
    let mut out = Vec::new();
    run_loop(&mut t, vec!["go".to_string()].into_iter(), &mut out).await.unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("edited a.rs"));
    assert!(!text.contains("EventKind"), "must never fall back to Debug rendering");
}

#[tokio::test]
async fn loop_reports_a_server_error_and_keeps_going() {
    let session = SessionId::new();
    let mut t = FakeTransport::new(vec![
        ServerMessage::Ready { session, capabilities: Default::default() },
        ServerMessage::Error { message: "boom".to_string() },
        ServerMessage::Event { event: Event { seq: 0, session, kind: EventKind::TurnComplete { ok: true } } },
    ]);
    let mut out = Vec::new();
    // A failing turn must not end the session.
    run_loop(&mut t, vec!["a".to_string(), "b".to_string()].into_iter(), &mut out).await.unwrap();
    assert!(String::from_utf8(out).unwrap().contains("boom"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-cli loop_`
Expected: FAIL — `run_loop` does not exist.

- [ ] **Step 3: Implement `run_loop`**

Input-source-agnostic so it is testable without a terminal:

```rust
/// Drive one prompt per input item, rendering events until each turn completes.
///
/// Generic over the input iterator and the output sink so the loop can be tested with scripted
/// input and an in-memory buffer — no TTY, no PTY, no engine.
pub(crate) async fn run_loop<T: ClientTransport>(
    transport: &mut T,
    input: impl Iterator<Item = String>,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let color = crate::render::color_enabled();

    transport.send(Command::CreateSession).await?;
    let session = loop {
        match transport.recv().await {
            Some(ServerMessage::Ready { session, .. }) => break session,
            Some(ServerMessage::Error { message }) => anyhow::bail!("{message}"),
            Some(_) => continue,
            None => anyhow::bail!("engine closed before creating a session"),
        }
    };

    for line in input {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        transport.send(Command::SendPrompt { session, text: line }).await?;

        // Drain until this turn finishes.
        loop {
            match transport.recv().await {
                Some(ServerMessage::Event { event }) => {
                    for l in crate::render::render(&event.kind, color) {
                        writeln!(out, "{l}")?;
                    }
                    if matches!(event.kind, EventKind::TurnComplete { .. }) {
                        break;
                    }
                }
                // Server-originated text, reproduced verbatim. A failed turn returns to the
                // prompt; it never ends the session.
                Some(ServerMessage::Error { message }) => {
                    writeln!(out, "error: {message}")?;
                    break;
                }
                Some(_) => continue,
                None => return Ok(()),
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Implement `repl`**

```rust
/// Start the interactive REPL against an in-process engine rooted at `root`.
pub async fn repl(root: std::path::PathBuf) -> anyhow::Result<()> {
    // Same fail-fast posture as `otto run`: a bad *_BASE_URL must not silently degrade to the
    // canned offline provider inside an interactive session.
    otto_engine::preflight_base_urls()?;

    let mut transport = crate::embedded::EmbeddedTransport::new(root).await?;
    let mut stdout = std::io::stdout();

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Piped input: run each line as a turn, then exit. Keeps the REPL scriptable.
        let lines: Vec<String> = std::io::stdin().lines().collect::<Result<_, _>>()?;
        return run_loop(&mut transport, lines.into_iter(), &mut stdout).await;
    }

    let mut rl = rustyline::DefaultEditor::new()?;
    loop {
        match rl.readline("otto> ") {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                run_loop(&mut transport, std::iter::once(line), &mut stdout).await?;
            }
            // Ctrl-C at the prompt, or Ctrl-D: exit cleanly.
            Err(rustyline::error::ReadlineError::Interrupted)
            | Err(rustyline::error::ReadlineError::Eof) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}
```

Note: `run_loop` sends `CreateSession` each call, which would mint a session per line in the interactive path. **Split it**: extract `create_session(transport) -> SessionId` and `run_one(transport, session, text, out)`, have `run_loop` call `create_session` once then `run_one` per line, and have `repl` call `create_session` once before the readline loop and `run_one` per line. Update the Step 1 tests only if their assertions on `CreateSession` still hold — they should, since `run_loop` keeps its behavior.

For Ctrl-C **during** a turn: wrap the `run_one` await in `tokio::select!` against `tokio::signal::ctrl_c()`; on signal, `transport.send(Command::Abort { session })` and return to the prompt.

- [ ] **Step 5: Wire the binary**

In `crates/engine/Cargo.toml`, add `otto-cli = { path = "../cli" }`.

In `crates/engine/src/main.rs`, change the fallthrough arm so a **bare** `otto` starts the REPL while an unknown subcommand still errors:

```rust
"" => {
    let (root, _) = parse_root(&rest);
    otto_cli::repl(root).await
}
```

Place this arm before the existing `_ =>` error arm. Add to `USAGE`:

```
  otto                                         interactive session in the current directory
```

- [ ] **Step 6: Run the full suite and try it**

```bash
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets
echo "add a hello function" | cargo run -p otto-engine
```

Expected: tests PASS; the piped run prints rendered lines (`• planner`, `edited …`, `done`), not `[  0] AgentStarted { … }`.

- [ ] **Step 7: Commit**

```bash
git add crates/cli crates/engine Cargo.lock
git commit -m "cli: add the interactive REPL loop and start it for a bare otto invocation"
```

---

### Task 11: Documentation

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `docs/ARCHITECTURE.md`

- [ ] **Step 1: `CLAUDE.md`**

Add a `cli` row to the crate table:

> | `cli` | The interactive terminal client. A REPL written against the `Command`/`Event` protocol through a `ClientTransport` seam — it never touches `EngineService` directly — with `EmbeddedTransport` (in-process engine, no socket, no auth) as the only impl in this slice. English-only: `ui-dioxus`'s i18n boundary stops at this crate. |

In the orchestrator-spine section, record that `run_turn` now takes a bounded `SessionHistory` (last `HISTORY_TURNS` turns, derived from persisted `TurnRecord`s alone via `TurnOutcome`, no event-log join), reaching the Planner and ContextFinder through `AgentCtx::history()`, and that an **empty history produces byte-identical prompts** to the pre-history spine — the invariant the offline suite rests on.

Add `otto` (no subcommand) to the Commands block.

- [ ] **Step 2: `README.md`**

Add to Quick start, above the `otto run` example:

````markdown
Start an interactive session in a repo:

```bash
cd /path/to/repo && otto
```

Each prompt runs a full turn (Plan → find context → code → verify) and the session remembers
prior turns. Ctrl-C cancels a running turn; Ctrl-D exits.
````

- [ ] **Step 3: `docs/ARCHITECTURE.md`**

Line 44 lists a `cli` crate as intended-but-absent. Update that entry to describe what now exists, and mark the interactive-approval note near line 180 as still pending (diff review is the next slice).

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md docs/ARCHITECTURE.md
git commit -m "docs: record the cli crate and conversation history in the spine"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| §1 crate boundary | 8 |
| §1.1 `ClientTransport` | 8 |
| §1.2 `EmbeddedTransport` | 9 |
| §2.1 `SessionHistory` shape | 4 |
| §2.1.1 `SessionStore::turns` | 3 |
| §2.2 `TurnOutcome` summary + `record_turn` | 1, 2 |
| §2.3 `AgentCtx` threading, Planner + ContextFinder | 6, 7 |
| §2.4.1 byte-identical empty history | 5, 7 |
| §2.4.2 bounded history | 4 |
| §3 REPL, rustyline, interrupt, session lifecycle | 10 |
| §3.1 pure rendering | 8 |
| §3.2 English-only | 8 (lib.rs doc), 11 |
| §4 edge cases | 9 (unsupported command, turn error), 10 (non-TTY, error-keeps-going, preflight) |
| §5 semver | 1, 3, 8 |
| §6 testing matrix | every task |

**Known gaps, deliberate:** §4's "store open fails" and "not a git repo" rows are covered by `EmbeddedTransport::new` returning `Result` and by `LocalWorkspace` accepting any directory; neither gets a dedicated test, since both are existing engine behavior this slice does not change.

**Type consistency:** `SessionHistory`/`TurnSummary`/`VerifySummary`/`HISTORY_TURNS` are defined in Task 1 (`VerifySummary`) and Task 4 (the rest) and used with identical names in Tasks 6, 7, 9. `history_from_records` (Task 4) is consumed only in Task 6. `render(&EventKind, bool)` is defined in Task 8 and called in Task 10. `ClientTransport::{send,recv}` is defined in Task 8 and implemented in Task 9.

**Verification note for the executor:** the `ServerMessage` variants used throughout were read from `crates/protocol/src/lib.rs:278` and are correct as written — `Event { event }`, `Error { message }`, `Ready { session, capabilities }`. What was **not** verified line-by-line: the sqlite column names and `UserId` accessor in Task 3's query, the exact fixture values in Task 1's orchestrator test, and the test-helper names in Tasks 2 and 6 (`test_service`, `StubWorkspace`, `test_orchestrator_deps`). Each of those sites says so and tells you to read the real definition first. Treat the surrounding structure as authoritative and those identifiers as needing confirmation.
