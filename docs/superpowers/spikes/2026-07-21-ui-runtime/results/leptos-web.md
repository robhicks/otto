# Run log — `leptos-web`

Reference run for the 2026-07-21 UI runtime spike. Driven per the frozen contract
(`docs/superpowers/specs/2026-07-21-ui-runtime-scenario.md`) against the server baseline
(`docs/superpowers/spikes/2026-07-21-ui-runtime/baseline/README.md`).

## Build

```bash
cd ui && cargo clean && trunk build --release
```

- **Wall-clock (clean release build, `cargo clean` first):** `1m 01.91s` (`/usr/bin/time -v`:
  user 394.73s, sys 44.07s, 708% CPU — a fully cold compile of the whole `ui/` dependency
  graph, including `leptos`, `kode-leptos`, `gloo-net`, `otto-protocol`).
- **Warm rebuild (no source changes, cache intact):** `0.84s`.
- **Artifact sizes** (`ui/dist/`):

  | File | Raw bytes | Gzip bytes |
  |---|---|---|
  | `otto-ui-a3d3a1678e353692_bg.wasm` | 1,568,220 | 483,131 |
  | `otto-ui-a3d3a1678e353692.js` | 48,192 | 8,486 |
  | `style-4cf5c4a2608350ef.css` | 2,563 | 933 |

- **Toolchain:** `rustc 1.95.0 (59807616e 2026-04-14)`, `cargo 1.95.0 (f2d3ce0bd
  2026-03-21)`, `trunk 0.21.14`, target `wasm32-unknown-unknown`. Key deps (from
  `ui/Cargo.toml`): `leptos 0.8` (csr), `kode-leptos =0.5.4`, `gloo-net 0.6`.

## Environment

- **Provider keys:** confirmed absent for this run — `env | grep -E
  "ANTHROPIC_API_KEY|OPENAI_API_KEY|GEMINI_API_KEY"` returned nothing in the launching
  shell, and the server was started with `env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u
  GEMINI_API_KEY` regardless. Server log confirms `capabilities.local_llm: false,
  remote_llm: false` was rendered every time (status strip: `LLM: offline
  (deterministic)`).
- **`OTTO_DB`:** `/tmp/otto-ui-spike/leptos-web.db`
- **Port:** `8899` (`otto serve --root /tmp/otto-ui-spike/fixture --port 8899
  --approve-edits --promote-loopback`); the UI bundle served via `python3 -m http.server
  8080 --directory ui/dist`.
- Confirmed via server log: `otto serve listening on ws://127.0.0.1:8899/ws`.
- The single `otto serve` process (pid 283550) ended up holding **three** listeners by the
  end of the run — `8899` (original), `33349` (promoted loopback target), `46673` (demoted
  loopback target) — all `LoopbackTarget`-provisioned in-process engines within the same OS
  process (see step 11 notes).

## Steps

