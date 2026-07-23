# Run log — `dioxus-web`

Second client run for the 2026-07-21 UI runtime spike. Driven per the frozen contract
(`docs/superpowers/specs/2026-07-21-ui-runtime-scenario.md`) against the server baseline
(`docs/superpowers/spikes/2026-07-21-ui-runtime/baseline/README.md`), asserting the same things
the reference run (`results/leptos-web.md`) asserted, the same way, against the same server.

**Headline:** one runtime bug found and fixed in `ui-dioxus/` — a compile-clean, unit-test-passing
port gap: URL-param autoconnect was never wired on the web target (the `parse_launch_params` helper
was ported byte-identical and host-tested, but its web-startup call site was not). Cause class:
**other** (missing integration wiring — not one of spike #1's three reactivity classes). After the
one-file fix, all 11 steps behave as the reference did; the two spike-#1 hot paths (reconnect
replay, promote/demote handover) are **clean** on Dioxus. No `crates/` change was needed.

## Build

```bash
cd ui-dioxus && cargo clean && dx build --release --features web --platform web
```

- **Wall-clock (clean release build, `cargo clean` first):** `36.17s` (`/usr/bin/time -v`: 467% CPU,
  max RSS 603 MB — a fully cold compile of the whole `ui-dioxus` web dependency graph: `dioxus 0.7`,
  `gloo-net`, `web-sys`, `wasm-bindgen`, `otto-protocol`; no `tree-sitter`/`kode-*` because those are
  `desktop`-only). Measured on the pre-fix source; the fix is +35 LoC and does not materially change
  the clean-build time.
- **Incremental rebuild after the fix (warm cache):** `6.05s`.
- **Artifact sizes** (fixed build, `target/dx/otto-ui-dioxus/release/web/public/assets/`):

  | File | Raw bytes | Gzip bytes |
  |---|---|---|
  | `otto-ui-dioxus_bg-…9e6fc2cee5dd39c7.wasm` | 2,164,972 | 575,375 |
  | `otto-ui-dioxus-…46eedf92c2292516.js` | 59,928 | 14,269 |
  | `style-…529fbae8e831ea.css` | 3,021 | 1,124 |

  (Pre-fix wasm was 2,145,179 B; the +35-LoC autoconnect wiring added ~19.8 KB raw / ~4.9 KB gzip.)
- **Toolchain:** `rustc 1.95.0 (59807616e 2026-04-14)`, `dx` (Dioxus CLI) `0.7.9 (3e43ffa)`,
  `dioxus 0.7`, target `wasm32-unknown-unknown`.
- **How the bundle is served (toolchain data point):** `dx build` emits hashed assets under
  `public/assets/` and an `index.html` that references them with **absolute** `/./assets/…` paths and
  a `<title>` that dx doubles to `otto (Dioxus)otto (Dioxus)`. A plain root-directory static server
  works with no base-path fixup: `python3 -m http.server 8082 --directory
  ui-dioxus/target/dx/otto-ui-dioxus/release/web/public` returns `HTTP/1.0 200 OK` for `/`, the js,
  and the wasm. `dx serve` was **not** required.

## Environment

- **Provider keys:** confirmed absent for this run — the server was launched with
  `env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GEMINI_API_KEY`. Every `ready` frame carried
  `capabilities.local_llm: false, remote_llm: false`; the status strip rendered
  `LLM: offline (deterministic)` throughout.
- **`OTTO_DB`:** `/tmp/otto-ui-spike/dioxus-web.db`
- **Port:** `8899` (`otto serve --root /tmp/otto-ui-spike/fixture --port 8899 --approve-edits
  --promote-loopback`); the Dioxus bundle served via `python3 -m http.server 8082`.
- Confirmed via server log: `otto serve listening on ws://127.0.0.1:8899/ws`.
- By the end of the run the single `otto serve` process (pid 304022) held **three** listeners —
  `8899` (original), `42847` (promoted loopback target), `38557` (demoted loopback target) — all
  `LoopbackTarget`-provisioned in-process engines within the same OS process (see step 11).

## Steps

