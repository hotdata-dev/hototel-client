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
# downloads can carry macOS quarantine. Signed+notarized releases pass
# Gatekeeper on their own; clearing it also covers unsigned builds and machines
# that cannot reach Apple to verify the notarization ticket.
[ "$os" = Darwin ] && xattr -d com.apple.quarantine "$tmp/$BIN" 2>/dev/null || true

# install without sudo when possible
if [ -w /usr/local/bin ] 2>/dev/null; then
  dest=/usr/local/bin
else
  dest="$HOME/.local/bin"
  mkdir -p "$dest"
fi
# unlink first: a cross-device mv falls back to a copy, and writing over a
# RUNNING executable fails with ETXTBSY on Linux (every re-run while the
# agent is up). Removing the old inode leaves the running process untouched.
rm -f "$dest/$BIN"
mv "$tmp/$BIN" "$dest/$BIN"
echo "installed $dest/$BIN"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "note: $dest is not on your PATH - add it to your shell profile" ;;
esac

# register the login/background agent (idempotent; re-running re-points it).
# Non-fatal: `install` needs a desktop/systemd user session, which a plain SSH
# shell does not have -- the binary is still usable, so keep going and print
# the next steps rather than aborting under `set -e`.
"$dest/$BIN" install || echo "note: agent registration failed (no desktop/systemd session?) - run '$dest/$BIN install' from a login session" >&2

cat <<TXT

next: set your identity and the shared ingest token in
  ~/.hotusage/collector.json      (server_url defaults to https://hotusage.ai)
then sync immediately with:
  $dest/$BIN --once
TXT
