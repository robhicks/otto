#!/usr/bin/env python3
"""Rewrite the Homebrew formula for a release, from that release's own checksum sidecars.

Usage:  update-homebrew-formula.py <tag> <sha-dir> [formula]

`<tag>` is the release tag (`v0.4.0`). `<sha-dir>` holds one `otto-<target>.sha256` per
Homebrew target, as produced by the release workflow's `checksum: sha256`. The formula path
defaults to `deploy/homebrew/otto.rb`.

Kept as a standalone script — rather than inline `run:` YAML in the release workflow — so the
transform can be exercised locally against real fixtures. A formula bug otherwise surfaces only
when a tag fires, which is the worst possible time to find one: `brew install` is already broken
for everyone by then.

The release builds six targets; only these four are Homebrew targets. The two musl archives are
published as release assets for Alpine/old-glibc users and are deliberately not served here.

Fails loudly and writes nothing unless every substitution lands. A half-updated formula — new
URLs against old checksums — fails `brew install` for everyone, which is strictly worse than a
formula that is merely a release behind.
"""

from __future__ import annotations

import pathlib
import re
import sys

TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)

SHA_RE = re.compile(r"\b([0-9a-f]{64})\b")


def read_sha(sha_dir: pathlib.Path, target: str) -> str:
    """The hex digest for `target`.

    Accepts either a bare digest or `sha256sum` output (`<digest>  <filename>`), since which one
    a checksum producer emits is a detail we should not be coupled to.
    """
    path = sha_dir / f"otto-{target}.sha256"
    if not path.is_file():
        raise SystemExit(f"error: missing checksum sidecar {path}")
    match = SHA_RE.search(path.read_text())
    if not match:
        raise SystemExit(f"error: no sha256 digest found in {path}")
    return match.group(1)


def rewrite(text: str, tag: str, version: str, shas: dict[str, str]) -> str:
    """Return `text` with the version line and every target's url+sha256 pair updated.

    Each target's URL and checksum are matched as one pattern spanning both lines, so a
    substitution cannot pair a new URL with a stale digest — the failure mode this whole script
    exists to prevent.
    """
    text, count = re.subn(
        r'^(  version ")[^"]+(")$',
        rf"\g<1>{version}\g<2>",
        text,
        count=1,
        flags=re.MULTILINE,
    )
    if count != 1:
        raise SystemExit("error: could not find the formula's `version` line")

    for target, sha in shas.items():
        pattern = (
            r'(url "https://github\.com/robhicks/otto/releases/download/)[^/"]+'
            rf'(/otto-{re.escape(target)}\.tar\.gz"\s*\n\s*sha256 ")[0-9a-f]{{64}}(")'
        )
        text, count = re.subn(pattern, rf"\g<1>{tag}\g<2>{sha}\g<3>", text, count=1)
        if count != 1:
            raise SystemExit(f"error: could not find the url/sha256 pair for {target}")

    return text


def main(argv: list[str]) -> int:
    if not 3 <= len(argv) <= 4:
        raise SystemExit(__doc__)

    tag = argv[1]
    if not tag.startswith("v"):
        raise SystemExit(f"error: tag {tag!r} does not start with 'v'")
    version = tag[1:]

    sha_dir = pathlib.Path(argv[2])
    formula = pathlib.Path(argv[3] if len(argv) == 4 else "deploy/homebrew/otto.rb")
    if not formula.is_file():
        raise SystemExit(f"error: formula not found at {formula}")

    shas = {target: read_sha(sha_dir, target) for target in TARGETS}
    original = formula.read_text()
    updated = rewrite(original, tag, version, shas)

    if updated == original:
        print(f"{formula} already describes {tag}; nothing to do")
        return 0

    formula.write_text(updated)
    print(f"{formula} updated to {tag}")
    for target, sha in shas.items():
        print(f"  {target}  {sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
