# Homebrew formula for otto.
#
# Installs the prebuilt release binary (no bottle; builds are downloaded from the GitHub release
# attached to the tag in `url`). The release workflow names archives `otto-<target>.tar.gz` (see
# .github/workflows/release.yml); this formula must track the *current* release's sha256 below.
#
#     brew install --formula https://raw.githubusercontent.com/robhicks/otto/main/deploy/homebrew/otto.rb
#
# NOTE: the sha256 values are filled in when the first release is cut — install from the curl
# script (deploy/install.sh) or `cargo binstall otto` until then.

class Otto < Formula
  desc "Agentic coding engine: a deterministic orchestrator drives a spine of atomic agents"
  homepage "https://github.com/robhicks/otto"
  version "0.1.0"
  license "MIT OR Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/robhicks/otto/releases/download/v0.1.0/otto-aarch64-apple-darwin.tar.gz"
      sha256 "TODO-fill-in-at-release"
    end
    on_intel do
      url "https://github.com/robhicks/otto/releases/download/v0.1.0/otto-x86_64-apple-darwin.tar.gz"
      sha256 "TODO-fill-in-at-release"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/robhicks/otto/releases/download/v0.1.0/otto-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "TODO-fill-in-at-release"
    end
    on_intel do
      url "https://github.com/robhicks/otto/releases/download/v0.1.0/otto-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "TODO-fill-in-at-release"
    end
  end

  # The archive nests the binary under `otto-<target>/` (cargo-binstall convention); accept that
  # or a bare `otto` so the formula is independent of the exact archive layout.
  def install
    binary = Dir["otto-*/otto", "otto"].find { |f| File.file?(f) }
    raise "otto binary not found in #{buildpath}" unless binary

    bin.install binary => "otto"
  end

  test do
    assert_match "otto ", shell_output("#{bin}/otto --version")
  end
end
