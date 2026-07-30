#!/usr/bin/env bash
#
# Build the Dioxus web release bundle, refusing to produce one that cannot be trusted.
#
#   cd ui-dioxus && ./scripts/build-web.sh
#
# Prints the bundle directory as its final line — feed it to `otto serve --ui-dir`:
#
#   otto serve --ui-dir "$(cd ui-dioxus && ./scripts/build-web.sh | tail -1)"
#
# This owns the four guards that used to live in measure-web-bundle.sh, which is now a thin
# wrapper over this script. They live in exactly one place on purpose: `dx` reports success
# (exit 0) even when its `wasm-opt` step crashes, in which case the bundle ships UNOPTIMIZED
# wasm — 2.16 MB instead of 795 KB. That is precisely what happened before the
# [profile.wasm-release] fix in Cargo.toml (see the comment there). Two copies of these guards
# would drift, and their not drifting is the only thing standing between this project and
# silently re-shipping that bundle.
#
#   1. wipes target/dx first, because dx never prunes stale hashed assets from a previous build
#      and leaving them behind makes "which wasm is the current one?" ambiguous;
#   2. fails if dx logged a wasm-opt failure;
#   3. fails if the emitted wasm still carries DWARF (`.debug_*` sections) — the signature of the
#      Cargo.toml `strip` having been dropped;
#   4. fails if the wasm exceeds MAX_WASM_BYTES. Guards 2 and 3 both fail open: 2 is a string
#      match on dx's log wording, and 3 cannot fire at all once `strip` removes the DWARF up
#      front, so a wasm-opt that is skipped or silently dropped by a future dx would produce a
#      clean-but-unoptimized wasm that sails past both. Guard 4 is value-based and cannot.
#
set -euo pipefail

# Ceiling, not a target. Optimized wasm measured 795,188 B on 2026-07-24; unoptimized was
# 2,164,985 B. Anything above this is far likelier to be a broken wasm-opt than real growth.
# Raise it deliberately (with a new measurement) when the app legitimately grows.
MAX_WASM_BYTES=${MAX_WASM_BYTES:-1200000}

cd "$(dirname "$0")/.."

command -v dx >/dev/null 2>&1 || {
    echo "error: 'dx' (Dioxus CLI) not found on PATH — install with: cargo install dioxus-cli" >&2
    exit 1
}

# Everything below assumes the build lands in ./target; CARGO_TARGET_DIR would relocate it and
# make `rm -rf target/dx` a no-op, so the run would fail later with a confusing "asset dir not
# found" instead of naming the cause.
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    echo "error: CARGO_TARGET_DIR is set ($CARGO_TARGET_DIR); this script expects ./target." >&2
    echo "       Unset it and re-run." >&2
    exit 1
fi

# GNU `mktemp -t` requires the trailing X's; BSD/macOS treats the whole string as a filename
# prefix and appends its own suffix. This form works on both.
log=$(mktemp -t build-web.XXXXXX)
trap 'rm -f "$log"' EXIT

# 1. Stale assets from earlier builds would make the artifact set ambiguous.
rm -rf target/dx

echo "==> dx build --release --platform web --features web" >&2
# `--platform web` is required in addition to `--features web`: the feature only enables the
# crate's own cargo feature, it does not tell dx which platform to build for.
dx build --release --platform web --features web 2>&1 | tee "$log" >&2

# 2. dx exits 0 even when wasm-opt aborts, so its log is the only signal.
if grep -qi "wasm-opt failed" "$log"; then
    echo >&2
    echo "error: wasm-opt failed — the bundle is UNOPTIMIZED and must not be shipped." >&2
    echo "       See the [profile.wasm-release] comment in Cargo.toml." >&2
    exit 1
fi

public="target/dx/otto-desktop/release/web/public"
assets="$public/assets"
[ -d "$assets" ] || { echo "error: expected asset dir not found: $assets" >&2; exit 1; }

# Deliberately not `mapfile` — that is a bash 4 builtin and macOS still ships bash 3.2.
# `$(( ))` normalizes BSD `wc`'s leading whitespace.
wasm=$(find "$assets" -maxdepth 1 -name '*.wasm' -type f)
wasm_count=$(( $(printf '%s' "$wasm" | grep -c . || true) ))
if [ "$wasm_count" -ne 1 ]; then
    echo "error: expected exactly one .wasm in $assets, found $wasm_count:" >&2
    printf '  %s\n' $wasm >&2
    exit 1
fi

# 3. DWARF in the shipped wasm means the Cargo.toml `strip` is gone — wasm-opt will have aborted.
if grep -aq '\.debug_info' "$wasm"; then
    echo >&2
    echo "error: $wasm still contains DWARF (.debug_info) — the [profile.wasm-release] strip" >&2
    echo "       setting in Cargo.toml is missing or ineffective." >&2
    exit 1
fi

# 4. The one guard that cannot fail open. `$(( ))` strips BSD `wc`'s leading whitespace.
wasm_bytes=$(( $(wc -c <"$wasm") ))
if [ "$wasm_bytes" -gt "$MAX_WASM_BYTES" ]; then
    echo >&2
    echo "error: wasm is $wasm_bytes B, over the $MAX_WASM_BYTES B ceiling — wasm-opt most likely" >&2
    echo "       did not run. If this is genuine growth, raise MAX_WASM_BYTES in this script." >&2
    exit 1
fi

# Progress goes to stderr (above) so that stdout carries exactly one thing: the bundle dir.
echo "$public"