| N | Status | Evidence | Notes |
|---|---|---|---|
| 1 | PASS (after fix) | **Pre-fix:** navigating to `…/?ws=ws://127.0.0.1:8899&token=spike-token&autoconnect=1` left the UI on `status-conn: "disconnected"` with the URL box still on the default `ws://127.0.0.1:8787` and token empty — autoconnect did **nothing** (Bug 1). **Post-fix:** same URL → status strip `connected` · `0399…` · `seq —`; URL box filled to `ws://127.0.0.1:8899`, token `spike-token`, `Disconnect` button shown, and the query string was scrubbed to `http://127.0.0.1:8082/` (token no longer in the address bar). A `Ready` frame with `local_llm:false, remote_llm:false` was handled. | Autoconnect required the fix (see `## Bugs`). Dioxus status text has no `status:` prefix and no `·` separators — see rendering divergence note below. |
| 2 | PASS | Same connected snapshot: capability strip `engine: local` · `LLM: offline (deterministic)` · `sandbox: on`; the `LLM` segment carries the `cap-degraded` class (offline/degraded rendered visibly, not blank/healthy). | — |
| 3 | PASS | Post-send `.log .row` DOM (11 rows, in order): `▸ Planner started` / `· planned 1 milestone(s)` / `▸ Planner finished` / `▸ ContextFinder started` / `▸ ContextFinder finished` / `▸ Coder started` / `▸ Coder finished` / `▸ Verifier started` / `✓ Verify cargo test passed` / `▸ Verifier finished` / `● TurnComplete ok`, status `seq 10`. Store: `sqlite3 dioxus-web.db "select session_id,count(*),count(distinct seq),min,max"` → `039911e0-…\|11\|11\|0\|10`; the 11 persisted `kind` rows match the baseline `AgentStarted→Log→AgentFinished→…→VerifyResult→…→TurnComplete` exactly. | Frame-for-frame identical to `leptos-web` and to the baseline. |
| 4 | NOT-VERIFIABLE (offline turn completes faster than interrupt round-trip) | Sent a fresh prompt then clicked `Abort`; the snapshot showed the full second 11-frame turn already rendered (`● TurnComplete ok`, 22 rows total) and status `disconnected`, `Connect` button re-shown. Console: 0 errors/0 warnings during teardown (only a favicon 404). | The turn finished before `Abort` landed (expected on this offline path). The resulting disconnect is the same **pre-existing server** behavior documented in the reference run: `crates/engine/src/serve.rs` unconditionally `break`s the connection loop on any `Abort`. Not a client defect — the Dioxus UI handled the disconnect gracefully (no crash, no wedge, clean idle return). |
| 5 | PASS | Before reconnect the store held `039911e0-…\|22\|22\|0\|21` (two turns, `count(*)==count(distinct seq)`, no dupes/gaps). Clicked `Connect` (reuses session + `last_seq=21`): reconnected to the **same** session `0399…`, `seq 21` (no reset to 0, no gap), rendered rows stayed **22** (the replayed tail appended **no** duplicate rows — `should_apply` dedup + generation guard). Console during teardown+reconnect: 0 errors/0 warnings. | **spike-#1 socket-teardown-race path — clean.** No duplicate frames, no gap, no panic. |
| 6 | PASS | Tree rendered `▾ src` → (`▾ util` → `mod.rs`), `lib.rs`, `Cargo.lock`, `Cargo.toml`, `README.md`. Clicking `util` collapsed it (`▸ util`, `mod.rs` gone); clicking again re-expanded (`mod.rs` back). A regex scan of `.file-tree` innerText for `.env` returned **false** in every snapshot. | `.env` filtered server-side by the sensitive-path floor before the `POST /workspace` listing — nothing to render or deny. Per-node local `expanded` signal toggles both ways. |
| 7 | PASS | Clicked `lib.rs`: `.editor-path` = `src/lib.rs`; `.editor-overlay` textarea `value` = the full real 16-line file (`pub mod util;` … `#[cfg(test)] mod tests { … assert_eq!(add(2,3),5); }`); 15 `.hl-line`s. Distinct highlight-span classes = `["tok-plain"]` (a single uniform class). | Plain-text (unhighlighted) render on web is the known, already-recorded gap (`tokens::plain_spans`; tree-sitter is desktop-only) — not a new bug. |
| 8 | PASS (buffer) — see divergence | Focused `.editor-overlay`, pressed `X`: buffer length 194→195, `value.includes('X')` true (edit reflected in the controlled buffer). `cat /tmp/otto-ui-spike/fixture/src/lib.rs` unchanged and `grep -c 'X'` = 0 — the edit is local-buffer-only, never persisted. | **Rendering divergence:** the Dioxus web editor has **no** visible unsaved/dirty marker (the `leptos-web` editor showed `src/lib.rs ●`). The `.editor-path` shows only the path; the minimal textarea+overlay editor tracks no dirty state. So the contract's "visibly marked unsaved/dirty" sub-clause is **not** met — a feature/rendering divergence from the reference, not a runtime bug (the local-unsaved-buffer behavior itself is correct and provable). |
| 9 | NOT-APPLICABLE | Per the frozen contract and the Task 2 baseline: the offline-deterministic Coder proposes zero edits against this fixture (`grep -c ApprovalRequest` = 0 in every baseline capture). Corroborated live: 6 real turns during this run, zero `ApprovalRequest` events, the approval panel never appeared. | Not attempted/faked, per contract. |
| 10 | PASS (pause/resume) / NOT-VERIFIABLE (meter) | Filled the prompt, then in **one** `browser_evaluate` clicked `Send` and `Pause` synchronously. The pause landed **before** the new turn's first frame: store `seq 22 = {"Log":{"message":"turn paused"}}` with **no** `AgentStarted` after it; button flipped to `Resume`. Clicking `Resume` produced `· turn resumed` (seq 23) then the full 11-frame turn to `● TurnComplete ok` (seq 34) — genuine pause-then-resume-to-completion. | **Meter half NOT-VERIFIABLE:** the offline path never emits `TokenCostMeter` (0 in every baseline capture; the orchestrator only emits when `meter.total() > 0`, structurally impossible with `LocalProvider`). The `.meter` span never appeared (`meterPresent:false` in every read). Same honest treatment as the reference. |
| 11 | PASS | Pre-promote: `engine: local`, url `ws://127.0.0.1:8899`, `seq 34`, session `0399…`, Promote enabled / Demote disabled. Clicked **Promote to remote** → `connected`, `engine: local→remote`, url `ws://127.0.0.1:8899→ws://127.0.0.1:42847`, session + `seq 34` **preserved**, event log intact (35 rows, no dup), Demote enabled. Ran a turn on the promoted engine: `seq 34→45` (exactly 11 new frames, continuous). Clicked **Demote to local** → `engine: remote→local`, url `→ws://127.0.0.1:38557`, `seq 45` (no reset), log intact (46 rows), Promote re-enabled. `ss -tlnp`: ports `8899`, `42847`, `38557` all held by the **same** pid 304022 (source never killed). Console across the whole handover: 0 errors/0 warnings. | **spike-#1 dead-handover-reconnect + Promoted-token path — clean.** The client auto-reconnected to the handed-back endpoint each time (token from the handover frame applied via `build_ws_url`), `seq` continued monotonically, engine/LLM/sandbox strip returned to its prior rendered state. The demoted-to port (`38557`) is a *new* ephemeral loopback listener, not literally `8899` — by design (each `LoopbackTarget::provision` opens a fresh store/engine), same as the reference. |

