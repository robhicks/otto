#!/bin/sh
# otto installer — fetch a prebuilt release binary and drop it on PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/robhicks/otto/main/deploy/install.sh | sh
#
# Environment overrides:
#   OTTO_VERSION   release tag to fetch, e.g. v0.3.0 (default: latest)
#   OTTO_BINDIR    install directory (default: $HOME/.local/bin)
#   OTTO_BASE_URL  download base, for mirrors (default: GitHub releases)
#
# Requires: curl, tar, and sha256sum (Linux) or shasum (macOS). The archive is verified against
# the per-archive `.sha256` checksum published with the release before anything is written.

set -eu

OTTO_VERSION="${OTTO_VERSION:-latest}"
OTTO_BINDIR="${OTTO_BINDIR:-}"
OTTO_BASE_URL="${OTTO_BASE_URL:-https://github.com/robhicks/otto/releases}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# Map the host to the release archive triple. Exposed as a function so the mapping is testable.
target_triple() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)
            os_triple="unknown-linux-gnu"
            # glibc hosts fetch the glibc build; musl hosts (Alpine et al.) need the static musl
            # build. Both Linux triples are published by the release workflow.
            if [ -f /etc/alpine-release ] || (ldd --version 2>/dev/null | grep -qi musl); then
                os_triple="unknown-linux-musl"
            fi
            ;;
        Darwin) os_triple="apple-darwin" ;;
        *) die "unsupported OS: $os (only Linux and macOS releases are built)" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch_triple="x86_64" ;;
        aarch64 | arm64) arch_triple="aarch64" ;;
        *) die "unsupported architecture: $arch" ;;
    esac
    printf '%s' "${arch_triple}-${os_triple}"
}

# Hidden self-test: print the target for this host (used by CI and debugging).
if [ "${OTTO_PRINT_TARGET:-0}" = "1" ]; then
    target_triple
    exit 0
fi

target="$(target_triple)"

if [ -z "$OTTO_BINDIR" ]; then
    if [ -n "${HOME:-}" ] && [ -d "$HOME" ]; then
        OTTO_BINDIR="$HOME/.local/bin"
    else
        OTTO_BINDIR="/usr/local/bin"
    fi
fi

if [ "$OTTO_VERSION" = "latest" ]; then
    release_url="$OTTO_BASE_URL/latest/download"
else
    release_url="$OTTO_BASE_URL/download/$OTTO_VERSION"
fi
archive="otto-$target.tar.gz"
# The release publishes the checksum as `otto-<target>.sha256` — the target stem WITHOUT the
# `.tar.gz` suffix. Deriving it as "$archive.sha256" 404s and, since verification fails closed,
# breaks every install.
checksum="otto-$target.sha256"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/otto-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

say "downloading otto ($target, $OTTO_VERSION)…"
curl -fsSL "$release_url/$archive" -o "$tmp/$archive" || die "download failed ($release_url/$archive)"
curl -fsSL "$release_url/$checksum" -o "$tmp/$checksum" || die "checksum file not found"

expected="$(awk '{print $1}' "$tmp/$checksum" || true)"
[ -n "$expected" ] || die "no checksum for $archive"
actual="$(
    (sha256sum "$tmp/$archive" 2>/dev/null || shasum -a 256 "$tmp/$archive") |
        awk '{print $1}'
)"
[ "$actual" = "$expected" ] || die "checksum mismatch for $archive"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$OTTO_BINDIR"
binary="$tmp/otto-$target/otto"
[ -f "$binary" ] || binary="$tmp/otto"
[ -f "$binary" ] || die "otto binary not found inside $archive"
cp "$binary" "$OTTO_BINDIR/otto"
chmod +x "$OTTO_BINDIR/otto"

say "installed otto to $OTTO_BINDIR/otto"
"$OTTO_BINDIR/otto" --version

case ":$PATH:" in
    *":$OTTO_BINDIR:"*) ;;
    *)
        say "note: $OTTO_BINDIR is not on your PATH — add it, e.g.:"
        say "  export PATH=\"\$HOME/.local/bin:\$PATH\""
        ;;
esac
