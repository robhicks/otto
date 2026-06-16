# otto Design — Session Persistence (distribution-axis foundation)

**Status:** approved design (spec). Implementation plans to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-16

## Goal

Give the engine a durable session store: persist each session, its turn history, and its
event log to sqlite, with seq-ordered event replay. This is the first increment of the
**distribution axis** — the architecture's thesis that "the frontend never branches on local
vs remote." A persisted, replayable event log is the prerequisite for a future `serve` mode
(reconnect via `Last-Event-ID`) and for promote-to-remote (snapshot/restore a session).

Today `run_goal` runs a single turn, collects events in an in-memory `Vec`, and returns them;
the protocol's `Command::CreateSession`/`SendPrompt`/`Abort` exist but nothing consumes them
as a lifecycle. This design adds the store, a session-aware engine path that consumes those
commands, and a snapshot derived from the persisted rows.

## Scope reality (what exists vs. what's deferred)

The engine has little session state to snapshot today: the orchestrator runs one turn and is
stateless between turns; there is no persisted context and no `Workspace.snapshot()` seam
(that arrives with `RemoteWorkspace`, a v2 item). This design therefore captures a snapshot
over **what actually exists** — session metadata, the event log, turn history, workspace root
reference, and config — and keeps `SessionState` forward-compatible. The workspace
patch-bundle (uncommitted diffs) is **explicitly deferred** until `RemoteWorkspace` lands.

## Decisions (locked during brainstorming)

1. **DB driver = `sqlx` with runtime queries** (the `query()`/`query_as()` API, *not* the
   `query!` compile-time macros). One driver covers sqlite now and the architecture's
   "postgres (remote, optional)" later. Runtime queries need no `DATABASE_URL` at build, so
   builds and the test suite stay fully offline and deterministic.
2. **Seam placement = trait + impl both in the `persistence` crate.** `engine` depends on
   `persistence` directly and holds a `Box<dyn SessionStore>`, so an in-memory or postgres
   impl can slot in later without touching the engine's call sites.
3. **Snapshot is derived from the tables, not stored separately** — one source of truth.
   `snapshot()` reads the rows and serializes; `restore()` writes them back.
4. **Scope = store + lifecycle + snapshot**, delivered as three plans (below).

## Decomposition (the arc → 3 plans)

- **Plan A — `persistence` foundations:** the `SessionStore` trait + `SqliteStore` (sqlx),
  schema/migrations, CRUD + `replay_since`. Tested standalone against a tempfile DB.
- **Plan B — lifecycle + engine wiring:** a session-aware engine path consuming
  `CreateSession`/`SendPrompt`/`Abort`; events persisted *as they are emitted*; multi-turn
  history; `run_goal` refactored to run through the store while preserving its current return.
- **Plan C — `SessionState` snapshot:** define `SessionState`, `snapshot()`/`restore()`,
  capturing session metadata + event log + turn history + config + workspace root. Workspace
  patch-bundle deferred.

## Architecture

### New crate: `persistence`

Owns both the `SessionStore` trait and its sqlite implementation. Depends on `protocol`
(for `SessionId`, `Event`, `EventKind`) and the orchestrator's `TurnOutcome` type. It does
**not** depend on `engine-core`'s orchestrator internals; it is a leaf the `engine` layer
wires in. (`engine-core` remains free of any persistence dependency — the orchestrator never
touches the store; the engine layer does.)

### Data model (one sqlite file, three tables)

```
sessions(id TEXT PK, goal TEXT, status TEXT, created_at, updated_at, config TEXT/JSON)
events(session_id TEXT FK, seq INTEGER, kind TEXT/JSON, PK(session_id, seq))
turns(session_id TEXT FK, turn_index INTEGER, goal TEXT, outcome TEXT/JSON, started_at,
      PK(session_id, turn_index))
```

- `status` ∈ `active | done | aborted | failed`.
- `config` is the router/model selection captured at session start (JSON `Value`).
- `events` is both the turn history and the replay log; `(session_id, seq)` is the PK so seq
  monotonicity is enforced by the schema.
- `turns` lets one session span many prompts; `outcome` is the serialized `TurnOutcome`.

`SessionState` (snapshot) is **derived** from these rows, not a fourth table.

### The `SessionStore` trait

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, goal: &str, config: &Value) -> Result<SessionId>;
    async fn append_event(&self, session: SessionId, event: &Event) -> Result<()>;
    async fn record_turn(&self, session: SessionId, turn: &TurnRecord) -> Result<()>;
    async fn set_status(&self, session: SessionId, status: SessionStatus) -> Result<()>;
    async fn replay_since(&self, session: SessionId, after_seq: u64) -> Result<Vec<Event>>;
    async fn snapshot(&self, session: SessionId) -> Result<SessionState>;
    async fn restore(&self, state: &SessionState) -> Result<SessionId>;
}
```

`SqliteStore` is the impl. `replay_since(session, after_seq)` returns events with
`seq > after_seq` in seq order — the function a future `serve` mode exposes for
`Last-Event-ID` reconnection.

### Engine wiring

`run_goal` becomes a thin wrapper over a session-aware path:

1. `create_session(goal, config)` (or load an existing session).
2. Run the turn with a sink that, as today, maps each bare `EventKind` → `Event` (assigning
   the monotonic per-session `seq`), **and** additionally calls `append_event` so the log is
   durable as it streams.
3. `record_turn` + `set_status` at turn end.

The in-memory `Vec<Event>` return value is preserved for existing callers and tests.
`Command::CreateSession`/`SendPrompt`/`Abort` map onto `create_session` / run-a-turn /
`set_status(aborted)` respectively (Plan B).

### Data flow

```
Command::CreateSession ─► store.create_session(goal, config) ─► SessionId
Command::SendPrompt    ─► run_turn(session, goal, sink)
                              sink: EventKind ─► Event{seq} ─► (a) collected Vec
                                                              (b) store.append_event
                           ─► store.record_turn + store.set_status
Command::Abort         ─► store.set_status(session, aborted)

(future serve) reconnect@last_seq ─► store.replay_since(session, last_seq) ─► gap events
promote (Plan C)                  ─► store.snapshot(session) ─► SessionState ─► restore()
```

## Error handling & determinism

- Store errors surface as `anyhow::Error` to the engine. A persistence failure **fails the
  turn** rather than silently dropping events — fail-closed, consistent with the permission
  gate's ethos. (Silent event loss would corrupt replay, the whole point of the store.)
- `sqlx` runtime queries (no `query!` macros) + `sqlx::migrate!` for schema means no
  `DATABASE_URL` and no live DB at build time; the offline build/test invariant holds.
- Tests use a tempfile sqlite path (and/or `sqlite::memory:`) — no setup, no network,
  reproducible.

## Testing

- **`persistence` unit tests** (tempfile DB): create → append → `replay_since` round-trips;
  `replay_since` gap correctness (returns only `seq > after_seq`, in order); snapshot/restore
  fidelity (restored session reproduces the same event sequence); status transitions; seq
  PK rejects duplicate `(session_id, seq)`.
- **Engine test:** a turn persists its events and `replay_since(session, 0)` returns the same
  sequence the in-memory path produced — proving the durable log and the returned `Vec` agree.

## Out of scope (named, not silently dropped)

- `serve` mode / WSS transport / `Last-Event-ID` wire handling — next arc, builds on
  `replay_since`.
- `RemoteWorkspace` and the workspace patch-bundle (uncommitted-diff capture) — v2.
- Postgres backend — the driver choice keeps the door open; no postgres impl ships here.
