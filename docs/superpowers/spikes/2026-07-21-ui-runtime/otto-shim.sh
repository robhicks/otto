#!/usr/bin/env bash
# Sidecar wrapper for the 2026-07-21 UI runtime spike. Both desktop apps spawn
# `otto serve --root <picked> --port 8787` with no capability flags; this appends the
# two the scenario needs so desktop can reach steps 9 and 11. No app code is changed.
set -euo pipefail
real="${OTTO_REAL_BIN:-/home/robhicks/dev/otto-next/target/release/otto}"
echo "otto-shim: exec $real $* --approve-edits --promote-loopback" >&2
exec "$real" "$@" --approve-edits --promote-loopback
