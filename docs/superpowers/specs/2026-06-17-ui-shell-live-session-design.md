# otto UI — Sub-project A: App Shell + Live Session

**Date:** 2026-06-17
**Status:** Draft for review
**Roadmap:** `docs/superpowers/specs/2026-06-17-ui-roadmap.md` (sub-project A)

## Summary

Stand up the otto frontend as a browser-first **Leptos CSR** app that connects to a running
`otto serve` over WebSocket, runs a turn, and renders the live event stream. This is the
reusable **app shell** that every later UI sub-project (B–F) extends. It is deliberately the
thinnest end-to-end vertical slice: connect → prompt → watch events → abort → reconnect.

## Goals

- A `ui/` project that builds to WASM with `trunk` and runs in a browser tab.
- Connect to a user-supplied `ws://`/`wss://` engine URL with a bearer token.
- Receive `Ready { session }`, send `SendPrompt`, render the streamed `Event` log live.
- `Abort` an in-flight turn.
- Reconnect to an existing session and replay the gap via `last_seq` (`Last-Event-ID`).
- Establish the component/layout/state-management patterns the rest of the UI inherits.

## Non-Goals (this sub-project)

- No file tree, file view, or editor (sub-project C).
- No diff rendering or approval (sub-project D).
- No capabilities/status-strip negotiation beyond a raw connection-state line (sub-project B).
- No Tauri wrapper — browser-only. The Tauri shell reuses this WASM bundle later.
- No token/cost meter, pause/resume, or promote (sub-projects E/F).
- No persistence of UI state beyond the current tab (localStorage is a later nicety).

## Engine-side changes (small, additive)

Browser-first forces two minimal changes. Both are additive — the existing header-auth path
and the current `serve.rs` tests stay green.

### 1. Move the WS framing enum into `protocol`

`ServerMessage` (`Ready`/`Event`/`Error`, `#[serde(tag = "type", rename_all = "snake_case")]`)
is currently **private** in `crates/engine/src/serve.rs`. The UI must deserialize it but may
only depend on `protocol`. **Move the enum into `protocol`** (e.g. `protocol::ServerMessage`)
and have `serve.rs` import it instead of defining it. No wire-format change — the JSON tag and
field shapes are identical, so existing clients and tests are unaffected.

The inbound direction needs nothing: the client already sends raw `protocol::Command` JSON
(`serve.rs` does `serde_json::from_str::<Command>`), and `Command` is already in `protocol`.

### 2. Accept the bearer token via `?token=` query param

A browser's native `WebSocket` constructor cannot set an `Authorization` header on the
handshake. Extend `serve.rs` auth so the `/ws` upgrade accepts the token from **either** the
`Authorization: Bearer <token>` header **or** a `?token=<token>` query param, with the header
taking precedence. The `POST /workspace` RPC is unchanged (header-only; not browser-driven in A).

- `ConnectParams` gains an optional `token` field.
- `ws_handler` authorizes if the header matches **or** `params.token` matches.
- **Security note (documented, not silently accepted):** tokens in URLs can leak into server
  logs and browser history. This is acceptable for the loopback/dev posture of A. The header
  path remains the recommended one for non-browser clients, and a later sub-project may switch
  the browser to the `Sec-WebSocket-Protocol` subprotocol carrier or route through Tauri's
  Rust-side WS client (which can set headers). Capture this tradeoff in the plan.

## Frontend architecture

### Project layout

```
ui/
├── Cargo.toml          # standalone; NOT a workspace member. Depends on ../crates/protocol.
├── Trunk.toml          # trunk build config
├── index.html          # trunk entry; mounts the Leptos app
├── src/
│   ├── main.rs         # mount App
│   ├── app.rs          # App root + connection state
│   ├── ws.rs           # WebSocket client wrapper (web_sys::WebSocket)
│   └── components/
│       ├── connection_form.rs   # URL + token inputs, connect/disconnect
│       ├── prompt_bar.rs        # prompt input, Send, Abort
│       ├── event_log.rs         # scrolling list of received events
│       └── status_line.rs       # connection state, session id, last seq
└── style.css           # minimal terminal-like styling
```

`ui/` is excluded from the workspace so `cargo build --workspace` and the offline test suite
(the determinism invariant) are untouched; it builds only via `trunk` / `cargo build` from
inside `ui/` with a wasm target.

### Connection state machine

A single `ConnectionState` signal drives the UI:

```
Disconnected ──connect()──► Connecting ──Ready{session}──► Connected { session, last_seq }
     ▲                          │                                  │
     └──────────error / close───┴──────────────────────────────────┘
```

- **Disconnected:** connection form enabled; prompt bar disabled.
- **Connecting:** socket opening; form shows a spinner/“connecting…”.
- **Connected:** store `session` and a running `last_seq` (highest `Event.seq` seen). Prompt
  bar enabled. Each inbound `Event` appends to the log and advances `last_seq`.
- On socket error/close: drop to `Disconnected`, but **retain the last `session` and
  `last_seq`** so a subsequent connect can reconnect-and-replay.

### WebSocket client (`ws.rs`)

