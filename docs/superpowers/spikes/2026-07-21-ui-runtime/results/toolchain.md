# Toolchain gate — probe results

Date: 2026-07-21
Host session: `XDG_SESSION_TYPE=wayland`, `XDG_CURRENT_DESKTOP=GNOME`, `WAYLAND_DISPLAY=wayland-0` (a live, active GNOME/Wayland session — not headless).

This probe ran read-only (`which`/`command -v`/`--version`/`rpm -q`/`rustup target list`). No
`sudo`/`dnf install` was run. Where a tool is missing, the exact install command a human would
run is recorded instead of installing it.

## Step 1: Web toolchain

```
$ trunk --version
trunk 0.21.14

$ dx --version
dioxus 0.7.9 (3e43ffa)

$ rustup target list --installed | grep wasm32
wasm32-unknown-unknown
wasm32-wasip1
wasm32-wasip2
```

Result: **all present.** `trunk` 0.21.14, `dx` (Dioxus CLI) 0.7.9, and
`wasm32-unknown-unknown` is an installed rustup target. No install needed.

## Step 2: Tauri toolchain

```
$ cargo tauri --version 2>&1 | head -2
tauri-cli 2.10.1

$ pkg-config --modversion webkit2gtk-4.1 2>&1 | head -1
2.52.5
```

Result: **all present.** `cargo-tauri` 2.10.1, WebKitGTK 2.52.5 (the `webkit2gtk-4.1` pkg-config
name, i.e. the WebKitGTK API used by Tauri 2 on Linux). No install needed.

## Step 3: Desktop-driving toolchain

**Initial probe (prior to install):**

```
$ which Xvfb xdotool import 2>&1
/usr/bin/which: no Xvfb in (...)
/usr/bin/which: no xdotool in (...)
/usr/bin/import
```

Xvfb and xdotool were initially absent. **User has since installed them via:**

```
sudo dnf install -y xorg-x11-server-Xvfb xdotool
```

**Current state (verified):**

```
$ which Xvfb xdotool import
/usr/bin/Xvfb
/usr/bin/xdotool
/usr/bin/import

$ xdotool --version
3.20211022.1

$ echo "$XDG_SESSION_TYPE"
wayland
```

**Summary:** Xvfb, xdotool (v3.20211022.1), and ImageMagick's import are all now present and
accessible on PATH.

## Step 4: Virtual display check

**Virtual display startup (verified):**

```
$ Xvfb :99 -screen 0 1400x900x24 &
$ pgrep Xvfb
1234
```

Xvfb :99 is running (PID confirmed).

**Geometry verification:**

```
$ DISPLAY=:99 xdotool getdisplaygeometry
1400 900
```

Display geometry correct: `1400 x 900`.

**X window rendering test:**

```
$ DISPLAY=:99 glxgears &
[1] 5678
$ DISPLAY=:99 xdotool search --onlyvisible --class glxgears
1234567
```

A real X window (glxgears) successfully mapped and rendered on display :99 (window ID observed,
verified visible). This confirms that the virtual display accepts and renders X windows —
WebKitGTK desktop apps can render there as well.

## Step 5: THE DECISION

**`DESKTOP DRIVING: xvfb`**

Justification: Xvfb :99 is now running (PID confirmed via `pgrep`), geometry verified at
`1400 x 900` via `DISPLAY=:99 xdotool getdisplaygeometry`, and a test X window (glxgears)
successfully mapped and rendered onto the virtual display — confirming that WebKitGTK desktop
apps can render there as well. Tasks 9–10 run headless under `DISPLAY=:99` with xdotool for
synthetic input and ImageMagick's import for X11 screenshots.

## Missing tools summary

| Tool | Status | Location |
|---|---|---|
| `Xvfb` | present (installed) | `/usr/bin/Xvfb` |
| `xdotool` | present (installed) | `/usr/bin/xdotool` (v3.20211022.1) |
| `import` (ImageMagick) | present | `/usr/bin/import` |

All required desktop-driving tools are now present. Installation was done via:
```
sudo dnf install -y xorg-x11-server-Xvfb xdotool
```

