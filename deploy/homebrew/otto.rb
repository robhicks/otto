# Homebrew formula for otto.
#
# Installs the prebuilt release binary (no bottle; builds are downloaded from the GitHub release
# attached to the tag in `url`). The release workflow names archives `otto-<target>.tar.gz` (see
# .github/workflows/release.yml); this formula must track the *current* release's sha256 below.
#
#     brew install --formula https://raw.githubusercontent.com/robhicks/otto/main/deploy/homebrew/otto.rb
#
# NOTE: when cutting a new release, bump `version`/the tag in each `url` and replace every sha256
# with the value from that release's `otto-<target>.sha256` sidecar.

class Otto < Formula
  desc "Agentic coding engine: a deterministic orchestrator drives a spine of atomic agents"
  homepage "https://github.com/robhicks/otto"
  version "0.3.0"
  license "MIT OR Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/robhicks/otto/releases/download/v0.3.0/otto-aarch64-apple-darwin.tar.gz"
      sha256 "6fffd5eccb9e1c66194ba4e12de995fe3eef9ef68f3ca255f016ab1edede937e"
    end
    on_intel do
      url "https://github.com/robhicks/otto/releases/download/v0.3.0/otto-x86_64-apple-darwin.tar.gz"
      sha256 "0c1694a19cba898107a3bfd7fa112bc3a452dcfe7130ed1c5fa7da950f4a874e"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/robhicks/otto/releases/download/v0.3.0/otto-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "35d314e44e7ec98ca3c7521cfacc4675ac7b421794ca360217496993234082ad"
    end
    on_intel do
      url "https://github.com/robhicks/otto/releases/download/v0.3.0/otto-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0f56236ecf9e6fbe8fa89a65459059bc3ed77bd7799d27517128c0104f512887"
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
