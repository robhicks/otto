#!/usr/bin/env bash
#
# Build the installable desktop package (.deb) with the otto sidecar staged inside.
#
#   cd ui-dioxus && ./scripts/build-desktop.sh
#
# Staging must happen before `dx bundle`: `[bundle] external_bin` in Dioxus.toml reads
# binaries/otto-sidecar-<triple> at bundle time, and dx does not build it. Skipping the staging
# step produces a package that installs cleanly and then cannot start a server — which is why the
# two steps live in one script rather than being left to a reader's memory.
#
# --package-types rpm is deliberately NOT requested here. Measured on 2026-07-28 (dx 0.7.9,
# Fedora): `dx bundle --package-types deb --package-types rpm` builds the .deb fine (dx bundles
# its own packer — dpkg/dpkg-deb need not be on the host) but the .rpm step fails every time with
#   ERROR Failed to add converted ICO icon to RPM: invalid destination path
#   /usr/share/icons/hicolor/256x256/apps/otto-ui-dioxus.png - duplicate file entry
# This is a dx bug, not a missing host tool: `icons/icon.ico` embeds a 256x256 frame that dx
# converts to that same hicolor path `icons/128x128@2x.png` (also 256x256) already occupies, and
# dx's RPM bundler rejects the resulting duplicate destination. Follow-up: either drop the 2x PNG
# from `[bundle] icon` in Dioxus.toml or file the collision upstream, then re-add
# `--package-types rpm` here.
#
set -euo pipefail

cd "$(dirname "$0")/.."

command -v dx >/dev/null 2>&1 || {
    echo "error: 'dx' (Dioxus CLI) not found on PATH — install with: cargo install dioxus-cli" >&2
    exit 1
}

./scripts/stage-sidecar.sh

echo "==> dx bundle --release --platform desktop --features desktop"
dx bundle --release --platform desktop --features desktop \
    --package-types deb

echo
echo "==> packages:"
find target/dx -name '*.deb' -o -name '*.rpm'
