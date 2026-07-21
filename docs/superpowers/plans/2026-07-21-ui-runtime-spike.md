# Dioxus runtime-verification spike (#2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drive all four UI builds (Leptos web, Leptos/Tauri desktop, Dioxus web, Dioxus desktop) through one identical scenario contract against one shared engine configuration, and produce a narrative adopt-vs-keep verdict grounded in runtime evidence.

**Architecture:** This is an *investigation*, not a feature. The product code under test (`ui/`, `desktop/`, `ui-dioxus/`) is modified only to fix runtime bugs the scenario exposes. All spike tooling — fixture generator, headless protocol driver, sidecar shim, run logs — lives under one deletable directory, `docs/superpowers/spikes/2026-07-21-ui-runtime/`. Web builds are driven in-page with Playwright; desktop builds are driven under a virtual X display (Xvfb) with synthetic input (xdotool) and asserted on observable effects (process tree, sqlite store, files on disk).

**Tech Stack:** Rust (workspace + three excluded UI crates), `trunk` (Leptos web), `dx` / Dioxus 0.7 (Dioxus web+desktop), `cargo tauri` (Leptos desktop), Node + `ws` (headless protocol driver), Playwright (web driving), Xvfb + xdotool + ImageMagick (desktop driving), sqlite (assertion source).

**Spec:** [`docs/superpowers/specs/2026-07-21-ui-dioxus-runtime-spike-design.md`](../specs/2026-07-21-ui-dioxus-runtime-spike-design.md)

## Global Constraints

- **No engine or protocol changes.** A bug fixable *only* by changing `crates/protocol` or `crates/engine` **stops the spike** and is escalated to the user — it would overturn spike #1's headline finding. Verify with `git status crates/` before every commit: it must be empty.
- **No workspace disturbance.** `ui/`, `desktop/`, `ui-dioxus/` are excluded from the cargo workspace. `cargo build --workspace` and `cargo test --workspace` must stay byte-for-byte unaffected. Never add the spike dir or the UI crates to the root `Cargo.toml`.
- **Engine config is identical across all four runs:** `otto serve --root <fixture> --port <p> --approve-edits --promote-loopback`, `OTTO_TOKEN` fixed, `OTTO_DB` pointed at a per-run fresh file.
- **LLM is offline-deterministic.** No `OTTO_*` provider vars, no `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/`GEMINI_API_KEY` in any run environment. Both router slots must be `LocalProvider`. Confirm per run; a leaked key voids the run's event-latency measurement.
- **All measurements on `--release` builds**, same machine, no competing load, **3 repetitions, median reported with spread**.
- **Bug policy:** fix and log. Every runtime bug records failing step, cause class, whether a compiler/test could plausibly have caught it, and fix wall-clock. Bugs in `ui/`/`desktop/` are logged and fixed on exactly the same terms as bugs in `ui-dioxus/`.
- **Never claim a step passed without its assertion output pasted into the run log.** A step that could not be driven is `NOT-VERIFIABLE` with a reason — never silently `PASS`.
- **No self-attribution in any commit message** (no `Co-Authored-By: Claude`, no "Generated with Claude Code", no 🤖).
- **Spike root:** `docs/superpowers/spikes/2026-07-21-ui-runtime/` (referred to below as `$SPIKE`). Chosen because the whole spike — tooling, logs, report inputs — must be deletable in one `rm -rf` under the "keep" disposition.

## File Structure

| Path | Responsibility |
|---|---|
| `$SPIKE/fixture.sh` | Creates the disposable workspace fixture git repo. Idempotent, takes target dir. |
| `$SPIKE/driver.mjs` | Headless Node protocol driver: connects to `/ws`, runs commands, dumps the raw event stream to JSON. Ground truth for all client runs. |
| `$SPIKE/otto-shim.sh` | Sidecar wrapper: execs the real `otto` with the app's own args plus `--approve-edits --promote-loopback`. |
| `$SPIKE/baseline/` | Captured server-side event streams (JSON) + the derived expected sequences. |
| `$SPIKE/results/<build>.md` | One run log per build, all four sharing an identical schema (defined in Task 3). |
| `$SPIKE/results/shots/` | Desktop screenshots, named `<build>-step<N>.png`. |
| `docs/superpowers/specs/2026-07-21-ui-runtime-scenario.md` | The scenario contract. Written before any run; frozen once runs start. |
| `docs/superpowers/specs/2026-07-21-ui-dioxus-runtime-spike-report.md` | The deliverable. |

**Build identifiers** used consistently in every filename, table column, and log: `leptos-web`, `leptos-desktop`, `dioxus-web`, `dioxus-desktop`.

---

### Task 1: Workspace fixture generator

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh`

**Interfaces:**
- Produces: `fixture.sh <target-dir>` — creates a fresh git repo at `<target-dir>` (deleting it first if present) containing a minimal Rust crate with a passing test, a nested source directory, and a `.env`. Exit 0 on success. Every later task calls it to get a clean workspace.

- [ ] **Step 1: Write the fixture script**

Create `docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh`:

```bash
#!/usr/bin/env bash
# Creates the disposable workspace fixture for the 2026-07-21 UI runtime spike.
# Usage: fixture.sh <target-dir>
set -euo pipefail

target="${1:?usage: fixture.sh <target-dir>}"
rm -rf "$target"
mkdir -p "$target/src/util"

cat > "$target/Cargo.toml" <<'EOF'
[package]
name = "otto-spike-fixture"
version = "0.1.0"
edition = "2021"

[dependencies]
EOF

cat > "$target/src/lib.rs" <<'EOF'
pub mod util;

pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_sums_two_numbers() {
        assert_eq!(add(2, 3), 5);
    }
}
EOF

cat > "$target/src/util/mod.rs" <<'EOF'
/// Doubles a value. Lives in a nested directory so the workspace tree has
/// something to expand and collapse.
pub fn double(n: i64) -> i64 {
    n * 2
}
EOF

# Sensitive-path floor probe: must be listed by the tree but never openable.
cat > "$target/.env" <<'EOF'
FAKE_SECRET=not-a-real-secret
EOF

cat > "$target/README.md" <<'EOF'
# otto spike fixture

Disposable workspace for the 2026-07-21 UI runtime spike. Not a real project.
EOF

cd "$target"
git init -q
git add -A
git -c user.email=spike@example.com -c user.name=spike commit -q -m "fixture"
echo "fixture ready: $target"
```

- [ ] **Step 2: Make it executable and run it**

```bash
cd /home/robhicks/dev/otto-next
chmod +x docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/fixture
```

Expected: `fixture ready: /tmp/otto-ui-spike/fixture`

- [ ] **Step 3: Verify the fixture's own test command passes**

The Verifier will run this; if it fails, every turn ends in Repair and the event streams get noisy.

