# Run log — `leptos-desktop` (Tauri)

Driven per the frozen contract (`docs/superpowers/specs/2026-07-21-ui-runtime-scenario.md`)
against the server baseline (`docs/superpowers/spikes/2026-07-21-ui-runtime/baseline/README.md`).

**Headline result: the shell-script sidecar was ACCEPTED by `cargo tauri build` (the named
risk this task exists to test). The scenario itself could not be driven past the native
folder-picker dialog under this headless X11 harness — extensively diagnosed below as an
environment limitation (no window manager, no root to install one, GTK3/rfd dialog thread
behavior under Xvfb), not a defect surfaced in the shipped app's own logic.**

## Build

```bash
cp docs/superpowers/spikes/2026-07-21-ui-runtime/otto-shim.sh \
   desktop/src-tauri/binaries/otto-x86_64-unknown-linux-gnu
chmod +x desktop/src-tauri/binaries/otto-x86_64-unknown-linux-gnu
cd desktop && cargo tauri build --bundles deb,rpm
```

- **Tauri accepted the shell-script sidecar without complaint.** No validation against the
  `externalBin` target-triple file's content/format was performed — `cargo tauri build`
  matches the sidecar purely by filename (`binaries/otto-<target-triple>`), copies whatever is
  there into the bundle, and patches ELF bundle-type metadata into the *app* binary only. The
  resulting `desktop/src-tauri/target/release/otto` (the bundled sidecar copy) is confirmed to
  be the 487-byte shim script (`file` reports "Bourne-Again shell script, ASCII text
  executable"), not the 27,353,040-byte real `otto` binary. **This resolves the task's named
  risk: the shim mechanism is usable for Tauri, so this run is FULL scope per the brief (not
  degraded for the shim-acceptance reason).**
- **Wall-clock:** `20.67s` compile (`Finished release profile ... in 20.67s`) + bundling ≈
  `22s` total, measured incrementally (dependencies were already built by a prior, unrelated
  build in this checkout — see Task 5's report). This is not a clean/cold build number.
- **Artifact sizes:**

  | Artifact | Bytes |
  |---|---|
  | `desktop/src-tauri/target/release/app` (the Tauri/GUI binary) | 13,748,208 |
  | `bundle/deb/otto-desktop_0.1.0_amd64.deb` | 4,568,204 |
  | `bundle/rpm/otto-desktop-0.1.0-1.x86_64.rpm` | 4,569,309 |

- **Toolchain:** `rustc 1.95.0 (59807616e 2026-04-14)`, `cargo 1.95.0 (f2d3ce0bd
  2026-03-21)`, `tauri-cli 2.10.1`. Key deps (`desktop/src-tauri/Cargo.toml`): `tauri 2.10.3`,
  `tauri-plugin-dialog 2.7.1` (→ `rfd 0.16.0`, GTK3 backend, no `ashpd`/portal Rust
  dependency), `tauri-plugin-shell 2.3.5`.

## Environment

- **Provider keys:** confirmed absent — `env | grep -E
  "ANTHROPIC_API_KEY|OPENAI_API_KEY|GEMINI_API_KEY"` returned nothing before every launch.
- **`OTTO_DB`:** `/tmp/otto-ui-spike/leptos-desktop.db` (recreated fresh before each launch
  attempt).
- **Port:** N/A this run — the sidecar (`otto serve --root <picked> --port 8787`, wrapped by
  the shim to append `--approve-edits --promote-loopback`) was **never invoked**, because the
  app's own folder-picker dialog — the mandatory first interaction before the sidecar spawns
  — could not be driven to completion (see below). `sqlite3 leptos-desktop.db "select
  count(*) from sessions;"` → `Error: in prepare, no such table: sessions` — the database
  file exists (0 bytes) but was never opened/initialized by `otto serve` at all, confirming
  the sidecar process was never started even once across every attempt in this run.
- **Display:** Xvfb `:99`, 1400x900 (pre-existing, confirmed running throughout). No X11
  window manager was present or installable (no root/sudo on this host; `openbox`,
  `fluxbox`, `icewm`, `twm`, `matchbox-window-manager` all absent; `mutter` is present but is
  a Wayland-only compositor in this build and refuses to attach as a plain X11 WM). A
  from-scratch ~40-line minimal reparenting-free WM (`python3-xlib`, installed via
  `pip install --user`) was written and run to correctly answer `MapRequest`/
  `ConfigureRequest` on root, to rule out "no WM" as the root cause — documented under Bugs
  below.
- **Confound specific to this host:** `WAYLAND_DISPLAY=wayland-0` and
  `DBUS_SESSION_BUS_ADDRESS` point at the operator's own live GNOME/Wayland desktop session
  (this is a workstation, not a clean CI box.) Every launch explicitly used `env -u
  WAYLAND_DISPLAY` (and `GDK_BACKEND=x11`) to force the app onto `:99` rather than the real
  Wayland compositor — confirmed necessary: without it, the app connects to the *real*
  desktop's Wayland socket and zero windows appear on `:99` at all.

## Steps

Every step below except 9 depends on the sidecar being spawned with `--root
/tmp/otto-ui-spike/fixture`, which never happened (see Environment). Root cause is detailed
once, in full, under `## Bugs`; each row below is marked against the contract's own literal
"How asserted (desktop)" assertion rather than softened.

| N | Status | Evidence | Notes |
|---|---|---|---|
| 1 | FAIL | `sqlite3 leptos-desktop.db "select count(*) from sessions;"` → `Error: in prepare, no such table: sessions`; `pgrep -af "serve --root /tmp/otto-ui-spike/fixture"` → no match, ever, across every launch attempt in this run. The app's main window (`otto`, 800x600, confirmed via `xdotool getwindowgeometry`) does appear under `:99` within ~6s of launch — so the app itself starts fine under the virtual display — but the sidecar the contract's assertion is keyed on never spawns. | The literal contract assertion for this step ("sidecar process running + one new `sessions` row") could not be satisfied. See Bugs for why. |
| 2 | FAIL | `docs/superpowers/spikes/2026-07-21-ui-runtime/results/shots/leptos-desktop-step2-connected.png` (force-added): a same-geometry capture of the `otto` window — solid black, 1-bit/2-color PNG, no visible content. Confirmed via `python3 -c "Image.open(...).getcolors()"`: `[(pixel_count, 0)]`, i.e. every pixel is the same value. | Cannot confirm the status strip rendered at all — the window paints uniformly black in every capture taken across this run (also true before any interaction attempt, so this is not caused by the picker being stuck; WebKitGTK's own accelerated-compositing path is unavailable here too — `libEGL warning: DRI3 error: Could not get DRI3 device` on every launch — a second, independent rendering issue layered on top of the picker blocker). |
| 3 | FAIL | No prompt could be sent — no workspace was ever connected. | Blocked upstream. |
| 4 | FAIL | N/A | Blocked upstream. |
| 5 | FAIL | N/A | Blocked upstream. |
| 6 | NOT-VERIFIABLE | N/A | Contract already declares the in-window expand/collapse animation NOT-VERIFIABLE (desktop) regardless; the `.env`-omission half (normally checked via the `/workspace` RPC payload) is *also* unreachable here since no `/workspace` RPC was ever made. |
| 7 | FAIL | N/A | Blocked upstream. |
| 8 | NOT-VERIFIABLE | N/A | Contract already declares this NOT-VERIFIABLE (desktop) unconditionally (no external effect from an in-window unsaved buffer); also unreachable here regardless. |
| 9 | NOT-APPLICABLE | Per the frozen contract: offline-deterministic Coder proposes zero edits against this fixture in every build. Recorded per the contract's mandate regardless of this run's other findings. | Not attempted/faked, per contract. |
| 10 | FAIL | N/A | Blocked upstream. |
| 11 | FAIL | N/A | Blocked upstream; shim *was* accepted by Tauri (see Build), so had the picker been drivable this step would have been in scope (FULL run), not degraded. |

## Measurements

Bundle size and cold-start/render-latency/reconnect-replay measures all require a connected
session, which was never reached — recorded `VOID` rather than fabricated. Build wall-clock,
binary size, and RSS (at the one reachable state — launched, stuck at the folder-picker) are
real and reported.

1. **Web bundle size** — `VOID (desktop build, not applicable)`.

2. **Cold start → first paint** — `VOID (no in-page instrumentation reachable — WebKitGTK
   webview never receives a connected page load; window itself paints solid black from
   launch, see step 2)`.

3. **Cold start → `Ready` handled** — `VOID (no session ever reaches Ready — sidecar never
   spawns, see ## Bugs)`.

4. **Event render latency** — `VOID (no turn was ever run — no workspace connection)`.

5. **Reconnect replay time** — `VOID (no session to reconnect to)`.

6. **Desktop RSS** (app process only, sidecar excluded — caveat: measured at the **only**
   state this run could reach, launched-and-stuck-at-the-folder-picker, *not* the
   contract's intended "connected, workspace open" state; recorded honestly rather than
   omitted):
   - Rep 1: 112,316 KB
   - Rep 2: 112,128 KB
   - Rep 3: 111,932 KB
   - **Median: 112,128 KB** (min 111,932, max 112,316) — **caveated, not a like-for-like
     comparison point against the web builds' measurements.**

7. **Desktop binary size** — `app` binary: 13,748,208 bytes; `.deb`: 4,568,204 bytes; `.rpm`:
   4,569,309 bytes (see `## Build`).

8. **Build wall-clock** — `~22s` (incremental; see `## Build` — not a clean/cold build
   measurement, the dependency graph was already compiled by an unrelated prior build in this
   checkout).

## Bugs

### Finding 1 — native folder-picker dialog cannot be driven under this headless X11 harness (environment limitation, not a confirmed app defect)

- **Failing step:** 1 (blocks 1–8, 10, 11 as a consequence; 9 is contract-mandated
  NOT-APPLICABLE regardless).
- **Symptom:** `app.dialog().file().blocking_pick_folder()`
  (`desktop/src-tauri/src/lib.rs:32`) never returns. `gdb -p <pid> -batch -ex "thread apply
  all bt"` on the main thread shows it parked cleanly on an internal channel recv:
  ```
  #4  tauri_plugin_dialog::FileDialogBuilder<R>::blocking_pick_folder ()
  #3  std::sync::mpmc::Receiver<T>::recv ()
  ```
  A *separate* OS thread (rfd's dedicated GTK thread, per
  `rfd-0.16.0/src/backend/gtk3/utils.rs`'s `GtkGlobalThread`) is confirmed alive and actively
  iterating `gtk_main_iteration()` in a loop — i.e. GTK itself is initialized and pumping, not
  deadlocked — yet no distinguishable, focusable, correctly-sized top-level window for the
  dialog ever appears. `xdotool search --name ".*"` consistently enumerates only: the app's
  main "otto" window (800x600, renders solid black — see step 2), one degenerate
  10x10-at-(10,10) window titled "app" (confirmed **override-redirect** — a custom minimal WM
  written for this investigation, which correctly intercepted and granted the main window's
  own `MapRequest`, never received a `MapRequest` for this one, proving it bypasses window-manager
  redirection entirely and is not a normal application dialog), and two 1x1 windows at
  (-1,-1) (GTK/GDK internal helper windows, not user-facing).
- **Root-cause investigation performed** (all inconclusive as to a definitive single cause,
  but collectively rule out the obvious candidates):
  - Confirmed **not** an XDG Desktop Portal delegation issue: `rfd`'s GTK3 backend
    (`gtk3/file_dialog.rs`) calls `gtk_file_chooser_native_new()` + `gtk_native_dialog_run()`
    directly (no `ashpd` dependency in `Cargo.lock`); `GTK_USE_PORTAL=0` was set on every
    later attempt; `G_DBUS_DEBUG=all` captured over a full 15s run and `grep -c
    "FileChooser\|OpenFile\|SelectFolder"` → `0` — no portal FileChooser call is ever made.
  - Ruled out "no window manager" as sufficient explanation: wrote and ran a ~40-line
    Python/`python3-xlib` minimal WM that selects `SubstructureRedirectMask` on root and grants
    every `ConfigureRequest`/`MapRequest`. It correctly mapped and focused the app's own main
    window — proving the WM itself works — but received no additional `MapRequest` for a
    dialog, and 30+ seconds of continuous monitoring showed no change.
  - Ruled out GPU/rendering as the *sole* cause of the picker specifically (though a *second*,
    independent rendering problem is also present: `libEGL warning: DRI3 error: Could not get
    DRI3 device` on every launch, and the main window paints solid black even before any
    picker interaction is attempted — WebKitGTK has no accelerated compositing path available
    on this host under Xvfb). Forcing `LIBGL_ALWAYS_SOFTWARE=1` made the app crash outright
    (process exits within the launch window) rather than help, so it was not pursued further.
  - Attempted full session isolation via `dbus-run-session` (a fresh, private D-Bus bus with
    no live desktop's portal/GVfs services registered on it) to rule out interference from the
    operator's real GNOME/Wayland session sharing `DBUS_SESSION_BUS_ADDRESS`/`XDG_RUNTIME_DIR`
    with this test. This still didn't produce a picker window within a 15s foreground run
    (though it does progress further into GNOME-portal negotiation logs, it stalls the same
    way afterward).
  - A plausible (not confirmed) contributing factor, noted in `rfd`'s own source comment:
    *"GTK functions are not thread-safe, and must all be called from the thread that
    initialized GTK. ... You're stuck on the thread on which you first initialize GTK."*
    `tauri-runtime-wry`/`tao` almost certainly already calls `gtk_init()` on the main thread
    (GTK3 is the Linux windowing backend for `wry`); `rfd`'s `GtkGlobalThread` then calls
    `gtk_init_check()` again from a **second**, dedicated thread. Two independent
    `gtk_init()` calls from different threads against the same X display is a documented GTK
    thread-safety violation upstream; this may or may not manifest under a normal desktop
    session (with a real WM doing size/focus negotiation) but could plausibly produce exactly
    this kind of degenerate, never-realized dialog window under a display server that doesn't
    paper over the race the way a full desktop compositor does. **Not confirmed** — flagged as
    the most likely lead for anyone continuing this investigation, not asserted as the cause.
- **Could a compiler or test plausibly have caught it?** No. This is a runtime,
  environment-dependent interaction between two independent GTK initializations, `rfd`'s
  dialog delivery mechanism, and the presence/absence of a window manager — none of it is
  visible to `cargo build`/`cargo test`, and the app almost certainly works normally for a
  human running it on a real desktop (this exact code path was already built and is presumed
  functional; nothing here is a regression introduced by this task — no `desktop/src-tauri/src/*.rs`
  changes were made or are proposed, per the brief's "only modify if a bug is found" — this is
  not attributed to a specific line of app code, it's an emergent multi-component interaction
  that a source fix in this crate could not obviously target).
- **Fix commit:** none — no source change is proposed. This is recorded as a spike finding,
  not remediated.
- **Fix wall-clock:** N/A (not fixed).

### Finding 2 — WebKitGTK renders solid black under this host regardless of the picker issue

- **Failing step:** 2 (compounds Finding 1; independently observed even where the window
  itself is confirmed mapped and visible).
- **Symptom:** the app's main "otto" window (800x600) captures as a uniform single-color PNG
  in every screenshot taken across this run, from the moment it appears. `libEGL warning: DRI3
  error: Could not get DRI3 device` / `Ensure your X server supports DRI3 to get accelerated
  rendering` appears on every launch. `WEBKIT_DISABLE_COMPOSITING_MODE=1` (per the task's own
  launch recipe) did not resolve it; `LIBGL_ALWAYS_SOFTWARE=1` made the process crash instead.
- **Cause class:** host/display-server GPU-acceleration gap (Xvfb has no real DRI3-capable
  GPU device backing it), not a Leptos/Rust/app logic defect.
- **Could a compiler or test plausibly have caught it?** No — a rendering/compositing
  capability gap in the test host, invisible to any build or unit test.
- **Fix commit / wall-clock:** none — not pursued further once Finding 1 made the whole
  scenario unreachable regardless.

## For the controller: Task 10 (`dioxus-desktop`) degradation

Per the brief: *"whatever degradation you hit here, Task 10 (dioxus-desktop) MUST degrade
identically to keep the comparison fair."* This run did **not** hit the brief's anticipated
degradation trigger (Tauri rejecting the shim — that was **accepted**, confirmed above). It
hit a **different, new** blocker: the native folder-picker dialog could not be driven
headlessly at all, for any build, before the sidecar ever spawns. If `dioxus-desktop` also
opens a native folder-picker before spawning its sidecar (`ui-dioxus/src/desktop_boot.rs`,
per the contract's own "Desktop runs" section), it is very likely to hit the **same**
class of blocker in this environment (same GTK3/`rfd`-family dialog stack is the typical
Linux implementation for both Tauri and Dioxus desktop shells) — Task 10 should attempt its
own picker interaction independently (do not assume failure) but, if it hits the same wall,
should degrade to the same set of `FAIL`/blocked-upstream rows as this run for steps 1–8,
10–11, keeping 9 `NOT-APPLICABLE`, so the two desktop runs remain comparable.

---

## Addendum — real-session attempt + early close (2026-07-22)

After the headless run above hit the Xvfb/WebKitGTK wall (no WM, black render), the desktop leg was
retried on the user's real GNOME/Wayland session (openbox was installed and confirmed to fix the
window-manager gap headlessly, but WebKitGTK still rendered solid black under Xvfb with software
rendering + malformed 1×1 GTK windows — a bare-Xvfb limitation, not an app defect). The Tauri app was
rebuilt with the shim staged as its sidecar and **launched successfully on the real session (pid alive,
window created)** — proving the app starts and the shell-script sidecar shim is accepted by Tauri
(the plan's named risk, resolved: **ACCEPTED**).

Before the folder-pick interaction completed, the user made the **ADOPT-Dioxus decision** on the
strength of the web runs + spike #1's unification evidence, and elected to **skip the remaining desktop
runtime verification**. The apps were closed and the tree cleaned.

**Net desktop-leg status for leptos-desktop:** launch + sidecar-shim-acceptance verified on the real
session; the full 11-step scenario was NOT completed. Steps 1–11 remain **runtime-unverified** on this
client (the headless run's FAIL rows reflect the harness limitation, not the app). This is recorded
honestly rather than papered over.
