# Server baseline capture — 2026-07-21 UI runtime spike

Ground-truth event streams captured directly from `otto serve` over the
`Command`/`Event` protocol, with `driver.mjs` (no UI client involved), before
any UI client under test.

## Server command line used

```bash
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/fixture

env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GEMINI_API_KEY \
  OTTO_TOKEN=spike-token OTTO_DB=/tmp/otto-ui-spike/baseline.db \
  ./target/release/otto serve --root /tmp/otto-ui-spike/fixture --port 8899 \
  --approve-edits --promote-loopback
```

The server started fully offline-deterministic: no `ANTHROPIC_API_KEY` /
`OPENAI_API_KEY` / `GEMINI_API_KEY` / `OTTO_*` provider vars were set (the
`env -u` prefixes strip any inherited from the shell), so both router slots
resolved to `LocalProvider`. Confirmed via `otto serve listening on
ws://127.0.0.1:8899/ws` in the server log and `capabilities.local_llm: false,
remote_llm: false` in every captured `ready` frame.

## Driver deviation from the brief (required to make it work at all)

The brief's literal `driver.mjs` sends commands like `{ SendPrompt: { text:
PROMPT } }`, with no `session` field. Every `Command` variant in
`crates/protocol/src/lib.rs` (`SendPrompt`, `Abort`, `ApproveDiff`,
`PromoteToRemote`, …) requires `session: SessionId`. Sent as literally
transcribed, the very first `SendPrompt` on every script failed immediately
with `{"type":"error","message":"bad command: missing field \`session\` at
line 1 column 76"}` and the driver then only ever captured the `ready` frame
plus that one `error` frame before timing out.

`driver.mjs` was corrected to thread `session` (read off the `ready` frame)
into every outgoing command: `SendPrompt`, `Abort`, `ApproveDiff`, and
`PromoteToRemote` all now include `session`. No other change was made — the
rest of the file (arg parsing, wire framing, event bookkeeping, per-script
branching) is transcribed exactly as given in the brief. This is a fix to a
docs-only spike script; no `crates/` file was touched (`git status crates/`
is clean).

## Event-variant sequence observed, per script

All four scripts against the fixture prompt (`Add a doc comment to the add
function in src/lib.rs`) produced the **same** ordered `EventKind` sequence
for the underlying turn — the four-stage spine (Plan → ContextFinder → Coder
→ Verify) surfaced as one `AgentStarted`/`AgentFinished` pair per agent, plus
one `Log` (from Planner) and one `VerifyResult` (from Verifier):

```
AgentStarted -> Log -> AgentFinished ->
AgentStarted -> AgentFinished ->
AgentStarted -> AgentFinished ->
AgentStarted -> VerifyResult -> AgentFinished ->
TurnComplete
```

- **`turn`** (`baseline/turn.json`): 12 captured frames (`ready` + 11
  `event` frames, the last being `TurnComplete`); driver exited on
  `TurnComplete` as designed. `lastSeq: 10`.
- **`approve`** (`baseline/approve.json`): same 12-frame sequence as `turn`
  (no `ApprovalRequest` fired — see decision gate below); driver exited on
  `TurnComplete`, `lastSeq: 10`.
- **`abort`** (`baseline/abort.json`): 9 captured frames (`ready` + 8 `event`
  frames — the sequence truncated after the 4th `AgentStarted`, where the
  driver sends `Abort` per its `events.length === 4` trigger). The driver has
  no explicit "abort acknowledged" handling (matching the brief as given), so
  it ran to its 120s timeout (`meta.timeout: true`) rather than exiting
  early; `lastSeq: 7` — no further events arrived after the abort was sent,
  consistent with the turn actually stopping.
- **`promote`** (`baseline/promote.json`): 13 captured frames (`ready` + 11
  `event` frames ending in `TurnComplete`, plus 1 `promoted` frame); driver
  exited on the `promoted` frame. `lastSeq: 10`.

## ApprovalRequest decision gate (brief Step 5)

```
grep -c ApprovalRequest baseline/approve.json
```

Result: **`0`**.

Per the brief, the fixture/prompt was adjusted once and the `approve` script
re-run: the driver's `PROMPT` constant was temporarily changed to `Implement
a new multiply function in src/lib.rs that does not exist yet, with a doc
comment.` (naming an obviously-missing function) against the same running
server/fixture. Result: still **`0`** — same `AgentStarted -> Log ->
AgentFinished -> … -> TurnComplete` sequence, no `ApprovalRequest` in any
captured frame. The temporary prompt change was then reverted; the committed
`driver.mjs` uses the brief's original `PROMPT` text
(`'Add a doc comment to the add function in src/lib.rs'`).

**Conclusion: the offline-deterministic Coder proposes no edits against this
fixture, for either prompt tried, so no `ApprovalRequest` event can ever
fire on this path.** Per the brief, this is recorded rather than faked:
**step 9 (diff approval) is `NOT-APPLICABLE (offline Coder proposes no
edits)` for all four builds**, and the diff-approval dimension must be
reported as *untested* in the eventual contract/report, not passed.

## Promote outcome (brief Step 6)

```
node -e "const d=require('./baseline/promote.json');console.log('promoted frame:',!!d.meta.promoted_frame,'lastSeq:',d.meta.lastSeq)"
```

Result: `promoted frame: true lastSeq: 10`.

`--promote-loopback` engaged correctly: after the turn's `TurnComplete`, the
driver sent `PromoteToRemote`, and the server replied with a `promoted`
`ServerMessage` frame (captured in `meta.promoted_frame`), which the driver
recognized and exited on. Step 11 (promote-to-remote) can proceed on this
basis.