```bash
cd /tmp/otto-ui-spike/fixture && cargo test 2>&1 | tail -5
```

Expected: `test result: ok. 1 passed; 0 failed`

- [ ] **Step 4: Verify idempotence**

```bash
cd /home/robhicks/dev/otto-next
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/fixture && ls -a /tmp/otto-ui-spike/fixture
```

Expected: succeeds again; listing shows `.env`, `.git`, `Cargo.toml`, `README.md`, `src`.

- [ ] **Step 5: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh
git commit -m "spike(ui-runtime): add disposable workspace fixture generator"
```

---

### Task 2: Headless protocol driver + baseline event capture

Establishes server-side ground truth **before** the contract is written, and answers the open question of whether the offline-deterministic path actually emits an `ApprovalRequest` (step 9 only exists if it does).

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/driver.mjs`
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/baseline/README.md`
- Create (output): `docs/superpowers/spikes/2026-07-21-ui-runtime/baseline/*.json`

**Interfaces:**
- Consumes: `fixture.sh` from Task 1.
- Produces: `node driver.mjs --url <ws-url> --token <tok> --script <name>` writing `{events:[...], meta:{...}}` JSON to stdout. Script names: `turn`, `abort`, `reconnect`, `approve`, `promote`. Later tasks read `baseline/turn.json` etc. to know the exact expected event sequence.

- [ ] **Step 1: Confirm Node and the `ws` package are available**

```bash
node --version && npm ls -g ws 2>/dev/null | head -3
```

Expected: a Node version. If `ws` is absent globally, install it locally in the spike dir:

```bash
cd docs/superpowers/spikes/2026-07-21-ui-runtime && npm init -y >/dev/null && npm i ws --no-fund --no-audit
```

Then add `node_modules/`, `package-lock.json`, and `package.json` to a `$SPIKE/.gitignore` containing exactly:

```
node_modules/
package.json
package-lock.json
baseline/*.json
results/shots/
```

- [ ] **Step 2: Write the driver**

Create `docs/superpowers/spikes/2026-07-21-ui-runtime/driver.mjs`:

```javascript
// Headless otto protocol driver for the 2026-07-21 UI runtime spike.
// Ground truth for what the server emits, independent of any UI client.
//
// Wire framing (see crates/protocol): ServerMessage is INTERNALLY tagged
// ({"type":"ready"|"event"|...}); Command is EXTERNALLY tagged ({"SendPrompt":{...}}).
import WebSocket from 'ws';

const arg = (name, def) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? def : process.argv[i + 1];
};

const url = arg('url', 'ws://127.0.0.1:8899');
const token = arg('token', 'spike-token');
const script = arg('script', 'turn');
const PROMPT = 'Add a doc comment to the add function in src/lib.rs';

const events = [];
const meta = { script, startedAt: Date.now() };

function connect(extra = '') {
  return new WebSocket(`${url}/ws?token=${encodeURIComponent(token)}${extra}`);
}

function send(ws, cmd) {
  ws.send(JSON.stringify(cmd));
}

const done = (code) => {
  meta.finishedAt = Date.now();
  process.stdout.write(JSON.stringify({ events, meta }, null, 2));
  process.exit(code);
};

const ws = connect();
let ready = null;

ws.on('message', (raw) => {
  const msg = JSON.parse(raw.toString());
  events.push({ at: Date.now(), msg });

  if (msg.type === 'ready') {
    ready = msg;
    meta.ready = msg;
    if (script === 'turn' || script === 'approve') send(ws, { SendPrompt: { text: PROMPT } });
    if (script === 'abort') send(ws, { SendPrompt: { text: PROMPT } });
    if (script === 'promote') send(ws, { SendPrompt: { text: PROMPT } });
    return;
  }

  if (msg.type === 'event') {
    const kind = Object.keys(msg.event?.kind ?? {})[0] ?? msg.event?.kind;
    meta.lastSeq = msg.event?.seq ?? meta.lastSeq;

    if (script === 'abort' && events.length === 4) send(ws, { Abort: {} });

    if (script === 'approve' && String(kind).includes('ApprovalRequest')) {
      const id = msg.event.kind.ApprovalRequest?.id;
      meta.approvalId = id;
      // Approve the first request, reject any second one.
      const approved = meta.approvedOnce ?? false;
      meta.approvedOnce = true;
      send(ws, { ApproveDiff: { id, approved: !approved } });
    }

    if (script === 'promote' && String(kind).includes('TurnComplete') && !meta.promoted) {
      meta.promoted = true;
      send(ws, { PromoteToRemote: {} });
    }

    if (String(kind).includes('TurnComplete') && script === 'turn') done(0);
  }

  if (msg.type === 'promoted') {
    meta.promoted_frame = msg;
    done(0);
  }
});

ws.on('error', (e) => { meta.error = String(e); done(1); });
setTimeout(() => { meta.timeout = true; done(0); }, Number(arg('timeout', '120000')));
```

- [ ] **Step 3: Start a server against the fixture**

```bash
cd /home/robhicks/dev/otto-next
cargo build --release -p otto-engine
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/fixture
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GEMINI_API_KEY \
  OTTO_TOKEN=spike-token OTTO_DB=/tmp/otto-ui-spike/baseline.db \
  ./target/release/otto serve --root /tmp/otto-ui-spike/fixture --port 8899 \
  --approve-edits --promote-loopback 2>/tmp/otto-ui-spike/serve.log &
sleep 2 && grep -c "otto serve listening" /tmp/otto-ui-spike/serve.log
```

Expected: `1`

- [ ] **Step 4: Capture the plain-turn baseline**

```bash
cd docs/superpowers/spikes/2026-07-21-ui-runtime
mkdir -p baseline
node driver.mjs --script turn --url ws://127.0.0.1:8899 --token spike-token > baseline/turn.json
node -e "const d=require('./baseline/turn.json');console.log('events:',d.events.length);console.log([...new Set(d.events.filter(e=>e.msg.type==='event').map(e=>Object.keys(e.msg.event.kind)[0]??e.msg.event.kind))].join(', '))"
```

Expected: a non-zero event count and a list of `EventKind` variant names. **Record both** — they are the expected sequence every client must reproduce.

- [ ] **Step 5: Capture approve / abort / promote baselines**

```bash
node driver.mjs --script approve  --url ws://127.0.0.1:8899 --token spike-token > baseline/approve.json
node driver.mjs --script abort    --url ws://127.0.0.1:8899 --token spike-token > baseline/abort.json
node driver.mjs --script promote  --url ws://127.0.0.1:8899 --token spike-token > baseline/promote.json
grep -c ApprovalRequest baseline/approve.json
```

**Decision gate — the answer determines whether step 9 exists at all:**
- If `grep -c ApprovalRequest` > 0: the offline path proposes edits. Step 9 stays in the contract as written.
- If it is 0: the offline-deterministic Coder proposes no edits, so no `ApprovalRequest` can ever fire. **Do not fake it.** Adjust the fixture/prompt once (e.g. a prompt naming an obviously-missing function) and re-run. If it is still 0, step 9 becomes `NOT-APPLICABLE (offline Coder proposes no edits)` for all four builds, recorded in the contract and the report, and the diff-approval dimension is reported as untested rather than passed.

- [ ] **Step 6: Verify the promote baseline actually handed over**

```bash
node -e "const d=require('./baseline/promote.json');console.log('promoted frame:',!!d.meta.promoted_frame,'lastSeq:',d.meta.lastSeq)"
```

Expected: `promoted frame: true`. If false, `--promote-loopback` did not engage; investigate before writing the contract, since step 11 depends on it.

- [ ] **Step 7: Write the baseline README recording what was captured**

Create `baseline/README.md` documenting, in prose: the exact server command line used, the event-variant sequence observed for each script, the `ApprovalRequest` decision-gate outcome from Step 5, and the promote outcome from Step 6.

- [ ] **Step 8: Stop the server and commit**

```bash
pkill -f "otto serve --root /tmp/otto-ui-spike/fixture" || true
cd /home/robhicks/dev/otto-next
git status crates/   # MUST be empty — no engine changes
git add docs/superpowers/spikes/2026-07-21-ui-runtime/driver.mjs \
        docs/superpowers/spikes/2026-07-21-ui-runtime/.gitignore \
        docs/superpowers/spikes/2026-07-21-ui-runtime/baseline/README.md
git commit -m "spike(ui-runtime): add headless protocol driver and capture server baseline"
```

---

### Task 3: The scenario contract

**Files:**
- Create: `docs/superpowers/specs/2026-07-21-ui-runtime-scenario.md`

**Interfaces:**
- Consumes: the observed event sequences from `$SPIKE/baseline/README.md` (Task 2).
- Produces: the frozen 11-step contract and the **run-log schema** every run task (Tasks 7–10) must follow.

- [ ] **Step 1: Write the contract document**

Create `docs/superpowers/specs/2026-07-21-ui-runtime-scenario.md` containing, in order:

1. A header noting: written before any client run; **frozen once Task 7 starts**; any later change must be recorded as a dated amendment with a reason.
2. The shared engine configuration verbatim from the spec's §Shared engine configuration.
3. The 11-step table from the spec's §The scenario contract, with each step's pass assertion **rewritten to name the concrete observed event variants from Task 2's baseline** instead of generic descriptions (e.g. step 3's assertion names the actual first and last event variants of a turn).
4. For each step, a **How driven (web)** line and a **How asserted (desktop)** line. Desktop assertions must be observable outside the window: process table, `OTTO_DB` sqlite rows, files on disk, exit behavior. Any step with no external effect is declared `NOT-VERIFIABLE (desktop)` **here, in advance** — not discovered later.
5. The step-9 status from Task 2's decision gate.
6. **The run-log schema** — every `$SPIKE/results/<build>.md` has exactly these sections:
   - `## Build` — command, wall-clock, artifact sizes, versions
   - `## Environment` — confirmation that no provider keys were set, DB path, port
   - `## Steps` — a row per step: `N | PASS/FAIL/NOT-VERIFIABLE/NOT-APPLICABLE | evidence (pasted output) | notes`
   - `## Measurements` — the three repetitions and the median for each applicable measure
   - `## Bugs` — one entry per bug: failing step, symptom, cause class, could-a-compiler-have-caught-it, fix commit, fix wall-clock

- [ ] **Step 2: Cross-check the contract against the spec**

Re-read the spec's §The scenario contract and §Measurements. Confirm every one of the 11 steps and every one of the 8 measures appears in the contract. List any that do not and add them.

- [ ] **Step 3: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add docs/superpowers/specs/2026-07-21-ui-runtime-scenario.md
git commit -m "spike(ui-runtime): freeze the 11-step scenario contract and run-log schema"
```

---

### Task 4: Toolchain gate

Surfaces toolchain friction **before** any build or run, per the spec's risk mitigation.

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/results/toolchain.md`

**Interfaces:**
- Produces: `results/toolchain.md` recording each tool's presence and version, and the **desktop driving decision** (Xvfb-based, real-session-based, or degraded) that Tasks 9–10 depend on.

- [ ] **Step 1: Probe the web toolchain**

```bash
trunk --version; dx --version; rustup target list --installed | grep wasm32
```

Expected: versions for `trunk` and `dx`, and `wasm32-unknown-unknown` present. Install a missing wasm target with `rustup target add wasm32-unknown-unknown`.

- [ ] **Step 2: Probe the Tauri toolchain**

```bash
cargo tauri --version 2>&1 | head -2; pkg-config --modversion webkit2gtk-4.1 2>&1 | head -1
```

Expected: a Tauri CLI version and a WebKitGTK version. If `cargo tauri` is missing: `cargo install tauri-cli --version '^2'`.

- [ ] **Step 3: Probe the desktop driving toolchain**

```bash
which Xvfb xdotool import 2>&1; echo "---"; echo "$XDG_SESSION_TYPE"
```

Expected: paths for `Xvfb`, `xdotool`, and ImageMagick's `import`. If any are missing, install (`sudo dnf install -y xorg-x11-server-Xvfb xdotool ImageMagick`).

- [ ] **Step 4: Verify a virtual display actually hosts a WebKitGTK window**

```bash
Xvfb :99 -screen 0 1400x900x24 >/tmp/otto-ui-spike/xvfb.log 2>&1 &
sleep 2
DISPLAY=:99 xdotool getdisplaygeometry
```

Expected: `1400 900`

- [ ] **Step 5: Record the desktop driving decision**

Write `results/toolchain.md` with every version from Steps 1–4 and one explicit decision line:
- `DESKTOP DRIVING: xvfb` — Steps 3–4 succeeded. Tasks 9–10 run under `DISPLAY=:99`.
- `DESKTOP DRIVING: real-session` — Xvfb unusable but the live session can host the apps; input via `ydotool`/`xdotool` against the real display.
- `DESKTOP DRIVING: degraded` — no synthetic input possible. Tasks 9–10 verify **boot, auto-connect, and window-close-kill only**; steps 3–11 are `NOT-VERIFIABLE (no synthetic input)` for both desktop builds equally, and the report says so prominently.

- [ ] **Step 6: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add docs/superpowers/spikes/2026-07-21-ui-runtime/results/toolchain.md
git commit -m "spike(ui-runtime): record toolchain probe and desktop driving decision"
```

---

### Task 5: Build all four artifacts

Per the spec: all artifacts built before any scenario is driven.

**Files:**
- Modify: `docs/superpowers/spikes/2026-07-21-ui-runtime/results/toolchain.md` (append a `## Builds` section)

**Interfaces:**
- Consumes: the toolchain decision from Task 4.
- Produces: four built artifacts and their recorded wall-clock + byte sizes, consumed by Task 11's measurements table.

- [ ] **Step 1: Build Leptos web, timed**

```bash
cd /home/robhicks/dev/otto-next/ui
rm -rf dist && /usr/bin/time -f "%e s" trunk build --release 2>&1 | tail -3
ls -l dist/
for f in dist/*.wasm dist/*.js dist/*.css; do echo "$f raw=$(stat -c%s "$f") gz=$(gzip -c "$f" | wc -c)"; done
```

Record wall-clock and every raw/gzip size.

- [ ] **Step 2: Build Dioxus web, timed**

```bash
cd /home/robhicks/dev/otto-next/ui-dioxus
/usr/bin/time -f "%e s" dx build --release --features web 2>&1 | tail -5
find target/dx -name '*.wasm' -o -name '*.js' -o -name '*.css' | head -20
```

Record wall-clock, then compute raw/gzip sizes for the emitted wasm/js/css exactly as in Step 1.

- [ ] **Step 3: Build Leptos desktop (Tauri), timed**

```bash
cd /home/robhicks/dev/otto-next
./desktop/build-sidecar.sh
cd desktop && /usr/bin/time -f "%e s" cargo tauri build 2>&1 | tail -5
ls -l src-tauri/target/release/ | grep -i otto
```

Record wall-clock and the shipped binary size.

- [ ] **Step 4: Build Dioxus desktop, timed**

```bash
cd /home/robhicks/dev/otto-next/ui-dioxus
/usr/bin/time -f "%e s" dx build --release --features desktop 2>&1 | tail -5
find target -name 'otto-ui-dioxus' -type f -newermt '-10 minutes' | head -3 | xargs ls -l
```

Record wall-clock and the shipped binary size.

- [ ] **Step 5: Append the builds section and commit**

Append a `## Builds` section to `results/toolchain.md` with a row per build: command, wall-clock, artifact paths, raw and gzip sizes. Note any build that required a fix to succeed — **that is a runtime-adjacent finding and belongs in the report**.

```bash
cd /home/robhicks/dev/otto-next
git status ui/ desktop/ ui-dioxus/ crates/   # note anything unexpectedly modified
git add docs/superpowers/spikes/2026-07-21-ui-runtime/results/toolchain.md
git commit -m "spike(ui-runtime): build all four artifacts and record sizes and wall-clock"
```

---

### Task 6: Desktop sidecar flag shim

Both desktop apps hardcode `otto serve --root <picked> --port 8787` with no `--approve-edits`/`--promote-loopback` (`ui-dioxus/src/desktop_boot.rs:70-77`, `desktop/src-tauri/src/lib.rs:45-53`), so steps 9 and 11 are unreachable on desktop without this shim. **No app code is changed.**

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/otto-shim.sh`

**Interfaces:**
- Produces: `otto-shim.sh` — accepts the app's own argv, appends `--approve-edits --promote-loopback`, execs the real release binary. Used by Task 9 (staged as the Tauri sidecar file) and Task 10 (via `OTTO_BIN`).

- [ ] **Step 1: Write the shim**

Create `docs/superpowers/spikes/2026-07-21-ui-runtime/otto-shim.sh`:

```bash
#!/usr/bin/env bash
# Sidecar wrapper for the 2026-07-21 UI runtime spike. Both desktop apps spawn
# `otto serve --root <picked> --port 8787` with no capability flags; this appends the
# two the scenario needs so desktop can reach steps 9 and 11. No app code is changed.
set -euo pipefail
real="${OTTO_REAL_BIN:-/home/robhicks/dev/otto-next/target/release/otto}"
echo "otto-shim: exec $real $* --approve-edits --promote-loopback" >&2
exec "$real" "$@" --approve-edits --promote-loopback
```

- [ ] **Step 2: Verify the shim forwards flags and still prints the readiness line**

The readiness line matters: both apps wait for `otto serve listening on` on stderr before connecting (`ui-dioxus/src/desktop_boot.rs:132`).

```bash
cd /home/robhicks/dev/otto-next
chmod +x docs/superpowers/spikes/2026-07-21-ui-runtime/otto-shim.sh
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/shimtest
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GEMINI_API_KEY \
  OTTO_TOKEN=spike-token OTTO_DB=/tmp/otto-ui-spike/shim.db \
  docs/superpowers/spikes/2026-07-21-ui-runtime/otto-shim.sh serve \
  --root /tmp/otto-ui-spike/shimtest --port 8898 2>/tmp/otto-ui-spike/shim.log &
sleep 3
grep "otto-shim: exec" /tmp/otto-ui-spike/shim.log
grep -c "otto serve listening" /tmp/otto-ui-spike/shim.log
```

Expected: the exec line shows both appended flags, and the readiness grep prints `1`.

- [ ] **Step 3: Verify the shimmed server actually accepts a promote**

```bash
cd docs/superpowers/spikes/2026-07-21-ui-runtime
node driver.mjs --script promote --url ws://127.0.0.1:8898 --token spike-token \
  | node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{const d=JSON.parse(s);console.log('promoted frame:',!!d.meta.promoted_frame)})"
pkill -f "port 8898" || true
```

Expected: `promoted frame: true`

**If Step 2 or 3 fails**, the shim is unusable: record it, and Tasks 9–10 degrade to steps 1–8 and 10 on **both** desktop builds equally (never on just one).

- [ ] **Step 4: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add docs/superpowers/spikes/2026-07-21-ui-runtime/otto-shim.sh
git commit -m "spike(ui-runtime): add desktop sidecar flag shim"
```

---

### Task 7: Run — `leptos-web`

The incumbent runs first: it defines what "working" looks like, so a Dioxus deviation later is measured against an observed reference rather than an assumption.

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/results/leptos-web.md`
- Modify (only if a bug is found): `ui/src/*.rs`

**Interfaces:**
- Consumes: the frozen contract (Task 3), the run-log schema (Task 3), baselines (Task 2).
- Produces: `results/leptos-web.md` in the schema — the reference run every later run is compared against.

- [ ] **Step 1: Start the shared engine configuration**

```bash
cd /home/robhicks/dev/otto-next
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/fixture
rm -f /tmp/otto-ui-spike/leptos-web.db
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GEMINI_API_KEY \
  OTTO_TOKEN=spike-token OTTO_DB=/tmp/otto-ui-spike/leptos-web.db \
  ./target/release/otto serve --root /tmp/otto-ui-spike/fixture --port 8899 \
  --approve-edits --promote-loopback 2>/tmp/otto-ui-spike/leptos-web-serve.log &
sleep 2 && tail -2 /tmp/otto-ui-spike/leptos-web-serve.log
```

Expected: the readiness line naming port 8899.

- [ ] **Step 2: Serve the built bundle**

```bash
cd /home/robhicks/dev/otto-next/ui && python3 -m http.server 8080 --directory dist >/tmp/otto-ui-spike/leptos-web-http.log 2>&1 &
sleep 1 && curl -sI http://127.0.0.1:8080/ | head -1
```

Expected: `HTTP/1.0 200 OK`

- [ ] **Step 3: Install the timing observer, then navigate**

Using Playwright: navigate to `http://127.0.0.1:8080/`, and immediately evaluate this instrumentation so the timing measures are captured without touching client code:

```javascript
() => {
  window.__spike = { marks: {} };
  const mark = (k) => { if (!window.__spike.marks[k]) window.__spike.marks[k] = performance.now(); };
  window.__spike.mark = mark;
  new MutationObserver(() => {
    if (document.body.innerText.match(/session/i)) mark('sessionVisible');
    const rows = document.querySelectorAll('[class*=event], li, tr').length;
    window.__spike.marks.lastRowAt = performance.now();
    window.__spike.rows = rows;
  }).observe(document.body, { childList: true, subtree: true, characterData: true });
  const fcp = performance.getEntriesByName('first-contentful-paint')[0];
  window.__spike.marks.fcp = fcp ? fcp.startTime : null;
  return 'installed';
}
```

- [ ] **Step 4: Execute contract steps 1–11**

Drive each step exactly as its contract **How driven (web)** line specifies. After **every** step, paste the actual assertion evidence into the run log — rendered text, DOM state, `sqlite3 /tmp/otto-ui-spike/leptos-web.db "select seq,kind from events order by seq desc limit 5"`, or file contents on disk — before moving to the next step. Mark `PASS`/`FAIL`/`NOT-VERIFIABLE` per step.

For step 5 (reconnect replay), assert **exactly once** delivery explicitly:

```bash
sqlite3 /tmp/otto-ui-spike/leptos-web.db "select count(*), count(distinct seq) from events;"
```

The two numbers must be equal, and the client's rendered event count must match.

- [ ] **Step 5: Collect the timing measurements, 3 repetitions**

For each of three fresh page loads, read the observer:

```javascript
() => ({ fcp: window.__spike.marks.fcp, sessionVisible: window.__spike.marks.sessionVisible, rows: window.__spike.rows })
```

Record all three values and the median for: first paint, `Ready` handled, event render latency (step 3's first-row → last-row delta), reconnect replay time (step 5).

- [ ] **Step 6: Log any bug, fix it, and record the cost**

If any step failed, fix it in `ui/` only, note the fix commit and wall-clock in the run log's `## Bugs` section, and re-run the affected step. If a fix requires touching `crates/`, **stop and escalate** per Global Constraints.

- [ ] **Step 7: Verify no workspace disturbance, then commit**

```bash
cd /home/robhicks/dev/otto-next
git status crates/   # MUST be empty
cargo test --workspace 2>&1 | tail -3   # MUST still pass
git add docs/superpowers/spikes/2026-07-21-ui-runtime/results/leptos-web.md ui/
git commit -m "spike(ui-runtime): record leptos-web scenario run"
pkill -f "port 8899"; pkill -f "http.server 8080"
```

---

### Task 8: Run — `dioxus-web`

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/results/dioxus-web.md`
- Modify (only if a bug is found): `ui-dioxus/src/*.rs`

**Interfaces:**
- Consumes: the frozen contract, the schema, and `results/leptos-web.md` as the observed reference.
- Produces: `results/dioxus-web.md` in the identical schema.

- [ ] **Step 1: Start the shared engine configuration**

Identical to Task 7 Step 1 except the DB and log names:

```bash
cd /home/robhicks/dev/otto-next
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/fixture
rm -f /tmp/otto-ui-spike/dioxus-web.db
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GEMINI_API_KEY \
  OTTO_TOKEN=spike-token OTTO_DB=/tmp/otto-ui-spike/dioxus-web.db \
  ./target/release/otto serve --root /tmp/otto-ui-spike/fixture --port 8899 \
  --approve-edits --promote-loopback 2>/tmp/otto-ui-spike/dioxus-web-serve.log &
sleep 2 && tail -2 /tmp/otto-ui-spike/dioxus-web-serve.log
```

- [ ] **Step 2: Serve the Dioxus bundle**

```bash
cd /home/robhicks/dev/otto-next/ui-dioxus
dxdist="$(dirname "$(find target/dx -name '*.wasm' | head -1)")"
echo "serving $dxdist"
python3 -m http.server 8081 --directory "$dxdist/.." >/tmp/otto-ui-spike/dioxus-web-http.log 2>&1 &
sleep 1 && curl -sI http://127.0.0.1:8081/ | head -1
```

Expected: `HTTP/1.0 200 OK`. If the emitted layout differs, use `dx serve --release --features web` instead and record the toolchain difference — *how the bundle is served is itself a toolchain data point*.

- [ ] **Step 3: Install the identical timing observer**

Evaluate the **exact same** instrumentation snippet from Task 7 Step 3. Do not adapt it to Dioxus's DOM; if it fails to find nodes, that difference is data — record it rather than special-casing one client's selectors.

- [ ] **Step 4: Execute contract steps 1–11**

Same discipline as Task 7 Step 4: paste real evidence per step; assert exactly-once replay for step 5 via the sqlite query against `dioxus-web.db`; mark each step.

Pay particular attention to the two steps where spike #1 found compile-clean bugs:
- **Step 5** (reconnect replay) — the socket-teardown race class.
- **Step 11** (promote handover) — the dead-handover-reconnect class, and the `Promoted` frame's token delivery.

- [ ] **Step 5: Collect the timing measurements, 3 repetitions**

Identical procedure and identical measures as Task 7 Step 5.

- [ ] **Step 6: Log any bug, fix it, and record the cost**

Fix in `ui-dioxus/` only. For each bug also record the **cause class** (tracked-read-to-subscribe / positional hooks / generation-guard teardown / other) and whether a compiler or test could plausibly have caught it — this is the evidence that quantifies spike #1's "unquantified further-bugs risk". Escalate any `crates/`-requiring fix.

- [ ] **Step 7: Verify and commit**

```bash
cd /home/robhicks/dev/otto-next
git status crates/   # MUST be empty
git add docs/superpowers/spikes/2026-07-21-ui-runtime/results/dioxus-web.md ui-dioxus/
git commit -m "spike(ui-runtime): record dioxus-web scenario run"
pkill -f "port 8899"; pkill -f "http.server 8081"
```

---

### Task 9: Run — `leptos-desktop` (Tauri)

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/results/leptos-desktop.md`
- Create (output): `docs/superpowers/spikes/2026-07-21-ui-runtime/results/shots/leptos-desktop-step*.png`
- Modify (only if a bug is found): `desktop/src-tauri/src/*.rs`

**Interfaces:**
- Consumes: the toolchain decision (Task 4), the shim (Task 6), the frozen contract.
- Produces: `results/leptos-desktop.md` in the schema, plus screenshots.

- [ ] **Step 1: Stage the shim as the Tauri sidecar**

Tauri resolves its sidecar by target-triple filename, so the shim replaces that file:

```bash
cd /home/robhicks/dev/otto-next
triple="$(rustc -vV | sed -n 's/^host: //p')"
cp target/release/otto /tmp/otto-ui-spike/otto-real
OTTO_REAL_BIN=/tmp/otto-ui-spike/otto-real
cp docs/superpowers/spikes/2026-07-21-ui-runtime/otto-shim.sh "desktop/src-tauri/binaries/otto-$triple"
chmod +x "desktop/src-tauri/binaries/otto-$triple"
ls -l "desktop/src-tauri/binaries/otto-$triple"
```

Note: the shim must be staged **before** `cargo tauri build` bundles it, so re-run the Task 5 Step 3 build after staging. Record whether Tauri accepted a shell script as an `externalBin` — **if it rejects it, that is the documented shim failure**, and this run degrades to steps 1–8 and 10 (and Task 10 must degrade identically, even if its `OTTO_BIN` shim would have worked, to keep the comparison fair).

- [ ] **Step 2: Launch under the virtual display**

```bash
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/fixture
rm -f /tmp/otto-ui-spike/leptos-desktop.db
export DISPLAY=:99
env OTTO_REAL_BIN=/tmp/otto-ui-spike/otto-real OTTO_DB=/tmp/otto-ui-spike/leptos-desktop.db \
    WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  ./desktop/src-tauri/target/release/otto-desktop >/tmp/otto-ui-spike/leptos-desktop.log 2>&1 &
sleep 5
DISPLAY=:99 xdotool search --name . getwindowname %@ 2>/dev/null | head -5
```

Expected: a window name for the app. If no window appears, capture `/tmp/otto-ui-spike/leptos-desktop.log` and record the failure — a desktop app that will not start under a virtual display is a real portability finding.

- [ ] **Step 3: Drive the folder picker**

```bash
DISPLAY=:99 import -window root docs/superpowers/spikes/2026-07-21-ui-runtime/results/shots/leptos-desktop-step1-picker.png
DISPLAY=:99 xdotool key --clearmodifiers ctrl+l
DISPLAY=:99 xdotool type --delay 40 "/tmp/otto-ui-spike/fixture"
DISPLAY=:99 xdotool key --clearmodifiers Return
sleep 6
pgrep -af "serve --root /tmp/otto-ui-spike/fixture" | head -3
```

Expected: the sidecar process appears, with `--approve-edits --promote-loopback` visible in its argv (proving the shim reached the real binary).

- [ ] **Step 4: Assert steps 1–2 (connect + status strip) by observable effect**

```bash
sqlite3 /tmp/otto-ui-spike/leptos-desktop.db "select count(*) from sessions;"
DISPLAY=:99 import -window root docs/superpowers/spikes/2026-07-21-ui-runtime/results/shots/leptos-desktop-step2-connected.png
```

Expected: at least one session row (the client connected and created one). The screenshot confirms the strip rendered; read it and record what it shows.

- [ ] **Step 5: Execute contract steps 3–11 with synthetic input**

For each step, use the contract's **How asserted (desktop)** line. Type into the app via `DISPLAY=:99 xdotool type`, click via `xdotool mousemove`/`click`, screenshot before and after, and assert on the external effect — new `events` rows, changed file contents in the fixture, a second engine appearing for promote. Any step the contract already declared `NOT-VERIFIABLE (desktop)` is recorded as such without attempting it.

- [ ] **Step 6: Assert window-close kills the sidecar**

This is the specific unresolved risk spike #1 named, and it must be tested on both desktop builds:

```bash
DISPLAY=:99 xdotool search --name . windowkill %@ 2>/dev/null || DISPLAY=:99 xdotool key --clearmodifiers alt+F4
sleep 3
pgrep -af "serve --root /tmp/otto-ui-spike/fixture" || echo "SIDECAR GONE (pass)"
```

Expected: `SIDECAR GONE (pass)`. A surviving sidecar is a `FAIL` on step 12-equivalent and a genuine product bug — log it.

- [ ] **Step 7: Measure desktop RSS (sidecar excluded)**

Re-launch, reach step 3, then:

```bash
app_pid=$(pgrep -f otto-desktop | head -1)
side_pid=$(pgrep -f "serve --root /tmp/otto-ui-spike/fixture" | head -1)
ps -o pid,rss,comm -p "$app_pid"; echo "--- sidecar (excluded) ---"; ps -o pid,rss,comm -p "$side_pid"
```

Record the app RSS only, three repetitions, median.

- [ ] **Step 8: Log bugs, fix, verify, commit**

```bash
cd /home/robhicks/dev/otto-next
git checkout -- "desktop/src-tauri/binaries/otto-$triple" 2>/dev/null || rm -f "desktop/src-tauri/binaries/otto-$triple"
git status crates/   # MUST be empty
git add docs/superpowers/spikes/2026-07-21-ui-runtime/results/leptos-desktop.md \
        docs/superpowers/spikes/2026-07-21-ui-runtime/results/shots/ desktop/
git commit -m "spike(ui-runtime): record leptos-desktop scenario run"
```

Note: `results/shots/` is gitignored per Task 2; if the screenshots are worth keeping as evidence, force-add the specific files with `git add -f` and say so in the run log.

---

### Task 10: Run — `dioxus-desktop`

The single most important run in the spike: this is Task 13 of spike #1, the compile-verified-only claim that a Dioxus crate replaces `desktop/` + Tauri.

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/results/dioxus-desktop.md`
- Create (output): `docs/superpowers/spikes/2026-07-21-ui-runtime/results/shots/dioxus-desktop-step*.png`
- Modify (only if a bug is found): `ui-dioxus/src/*.rs`

**Interfaces:**
- Consumes: the toolchain decision, the shim, the frozen contract, and `results/leptos-desktop.md` as the observed reference.
- Produces: `results/dioxus-desktop.md` in the identical schema.

- [ ] **Step 1: Launch under the virtual display with the shim**

The Dioxus app takes its sidecar path from `OTTO_BIN` (`ui-dioxus/src/desktop_boot.rs:70`), so no file staging is needed:

```bash
cd /home/robhicks/dev/otto-next
docs/superpowers/spikes/2026-07-21-ui-runtime/fixture.sh /tmp/otto-ui-spike/fixture
rm -f /tmp/otto-ui-spike/dioxus-desktop.db
app="$(find ui-dioxus/target -name 'otto-ui-dioxus' -type f -perm -u+x | head -1)"
echo "launching $app"
DISPLAY=:99 env OTTO_BIN=/home/robhicks/dev/otto-next/docs/superpowers/spikes/2026-07-21-ui-runtime/otto-shim.sh \
  OTTO_REAL_BIN=/tmp/otto-ui-spike/otto-real \
  OTTO_DB=/tmp/otto-ui-spike/dioxus-desktop.db WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  "$app" >/tmp/otto-ui-spike/dioxus-desktop.log 2>&1 &
sleep 5
DISPLAY=:99 xdotool search --name . getwindowname %@ 2>/dev/null | head -5
```

Expected: a window name. Note that the Dioxus app hardcodes port **8787**, same as Tauri — so only one desktop app may run at a time.

- [ ] **Step 2: Drive the folder picker and confirm the sidecar's argv**

```bash
DISPLAY=:99 import -window root docs/superpowers/spikes/2026-07-21-ui-runtime/results/shots/dioxus-desktop-step1-picker.png
DISPLAY=:99 xdotool key --clearmodifiers ctrl+l
DISPLAY=:99 xdotool type --delay 40 "/tmp/otto-ui-spike/fixture"
DISPLAY=:99 xdotool key --clearmodifiers Return
sleep 6
pgrep -af "serve --root /tmp/otto-ui-spike/fixture" | head -3
grep "otto-shim: exec" /tmp/otto-ui-spike/dioxus-desktop.log | head -2
```

Expected: the sidecar running with both appended flags. This single command also proves the app's readiness-line detection (`is_ready_line`) worked against a shimmed stderr stream.

- [ ] **Step 3: Assert steps 1–2 by observable effect**

```bash
sqlite3 /tmp/otto-ui-spike/dioxus-desktop.db "select count(*) from sessions;"
DISPLAY=:99 import -window root docs/superpowers/spikes/2026-07-21-ui-runtime/results/shots/dioxus-desktop-step2-connected.png
```

Expected: at least one session row. **If this is zero, the auto-connect that spike #1 could only compile-verify does not actually work** — that is the single highest-value finding available in this spike, positive or negative. Record it prominently either way.

- [ ] **Step 4: Execute contract steps 3–11 with synthetic input**

Same procedure as Task 9 Step 5, same assertions, same screenshots. Where the Dioxus app is driven differently than Tauri was, record the deviation explicitly in the run log — driver bias is a named risk in the spec.

- [ ] **Step 5: Assert window-close kills the sidecar**

The `kill_on_drop` claim in `desktop_boot.rs` is compile-verified only; this executes it.

```bash
DISPLAY=:99 xdotool search --name . windowkill %@ 2>/dev/null || DISPLAY=:99 xdotool key --clearmodifiers alt+F4
sleep 3
pgrep -af "serve --root /tmp/otto-ui-spike/fixture" || echo "SIDECAR GONE (pass)"
```

Expected: `SIDECAR GONE (pass)`. A surviving sidecar confirms the exact risk spike #1 flagged as unresolved — log it as such.

- [ ] **Step 6: Measure desktop RSS (sidecar excluded)**

```bash
app_pid=$(pgrep -f otto-ui-dioxus | head -1)
side_pid=$(pgrep -f "serve --root /tmp/otto-ui-spike/fixture" | head -1)
ps -o pid,rss,comm -p "$app_pid"; echo "--- sidecar (excluded) ---"; ps -o pid,rss,comm -p "$side_pid"
```

Three repetitions, median. Compare against Task 9 Step 7's number — this is the Tauri-vs-Dioxus shell overhead comparison.

- [ ] **Step 7: Log bugs, fix, verify, commit**

```bash
cd /home/robhicks/dev/otto-next
git status crates/   # MUST be empty
git add docs/superpowers/spikes/2026-07-21-ui-runtime/results/dioxus-desktop.md ui-dioxus/
git commit -m "spike(ui-runtime): record dioxus-desktop scenario run"
```

---

### Task 11: Consolidate the step matrix and measurements

**Files:**
- Create: `docs/superpowers/spikes/2026-07-21-ui-runtime/results/summary.md`

**Interfaces:**
- Consumes: all four `results/<build>.md` files and `results/toolchain.md`.
- Produces: `results/summary.md` containing the 11×4 step matrix and the consolidated measurements table — pasted directly into the report by Task 12.

- [ ] **Step 1: Build the step matrix**

Create the 11-row × 4-column table (`leptos-web`, `dioxus-web`, `leptos-desktop`, `dioxus-desktop`), each cell `PASS`/`FAIL`/`NOT-VERIFIABLE`/`NOT-APPLICABLE`. Every non-`PASS` cell carries a one-line reason.

- [ ] **Step 2: Build the measurements table**

One row per measure from the spec's §Measurements, one column per applicable build, each cell `median (min–max of 3)`. Mark inapplicable cells `n/a` (e.g. bundle size for desktop builds, RSS for web builds).

- [ ] **Step 3: Apply the two honesty constraints explicitly**

State in the summary, in prose:
1. Desktop RSS figures **exclude** the sidecar, and say so with the excluded numbers shown separately.
2. Whether the event counts matched between clients for the fixed turn. **If they did not, mark the event-latency row `VOID` and explain** — per the spec, the measure is only comparable under identical event counts.

- [ ] **Step 4: Consolidate the bug log**

Merge all four runs' `## Bugs` sections into one table: build, step, symptom, cause class, compiler-catchable?, fix wall-clock. Compute the per-client totals — bug count and total fix time — since those are the numbers that quantify spike #1's open risk.

- [ ] **Step 5: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add docs/superpowers/spikes/2026-07-21-ui-runtime/results/summary.md
git commit -m "spike(ui-runtime): consolidate step matrix, measurements, and bug log"
```

---

### Task 12: Write the report and the verdict

**Files:**
- Create: `docs/superpowers/specs/2026-07-21-ui-dioxus-runtime-spike-report.md`
- Modify: `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md` (add a forward cross-link)

**Interfaces:**
- Consumes: `results/summary.md`, all four run logs, `results/toolchain.md`.
- Produces: the spike deliverable, containing the verdict and the disposition instruction Task 13 executes.

- [ ] **Step 1: Write sections 1–4**

Per the spec's §Report structure: *What ran* (four builds, versions, machine, which steps executed and which were not-verifiable and why), *Step matrix* (from summary), *Runtime bug log* (from summary), *Measurements* (from summary).

- [ ] **Step 2: Write section 5, Findings — including the two mandatory re-checks**

Explicitly answer both spike-#1 claims:
1. **Did any protocol or engine change become necessary?** (spike #1: no.) Cite `git status crates/` being empty across every run task as the evidence.
2. **Does the Dioxus desktop app genuinely replace Tauri when actually launched?** (spike #1: compile-verified only.) Cite Task 10 Steps 2, 3 and 5 — sidecar argv, session row, sidecar-killed-on-close.

Also record the two findings this spike produced as a byproduct: the shipped desktop apps cannot reach approval/promote without a shim, and any build that needed a fix to compile in Task 5.

- [ ] **Step 3: Write section 6, the verdict**

Narrative form, per the user's choice. State plainly: `ADOPT`, `KEEP`, or `MIXED`, then the reasoning drawn from sections 2–5. Two discipline requirements:
- The verdict must cite specific rows from the step matrix and bug log. A verdict sentence with no evidence citation is a plan failure.
- If the evidence is genuinely mixed, say `MIXED` and name the one specific remaining unknown. Do not manufacture a decision, and do not hedge a clear result into a false `MIXED`.

- [ ] **Step 4: Write section 7, the disposition instruction**

Copy the matching branch from the spec's §Disposition — `ADOPT` → write a migration plan next; `KEEP` → delete `ui-dioxus/`; `MIXED` → name the bounded third probe or recommend dropping the question. This is the instruction Task 13 executes.

- [ ] **Step 5: Cross-link spike #1**

Append to the end of `docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md`:

```markdown
---

**Follow-up:** this spike's recommendation — a second, runtime-driven spike — was carried out on
2026-07-21. See [`2026-07-21-ui-dioxus-runtime-spike-report.md`](2026-07-21-ui-dioxus-runtime-spike-report.md)
for the runtime evidence and the resulting verdict.
```

- [ ] **Step 6: Commit**

```bash
cd /home/robhicks/dev/otto-next
git add docs/superpowers/specs/2026-07-21-ui-dioxus-runtime-spike-report.md \
        docs/superpowers/specs/2026-07-11-ui-dioxus-spike-report.md
git commit -m "spike(ui-runtime): add runtime spike report and verdict"
```

---

### Task 13: Execute the disposition

**Gated on explicit user confirmation of the verdict.** Do not begin this task autonomously.

**Files:**
- Depends on verdict. `KEEP`: delete `ui-dioxus/`, modify root `Cargo.toml` exclude list, modify `CLAUDE.md`. `ADOPT`: create a migration plan. `MIXED`: no code change.

**Interfaces:**
- Consumes: the verdict from Task 12.

- [ ] **Step 1: Present the verdict and ask for confirmation**

Show the user the verdict, the step matrix, and the disposition it implies. Ask whether to execute that disposition now. **Wait for an answer** — deleting a crate and rewriting `CLAUDE.md` are not reversible-feeling actions to take unprompted.

- [ ] **Step 2 (KEEP only): Remove the crate and its references**

```bash
cd /home/robhicks/dev/otto-next
git rm -r --quiet ui-dioxus
grep -n 'exclude' Cargo.toml
```

Edit the root `Cargo.toml` `exclude` list to drop `"ui-dioxus"`, leaving `exclude = ["ui", "desktop"]`. Then update `CLAUDE.md`'s UI paragraph to record that the Dioxus axis was evaluated across two spikes and closed, pointing at both reports.

```bash
cargo build --workspace 2>&1 | tail -3   # MUST still succeed
cargo test --workspace 2>&1 | tail -3    # MUST still pass
git add -A && git commit -m "spike(ui-runtime): remove ui-dioxus per keep-Leptos verdict"
```

- [ ] **Step 3 (ADOPT only): Write the migration plan**

Invoke the writing-plans skill for a new plan at `docs/superpowers/plans/YYYY-MM-DD-ui-dioxus-migration.md` covering: closing the web-highlighting gap, retiring `desktop/`+Tauri, cutover of the `ui/` build/serve story, and removal of the superseded crates. Do **not** execute it in this spike.

- [ ] **Step 4 (MIXED only): Record the open question**

Append the named remaining unknown to the report and stop. No code change.

- [ ] **Step 5: Clean up spike scratch state**

```bash
rm -rf /tmp/otto-ui-spike
pkill -f "Xvfb :99" || true
```

The spike directory `docs/superpowers/spikes/2026-07-21-ui-runtime/` stays in git as the evidence trail regardless of verdict; only `/tmp` scratch and the virtual display are cleaned.

---

## Self-Review

**Spec coverage:** §Goal & non-goals → Tasks 7–10 (four runs), Global Constraints (no engine/protocol change, no workspace disturbance). §Shared engine configuration → Task 1 (fixture), Task 7 Step 1 (web config), Task 6 (desktop shim), Global Constraints (offline LLM). §Recorded finding regardless of outcome → Task 6 preamble, Task 12 Step 2. §The scenario contract → Task 3. §Execution mechanics → Tasks 5 (build-first), 7–10 (driving). §Measurements → Task 5 (sizes, wall-clock), Tasks 7–10 Step 5/6/7 (timings, RSS), Task 11 Step 2–3 (consolidation + honesty constraints). §Bug policy → Tasks 7–10 bug steps, Task 11 Step 4, Global Constraints (escalation). §Report structure → Task 12. §Disposition → Task 13. §Risks → Task 4 (toolchain gate), Task 6 Step 3 (shim failure), Tasks 8/10 (deviation recording for driver bias). No spec section is unimplemented.

**Two gaps found and closed during review:** (1) the spec's desktop mechanism did not say how input is *synthesized* — Task 4 Step 5 now makes the Xvfb/xdotool decision explicit with a named degraded fallback; (2) whether the offline-deterministic path emits an `ApprovalRequest` at all was assumed — Task 2 Step 5 now makes it a decision gate before the contract is frozen.

**Placeholder scan:** no `TBD`/`TODO`; every code and command step carries literal content. Steps that intentionally cannot be pre-written (per-step driving inside a frozen contract) point at the contract's own `How driven`/`How asserted` lines, which Task 3 requires be written before any run.

**Naming consistency:** build identifiers `leptos-web` / `dioxus-web` / `leptos-desktop` / `dioxus-desktop` are used identically in filenames, matrix columns, and commit messages. `$SPIKE` = `docs/superpowers/spikes/2026-07-21-ui-runtime/` throughout. `OTTO_REAL_BIN` is defined in Task 6's shim and used in Tasks 9 and 10. `otto-shim.sh` is staged by filename in Task 9 and by `OTTO_BIN` in Task 10, matching each app's actual sidecar-resolution mechanism.
