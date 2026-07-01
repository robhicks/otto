#!/usr/bin/env bash
# Builds the otto binary and stages it as this platform's Tauri sidecar
# (desktop/src-tauri/binaries/otto-<target-triple>), per Tauri's externalBin convention.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
bin_dir="$repo_root/desktop/src-tauri/binaries"

echo "Building otto (release)..."
cargo build --release -p otto-engine --manifest-path "$repo_root/Cargo.toml"

mkdir -p "$bin_dir"
dest="$bin_dir/otto-$target_triple"
if [[ "$target_triple" == *windows* ]]; then
  dest="$dest.exe"
fi
cp "$repo_root/target/release/otto" "$dest"
echo "Staged sidecar: $dest"
