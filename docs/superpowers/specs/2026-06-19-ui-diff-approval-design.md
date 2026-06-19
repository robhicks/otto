# Sub-project D — Diff Approval (design)

**Date:** 2026-06-19
**Roadmap:** `docs/superpowers/specs/2026-06-17-ui-roadmap.md` (row **D**)
**Status:** Approved design — ready for an implementation plan.

## Goal

Let a human approve or reject each edit the Coder proposes, instead of applying it
unconditionally. When approval mode is on, an edit that the gate marks `Ask` pauses the turn:
the engine emits the proposed diff, the UI renders it with **Approve** / **Reject** buttons,
and the edit is applied only on an explicit approval. Headless/CLI behavior and the offline
determinism suite are unchanged.

## Where the current design blocks this

Three facts about the existing code shape every decision below:

1. **The orchestrator already gates every edit, fail-closed.** `Orchestrator::run_turn`
   (`crates/engine-core/src/orchestrator.rs`) calls `tools.check("fs.write", {path})` per edit
   and applies **only on `Decision::Allow`**; `Deny` and `Ask` are logged-and-skipped. Diff
   approval is therefore a new behavior on the **`Ask` branch only** — `Allow` and `Deny` are
   untouched, and the sensitive-path floor (`Deny`) is still evaluated first.

2. **Nothing prompts today.** `DefaultPermissionGate` returns `Allow` for ordinary `fs.write`
   paths (`Ask` only for `bash`). So `Ask`-on-write is currently an unreachable path. Approval
   mode is what makes it reachable, and it must be **opt-in** so existing behavior/tests hold.

3. **The serve loop blocks for the whole turn.** `handle_socket` (`crates/engine/src/serve.rs`)
   is a single `recv()` loop that calls `service.run_prompt(...).await` and cannot read the
   socket again until the turn finishes. A mid-turn `ApproveDiff` frame could never arrive.
   **Reading the socket concurrently with the running turn is the central change** of this
   sub-project.

## Decisions (locked)

- **Enabling: opt-in `otto serve --approve-edits` flag.** Off by default. The flag swaps in a
  gate that upgrades non-sensitive `fs.write` from `Allow` to `Ask`; everything else
  (CLI `run`, the offline suite, default `serve`) is unchanged. The interactive approver is
  *always* wired in `serve`; with the flag off the gate never returns `Ask` for writes, so the
  approver is simply never invoked.
- **Diff on the wire: old + new full contents.** The event carries `old: Option<String>`
  (`None` = new file) and `new: String`. The UI computes the line diff for display as a pure,
  host-testable function. `protocol` stays serde-only; no diff crate enters the engine or the
  protocol.