## Measurements

All in-turn timings captured via an in-page `performance.now()` instrument read through a **single**
`browser_evaluate` round trip (click + poll + return in one JS call) so tool-call latency never
contaminates the reading. Cold-start timings use an **in-page observer** injected into the served
`index.html` (a gitignored build artifact, not a `ui-dioxus/` source change), which records the true
navigation→connected delta at parse time — because on this build the connect completes **before** a
post-navigate `browser_evaluate` can even start polling (see measure 3).

1. **Web bundle size** — see `## Build` table (wasm 2,164,972 B / 575,375 B gz; js 59,928 B /
   14,269 B gz; css 3,021 B / 1,124 B gz). *vs `leptos-web`:* wasm 1,568,220 / 483,131 gz — Dioxus is
   **~38% larger raw wasm** (~19% larger gzipped).

2. **Cold start → first paint** (`performance.getEntriesByName('first-contentful-paint')`, 3 fresh
   loads): 72 ms / 72 ms / 80 ms → **median 72 ms** (min 72, max 80). *vs `leptos-web` median 72 ms —
   identical* (both paint the shell fast).

3. **Cold start → `Ready` handled** (in-page observer, navigation→`.status-conn === 'connected'`, 3
   fresh loads, **warm wasm cache**): 56.6 ms / 51.1 ms / 50.9 ms → **median 51.1 ms** (min 50.9, max
   56.6).
   - *Method caveat / comparison:* the reference's `2584 ms` was measured by polling **after**
     navigate. Reproducing that method on Dioxus returned `4472 ms` / `5116 ms` (with
     `alreadyConnectedAtFirstPoll: true` — i.e. the connect had already finished before the poll
     began), so those figures are the **tool-round-trip floor**, not wasm-boot time. The in-page
     observer above is the contamination-free measure; it shows Dioxus reaches connected in ~51 ms
     warm-cache. The two clients' step-3 numbers are therefore **not** directly comparable (the
     reference number is method-contaminated); the honest Dioxus value is ~51 ms.

4. **Event render latency** (Send-click → `TurnComplete` rendered, single-call instrument, 3 fresh
   turns): 71.4 ms / 59.1 ms / 56.9 ms → **median 59.1 ms** (min 56.9, max 71.4). *vs `leptos-web`
   median 56.8 ms — comparable* (Dioxus marginally higher, same ballpark).

5. **Reconnect replay time** (Disconnect → Connect-click → `.status-conn` re-`connected`, session
   holding 33 persisted events, 3 reps): 4.0 ms / 3.3 ms / 3.5 ms → **median 3.5 ms** (min 3.3, max
   4.0). Rendered row count stayed **33** across every rep (replayed-but-seen events append no DOM
   rows — the expected "exactly once" outcome). *vs `leptos-web` median 4.9 ms — comparable* (slightly
   faster).