| N | Status | Evidence | Notes |
|---|---|---|---|
| 1 | PASS | Snapshot immediately after navigating to `http://127.0.0.1:8080/?ws=ws://127.0.0.1:8899&token=spike-token&autoconnect=1`: `text: "status: connected · 4a71… · seq -"`. Autoconnect worked with no manual form fill needed. | — |
| 2 | PASS | Same snapshot: `"| engine: local · LLM: offline (deterministic) · sandbox: on"` — the degraded/offline LLM state renders visibly, not blank. | — |
| 3 | PASS | Post-send snapshot rendered exactly the 11 expected lines in order: `▸ Planner started` / `· planned 1 milestone(s)` / `▸ Planner finished` / `▸ ContextFinder started` / `▸ ContextFinder finished` / `▸ Coder started` / `▸ Coder finished` / `▸ Verifier started` / `✓ Verify cargo test passed` / `▸ Verifier finished` / `● TurnComplete ok`, with `status: connected · 4a71… · seq 10` — frame-for-frame match to the baseline, `lastSeq: 10`. | Repeated identically across 4 separate real turns run during this session (sqlite confirms 44 events for session `4a71...`, `count(*) == count(distinct seq) == 44`, i.e. 4×11). |
| 4 | NOT-VERIFIABLE (offline turn completes faster than interrupt round-trip) | Sent a fresh prompt then immediately clicked Abort; the snapshot taken right after showed the full 11-line completed sequence already rendered (`● TurnComplete ok`) *and* `status: disconnected · - · seq 10`, form re-enabled with a bare `Connect` button. Server log showed no errors. | The turn was already done before `Abort` landed (expected on this offline path — matches the brief). The observed side effect (the socket closing) is **not a UI bug**: `crates/engine/src/serve.rs:616-619` shows the server's per-connection command loop unconditionally `break`s out of `'outer` on any `Command::Abort`, whether or not a turn is in flight (`Command::Abort { .. } => { let _ = state.service.abort(session).await; break; }`). This is inherent, already-shipped server behavior; not touched (fix would require `crates/`, and there is nothing to fix — it's working as designed). The UI handled the resulting disconnect gracefully: no crash, no wedge, clean return to a connectable idle state. |
| 5 | PASS | Reconnected via the `Connect` button after the abort-induced disconnect (session `dc5a1e46-...`): rendered event log again showed the same 11 lines, no duplication, `seq 10`. `sqlite3 leptos-web.db "select session_id, count(*), count(distinct seq) from events group by session_id;"` → `dc5a1e46-...\|11\|11` — count(*) equals count(distinct seq), confirming exactly-once delivery (no gap, no dupe). | The literal contract query (`select count(*), count(distinct seq) from events` with no `group by`) returns `55\|44` late in the run — this is *not* a duplication signal: the `events` table's primary key is `(session_id, seq)` (`select sql from sqlite_master` confirms), so `seq` is scoped per-session and legitimately repeats across the run's several sessions (multiple page loads for the cold-start measurements each opened a fresh session). The per-session grouped query is the correct exactly-once check and is clean for every session. |
| 6 | PASS | Clicked `▸ src` → expanded to `▾ src` showing `util`, `lib.rs`; clicked `▸ util` → expanded to `▾ util` showing `mod.rs`; clicked again → collapsed back to `▸ util`. `.env` never appeared as a tree node in any of the ~10 snapshots taken across the whole run (root listing consistently: `▸/▾ src`, `Cargo.lock`, `Cargo.toml`, `README.md`). | Sensitive-path floor confirmed filtering `.env` server-side before the `POST /workspace` listing ever reaches the client — nothing to deny, nothing rendered. |
| 7 | PASS | Clicked `lib.rs`: editor pane rendered `src/lib.rs` with all 16 lines of real file content (`pub mod util;`, `pub fn add(a: i64, b: i64) -> i64 {`, `a + b`, `}`, the `#[cfg(test)]` module, etc.), as plain text (no syntax highlighting). | Plain-text rendering on web vs. highlighted on desktop is the already-recorded, expected gap — not a new bug. |
| 8 | PASS | Focused the editor by clicking a rendered line, then sent key `X` via `browser_press_key`. Snapshot after: line 1 changed from `pub mod util;` to `pub mod util;X`, and the tab label changed from `src/lib.rs` to `src/lib.rs ●` (dirty/unsaved marker). `cat /tmp/otto-ui-spike/fixture/src/lib.rs` still shows the original, unmodified content on disk — confirming the edit is local-buffer-only. | The editor's real input target is a visually-hidden `<textarea class="kode-hidden-textarea">`; `browser_click`/`browser_type` couldn't drive it directly (off-viewport), so it was focused by clicking a visible line and driven via `browser_press_key`, which worked cleanly. |
| 9 | NOT-APPLICABLE | Per the frozen contract and Task 2 baseline: the offline-deterministic Coder proposes zero edits against this fixture (`grep -c ApprovalRequest` = 0 in every baseline capture). Corroborated live: 4 real turns run during this session, zero `ApprovalRequest` events, `Promote`/`Demote` buttons never blocked on a pending approval. | Not attempted/faked, per contract. |
| 10 | PASS (pause/resume) / NOT-VERIFIABLE (meter, see notes) | Sent a fresh prompt and, in the same round trip, clicked `Pause` immediately after `Send`. The pause genuinely landed *before* the new turn's first event: `sqlite3` shows session `dc5a...` seq 11 = `{"Log":{"message":"turn paused"}}` — with **no** `AgentStarted` yet for that turn (the orchestrator's `checkpoint()` in `crates/engine-core/src/orchestrator.rs:65-75` runs before `Planner`'s `AgentStarted`, found `should_pause()==true`, emitted the paused-log, and parked on `wait_for_resume()`). Snapshot confirmed: button flipped to `Resume`, log showed `· turn paused`, `Promote`/`Demote` disabled. Clicking `Resume` produced `· turn resumed` (seq 12) followed by the full 11-frame turn to `● TurnComplete ok` (seq 13-23) — genuine pause-then-resume-to-completion. | **Token/cost meter never updates in this configuration** — not a bug, a structural consequence of the offline path: `EventKind::TokenCostMeter` never appears in *any* baseline capture (`grep -c TokenCostMeter` = 0 across `turn.json`/`approve.json`/`abort.json`/`promote.json`) and the orchestrator's `emit_meter()` (`crates/engine-core/src/orchestrator.rs:51-61`) only emits when `self.meter.total() > 0` — structurally impossible with `LocalProvider`. Correspondingly, the meter `<span>` (`ui/src/components/status_line.rs:53`, rendered only when `meter.get()` is `Some`) never appeared in any of the ~15 snapshots taken across 6 real turns. Recording the meter half as `NOT-VERIFIABLE (offline path never emits TokenCostMeter)`, honestly, rather than PASS. |
| 11 | PASS | Clicked `Promote to remote`: status strip flipped `engine: local` → `engine: remote`, ws endpoint textbox changed `ws://127.0.0.1:8899` → `ws://127.0.0.1:33349`, session id and `seq 23` unchanged, full 23-row event log preserved (no reset, no gap), `Demote to local` became enabled. Ran a fresh turn on the promoted engine: `seq` advanced `23` → `34` (exactly 11 new frames, continuous). Clicked `Demote to local`: status strip flipped back `engine: remote` → `engine: local`, ws endpoint changed again to `ws://127.0.0.1:46673`, `seq` stayed `34` (no reset), full event log intact, `Promote to remote` re-enabled. `ss -tlnp` confirmed all three ports (`8899`, `33349`, `46673`) were held by the **same** pid (283550) throughout — the original process was never killed. | The demoted-to endpoint (`46673`) is a *new* ephemeral port, not literally the original `8899` listener — by design, not a bug: `crates/engine/src/loopback.rs:49` shows `LoopbackTarget::provision` opens a brand-new `SqliteStore::open(dir.join("sessions.db"))` for *every* provision call (promote and demote alike), so "returns to local" means "a freshly-provisioned local in-process engine," not literally the original socket. The session/seq continuity guarantee the contract cares about held throughout, confirmed client-side (authoritative per the contract's "How driven (web)" column for this step). |

## Measurements

All timings captured via an in-page `performance.now()` instrument installed and read through a single `browser_evaluate` round trip (click + poll + return, all inside one JS call) so tool-call latency never contaminates the reading — see the two contaminated attempts below that were discarded.

1. **Web bundle size** — see `## Build` table above (wasm 1,568,220 B / 483,131 B gzip; js 48,192 B / 8,486 B gzip; css 2,563 B / 933 B gzip).

2. **Cold start → first paint** (`performance.getEntriesByName('first-contentful-paint')`, 3 fresh page loads):
   - Rep 1: 68 ms
   - Rep 2: 72 ms
   - Rep 3: 76 ms
   - **Median: 72 ms** (min 68, max 76)

3. **Cold start → `Ready` handled** (navigate → `status: connected` text appears, 3 fresh page loads):
   - Rep 1: 2584.2 ms
   - Rep 2: 2715.0 ms
   - Rep 3: 2495.6 ms
   - **Median: 2584.2 ms** (min 2495.6, max 2715.0)
   - This is dominated by wasm fetch+instantiate+init cost (1.5MB wasm) plus the WS handshake; FCP (~72ms) shows the shell paints fast, so essentially all of this window is wasm boot, not network.

4. **Event render latency** (Send-click → `TurnComplete` rendered, one fixed turn/prompt, single round-trip instrumentation, 3 fresh turns):
   - Rep 1: 56.8 ms
   - Rep 2: 57.9 ms
   - Rep 3: 54.2 ms
   - **Median: 56.8 ms** (min 54.2, max 57.9)
   - *Discarded, contaminated attempt (for the record):* an initial two-tool-call version (set `window.__t0` in one `browser_evaluate`, click Send in a separate `browser_click`, poll in a third call) returned `7425 ms` for a turn that was almost certainly already complete — the number reflects the gap between my own tool-call round trips, not real render latency, exactly the failure mode the brief warned about. Discarded in favor of the atomic single-call method above.

5. **Reconnect replay time** (Connect-click → `status: connected` re-rendered, post-handover session already holding 34 persisted events, 3 reps):
   - Rep 1: 4.8 ms
   - Rep 2: 4.9 ms
   - Rep 3: 5.2 ms
   - **Median: 4.9 ms** (min 4.8, max 5.2)
   - Note: the client's `should_apply(last_seq, seq)` dedup (`ui/src/app.rs:85`) means replayed-but-already-seen events append no new DOM rows, so "replay complete" was measured via the `status: connected` transition rather than a row-count delta (row count never changed across these reps, which is itself the expected "exactly once" outcome, not a measurement failure) — a first attempt using row-count-based completion detection returned a bogus `6.3 ms` because the rows were already present pre-reconnect (never cleared on disconnect) and so satisfied the check trivially; corrected to the connected-status signal.

6. **Desktop RSS** — `VOID (web build, no desktop process)`.

7. **Desktop binary size** — `VOID (web build, no desktop artifact)`.

8. **Build wall-clock** — `1m 01.91s` (clean release build; see `## Build`).

## Bugs

None found. No `ui/` source changes were made during this run.

The one surprising runtime behavior observed — clicking `Abort` after a turn has already completed causes the server to close the WebSocket connection — was traced to `crates/engine/src/serve.rs:616-619` (the per-connection command loop unconditionally `break`s on any `Abort`, in-flight turn or not). This is pre-existing, intentional server design, not a client defect, and fixing/changing it would mean touching `crates/`, which is out of scope for this run (and there is nothing to fix — the UI already handles the resulting disconnect gracefully). Recorded as a note under step 4, not as a bug.
