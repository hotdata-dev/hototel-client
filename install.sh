#!/bin/sh
# hotusage collector installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/hotdata-dev/hotusage-collector/main/install.sh | sh
#
# Downloads the latest release binary for this machine, puts it on PATH, and
# registers it as a background agent (macOS LaunchAgent / systemd user unit).
set -eu

REPO=hotdata-dev/hotusage-collector
BIN=hotusage-collector

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Darwin) suffix=macos-universal.tar.gz ;;
  Linux)
    case "$arch" in
      x86_64 | amd64) suffix=linux-x86_64.tar.gz ;;
      *)
        echo "error: no prebuilt Linux $arch binary; build from source with: cargo build --release" >&2
        exit 1 ;;
    esac ;;
  *)
    echo "error: unsupported OS '$os' (macOS and Linux only)" >&2
    exit 1 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "downloading the latest $BIN release ($os $arch)..."
# resolve the latest tag, then fetch the matching asset by name
tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
  sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
[ -n "$tag" ] || { echo "error: could not resolve the latest release tag" >&2; exit 1; }
asset="$BIN-${tag#v}-$suffix"
curl -fsSL -o "$tmp/$asset" \
  "https://github.com/$REPO/releases/download/$tag/$asset" || {
  echo "error: no release asset $asset in $tag" >&2
  exit 1
}
tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/$BIN" ] || { echo "error: release archive did not contain $BIN" >&2; exit 1; }
chmod +x "$tmp/$BIN"
# downloads can carry macOS quarantine; the binary is unsigned, so clear it
[ "$os" = Darwin ] && xattr -d com.apple.quarantine "$tmp/$BIN" 2>/dev/null || true

# install without sudo when possible
if [ -w /usr/local/bin ] 2>/dev/null; then
  dest=/usr/local/bin
else
  dest="$HOME/.local/bin"
  mkdir -p "$dest"
fi
mv "$tmp/$BIN" "$dest/$BIN"
echo "installed $dest/$BIN"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "note: $dest is not on your PATH - add it to your shell profile" ;;
esac

# register the login/background agent (idempotent; re-running re-points it)
"$dest/$BIN" install

cat <<TXT

next: set your identity and the shared ingest token in
  ~/.hotusage/collector.json      (server_url defaults to https://hotusage.ai)
then sync immediately with:
  $dest/$BIN --once
TXT