- **One folded event.** A single `ApprovalRequest { id, path, old, new }` event both carries
  the diff and signals that a verdict is needed; `ApproveDiff { session, id, approved }`
  responds. (The roadmap's separate `Diff` + `ApprovalRequest` are folded — the UI gets
  everything in one message and needn't correlate two events.)
- **Granularity: per-edit.** Each `Ask` edit is its own request/verdict, matching the existing
  per-edit gate loop. No batch-approve.
- **Reject and disconnect are fail-closed.** Reject ⇒ skip the edit and continue the turn
  (identical to today's `Ask → skip`, so the Repair loop can still react). Disconnect mid-wait
  ⇒ the pending verdict resolves `false` (skip), and the turn unwinds.

## Wire changes (`crates/protocol`, additive / semver-minor)

```rust
// Command — new variant
ApproveDiff { session: SessionId, id: Uuid, approved: bool }

// EventKind — new variant
ApprovalRequest { id: Uuid, path: PathBuf, old: Option<String>, new: String }
```

Both ride the existing `Command` / `ServerMessage::Event` JSON paths — no transport-framing
change. `Uuid` is already a `protocol` dependency. Round-trip tests added beside the existing
ones. An out-of-step peer that never sends `ApproveDiff` simply leaves the edit pending until
disconnect (fail-closed) — no new failure mode.

## The approval seam (`crates/engine-core`)

A new trait beside `AskResolver` in `tool.rs`, async because the verdict is a round-trip:

```rust
#[async_trait]
pub trait Approver: Send + Sync {
    /// Verdict for a proposed edit: true = apply, false = skip. Implementations must
    /// fail closed (return false) if they cannot obtain an answer.
    async fn request(&self, id: Uuid, path: &Path, old: Option<&str>, new: &str) -> bool;
}

/// Headless default: never approve (≙ today's `Ask → skip`).
pub struct DenyApprover;
```

`Orchestrator` gains `approver: &'a dyn Approver`. The edit loop's branch table becomes:

| Gate verdict | Behavior |
|---|---|
| `Allow` | apply + `FileEdit` (**unchanged**) |
| `Deny`  | log + skip (**unchanged**) |
| `Ask`   | read `old` from the workspace; mint `id`; emit `ApprovalRequest { id, path, old, new }`; `approver.request(...).await`; on `true` apply + `FileEdit`, on `false` log + skip |

The `ApprovalRequest` event is emitted through the existing synchronous `Emitter` (so ordering
and persistence are unchanged); the approver performs only the verdict round-trip and emits
nothing. Security invariants are preserved verbatim: the sensitive floor still `Deny`s before
approval is ever reached, and `DenyApprover` keeps CLI/headless fail-closed (the existing
`ask_verdict_also_skips_edit_fail_closed` test passes unchanged with `DenyApprover`).

`id` generation must stay out of `engine-core`'s deterministic core only insofar as it would
break offline reproducibility — but `Ask` edits never occur on the offline default path
(default gate `Allow`s writes), so a `Uuid::new_v4()` here is never hit by the determinism
suite. The orchestrator receives the id-minter as a small `Fn() -> Uuid` injected by the engine
layer (mirroring how `seq` is assigned outside the orchestrator), keeping `run_turn` itself free
of nondeterministic calls.

## Engine wiring (`crates/engine`)

### `run_prompt` gains an approver

`EngineService::run_prompt(session, goal, sink, approver: Arc<dyn Approver>)`. The approver is
cloned into the spawned turn task and handed to the `Orchestrator`. Callers:

- `run_goal` (CLI `run`) passes `Arc::new(DenyApprover)` → behavior unchanged.
- serve passes a per-connection `InteractiveApprover` (below).
- Existing `service.rs` tests pass `Arc::new(DenyApprover)`.

### The `--approve-edits` gate wrapper

A `PermissionGate` decorator (`ApprovalModeGate`) wrapping `DefaultPermissionGate`: returns the
inner verdict, except it upgrades `Allow → Ask` for tool `fs.write`. Sensitive `Deny` and the
`bash → Ask` classification pass through untouched. `build_tool_registry` /
`build_tools_preferring_mcp` gain an `approve_edits: bool` parameter that selects the wrapped vs.
plain gate; CLI `run` and tests pass `false`, `serve` passes the flag.

### Concurrent socket handling + verdict routing (`serve.rs`)

The one structural change. After `Ready` + replay, split the socket and read it concurrently
with the running turn:

```text
let (mut writer, mut reader) = socket.split();          // SplitSink + SplitStream
let approvals = ApprovalRegistry::new();                // connection-scoped

outer: while let Some(Ok(msg)) = reader.next().await {
    match command(msg) {
        SendPrompt { text, .. } => {
            let approver = Arc::new(InteractiveApprover::new(approvals.clone()));
            let mut sink = WsSink { writer: &mut writer };
            let turn = service.run_prompt(session, &text, &mut sink, approver);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    res = &mut turn => { /* send Error on Err; */ break }
                    inbound = reader.next() => match inbound {
                        Some(Ok(Text(t))) => match command(t) {
                            ApproveDiff { id, approved, .. } => approvals.resolve(id, approved),
                            Abort { .. }    => { service.abort(session).await; approvals.clear(); break outer }
                            SendPrompt { .. } => send Error "a turn is already in progress",
                            _ => {}
                        },
                        Some(Ok(Close(_))) | None => { approvals.clear(); break outer } // disconnect
                        _ => {}
                    }
                }
            }
        }
        Abort { .. } => { service.abort(session).await; break }
        ApproveDiff { .. } => { /* no turn in flight: ignore */ }
        CreateSession => {}
    }
}
```

Why this works:

- **Disjoint borrows.** The `turn` future borrows `&mut writer` (through `WsSink`);
  `reader.next()` borrows `&mut reader`. They are separate locals, so `select!` can drive both
  without `Arc<Mutex<sink>>` or spawning.
- **`ApprovalRegistry`** = `Arc<Mutex<HashMap<Uuid, oneshot::Sender<bool>>>>`.
  `InteractiveApprover::request(id, …)` inserts a oneshot and `rx.await.unwrap_or(false)`;
  `resolve(id, approved)` removes and fires the sender; `clear()` drops all senders.
- **Fail-closed teardown.** On disconnect/abort, `clear()` drops the senders → every pending
  `request()` resolves `false` → the (now-detached) orchestrator task skips its edit and the
  turn unwinds. A closed oneshot mapping to `false` is the single fail-closed rule.

`WsSink` and `send_msg` switch from `&mut WebSocket` to the `SplitSink<WebSocket, Message>`
writer half; no behavioral change to framing.

## UI (`ui/`)

- Decode the new `ApprovalRequest` event in `ws.rs`; hold a `pending: Option<PendingApproval>`
  signal in `app.rs`.
- A diff panel renders old-vs-new with line styling and **Approve** / **Reject** buttons;
  a click sends `ApproveDiff { session, id, approved }` over the existing socket and clears
  `pending`.
- **`fn diff_lines(old: Option<&str>, new: &str) -> Vec<DiffLine>`** in `view_model.rs` — a pure
  function (a simple line-level LCS or, to start, a context-free old-removed/new-added split)
  rendered with `row-del`/`row-add`/`row-ctx` classes, host-tested like `describe_event` /
  `capability_segments`. No new heavy deps; the `ui` crate still depends only on `protocol`
  (+ existing `kode-leptos`/`gloo-net`).

## Testing

- **engine-core (`orchestrator.rs`):** a fake `Approver` recording its args and returning a
  scripted verdict. Assert (a) approve → edit applied + `ApprovalRequest`/`FileEdit` emitted with
  the right `id`/`path`/`old`/`new`; (b) reject → no edit, turn still completes; (c) the existing
  `ask_verdict_also_skips_edit_fail_closed` passes with `DenyApprover`.
- **engine (gate):** `ApprovalModeGate` upgrades `fs.write` `Allow → Ask`, leaves sensitive
  `Deny` and `bash → Ask` intact.
- **engine (serve integration, ephemeral port):** with `approve_edits = true` and a scripted
  Coder edit — connect, send a prompt, receive `ApprovalRequest`, reply `ApproveDiff{true}` →
  file written; a second run replies `false` → not written; a third disconnects mid-wait →
  not written (fail-closed) and the session does not hang.
- **ui (host):** `diff_lines` over new-file / add / remove / modify cases; the pending-approval
  reducer (set on `ApprovalRequest`, cleared on send).

## Out of scope (deferred)

- Batch approval, partial-hunk approval, and editing-before-apply.
- Approval for non-`fs.write` `Ask` tools (e.g. `bash`) — the seam is general, but only the edit
  path is wired here.
- A syntax-highlighted diff (the diff panel is plain line styling; richer rendering can reuse
  `kode-leptos` later).
- Persisting approval verdicts as their own record type (the `ApprovalRequest`/`FileEdit` events
  already persist via the normal log).

## Invariants this must not break

- The sensitive-path floor still `Deny`s first; approval can never reach a sensitive path.
- Default `serve`, CLI `run`, and the offline determinism suite behave exactly as before
  (approval is opt-in; the offline path never produces an `Ask` write).
- The orchestrator still applies edits **only** on an explicit approval; reject/deny/disconnect
  all fail closed.
- `protocol` depends only on serde; `ui` depends only on `protocol` (+ its existing UI deps).
