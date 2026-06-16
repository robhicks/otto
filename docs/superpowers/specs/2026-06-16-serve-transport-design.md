# otto Design — Serve Transport (WebSocket)

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-16

## Goal

Expose the engine over the network: an `otto serve` WebSocket server that speaks the existing
`Command`/`Event` protocol, streams a turn's events live as they are emitted, persists them
through the `SessionStore`, and supports `Last-Event-ID`-style reconnect (replay the gap via
`replay_since`). This is the first transport on the distribution axis — the thing that makes
the persisted, replayable event log externally observable. It builds directly on the
persistence arc (store → lifecycle → snapshot, PRs #18/#19/#21).

## Decisions (locked during brainstorming)

1. **Transport = WebSocket via `axum`.** Bidirectional duplex: `Command` frames up, event
   frames down. Matches the architecture's WSS remote topology — extends to remote later by
   adding TLS termination. `axum` is the tokio-native HTTP/WS framework.
2. **Bearer-token auth now, fail-closed.** `serve` requires `OTTO_TOKEN`; if it is unset the
   server refuses to start. Each WS upgrade must present `Authorization: Bearer <OTTO_TOKEN>`
   or it is rejected (401). Binds `127.0.0.1` (loopback); TLS is deferred to the actual remote
   deployment (the token is the v1 guard on loopback).
3. **One end-to-end plan** delivering a working server (not split into service-first / server-later).
4. **`EngineService` replaces Plan B's `Session`.** A single streaming run path, store-cursor
   based, used by both the CLI and serve — no second batch path to keep correct.

## Architecture

Two layers, shipped together, both in the `engine` crate (matching "engine: binary + library;
embedded and serve modes"):

- **`EngineService`** (`crates/engine/src/service.rs`) — transport-agnostic core. Owns
  `Arc<dyn SessionStore>` + the shared `Arc<AgentRegistry>` / `Arc<dyn Router>` /
  `Arc<dyn Workspace>` / `Arc<ToolRegistry>`, plus a `tokio::Mutex` that serializes turns.
  Exposes `create_session`, `run_prompt`, `abort`. Knows nothing about WebSockets.
- **Serve layer** (`crates/engine/src/serve.rs`) — the `axum` WS server. Maps WS frames to
  `Command`/event frames over an `EngineService`, handling auth, session resolution, reconnect,
  and live streaming. Exposed via an `otto serve` subcommand in `main.rs`.

### The streaming emitter (the piece deferred in Plan B)

The orchestrator's `Emitter` is synchronous and a turn is one long async call. To stream
events live while the turn runs, `EngineService::run_prompt` bridges sync→async with a channel:

1. Compute `start_seq = store.next_seq(session)` and `turn_index = store.next_turn(session)`.
2. Spawn the orchestrator turn as a task. Its sync sink assigns `seq` from an `AtomicU64`
   (seeded at `start_seq`) and pushes each `Event` into an `mpsc::UnboundedSender`.
3. The caller's task drains the receiver: for each event, **`append_event` (fail-closed) then
   `sink.emit(&event).await`**.
4. When the turn task completes the sender drops, the drain loop ends, and we `record_turn`
   + `set_status` (`Done`/`Failed`; a turn that *errors* sets `Failed`).

```rust
#[async_trait]
pub trait EventSink: Send {
    /// Called for each event, in seq order, after it is durably persisted.
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()>;
}
```

- The **CLI** passes a collecting sink (gathers events into a `Vec`), so `run_goal` keeps its
  `(Vec<Event>, TurnOutcome)` return and external behavior.
- **Serve** passes a sink that writes each event to the WebSocket.

### New `SessionStore` methods (cursors)

```rust
async fn next_seq(&self, session: SessionId) -> anyhow::Result<u64>;    // COALESCE(MAX(seq)+1, 0)
async fn next_turn(&self, session: SessionId) -> anyhow::Result<u32>;   // COALESCE(MAX(turn_index)+1, 0)
```

Sourcing the cursors from the store (not in-memory counters) makes seq continuity survive
reconnect and restart. Unknown session → `0` for both (a fresh session starts at seq 0).

### `Session` → `EngineService` consolidation

Plan B's `Session` (create/run_prompt/abort, batch persist, in-memory `next_seq`) is removed;
`EngineService` provides the same surface with streaming persistence and store cursors.
`run_goal` is refactored to build an `EngineService` and call `run_prompt` with a collecting
sink. The Plan B session tests port to `EngineService` unchanged in intent: create→`Active`,
streamed events equal `replay_since(None)`, multi-turn seq continuity, abort→`Aborted`,
orchestrator error→`Failed`.

### The axum WS server

- Endpoint: `GET /ws` (WebSocket upgrade).
- **Inbound** frames: JSON `protocol::Command` (`SendPrompt { session, text }`, `Abort { session }`).
- **Outbound** frames: a thin serve-level framing enum reusing the core types (transport
  detail, not a protocol change):

```rust
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Ready { session: SessionId },   // sent once on connect: the resolved session id
    Event { event: Event },         // an engine event
}
```

- **Connect flow:** authenticate → resolve the session (mint a new one via `create_session`,
  or use `?session=<id>`) → send `Ready { session }` → if `?last_seq=N` is present, replay the
  gap with `replay_since(Some(N))` as `Event` frames → then loop reading `Command` frames and
  streaming the turn's events live.
- **Reconnect** is just connect-with-`?session=<id>&last_seq=<n>`: `Ready`, then the replayed
  gap, then live events — the `Last-Event-ID` story end to end.
- **Concurrency:** one turn at a time per server, enforced by the `EngineService` turn mutex.
  Multiple connections may connect, but a `SendPrompt` runs to completion before the next.
  (Concurrent turns on one workspace are unsafe and out of scope for v1.)

### Server bootstrap

`otto serve [--port <p>]` reads `OTTO_TOKEN` (required; absent → print an error and exit
non-zero), opens the `SqliteStore` (`OTTO_DB` or default), builds the router/workspace/tools
against `--root` (as the `run` subcommand does), constructs the `EngineService`, and binds
`127.0.0.1:<port>` (`--port` / `OTTO_PORT`, default `7878`).

## Data flow

```
client ──WS GET /ws (Authorization: Bearer OTTO_TOKEN)[?session&last_seq]──► serve
  serve: auth ok → resolve session → send Ready{session}
         → if last_seq: replay_since(Some(n)) → Event frames (the gap)
  client → {Command::SendPrompt{session, text}}
  serve  → EngineService::run_prompt(session, text, ws_sink)
             spawn turn → sink assigns seq → mpsc
             drain: append_event (persist) → ws_sink.emit → {Event{seq,..}} frame
             record_turn + set_status
  client → {Command::Abort{session}} → set_status(Aborted)
```

## Error handling & determinism

- Persistence is fail-closed: an `append_event`/`record_turn`/`set_status` error fails the
  turn. An orchestrator-turn error sets status `Failed` (carried from Plan B).
- Auth failures reject the upgrade with `401` before any session work.
- A malformed inbound frame (not a valid `Command`) is reported to the client as an error
  frame / close, not a panic.
- The offline default (`LocalProvider`) is unchanged. The integration test runs the server on
  `127.0.0.1:0` (ephemeral port, pure loopback — no external network), keeping the suite
  deterministic and key-free.

## Testing

- **Unit (`EngineService`):** against a collecting `EventSink` + a tempfile store — create→
  `Active`; streamed events equal `replay_since(session, None)`; multi-turn seq continuity;
  `abort`→`Aborted`; orchestrator error (empty registry)→`Failed`. (Ports Plan B's tests.)
- **Integration (loopback WS):** start the server on `127.0.0.1:0`; connect with the token;
  send `SendPrompt`; assert a `Ready` frame followed by streamed `Event` frames ending in
  `TurnComplete { ok: true }`; then reconnect with `?session&last_seq` and assert the replayed
  gap matches. Plus an **auth test**: a missing/wrong token is rejected (no `Ready`, connection
  refused).

## Out of scope (named, not silently dropped)

- **TLS / WSS** — deferred to the remote deployment (`RemoteTarget`); loopback + token for v1.
- **Concurrent turns / multi-session fan-out** — one turn at a time; no live broadcast to a
  second simultaneous observer (reconnect-after-the-fact via `replay_since` is supported).
- **`RemoteWorkspace`, promote-to-remote, `RemoteTarget`** — separate v2 arc; this server runs
  against the local workspace it was started with.
- **Browser-native auth** (query-param token / subprotocol) — v1 uses the `Authorization`
  header (programmatic client); a query-param path can be added when a browser client needs it.
