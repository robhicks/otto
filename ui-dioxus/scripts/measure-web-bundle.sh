#!/usr/bin/env bash
#
# Report the size of the Dioxus web release bundle.
#
#   cd ui-dioxus && ./scripts/measure-web-bundle.sh
#
# This is the sanctioned way to produce a bundle-size figure for this crate. The build itself —
# and the four guards that make a reported figure trustworthy — live in build-web.sh, which this
# script calls. They are not duplicated here on purpose: two copies would drift, and a drifted
# guard means silently quoting a size taken from an unoptimized bundle.
#
# So the contract is: if build-web.sh exits non-zero, no number is printed at all.
#
set -euo pipefail

cd "$(dirname "$0")/.."

# stdout of build-web.sh is exactly the bundle dir; its progress output already went to stderr.
public=$(./scripts/build-web.sh)
assets="$public/assets"

# Every emitted asset, not just wasm/js/css — a future item (e.g. web syntax highlighting via JS
# interop) can add an `assets/snippets/` tree, and a TOTAL that quietly skipped it would be an
# understatement waiting to be quoted.
echo
echo "==> bundle: $assets"
printf '%-46s %12s %12s\n' "FILE" "RAW" "GZIP(-9)"
total_raw=0
total_gz=0
while IFS= read -r f; do
    raw=$(( $(wc -c <"$f") ))
    gz=$(( $(gzip -9 -c "$f" | wc -c) ))
    total_raw=$((total_raw + raw))
    total_gz=$((total_gz + gz))
    printf '%-46s %12s %12s\n' "$(basename "$f")" "$raw" "$gz"
done <<EOF
$(find "$assets" -type f | sort)
EOF
printf '%-46s %12s %12s\n' "TOTAL" "$total_raw" "$total_gz"
echo
echo "(dir: $assets — excludes the generated index.html, which lives one level up in public/)"
