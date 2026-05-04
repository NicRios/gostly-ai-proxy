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
  version "0.2.0"
  license "FSL-1.1-Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/NicRios/gostly-ai-proxy/releases/download/v0.2.0/gostly-proxy-darwin-arm64.tar.gz"
      sha256 "bdd1a0d64b50387b73e180468cc57251ea7a88a49fe437755a6f5f894fa79270"
    end
    on_intel do
      url "https://github.com/NicRios/gostly-ai-proxy/releases/download/v0.2.0/gostly-proxy-darwin-amd64.tar.gz"
      sha256 "5dea3dfae3f783627af688b123d0fa4d52fbfde577cee6090b337e648aa58371"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/NicRios/gostly-ai-proxy/releases/download/v0.2.0/gostly-proxy-linux-arm64.tar.gz"
      sha256 "e2ecc368f422b7fa572707f0ebb27e7009df4fec63a2e1847cca07d0207e61f5"
    end
    on_intel do
      url "https://github.com/NicRios/gostly-ai-proxy/releases/download/v0.2.0/gostly-proxy-linux-amd64.tar.gz"
      sha256 "85bbf941d95ee84105f9b3b8522eb37e6e8c352ef3aed5477671fe1a6748226f"
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
