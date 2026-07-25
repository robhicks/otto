#!/usr/bin/env bash
#
# Build the Dioxus web release bundle and report its size.
#
#   cd ui-dioxus && ./scripts/measure-web-bundle.sh
#
# This is the sanctioned way to produce a bundle-size figure for this crate. It exists because
# `dx` reports success (exit 0) even when its `wasm-opt` step crashes, in which case the bundle
# ships UNOPTIMIZED wasm and any size figure taken from it is meaningless — that is exactly what
# happened before the `[profile.wasm-release]` fix in Cargo.toml (see the comment there).
#
# So this script does not just measure; it refuses to report a number it cannot trust:
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

# BSD/macOS mktemp requires an explicit template.
log=$(mktemp -t measure-web-bundle.XXXXXX)
trap 'rm -f "$log"' EXIT

# 1. Stale assets from earlier builds would make the artifact set ambiguous.
rm -rf target/dx

echo "==> dx build --release --platform web --features web"
# `--platform web` is required in addition to `--features web`: the feature only enables the
# crate's own cargo feature, it does not tell dx which platform to build for.
dx build --release --platform web --features web 2>&1 | tee "$log"

# 2. dx exits 0 even when wasm-opt aborts, so its log is the only signal.
if grep -qi "wasm-opt failed" "$log"; then
    echo >&2
    echo "error: wasm-opt failed — the bundle is UNOPTIMIZED and its size is not a fair figure." >&2
    echo "       See the [profile.wasm-release] comment in Cargo.toml." >&2
    exit 1
fi

assets="target/dx/otto-ui-dioxus/release/web/public/assets"
[ -d "$assets" ] || { echo "error: expected asset dir not found: $assets" >&2; exit 1; }

mapfile -t wasms < <(find "$assets" -maxdepth 1 -name '*.wasm' -type f)
[ "${#wasms[@]}" -eq 1 ] || {
    echo "error: expected exactly one .wasm in $assets, found ${#wasms[@]}:" >&2
    printf '  %s\n' "${wasms[@]}" >&2
    exit 1
}
wasm="${wasms[0]}"

# 3. DWARF in the shipped wasm means the Cargo.toml `strip` is gone — wasm-opt will have aborted.
if grep -aq '\.debug_info' "$wasm"; then
    echo >&2
    echo "error: $wasm still contains DWARF (.debug_info) — the [profile.wasm-release] strip" >&2
    echo "       setting in Cargo.toml is missing or ineffective." >&2
    exit 1
fi

# 4. The one guard that cannot fail open.
wasm_bytes=$(wc -c <"$wasm")
if [ "$wasm_bytes" -gt "$MAX_WASM_BYTES" ]; then
    echo >&2
    echo "error: wasm is $wasm_bytes B, over the $MAX_WASM_BYTES B ceiling — wasm-opt most likely" >&2
    echo "       did not run. If this is genuine growth, raise MAX_WASM_BYTES in this script." >&2
    exit 1
fi

echo
echo "==> bundle: $assets"
printf '%-46s %12s %12s\n' "FILE" "RAW" "GZIP(-9)"
total_raw=0
total_gz=0
for f in "$wasm" "$assets"/*.js "$assets"/*.css; do
    [ -f "$f" ] || continue
    raw=$(wc -c <"$f")
    gz=$(gzip -9 -c "$f" | wc -c)
    total_raw=$((total_raw + raw))
    total_gz=$((total_gz + gz))
    printf '%-46s %12s %12s\n' "$(basename "$f")" "$raw" "$gz"
done
printf '%-46s %12s %12s\n' "TOTAL" "$total_raw" "$total_gz"
echo
echo "(dir: $assets)"
