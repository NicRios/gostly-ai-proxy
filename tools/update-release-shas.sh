#!/usr/bin/env bash
# update-release-shas.sh — pull SHA256 sidecars from a release, splice into
# Formula/gostly.rb and bucket/gostly.json.
#
# Run this AFTER a `v*` tag has triggered the release workflow and
# produced uploaded assets:
#
#   bash tools/update-release-shas.sh v0.1.0
#
# What it does:
#   1. Downloads gostly-proxy-{linux,darwin,windows}-{amd64,arm64}.{tar.gz,zip}.sha256
#      from the GitHub release for the given tag.
#   2. Patches the per-platform `sha256 "TODO_..."` lines in Formula/gostly.rb.
#   3. Patches the `hash` field in bucket/gostly.json.
#   4. Patches the `version` and URL strings in both files to the new tag.
#   5. Leaves you with an unstaged diff to review and commit. The commit +
#      PR step is intentionally manual so a human eyeballs the values
#      before downstream channels start serving them.
#
# Requires: curl, jq.

set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <tag, e.g. v0.1.0>" >&2
  exit 2
fi

TAG="$1"
VERSION="${TAG#v}"
REPO="NicRios/gostly-ai-proxy"
ROOT="$(git rev-parse --show-toplevel)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PLATFORMS=(
  "linux-amd64.tar.gz"
  "linux-arm64.tar.gz"
  "darwin-amd64.tar.gz"
  "darwin-arm64.tar.gz"
  "windows-amd64.zip"
)

declare -A SHAS

for p in "${PLATFORMS[@]}"; do
  url="https://github.com/${REPO}/releases/download/${TAG}/gostly-proxy-${p}.sha256"
  echo "Fetching $url ..."
  curl -fsSL -o "$TMP/${p}.sha256" "$url"
  # The .sha256 file is `<hash>  <filename>` — first token only.
  SHAS["$p"]="$(awk '{print $1}' "$TMP/${p}.sha256")"
done

echo
echo "Resolved SHAs for $TAG:"
for p in "${PLATFORMS[@]}"; do
  printf '  %-22s %s\n' "$p" "${SHAS[$p]}"
done
echo

# ── Patch Formula/gostly.rb ─────────────────────────────────────────────────
FORMULA="$ROOT/Formula/gostly.rb"
echo "Patching $FORMULA ..."

sed -i.bak \
  -e "s|version \"[^\"]*\"|version \"$VERSION\"|" \
  -e "s|releases/download/v[^/]*/|releases/download/$TAG/|g" \
  "$FORMULA"

# Patch SHA256 values per-platform by walking the formula in Python — sed
# can't reliably know which sha256 line follows which url. Python keeps
# this idempotent across releases (initial placeholders, last release's
# values, or any combination).
python3 - "$FORMULA" "${SHAS[darwin-arm64.tar.gz]}" "${SHAS[darwin-amd64.tar.gz]}" "${SHAS[linux-arm64.tar.gz]}" "${SHAS[linux-amd64.tar.gz]}" <<'PY'
import re, sys
path, dar, dam, lar, lam = sys.argv[1:]
text = open(path).read()
# Replace each `sha256 "..."` line based on the immediately-preceding
# `url "...PLATFORM..."` line, since brew formulas list them in pairs.
def patch(text, platform_marker, new_sha):
    pat = re.compile(
        r'(url "[^"]*' + re.escape(platform_marker) + r'[^"]*"\s*\n\s*sha256 ")[0-9a-f]{64}(")',
        flags=re.MULTILINE,
    )
    new_text, n = pat.subn(r'\g<1>' + new_sha + r'\g<2>', text)
    if n != 1:
        print(f"WARN: expected 1 match for {platform_marker}, got {n}", file=sys.stderr)
    return new_text
text = patch(text, "darwin-arm64", dar)
text = patch(text, "darwin-amd64", dam)
text = patch(text, "linux-arm64",  lar)
text = patch(text, "linux-amd64",  lam)
open(path, "w").write(text)
print("formula patched")
PY

rm -f "$FORMULA.bak"

# ── Patch bucket/gostly.json ─────────────────────────────────────────────────
SCOOP="$ROOT/bucket/gostly.json"
echo "Patching $SCOOP ..."

python3 - "$SCOOP" "$VERSION" "$TAG" "${SHAS[windows-amd64.zip]}" <<'PY'
import json, sys
path, version, tag, win_hash = sys.argv[1:]
with open(path) as f:
    m = json.load(f)
m["version"] = version
m["architecture"]["64bit"]["url"] = f"https://github.com/NicRios/gostly-ai-proxy/releases/download/{tag}/gostly-proxy-windows-amd64.zip"
m["architecture"]["64bit"]["hash"] = win_hash
with open(path, "w") as f:
    json.dump(m, f, indent=2)
    f.write("\n")
print("scoop manifest patched")
PY

echo
echo "Done. Review the diff and open a PR:"
echo "  git diff Formula/gostly.rb bucket/gostly.json"
