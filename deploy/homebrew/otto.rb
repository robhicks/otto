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
  version "0.4.0"
  license "MIT OR Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/robhicks/otto/releases/download/v0.4.0/otto-aarch64-apple-darwin.tar.gz"
      sha256 "2b6b9a7e4f43fef11d769cba7f45923b24db565fc4c9309bfc11f0a14e15e815"
    end
    on_intel do
      url "https://github.com/robhicks/otto/releases/download/v0.4.0/otto-x86_64-apple-darwin.tar.gz"
      sha256 "112f4c4375231095ec95686ffde9a9db805c0fd75c1c2a7a4496918d526a201b"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/robhicks/otto/releases/download/v0.4.0/otto-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "e18d170f37b131bd1e6200b30803160d223c93ca85459ae2fc18152eb7c47926"
    end
    on_intel do
      url "https://github.com/robhicks/otto/releases/download/v0.4.0/otto-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "2d3cae6a269657d1d354b63da0f97e4c67d17e28e05a5d66f9a1998876795462"
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