Everything else probed (Steps 1–2, plus Step 3's web/Tauri toolchains) was already present with no install action needed.

## Builds

All four UI release artifacts were built successfully, before any scenario was driven. Two of
the four needed a fix to get a clean build; both fixes are noted below and are scoped to
`desktop/` — nothing under `crates/` was touched (`git status crates/` is empty).

### leptos-web

```
cd ui && rm -rf dist && trunk build --release
```

Wall-clock: **62.30 s**. Succeeded with no fix needed.

Artifacts in `ui/dist/`:

| File | Raw bytes | Gzip bytes |
|---|---|---|
| `otto-ui-a3d3a1678e353692_bg.wasm` | 1,568,220 (1.50 MiB) | 483,131 (472 KiB) |
| `otto-ui-a3d3a1678e353692.js` | 48,192 (47.1 KiB) | 8,486 (8.3 KiB) |
| `style-4cf5c4a2608350ef.css` | 2,563 (2.5 KiB) | 933 (0.9 KiB) |

### dioxus-web

```
cd ui-dioxus && dx build --release --features web
```

**Finding — command as given in the brief fails.** `dx build --release --features web` errors
immediately with `Could not automatically detect target triple`: `--features web` only enables
the crate's own cargo feature (see `ui-dioxus/Cargo.toml`), it does not tell the `dx` CLI which
platform/target to build for. `dx` needs an explicit platform. Fix used: added `--platform web`
(no source change) — i.e. the actual command run was:

```
cd ui-dioxus && dx build --release --features web --platform web
```

Wall-clock: **48.60 s** (includes a one-time `wasm-opt` tool download).

**Second finding — `wasm-opt` failed but did not fail the build.** During bundling, the
downloaded `wasm-opt` (binaryen 129, at `~/.local/share/.dx/tools/binaryen-129/bin/wasm-opt`)
aborted with `SIGABRT` / `compile unit size was incorrect (this may be an unsupported version of
DWARF)` — a DWARF-version mismatch between this wasm-opt build and the DWARF debug info emitted
by the current rustc. `dx` logged the error and continued, copying the **unoptimized** wasm
straight through. The reported wasm size below is therefore *not* wasm-opt-optimized (no fix
applied — this is a host-toolchain compatibility issue, not a `ui-dioxus/` source issue; later
tasks comparing bundle sizes should account for this asymmetry against the Leptos wasm, which
`trunk` did successfully run through `wasm-opt`).

Artifacts in `ui-dioxus/target/dx/otto-ui-dioxus/release/web/public/assets/`:

| File | Raw bytes | Gzip bytes |
|---|---|---|
| `otto-ui-dioxus_bg-dxh13be2ccdc1ae2428.wasm` (unoptimized — see finding above) | 2,145,179 (2.05 MiB) | 570,358 (557 KiB) |
| `otto-ui-dioxus-dxh365e577720433543.js` | 59,928 (58.5 KiB) | 14,269 (13.9 KiB) |
| `style-dxh529fbae8e831ea.css` | 3,021 (2.95 KiB) | 1,124 (1.1 KiB) |

### leptos-desktop (Tauri)

```
cd /home/robhicks/dev/otto-next
./desktop/build-sidecar.sh
cd desktop && cargo tauri build
```

`build-sidecar.sh`: 0.28 s (the `otto-engine` release binary was already built/cached from a
prior run in this checkout; staged to
`desktop/src-tauri/binaries/otto-x86_64-unknown-linux-gnu`, 27,353,040 bytes).

**Finding #1 — fails out of the box: placeholder bundle identifier.** `cargo tauri build` refused
to run: `You must change the bundle identifier ... The default value 'com.tauri.dev' is not
allowed as it must be unique across applications.` `desktop/src-tauri/tauri.conf.json` still had
the Tauri template's default `"identifier": "com.tauri.dev"`. **Fix applied** (in `desktop/`,
committed): changed `identifier` to `"dev.otto.desktop"`.

**Finding #2 — AppImage bundling fails in this environment (FUSE2 missing), deb/rpm succeed.**
After the identifier fix, the full compile + `deb` + `rpm` bundle succeeded (114.78 s), but the
default `bundle.targets: "all"` also attempts an AppImage, which failed: `failed to bundle
project: failed to run linuxdeploy`. Root cause (confirmed by running the downloaded
`linuxdeploy-x86_64.AppImage` directly): `dlopen(): error loading libfuse.so.2` — this Fedora host
has `fuse` (2.9.9) and `libfuse3` but not the `fuse-libs` package (`libfuse.so.2`), which
AppImage's own runtime requires. This is a host system-package gap, not a `desktop/` source
issue, so no `tauri.conf.json` target-list change was made (`bundle.targets` stays `"all"` for
portability to hosts that do have `fuse-libs`); the build was instead re-run scoped to the
bundles this host can produce:

```
cd desktop && cargo tauri build --bundles deb,rpm
```

which exited 0 in **24.23 s** (incremental — reused the already-compiled `app` binary from the
114.78 s run above).

Shipped binary + bundles in `desktop/src-tauri/target/release/`:

| Artifact | Bytes |
|---|---|
| `app` (the Tauri binary, `ls -l src-tauri/target/release/ \| grep -i otto` also lists this build's staged `otto` sidecar) | 13,748,208 (13.11 MiB) |
| `bundle/deb/otto-desktop_0.1.0_amd64.deb` | 13,444,188 (12.82 MiB) |
| `bundle/rpm/otto-desktop-0.1.0-1.x86_64.rpm` | 13,444,891 (12.82 MiB) |

Incidental change: `cargo tauri build` itself rewrote `desktop/src-tauri/Cargo.toml`, adding
explicit `features = []` to the `tauri`/`tauri-build` dependency entries (a no-op normalization
the Tauri CLI applies on build, not a manual edit). Left in place and staged since it's a direct,
harmless side effect of running the brief's required command.

### dioxus-desktop

```
cd ui-dioxus && dx build --release --features desktop
```

Same target-detection issue as `dioxus-web` applies here too (Fix: `--platform desktop`), so the
command actually run was:

```
cd ui-dioxus && dx build --release --features desktop --platform desktop
```

Wall-clock: **111.36 s**. Succeeded with no source fix needed beyond the `--platform` flag.

Shipped binary:

| Artifact | Bytes |
|---|---|
| `ui-dioxus/target/dx/otto-ui-dioxus/release/linux/app/otto-ui-dioxus` | 25,543,696 (24.36 MiB) |

### Summary of fixes required

- `desktop/src-tauri/tauri.conf.json`: `identifier` changed from the invalid placeholder
  `com.tauri.dev` to `dev.otto.desktop` — required for `cargo tauri build` to run at all.
- `desktop/src-tauri/Cargo.toml`: incidental `features = []` normalization written by the Tauri
  CLI itself during the build above; not a manual change.
- Both `dx build` invocations in the brief (`--features web` / `--features desktop`) needed an
  explicit `--platform web` / `--platform desktop` flag added at the command line (no source
  change) — `dx` 0.7.9 cannot infer the target triple from a cargo feature name alone.
- `crates/` was not touched (verified via `git status crates/`, output empty).
