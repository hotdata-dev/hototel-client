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

# Verify against the release's SHA256SUMS. The binaries are unsigned, so this
# is the only check that the archive is the one CI built; refuse to install
# anything that does not match, and refuse to skip the check silently.
if curl -fsSL -o "$tmp/SHA256SUMS" \
     "https://github.com/$REPO/releases/download/$tag/SHA256SUMS"; then
  if command -v sha256sum >/dev/null 2>&1; then
    sum=$(sha256sum "$tmp/$asset" | cut -d" " -f1)
  else
    sum=$(shasum -a 256 "$tmp/$asset" | cut -d" " -f1)
  fi
  want=$(grep " $asset\$\|  $asset\$" "$tmp/SHA256SUMS" | cut -d" " -f1 | head -1)
  if [ -z "$want" ]; then
    echo "error: $asset is not listed in SHA256SUMS for $tag" >&2
    exit 1
  fi
  if [ "$sum" != "$want" ]; then
    echo "error: checksum mismatch for $asset" >&2
    echo "  expected $want" >&2
    echo "  got      $sum" >&2
    exit 1
  fi
  echo "checksum ok"
else
  # releases cut before SHA256SUMS existed have nothing to check against
  echo "warning: $tag publishes no SHA256SUMS - installing unverified" >&2
fi

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

# Sign in straight away: opens the browser, waits for approval, then syncs.
# A no-op when this machine is already signed in (upgrades, re-runs). Failure
# is not fatal -- the binary is installed either way, so say how to retry.
echo
if [ ! -t 1 ]; then
  # no terminal: a CI or Dockerfile install has no browser and nobody to
  # approve, and signin would block until the request expires
  cat <<TXT
installed. sign in when a browser is available:
  $dest/$BIN signin
TXT
elif "$dest/$BIN" signin; then
  cat <<TXT

done - usage syncs every 15 minutes from now on.
TXT
else
  cat <<TXT

sign-in did not complete. run this when you are ready:
  $dest/$BIN signin
TXT
fi
