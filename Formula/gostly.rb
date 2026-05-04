# Homebrew formula for the gostly recording proxy.
#
# This formula lives in-tree for v1; users install via:
#
#   brew tap nicrios/gostly https://github.com/NicRios/gostly-ai-proxy
#   brew install gostly
#
# After every `v*` tag-triggered release, the SHA256 lines below MUST be
# replaced with the real per-platform values. The release workflow uploads
# `*.sha256` sidecar files alongside each tarball; use:
#
#   bash tools/update-release-shas.sh v0.1.0
#
# to fetch them and patch the formula. The current `0000…` values are
# obvious-fake placeholders that pass `brew style` syntactically but will
# fail any download attempt — the formula CANNOT install successfully
# until those values are replaced post-release.
class Gostly < Formula
  desc "OSS recording proxy — record, mock, replay HTTP traffic"
  homepage "https://gostly.ai"
  version "0.1.1"
  license "FSL-1.1-Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/NicRios/gostly-ai-proxy/releases/download/v0.1.1/gostly-proxy-darwin-arm64.tar.gz"
      sha256 "6160d7d6d8e9c83dbab7d67af56a56d3ed1f769162c53aa9036c1cd51ae6da01"
    end
    on_intel do
      url "https://github.com/NicRios/gostly-ai-proxy/releases/download/v0.1.1/gostly-proxy-darwin-amd64.tar.gz"
      sha256 "8e283fcd454be33c67a03ae5ccc943b3a4753b38963c0b17f2ca7ab6e65f2c3c"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/NicRios/gostly-ai-proxy/releases/download/v0.1.1/gostly-proxy-linux-arm64.tar.gz"
      sha256 "761e980f04f127708961de3d003c12bda83b007b169023972c394aa21469ea31"
    end
    on_intel do
      url "https://github.com/NicRios/gostly-ai-proxy/releases/download/v0.1.1/gostly-proxy-linux-amd64.tar.gz"
      sha256 "03c8e610c6ec75bdf8144998d38cab5ccc9ca523a679829babae467c61dd0435"
    end
  end

  def install
    # Tarballs ship the binary as `gostly-proxy`; install it as `gostly`
    # so the user-facing command matches docs and feels native.
    bin.install "gostly-proxy" => "gostly"
  end

  test do
    assert_match "gostly", shell_output("#{bin}/gostly --version")
  end
end
