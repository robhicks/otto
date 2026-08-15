# Homebrew formula for otto.
#
# Installs the prebuilt release binary (no bottle; builds are downloaded from the GitHub release
# attached to the tag in `url`). The release workflow names archives `otto-<target>.tar.gz` (see
# .github/workflows/release.yml); this formula must track the *current* release's sha256 below.
#
#     brew install --formula https://raw.githubusercontent.com/robhicks/otto/main/deploy/homebrew/otto.rb
#
# NOTE: this file is generated. The release workflow's `bump-homebrew` job rewrites `version`, the
# four `url`s, and the four sha256s from each release's `otto-<target>.sha256` sidecars and commits
# the result to `main` (see .github/workflows/release.yml and deploy/update-homebrew-formula.py).
# Hand-edits to those fields will be overwritten by the next release; everything else here is
# hand-maintained and is left alone.

class Otto < Formula
  desc "Agentic coding engine: a deterministic orchestrator drives a spine of atomic agents"
  homepage "https://github.com/robhicks/otto"
  version "0.5.0"
  license "MIT OR Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/robhicks/otto/releases/download/v0.5.0/otto-aarch64-apple-darwin.tar.gz"
      sha256 "326e84b1ced6d9b15af5cdc400c03bfd20f71740697adbbc167dd9508c7dd9d2"
    end
    on_intel do
      url "https://github.com/robhicks/otto/releases/download/v0.5.0/otto-x86_64-apple-darwin.tar.gz"
      sha256 "53c7d442153f46e38f1d04f1ed67c49448de37ba3adaff6efa9f2fc0d22b8eeb"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/robhicks/otto/releases/download/v0.5.0/otto-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "2619570f1127f507bd93f050b1def05de6cf302427175ed52c576927f5a0e566"
    end
    on_intel do
      url "https://github.com/robhicks/otto/releases/download/v0.5.0/otto-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "2e35e476899ed1e7320831dade9fe8ea1c24c58f019715eae1b004c88f2971c6"
    end
  end

  # The archive holds `otto` at its root; also accept a nested `otto-<target>/otto` so the formula
  # survives a switch to the directory-prefixed cargo-binstall layout.
  def install
    binary = Dir["otto-*/otto", "otto"].find { |f| File.file?(f) }
    raise "otto binary not found in #{buildpath}" unless binary

    bin.install binary => "otto"
  end

  test do
    assert_match "otto ", shell_output("#{bin}/otto --version")
  end
end
