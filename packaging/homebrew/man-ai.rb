# tap yuhaoxin/homebrew-tap 的 Homebrew formula 模板。
#
# 发布时 cargo-dist 会把自己的 formula 写进 tap，填入版本与两个 target 的
# sha256；本文件说明期望形态，也是手工初始化 tap 时的副本。把 VERSION、
# ARM64_SHA256、X86_64_SHA256 换成 release 资产里的值。不发布 bottle：
# formula 直接引用 release tarball。
class ManAi < Formula
  desc "Translate rendered man pages into a target language"
  homepage "https://github.com/yuhaoxin/man-ai"
  version "VERSION"
  license "MIT"

  if Hardware::CPU.arm?
    url "https://github.com/yuhaoxin/man-ai/releases/download/v#{version}/man-ai-aarch64-apple-darwin.tar.gz"
    sha256 "ARM64_SHA256"
  else
    url "https://github.com/yuhaoxin/man-ai/releases/download/v#{version}/man-ai-x86_64-apple-darwin.tar.gz"
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
