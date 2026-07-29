#!/usr/bin/env bash
# Builds the otto binary and stages it as this platform's Dioxus bundle sidecar
# (ui-dioxus/binaries/otto-sidecar-<target-triple>), per the `[bundle] external_bin` entry in
# Dioxus.toml.
#
# dx uses the same target-triple-suffix convention Tauri's `externalBin` did.
# Retired the old Tauri wrapper's desktop/build-sidecar.sh in Phase 4 of the
# Dioxus migration.
#
# Staged as `otto-sidecar-<triple>`, NOT `otto-<triple>`, even though the copied file is
# `target/release/otto`. This is deliberate: dx strips the triple suffix from `external_bin`
# entries when it installs them, so a staged `otto-<triple>` would install as bare `otto` and
# collide with (silently overwrite / get removed alongside) this project's own `otto` CLI on
# the user's PATH. Measured on a built `.deb` via `ar x` + `tar tzvf` — do not rename this back
# to `otto-<triple>` without re-measuring; it looks like needless naming noise until you check.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
bin_dir="$repo_root/ui-dioxus/binaries"

echo "Building otto (release)..."
cargo build --release -p otto-engine --manifest-path "$repo_root/Cargo.toml"

mkdir -p "$bin_dir"
dest="$bin_dir/otto-sidecar-$target_triple"
if [[ "$target_triple" == *windows* ]]; then
  dest="$dest.exe"
fi
cp "$repo_root/target/release/otto" "$dest"
echo "Staged sidecar: $dest"
