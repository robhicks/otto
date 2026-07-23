# Run log — `dioxus-desktop`

## Status: NOT RUN AT RUNTIME

The `dioxus-desktop` scenario run was **not executed**. Sequence of events:

1. The headless driving harness (bare Xvfb `:99`) could not render either desktop client — WebKitGTK
   renders solid black under Xvfb and the GTK windows malform (1×1, no WM_CLASS), even after adding
   openbox as a window manager and forcing software rendering
   (`WEBKIT_DISABLE_DMABUF_RENDERER=1`, `LIBGL_ALWAYS_SOFTWARE=1`, `GALLIUM_DRIVER=llvmpipe`). This is
   a bare-Xvfb/WebKitGTK limitation shared by **both** desktop clients (Tauri and Dioxus both use the
   `webkit2gtk` webview backend), **not** a defect in either app.
2. The user elected to finish the desktop leg on the real GNOME/Wayland session. Both desktop apps
   were rebuilt (the Dioxus desktop binary had been reclaimed by Task 8's web rebuild; it was rebuilt
   with `dx build --release --features desktop --platform desktop`, incorporating the Task 8
   autoconnect fix — which is `#[cfg(feature="web")]` and so does not affect the desktop path).
3. Before the desktop runs completed, the user made the **ADOPT-Dioxus decision** on the strength of
   the two fully-verified web runs plus spike #1's unification evidence, and chose to **skip the
   remaining desktop runtime verification** (`dioxus-desktop` was next in line).

## What this means for the verdict

The single highest-value question this spike set out to answer at runtime — **does the Dioxus desktop
app actually launch, auto-connect, and kill its sidecar on window close** (all compile-verified-only in
spike #1) — **remains runtime-unverified.** The `dioxus-desktop` binary builds cleanly
(25,543,536 bytes) and spike #1 compile-verified its desktop auto-connect logic and `kill_on_drop`
sidecar teardown, but neither was exercised at runtime here.

The ADOPT decision therefore rests on:
- **Fully runtime-verified:** both web clients (`leptos-web`, `dioxus-web`) through the complete
  11-step scenario.
- **Compile-verified only (spike #1), NOT advanced by spike #2:** the Dioxus desktop client and its
  Tauri-replacement claim.

This gap is stated plainly in the spike report's verdict and carried into the migration plan as an
explicit pre-migration runtime-verification task, so the desktop client is exercised before `desktop/`
+ Tauri are retired.

## Desktop metrics captured (build-time only, no runtime)

- `dioxus-desktop` binary size: **25,543,536 bytes** (~24.4 MiB), single self-contained executable.
- For comparison, the `leptos-desktop` (Tauri) `app` binary is **13,748,208 bytes** (~13.1 MiB) — but
  Tauri additionally requires the bundled `otto` sidecar (~27 MB) and the system WebKitGTK, so the
  shipped-footprint comparison is not a straight binary-size diff. Recorded in the report as
  build-time-only data, not a runtime RSS measurement (which was never taken).
