# Homebrew formula template for the tap yuhaoxin/homebrew-tap.
#
# cargo-dist writes its own generated formula into the tap on release, filling
# in the version and the two per-target sha256 values; this file documents the
# expected shape and is the copy to use when the tap is bootstrapped by hand.
# Replace VERSION, ARM64_SHA256 and X86_64_SHA256 with the values from the
# release assets. No bottle is published: the formula wraps the release tarball.
class ManAi < Formula
  desc "Translate rendered man pages into a target language"
  homepage "https://github.com/yuhaoxin/man-ai"
  version "VERSION"
  license "MIT"

  if Hardware::CPU.arm?
    url "https://github.com/yuhaoxin/man-ai/releases/download/v#{version}/man-ai-v#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "ARM64_SHA256"
  else
    url "https://github.com/yuhaoxin/man-ai/releases/download/v#{version}/man-ai-v#{version}-x86_64-apple-darwin.tar.gz"
    sha256 "X86_64_SHA256"
  end

  def install
    bin.install "man-ai"
  end

  def caveats
    <<~EOS
      man-ai is not enabled automatically. Add one of the following to your
      shell startup file; you do not need to change a MANPAGER you already set.

      Export MANPAGER (applies to every invocation that goes through a pager):

        export MANPAGER=man-ai

      Or define a shell function (interactive shells only):

        man() { MANPAGER=man-ai command man "$@"; }

      If man-ai may not come first in PATH, use its absolute path:

        export MANPAGER="$(command -v man-ai)"

      Details: https://github.com/yuhaoxin/man-ai/blob/main/docs/install.md
    EOS
  end

  test do
    assert_match "man-ai #{version}", shell_output("#{bin}/man-ai --version")
  end
end
