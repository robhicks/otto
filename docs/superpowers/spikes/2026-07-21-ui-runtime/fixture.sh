#!/usr/bin/env bash
# Creates the disposable workspace fixture for the 2026-07-21 UI runtime spike.
# Usage: fixture.sh <target-dir>
set -euo pipefail

target="${1:?usage: fixture.sh <target-dir>}"
rm -rf "$target"
mkdir -p "$target/src/util"

cat > "$target/Cargo.toml" <<'EOF'
[package]
name = "otto-spike-fixture"
version = "0.1.0"
edition = "2021"

[dependencies]
EOF

cat > "$target/src/lib.rs" <<'EOF'
pub mod util;

pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_sums_two_numbers() {
        assert_eq!(add(2, 3), 5);
    }
}
EOF

cat > "$target/src/util/mod.rs" <<'EOF'
/// Doubles a value. Lives in a nested directory so the workspace tree has
/// something to expand and collapse.
pub fn double(n: i64) -> i64 {
    n * 2
}
EOF

# Sensitive-path floor probe: must be listed by the tree but never openable.
cat > "$target/.env" <<'EOF'
FAKE_SECRET=not-a-real-secret
EOF

cat > "$target/README.md" <<'EOF'
# otto spike fixture

Disposable workspace for the 2026-07-21 UI runtime spike. Not a real project.
EOF

cd "$target"
git init -q
git add -A
git -c user.email=spike@example.com -c user.name=spike commit -q -m "fixture"
echo "fixture ready: $target"
