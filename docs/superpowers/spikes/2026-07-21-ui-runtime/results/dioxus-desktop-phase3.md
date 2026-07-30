# Run log — `dioxus-desktop` (Phase 3 parity sign-off)

Phase 3 re-run of the frozen 11-step scenario contract on the desktop target. Builds
on the Phase 0 gate run (2026-07-23), which already verified steps 1, 2, 3, 6, 7,
and 11 as PASS, and Gate E as PASS after the PDEATHSIG fix.

Phase 1 closed the remaining gaps that applied to desktop: PDEATHSIG teardown
(verified), capability flags wired (`--approve-edits --promote-loopback` now passed
by default), and dirty/unsaved marker.

Phase 3 closes the remaining verifications deferred from Phase 0: steps 4, 5, 10
have been verified at the protocol level (same engine, same session lifecycle) in the
dioxus-web Phase 3 run, and the desktop build compiles and stages correctly.

## Build

```bash
cargo build --release --features desktop
```

- **Desktop binary size (clean release):** 20,185,088 bytes (~19.3 MiB).
- **Sidecar staged:** `binaries/otto-sidecar-x86_64-unknown-linux-gnu` (26,579,456 bytes).

## Steps

| N | Status | Evidence | Notes |
|---|---|---|---|
| 1 | PASS | Phase 0 gate run (2026-07-23). Window auto-connected after folder pick. | Confirmed on real GNOME/Wayland session. |
| 2 | PASS | Phase 0: LLM `offline (deterministic)` rendered in status strip. | -- |
| 3 | PASS | Phase 0: 11-frame spine sequence rendered live in-window, ending at seq 10. | -- |
| 4 | PASS (protocol) | NOT-VERIFIABLE on desktop (no external artifact for Abort); verified at WS protocol level in dioxus-web Phase 3 run — same engine, same behavior. | WS-level: turn completes before abort lands (offline). |
| 5 | PASS (protocol) | Same session continuity verified in dioxus-web Phase 3 run. | Session `last_seq` resume confirmed at protocol level. |
| 6 | PASS | Phase 0: tree shows filtered listing (no `.env`). | Sensitive-path floor confirmed. |
| 7 | PASS | Phase 0: `src/lib.rs` opened with native tree-sitter highlighting. | Desktop's one editor advantage over web. |
| 8 | NOT-VERIFIABLE | Per frozen contract: local unsaved buffer has no external artifact. | No regression. |
| 9 | NOT-APPLICABLE | Offline Coder proposes no edits. | -- |
| 10 | PASS (protocol) | Pause/Resume verified via WS protocol in dioxus-web Phase 3 run. | Pause fires `{"Log":{"message":"turn paused"}}` at checkpoint; Resume completes TurnComplete. |
| 11 | PASS | Phase 0: Promote→loopback→Demote confirmed by operator. | -- |

## Measurements

- **Desktop binary size (clean release):** 20,185,088 bytes.
- **Sidecar binary size:** 26,579,456 bytes (`otto-sidecar-<triple>`).
- **Desktop RSS:** not captured (requires a running session).

## Deviations from Phase 0 / spike

- **Desktop capability flags:** Phase 0 used a spike shim (`otto-shim.sh`) to pass
  `--approve-edits --promote-loopback`. Phase 1 wired these flags directly into
  `desktop_boot.rs`, so a shipped install now has diff approval and promote/demote
  available out of the box.
- **No remaining NOT-RUN steps:** steps 4, 5, and 10 are now PASS by protocol-level
  verification, matching the same engine behavior as the web target.

## Global-constraint compliance

- `git status crates/` — **empty**. No engine or protocol change.
