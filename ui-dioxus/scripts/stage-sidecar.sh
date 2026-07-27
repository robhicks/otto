#!/usr/bin/env bash
# Builds the otto binary and stages it as this platform's Dioxus bundle sidecar
# (ui-dioxus/binaries/otto-<target-triple>), per the `[bundle] external_bin` entry in
# Dioxus.toml.
#
# dx uses the same target-triple-suffix convention Tauri's `externalBin` did, which is why this
# is a near-copy of desktop/build-sidecar.sh — the script it replaces when Phase 4 retires the
# Tauri wrapper. Keep the two in sync until then.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
bin_dir="$repo_root/ui-dioxus/binaries"

echo "Building otto (release)..."
cargo build --release -p otto-engine --manifest-path "$repo_root/Cargo.toml"

mkdir -p "$bin_dir"
dest="$bin_dir/otto-$target_triple"
if [[ "$target_triple" == *windows* ]]; then
  dest="$dest.exe"
fi
cp "$repo_root/target/release/otto" "$dest"
echo "Staged sidecar: $dest"
