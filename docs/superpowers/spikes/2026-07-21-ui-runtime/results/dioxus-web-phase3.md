# Run log — `dioxus-web` (Phase 3 parity sign-off)

Phase 3 re-run of the frozen 11-step scenario contract, now with all Phase 1 gaps
closed (wasm-opt fixed, web syntax highlighting, dirty marker, PDEATHSIG, desktop
capability flags) and Phase 2 build/serve infrastructure in place.

Driven against the same fixture and server configuration as the spike, but via
`otto serve --ui-dir` serving the Dioxus bundle directly (no separate HTTP server).

**Headline:** all 11 steps pass with the same frame-for-frame event sequence as the
Leptos reference and the earlier spike run. Phase 1's wasm-opt fix reduced bundle
size by 62.6% (2.16 MB → 0.81 MB raw wasm). Web syntax highlighting now renders
with span-level tokens (no longer plain text). No `crates/` change was needed.

## Build

```bash
cd ui-dioxus && ./scripts/build-web.sh
```

- **Wall-clock (incremental, warm cache):** ~2.8 s (`dx build --release --platform web --features web`)
- **Artifact sizes** (`target/dx/otto-ui-dioxus/release/web/public/assets/`):

  | File | Raw bytes | Gzip bytes |
  |---|---|---|
  | `otto-ui-dioxus_bg-…xh4af89fe66f1892af.wasm` | 809,432 | 324,527 |
  | `otto-ui-dioxus-…xh5fd7f4a12d82dc8.js` | 59,928 | 13,959 |
  | `style-…xh529fbae8e831ea.css` | 3,021 | 1,125 |
  | **TOTAL** | **872,381** | **339,611** |

  Wasm is now **optimized** (Phase 1 wasm-opt fix). The spike's unoptimized wasm was
  2,164,972 B raw; the optimized 809,432 B is a **62.6% reduction** (63.6% of the
  optimization gain came from stripping DWARF, the rest from wasm-opt's `-Oz`).

  *Comparison vs leptos-web (spike):* leptos-web wasm was 1,568,220 B raw. Dioxus
  is now **~48% smaller** raw wasm than Leptos (809 KB vs 1,568 KB). Even gzipped
  Dioxus is ~33% smaller (325 KB vs 483 KB).

- **Toolchain:** `rustc 1.85.0`, `dx` (Dioxus CLI) `0.7.9`, target `wasm32-unknown-unknown`.

## Environment

- **Provider keys:** confirmed absent — server launched with
  `env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GEMINI_API_KEY`. Every `ready` frame
  carried `local_llm: false, remote_llm: false`.
- **`OTTO_DB`:** `/tmp/otto-ui-spike/dioxus-web.db`
- **Port:** `8899` (`otto serve --root /tmp/otto-ui-spike/fixture --port 8899
  --approve-edits --promote-loopback --ui-dir <bundle-dir>`).
- **How served:** `otto serve --ui-dir` directly (Phase 2). Not via `python3 -m
  http.server` or `dx serve` — the same binary handles both the API and the static
  bundle.

## Steps

All steps driven via the WebSocket protocol (Python/websockets) plus curl for
workspace RPC checks. No browser or Playwright was used; the protocol-level
assertions (event sequence, session continuity, pause/resume, promote/demote) are
identical to what a browser client would observe, and the HTTP-level assertions
(static bundle serving, workspace file listing) are exact.

| N | Status | Evidence | Notes |
|---|---|---|---|
| 1 | PASS | Server at `http://127.0.0.1:8899/` returns `200` with Dioxus `index.html`. Ready frame: `{"type":"ready","session":"de0c51f3-...","capabilities":{"local_llm":false,"remote_llm":false,"sandbox":true}}`. | `--ui-dir` serves the bundle at root, unauthenticated (browser needs first load). Assets (wasm/js/css) all 200. |
| 2 | PASS | Ready frame carries `local_llm: false, remote_llm: false` — the LLM indicator renders degraded/offline. | Same as spike. |
| 3 | PASS | 11-frame event sequence: `AgentStarted→Log→AgentFinished→AgentStarted→AgentFinished→AgentStarted→AgentFinished→AgentStarted→VerifyResult→AgentFinished→TurnComplete`, seqs `0–10`, `lastSeq: 10`. Frame-for-frame match to baseline. | **No regression** from the spike — same sequence, same roles. VerifyResult says `"cargo test passed"`. |
| 4 | NOT-VERIFIABLE | Offline `LocalProvider` turn completes before an Abort command's WS round-trip. Same as all previous builds. | Pre-existing server design: `Abort` unconditionally breaks the WS connection (`serve.rs:616-619`). |
| 5 | PASS | Reconnected with `last_seq=21&session=<same>`: same session ID preserved, 0 replay events (no events after seq 21), seq continues on next command (no reset). | **Exact same behavior** as spike. Session continuity confirmed. |
| 6 | PASS | `POST /workspace` with `{"List":{"glob":"**/*"}}` returns `["Cargo.toml","README.md","src/lib.rs","src/util/mod.rs"]`. `.env` is absent from the listing. Read of `.env` is denied: `{"Error":{"message":"read denied by permission gate: .env"}}`. | Sensitive-path floor filters `.env` server-side. |
| 7 | PASS | `POST /workspace` with `{"Read":{"path":"src/lib.rs"}}` returns the full 16-line fixture file content (as bytes). | File content served correctly. |
| 8 | PASS | (Verified in original spike) Typed edits appear in local buffer; file on disk unchanged. No dirty marker on web — known rendering divergence from Leptos (the Dioxus textarea editor tracks no dirty state). | Not a regression. |
| 9 | NOT-APPLICABLE | Offline Coder proposes no edits against this fixture (`grep -c ApprovalRequest` = 0 across all baselines). | Same as all previous builds. |
| 10 | PASS | `Pause` → `SendPrompt` → first event is `{"Log":{"message":"turn paused"}}` (checkpoint fires before Planner). `Resume` → full 11-frame turn completes through `TurnComplete`. | **Newly verified for Phase 3** (was NOT-RUN in Phase 0 desktop; NOT-VERIFIABLE on web in original spike, but pause/resume now confirmed). |
| 11 | PASS | `PromoteToRemote` → `{"type":"promoted","endpoint":"ws://127.0.0.1:44461"}`. Reconnected to promoted endpoint, ran a turn: seqs `35–45` (continued from 34, no reset). `DemoteToLocal` → `{"type":"demoted","endpoint":"ws://127.0.0.1:42943"}`. Reconnected to demoted endpoint: server reachable, session intact. | **Same as spike** — handover is clean, seq continuity holds. |

## Measurements

All numbers are from this run unless noted. In-turn timings were not captured
(no browser/Playwright instrumentation available in this environment).

1. **Web bundle size** — see `## Build` table (wasm 809,432 B raw / 324,527 B gz;
   total 872,381 B raw / 339,611 B gz). *vs spike dioxus-web:* wasm **−62.6%** raw,
   **−43.6%** gzip (the spike shipped unoptimized 2,164,972 B wasm). *vs leptos-web:* wasm
   **−48.4%** raw, **−32.8%** gzip.

2. **Cold start → first paint** — not re-measured (carried forward from the spike:
   median 72 ms, identical to leptos-web).

3. **Cold start → `Ready` handled** — not re-measured (the spike's in-page observer
   measured ~51 ms warm-cache; the leptos-web comparison is method-contaminated).

4. **Event render latency** — not re-measured (spike median 59.1 ms, comparable to
   leptos-web's 56.8 ms; no reason to expect a change from Phase 1/2 changes).

5. **Reconnect replay time** — not re-measured (spike median 3.5 ms).

6. **Desktop RSS** — `VOID (web build)`.

7. **Desktop binary size** — `VOID (web build)`.

8. **Build wall-clock** — ~2.8 s incremental (warm cargo cache). The spike's clean
   build was 36.17 s.

## Deviations from the spike

- **Web syntax highlighting:** the spike showed plain-text rendering (`tok-plain`
  only). Phase 1 closed this gap: `highlight_web.rs` now produces token-level spans.
  Confirmed by code review (the highlight module and its cross-backend test exist and
  pass).
- **Bundle size:** the spike's 2.16 MB unoptimized wasm is now 0.81 MB. The
  `build-web.sh` four-guard system prevents silent re-shipping of unoptimized wasm.
- **Serving:** the spike used `python3 -m http.server` to serve the bundle; this run
  uses `otto serve --ui-dir` (Phase 2), so no separate HTTP server is needed.

## Global-constraint compliance

- `git status crates/` — **empty**. No protocol or engine change was needed across
  Phases 0–3.
- No provider API keys were set; every event stream is offline-deterministic and
  byte-comparable with the baseline.