Wraps `web_sys::WebSocket`. Responsibilities:

- Build the URL: `<base>/ws?token=<token>[&session=<uuid>][&last_seq=<n>]`. Include `session`
  and `last_seq` only when reconnecting.
- `onmessage`: parse the text frame as `protocol::ServerMessage`; route `Ready`/`Event`/`Error`
  into Leptos signals via a callback.
- `onclose`/`onerror`: signal disconnect.
- `send(Command)`: serialize a `protocol::Command` to JSON and send as a text frame.
- Expose `send_prompt(text)` and `abort()` helpers that build the right `Command` (the
  `session` field is filled from the connection state; the server ignores it but the type
  requires it).

### Components

- **`ConnectionForm`** — inputs for engine URL (default `ws://127.0.0.1:<port>`) and token;
  Connect button (Disconnect when connected). Validates non-empty URL/token before connecting.
- **`PromptBar`** — multiline-ish text input; Send emits `SendPrompt`; Abort emits `Abort`.
  Both disabled unless `Connected`. Send clears the input.
- **`EventLog`** — renders each received `Event` as a row: a label/icon per `EventKind`
  (`AgentStarted`/`AgentFinished` with `role`, `FileEdit` with path + bytes, `VerifyResult`
  ok/detail, `Log` message, `TurnComplete` ok). Newest at the bottom; auto-scroll to bottom on
  append. `Error` frames render as a distinct error row.
- **`StatusLine`** — connection state, session id (short form), and `last_seq`. This is the
  seam sub-project B replaces/extends with the capabilities status strip.

### Styling

Minimal, terminal-like, monochrome-leaning CSS in `style.css` — consistent with the design
spec's “minimalist, terminal-like, lightning-fast” intent. No component framework. A single
column: status line on top, event log filling the middle (scrolls), prompt bar pinned at the
bottom, connection form shown when disconnected.

```
┌────────────────────────────────────────────┐
│ status: connected · 3f9a… · seq 12          │  ← StatusLine
├────────────────────────────────────────────┤
│ ▸ Planner started                           │
│ ▸ Planner finished                          │  ← EventLog (scrolls,
│ ✎ FileEdit src/main.rs (+42 bytes)          │     auto-scroll bottom)
│ ✓ Verify ok                                 │
│ ● TurnComplete ok                           │
├────────────────────────────────────────────┤
│ [ prompt…                       ] Send  Abort│  ← PromptBar
└────────────────────────────────────────────┘
```

## Data flow (happy path)

```
user fills URL+token → Connect
  → ws.rs opens ws://…/ws?token=…
  → server: Ready{session}        → state = Connected{session, last_seq:0}
user types prompt → Send
  → ws.send(Command::SendPrompt{session, text})
  → server streams Event{seq, kind} …  → each appended to EventLog, last_seq advances
  → Event{kind: TurnComplete{ok}}  → turn done; prompt bar re-enabled
user clicks Abort (mid-turn)
  → ws.send(Command::Abort{session})  → server aborts, closes socket → Disconnected (session retained)
user clicks Connect again
  → ws.rs opens ws://…/ws?token=…&session=<id>&last_seq=<n>
  → server replays Events after <n>, then Ready/live  → log catches up without duplicates
```

## Error handling

- **Bad URL / refused connection:** `onerror`/`onclose` → `Disconnected` with a visible error
  row in the log; form re-enabled.
- **401 on upgrade (bad token):** browsers surface a failed handshake as a close/error with no
  body; show a generic “connection rejected — check URL/token” error row.
- **`ServerMessage::Error` frame:** render as an error row; does not by itself disconnect.
- **Malformed frame:** log a client-side parse-error row; ignore the frame, keep the socket.
- **Reconnect replay:** `last_seq` is the highest seq applied, so replayed events are strictly
  newer — no dedup logic needed beyond “only advance, never re-apply ≤ last_seq”.

## Testing

- **`protocol`:** the relocated `ServerMessage` keeps/extends the existing round-trip test;
  add an explicit JSON-shape assertion (`{"type":"ready","session":…}`) to lock the wire tag.
- **`serve.rs`:** existing header-auth tests stay; add tests that (a) `?token=` authorizes a
  `/ws` upgrade and (b) a wrong/absent token is rejected. Reuse the ephemeral-port harness.
- **`ui/`:** WASM DOM testing is heavy; keep logic testable by extracting pure functions —
  URL building (`build_ws_url(base, token, session, last_seq)`) and `last_seq` advancement —
  into plain functions with `#[cfg(test)]` unit tests that need no browser. Component render is
  verified manually in the browser against a locally-running `otto serve` for this slice.

## Manual acceptance (definition of done)

1. `otto serve` running locally on a known port + token.
2. `trunk serve` the UI; open in a browser.
3. Enter URL + token, Connect → status line shows `connected` + a session id.
4. Send a prompt → event rows stream in and end with `TurnComplete`.
5. Send another prompt mid-stream-safe; Abort works.
6. Reload the tab / disconnect, reconnect with the same session → prior events replay, no
   duplicates, then live events resume.