6. **Desktop RSS** — `VOID (web build, no desktop process)`.

7. **Desktop binary size** — `VOID (web build, no desktop artifact)`.

8. **Build wall-clock** — `36.17s` (clean release build; see `## Build`). *vs `leptos-web` 61.91s —
   Dioxus's clean web build is faster* (web-only feature set pulls no `tree-sitter`/`kode-leptos`).

### Rendering divergences from `leptos-web` (same server, same data)

- **Status strip formatting:** Dioxus renders the segments as adjacent spans with no `status:`
  prefix and no `·` separators — `"connected 0399… seq 10 engine: local LLM: offline (deterministic)
  sandbox: on"` — vs the reference's `"status: connected · 4a71… · seq 10 · …"`. Same data, same
  degraded-LLM visibility; only the separators/label chrome differ. Seq placeholder is `—` (em dash)
  vs the reference `-`.
- **Editor dirty marker (step 8):** the Dioxus web editor shows **no** unsaved/dirty indicator; the
  reference (kode-leptos) showed `src/lib.rs ●`. See step 8.
- Event-log row text, tree labels (`▸`/`▾`), and file-open content are **byte-identical** to the
  reference.

## Bugs

### Bug 1 — web URL-param autoconnect never wired

- **Failing step:** 1 (Connect via URL+token autoconnect).
- **Symptom:** navigating to `…/?ws=…&token=…&autoconnect=1` did nothing — the client stayed
  `disconnected`, the URL/token fields kept their defaults (`ws://127.0.0.1:8787`, empty), and no
  connection was attempted. The reference (`leptos-web`) autoconnects from the same URL with no manual
  fill.
- **Root cause:** `parse_launch_params` (URL query → `LaunchParams`) was ported into
  `ui-dioxus/src/net/url.rs` **byte-identical** to `ui/src/url.rs`, with all its host unit tests
  passing — but the **web-startup call site was never ported.** `ui/src/app.rs` has a mount `Effect`
  that reads `web_sys::window().location().search()`, calls `parse_launch_params`, sets url/token, and
  connects; `ui-dioxus/src/app.rs` had **only** the `#[cfg(feature = "desktop")]` `boot()` auto-connect
  path — nothing read the browser query string on the web target. So the ported (and tested) parser
  was **dead code** on web.
- **Cause class:** **other** — a port gap / missing integration wiring. It is **not** one of spike
  #1's three reactivity classes (tracked-read-to-subscribe / positional-hooks / generation-guard
  teardown); the ported function and the reactivity spine are both correct. The defect is the
  *absence* of a call site.
- **Could a compiler or test plausibly have caught it?** **No** to the compiler (both the parser and
  the app compile cleanly; nothing references the missing wiring, so there is no type error or unused
  warning that points at it). **No** to the existing tests — the host unit tests exercise
  `parse_launch_params` in isolation and all pass; the bug is the missing *use* of that function at
  web startup, which only a runtime/wasm integration test (navigate with autoconnect params, assert a
  connection) could catch. This is precisely the compile-clean, unit-test-passing failure mode spike
  #1 flagged as an unquantified risk — it just manifested as a missing feature-wiring rather than a
  reactivity miswrite.
- **Fix:** `ui-dioxus/src/app.rs` — added a `#[cfg(feature = "web")]` `use_future` (run-once-on-mount,
  the same primitive the desktop `boot()` block already relies on) that reads
  `web_sys::window().location().search()`, and on a valid `parse_launch_params` result sets url/token,
  calls the existing hardened `do_connect`, and scrubs the token out of the visible URL via
  `history.replaceState` — mirroring the Leptos web-startup Effect. `web-sys`'s `Window`/`Location`/
  `History` features were already enabled in `Cargo.toml`; no dependency or `crates/` change. +35 LoC,
  one file. Host tests still 50/50.
- **Fix commit:** `spike(ui-runtime): record dioxus-web scenario run` (this commit — the run log and
  the fix are committed together).
- **Fix wall-clock:** ~5 min end-to-end (code read to confirm the gap → 35-LoC edit → `cargo check`
  wasm 28 s → incremental `dx build --release` 6 s → re-run step 1, which then passed).

No other runtime bugs were found. The two paths spike #1 warned about — reconnect replay (step 5) and
promote/demote handover (step 11) — ran **clean** on Dioxus (no duplicate/gapped frames, no
dead-handover, the `Promoted`/`Demoted` token was correctly applied on auto-reconnect, no console
panics anywhere in the run). This is the counter-evidence to spike #1's "unquantified further-bugs
risk": exercising the client at runtime surfaced exactly **one** additional compile-clean defect, and
it was a missing-wiring port gap (class *other*), not a further reactivity miswrite, fixable entirely
within `ui-dioxus/`.
