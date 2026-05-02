#!/usr/bin/env bash
# install.sh — one-line installer for the gostly recording proxy.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/NicRios/gostly-ai-proxy/main/install.sh | bash
#
# Environment overrides:
#   GOSTLY_VERSION       — pin to a specific tag (default: latest GitHub release)
#   GOSTLY_INSTALL_DIR   — install destination (default: /usr/local/bin)
#
# Exit codes:
#   0 — installed
#   1 — unsupported OS/arch, network failure, or extraction failure

set -euo pipefail

REPO="NicRios/gostly-ai-proxy"
VERSION="${GOSTLY_VERSION:-latest}"
INSTALL_DIR="${GOSTLY_INSTALL_DIR:-/usr/local/bin}"

OS="$(uname | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$ARCH" in
  x86_64|amd64)        ARCH=amd64 ;;
  aarch64|arm64)       ARCH=arm64 ;;
  *) echo "ERROR: Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

case "$OS" in
  darwin|linux) ;;
  *) echo "ERROR: Unsupported OS: $OS — Windows users should use Scoop (scoop install gostly)" >&2; exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
              | grep '"tag_name"' \
              | head -n1 \
              | cut -d'"' -f4)"
  if [ -z "$VERSION" ]; then
    echo "ERROR: could not resolve latest release tag from github.com/${REPO}" >&2
    exit 1
  fi
fi

URL="https://github.com/${REPO}/releases/download/${VERSION}/gostly-proxy-${OS}-${ARCH}.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $URL ..."
if ! curl -fsSL "$URL" | tar -xz -C "$TMP"; then
  echo "ERROR: download or extraction failed for $URL" >&2
  exit 1
fi

# Need root to write into /usr/local/bin on most distros; gracefully fall
# back to non-sudo when the user has write access (e.g. macOS Homebrew prefix
# or a custom GOSTLY_INSTALL_DIR under $HOME).
if [ -w "$INSTALL_DIR" ]; then
  install -m 755 "$TMP/gostly-proxy" "$INSTALL_DIR/gostly"
else
  sudo install -m 755 "$TMP/gostly-proxy" "$INSTALL_DIR/gostly"
fi

echo "Installed gostly $VERSION to $INSTALL_DIR/gostly"
echo
echo "Next steps:"
echo "  gostly --help              # see all commands"
echo "  gostly start --help        # start the recording proxy"
