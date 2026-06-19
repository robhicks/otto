# otto UI — Sub-project B: Capabilities + Status Strip

**Date:** 2026-06-17
**Status:** Draft for review
**Roadmap:** `docs/superpowers/specs/2026-06-17-ui-roadmap.md` (sub-project B)
**Builds on:** Sub-project A (`2026-06-17-ui-shell-live-session-design.md`, shipped PR #47)

## Summary

Make the running engine's capabilities **visible** in the UI. The engine sends a
`CapabilitiesManifest` on connect (carried in the `Ready` frame); the UI replaces sub-project
A's plain status line with a structured **status strip** that surfaces engine / LLM / sandbox
state and, crucially, shows **degradation** — a missing sandbox or a fully-offline deterministic
engine — in a way the user can see at a glance.

This is the second-thinnest slice on top of the app shell: it adds one additive protocol field,
one wiring-layer helper, one transport touch, and a frontend strip. It does not touch the
orchestrator, the gate, or the providers.

## Goals

- The engine emits its `CapabilitiesManifest` on connect, framed inside `Ready`.
- The manifest honestly distinguishes a **remote-LLM-backed** engine, a **local-LLM** engine, a
  **both** (BrainBlend) engine, and the **fully-offline deterministic fallback** (no real LLM).
- The UI status strip replaces the A-era one-line status, showing connection state, session,
  seq, and the engine/LLM/sandbox capability segments.
- Lost or absent capabilities (no sandbox → `bash` disabled; no LLM → deterministic stub output)
  render as **visibly degraded** segments, not silently.
- The capability-derivation logic is pure and host-tested; no new browser-only test surface.

## Non-Goals (this sub-project)

- No file tree, file view, or editor (sub-project C).
- No diff rendering or approval (sub-project D).
- No token/cost meter, pause/resume, or promote (sub-projects E/F). In particular, this slice
  does **not** wire the promote path that would flip `engine_remote` to `true`; it only reports
  the field honestly for the local serve (always `false` today).
- No per-tool capability inventory. The manifest stays the three-axis (engine/LLM/sandbox)
  summary the design intends; a richer tool listing is out of scope.
- No reconnect/transport changes beyond reading the new `Ready` field.

## Protocol changes (additive, semver-minor)

Both changes are additive to types already in `protocol`; no existing field is renamed or
removed, and the inbound (`Command`) direction is untouched.

### 1. Extend `CapabilitiesManifest` with `remote_llm`

The current manifest cannot express "a remote LLM (Anthropic) is configured," so a status strip
built on it could not tell the fully-offline deterministic fallback apart from a remote-backed
engine — defeating the point of a *visible degradation* strip. Add one field:

```rust
pub struct CapabilitiesManifest {
    pub engine_remote: bool,
    pub local_llm: bool,
    pub remote_llm: bool, // NEW: a remote provider (Anthropic) is configured
    pub sandbox: bool,
}
```

`remote_llm` is the honest signal that the engine has a real remote model wired, distinct from
`local_llm` (Ollama). With both `local_llm` and `remote_llm` false, the engine is on its
deterministic offline path (`LocalProvider`) — the degraded LLM state the strip must surface.

### 2. `Ready` carries the manifest

```rust
pub enum ServerMessage {
    Ready { session: SessionId, capabilities: CapabilitiesManifest }, // capabilities is NEW
    Event { event: Event },
    Error { message: String },
}
```

Wire shape becomes:

```json
{"type":"ready","session":"…","capabilities":{"engine_remote":false,"local_llm":false,"remote_llm":false,"sandbox":false}}
```

The `type` tag and `session` field are unchanged; `capabilities` is a new sibling object.
Because `capabilities` is a required field, a `protocol` consumer built before this change would
reject the new `Ready` frame — but the UI and the engine share the one `protocol` crate and are
upgraded together in this change set, so there is no version skew within the repo.

## Engine changes

### `build_capabilities()` — a wiring-layer helper

Capabilities are derived from the same environment that `build_router()` and `session_config()`
read. To preserve the determinism invariant ("anything reading `OTTO_*` / `ANTHROPIC_API_KEY`
belongs behind `build_router`, not in core logic"), the derivation lives in the engine wiring
crate (`crates/engine/src/lib.rs`), beside its peers:

```rust
pub fn build_capabilities() -> CapabilitiesManifest {
    CapabilitiesManifest {
        // `otto serve` is the local engine. The promote path (sub-project F) provisions a
        // separate remote engine that computes its own manifest with engine_remote = true;
        // nothing in the plain serve path sets it.
        engine_remote: false,
        local_llm: std::env::var("OTTO_OLLAMA").as_deref() == Ok("1"),
        remote_llm: std::env::var("ANTHROPIC_API_KEY")
            .map(|k| !k.is_empty())
            .unwrap_or(false),
        sandbox: os_sandbox_available(),
    }
}
```

This mirrors `session_config()`'s env reads exactly (same `OTTO_OLLAMA` / `ANTHROPIC_API_KEY`
predicates), so a session's recorded config and its reported capabilities stay consistent.

### Thread the manifest through `serve`

The manifest is computed once at startup and framed on every connect (so a reconnect re-reports
it). It flows transport-side, not through `EngineService` — `EngineService::new` is unchanged,
keeping the embedded/`run_goal` callers (which emit no `Ready` frame) untouched.

- `app()` / `serve_app()` gain a `capabilities: CapabilitiesManifest` parameter.
- `ServeState` gains a `capabilities: CapabilitiesManifest` field.
- `handle_socket` sends `ServerMessage::Ready { session, capabilities: state.capabilities.clone() }`.
- `main.rs`'s serve path computes `let capabilities = otto_engine::build_capabilities();` and
  passes it into `serve_app(service, token, capabilities)`.

The `POST /workspace` RPC is unchanged.

## Frontend changes

### State

`app.rs` adds one signal:

```rust
let capabilities = RwSignal::new(None::<CapabilitiesManifest>);
```

- On `ServerMessage::Ready { session, capabilities: caps }`: set `session`, set
  `capabilities = Some(caps)`, transition to `Connected`.
- On a fresh `connect()` and on `disconnect()`: set `capabilities = None`, so the strip never
  shows a stale manifest from a previous (or different) engine. Capability segments render only
  while `Connected` *and* `capabilities.is_some()`.

`ConnState` does **not** carry the manifest; keeping it in its own `Option` signal keeps the
state machine from sub-project A unchanged and the manifest's lifetime explicit.

### Pure derivation (`view_model.rs`, host-tested)

A capability segment is a label, a value, and a degraded flag:

```rust
pub struct CapSegment {
    pub label: &'static str,   // "engine" | "LLM" | "sandbox"
    pub value: String,         // "local", "offline (deterministic)", "off", …
    pub degraded: bool,        // true → rendered in the warning style
}

pub fn capability_segments(m: &CapabilitiesManifest) -> Vec<CapSegment>;
```

Derivation rules:

| Segment | Value | Degraded? |
|---|---|---|
| engine  | `engine_remote` ? `"remote"` : `"local"` | never (informational) |
| LLM     | `remote_llm && local_llm` → `"local+remote"`; `remote_llm` → `"remote"`; `local_llm` → `"local"`; else → `"offline (deterministic)"` | true only in the `offline` case |
| sandbox | `sandbox` ? `"on"` : `"off"` | true when `off` (`bash` disabled) |

The "offline (deterministic)" LLM value and the "off" sandbox value are the two degradations the
strip exists to make visible.

### Status strip component

The A-era `StatusLine` is **replaced** by the capabilities strip (the roadmap names `StatusLine`
as the seam B "replaces/extends"). The component takes the `conn`, `last_seq`, and `capabilities`
signals and renders a single row:

```
status: connected · 3f9a… · seq 12 | engine: local · LLM: offline (deterministic) · sandbox: off
                                                      └──── cap-degraded (amber) ──┘  └ degraded ┘
```

- Left half (transport): `status: <state> · <session> · seq <n>` — identical to A.
- A separator (`|`), then the capability segments joined by `·`, shown only while Connected with
  a manifest. Each segment renders as `<label>: <value>`; a degraded segment carries a
  `cap-degraded` CSS class.
- When Disconnected/Connecting (or no manifest yet), only the transport half renders — no
  capability segments, matching A's behavior during connect.

### Styling (`style.css`)

Add minimal classes consistent with A's terminal-like monochrome:

- `.cap` — the capability segment span (slightly dimmed default/"ok" color).
- `.cap-degraded` — amber/warning foreground for degraded segments, the one place the strip
  breaks monochrome, so degradation reads instantly.

## Data flow

```
otto serve startup
  → build_capabilities() reads OTTO_OLLAMA / ANTHROPIC_API_KEY / os_sandbox_available()
  → serve_app(service, token, capabilities) stores it in ServeState

client Connect / Reconnect
  → server: Ready { session, capabilities }
  → UI: session set, capabilities = Some(manifest), state = Connected
  → strip renders transport half + engine/LLM/sandbox segments (degraded ones amber)

client Disconnect / next Connect
  → capabilities = None  → strip drops the capability segments (no stale display)
```

## Error handling

- **Manifest absent / malformed `Ready`:** a `Ready` frame that fails to parse is surfaced the
  same way A surfaces any parse failure — a client-side error row in the log — and the connection
  does not advance to `Connected`. (There is no partial-manifest case: the field is required, so
  serde rejects a `Ready` missing `capabilities`.)
- **Stale manifest:** prevented structurally by clearing `capabilities` to `None` on
  disconnect/connect-start; segments never outlive the connection that produced them.
- No new transport failure modes: the strip is render-only over state the existing `on_msg`
  callback already owns.

## Testing

- **`protocol`:**
  - Update `server_message_ready_has_snake_case_tag` to construct `Ready { session, capabilities }`
    and additionally assert the nested shape: `v["capabilities"]["engine_remote"]` etc., locking
    the wire contract.
  - Add a `CapabilitiesManifest` round-trip test covering the new `remote_llm` field.
- **`engine`:**
  - `build_capabilities` unit test, env save/restore-guarded exactly like the existing
    `default_build_router_is_offline_and_deterministic` test: with `OTTO_OLLAMA` /
    `ANTHROPIC_API_KEY` unset, assert `local_llm == false`, `remote_llm == false`,
    `engine_remote == false`, and `sandbox == os_sandbox_available()`; with each var set, assert
    the corresponding flag flips.
  - `serve.rs`: add a test that connects and asserts the first frame is `Ready` carrying the
    expected manifest. Existing `app(...)` call sites in tests take the new manifest argument.
- **`ui/`:**
  - Host-side `view_model` tests for `capability_segments`: offline manifest → LLM segment
    `offline (deterministic)` with `degraded == true`; `remote_llm` → `remote`, not degraded;
    `local_llm && remote_llm` → `local+remote`; `sandbox == false` → sandbox `off`, degraded;
    engine segment never degraded.
  - `cargo build --target wasm32-unknown-unknown` compile check. Strip render verified manually
    in the browser against a locally-running `otto serve` (and against one started with
    `OTTO_OLLAMA=1` / `ANTHROPIC_API_KEY=…` to see the non-degraded LLM segment).

## Manual acceptance (definition of done)

1. `otto serve` running locally with **no** model env vars set.
2. `trunk serve` the UI; Connect → strip shows `engine: local · LLM: offline (deterministic) ·
   sandbox: <on|off>`, with the LLM segment (and sandbox, if no backend) rendered amber.
3. Restart `otto serve` with `ANTHROPIC_API_KEY=…` set → reconnect → LLM segment shows `remote`,
   no longer degraded.
4. Restart with `OTTO_OLLAMA=1` → LLM segment shows `local`.
5. Disconnect → capability segments disappear; only the transport half remains.
6. Existing A acceptance (connect/prompt/abort/reconnect-replay) still passes unchanged.
