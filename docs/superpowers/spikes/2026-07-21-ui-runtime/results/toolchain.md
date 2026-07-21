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

```
$ which Xvfb xdotool import 2>&1
/usr/bin/which: no Xvfb in (...)
/usr/bin/which: no xdotool in (...)
/usr/bin/import

$ echo "$XDG_SESSION_TYPE"
wayland
```

Confirmed via `rpm -q` (not just a PATH miss):

```
$ rpm -q xorg-x11-server-Xvfb xdotool ImageMagick ydotool
package xorg-x11-server-Xvfb is not installed
package xdotool is not installed
ImageMagick-7.1.2.27-1.fc44.x86_64
ydotool-1.0.4-8.fc44.x86_64
```

`xvfb-run` is also absent (`command -v xvfb-run` → exit 1; not even a stub). So Xvfb-based
driving (Step 4 as literally specified: `Xvfb` + `xdotool`) is **unavailable** and cannot be
installed non-interactively per this task's constraints.

**Missing tools and the install command a human would run:**

- `Xvfb` — `sudo dnf install -y xorg-x11-server-Xvfb`
- `xdotool` — `sudo dnf install -y xdotool`

**What IS present that changes the picture:** the session is a live, active GNOME/Wayland
session (`XDG_SESSION_TYPE=wayland`, seat0, state `active`), and `ydotool` 1.0.4-8 is already
installed with its daemon already running and usable by the current user with no `sudo`:

```
$ pgrep -a ydotoold
977 /usr/bin/ydotoold -P 0660 -o 1000:1000

$ ls -la /tmp/.ydotool_socket
srw-rw----. 1 robhicks robhicks 0 Jul 21 04:33 /tmp/.ydotool_socket
$ id
uid=1000(robhicks) gid=1000(robhicks) groups=1000(robhicks),10(wheel),104(input) ...

$ ydotool
Usage: ydotool <cmd> <args>
Available commands:
  click
  mousemove
  type
```

The `ydotoold` socket is owned `robhicks:robhicks` mode `0660` and the current user is uid
1000/gid 1000 — `ydotool` works against the real session with no privilege escalation needed.
ImageMagick's `import` (X11) is present too, but `import` is X11-only and this is a native
Wayland session; screenshotting the real desktop instead goes through GNOME Shell's D-Bus
Screenshot interface, confirmed reachable:

```
$ gdbus introspect --session --dest org.gnome.Shell --object-path /org/gnome/Shell/Screenshot
node /org/gnome/Shell/Screenshot { ... }   # interface present and introspectable
```

(`gnome-screenshot` itself is *not* installed — `rpm -q gnome-screenshot` → not installed —
and no wlroots tools (`grim`/`slurp`) are present either, which is expected since this
compositor is GNOME Mutter, not a wlroots compositor. The D-Bus Screenshot interface is the
GNOME-native capture path and needs no extra package.)

## Step 4: Virtual display check

**Skipped.** `Xvfb` is not installed (confirmed via both `which` and `rpm -q`, Step 3), so
`Xvfb :99 -screen 0 1400x900x24` cannot be started and `DISPLAY=:99 xdotool getdisplaygeometry`
cannot be run (`xdotool` is also absent). No Xvfb process was started; there is nothing running
on display `:99`.

## Step 5: THE DECISION

**`DESKTOP DRIVING: real-session`**

Justification: Xvfb-based driving is unusable (Step 3/4 — `xorg-x11-server-Xvfb` and `xdotool`
are both not installed, and this task is barred from `sudo dnf install`), but the live
GNOME/Wayland session already hosts a working synthetic-input path with no further setup:
`ydotool` is installed, `ydotoold` is already running, and its control socket is owned by the
current user (`robhicks`, uid 1000) at mode `0660` — so Tasks 9–10 can drive the desktop apps
in the real session via `ydotool click`/`mousemove`/`type` (uinput-level input, works
regardless of X11 vs Wayland), and capture screenshots via GNOME Shell's D-Bus `Screenshot`
interface (confirmed introspectable) rather than ImageMagick's X11-only `import`.

## Missing tools summary

| Tool | Status | Human install command |
|---|---|---|
| `Xvfb` | missing | `sudo dnf install -y xorg-x11-server-Xvfb` |
| `xdotool` | missing | `sudo dnf install -y xdotool` |
| `gnome-screenshot` | missing (not needed — D-Bus Screenshot interface covers this) | `sudo dnf install -y gnome-screenshot` |

Everything else probed (Steps 1–2, plus `ydotool`/`import`/D-Bus Screenshot in Step 3) is
present with no install action needed.
