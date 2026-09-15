#!/bin/sh
# hotusage installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/hotdata-dev/hototel-client/main/install.sh | sh
#
# Downloads the latest release binary for this machine, puts it on PATH,
# registers it as a background agent (macOS LaunchAgent / systemd user unit),
# and installs the agent skill so Claude Code and Codex can answer questions
# about your organization's usage.
set -eu

REPO=hotdata-dev/hototel-client
BIN=hotusage
# What this was called before 0.4.0. `$BIN install` retires the old service
# registration itself; the stale binary is this script's job, because a copy
# left on PATH would shadow or confuse the new one.
LEGACY_BIN=hotusage-collector

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
  # Fail, do not warn and carry on. This branch cannot tell "an old release
  # publishes no SHA256SUMS" from "something prevented me fetching it", and
  # every supported release publishes one -- so the only case it fires on in
  # practice is a verification that was blocked, which is precisely when
  # skipping it is worst. The binaries are unsigned; this is the only integrity
  # check there is.
  echo "error: could not fetch SHA256SUMS for $tag, so the download cannot be" >&2
  echo "  verified. Nothing was installed and nothing was changed." >&2
  exit 1
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

# Sanity-check the archive against its own name: $tag came from the same API
# call that chose the download, so this cannot tell whether that answer was
# current -- only that the binary inside the archive is the build the archive
# claims to be. That is a mis-built release, not a stale one, and it is the
# honest limit of what can be checked from here.
want="${tag#v}"
got=$("$dest/$BIN" version 2>/dev/null | awk '{print $2}')
if [ -z "$got" ]; then
  # no "verified" line anywhere: a check that announces success it did not
  # perform is worse than no check
  echo "note: $BIN has no 'version' command; skipping the archive check (pre-0.5.2 build?)" >&2
elif [ "$got" != "$want" ]; then
  echo "error: the $tag archive contains $BIN $got, not $want" >&2
  echo "  that release's assets are mis-built; report it rather than using this binary" >&2
  exit 1
fi
case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "note: $dest is not on your PATH - add it to your shell profile" ;;
esac

# register the login/background agent (idempotent; re-running re-points it).
# Non-fatal: `install` needs a desktop/systemd user session, which a plain SSH
# shell does not have -- the binary is still usable, so keep going and print
# the next steps rather than aborting under `set -e`.
if "$dest/$BIN" install; then
  # Only now is the old registration gone (`$BIN install` retires it), so the
  # old executable is finally safe to remove. Doing it earlier would leave the
  # old LaunchAgent/systemd unit pointing at a file that no longer exists, and
  # KeepAlive/Restart=always would respawn-fail in a loop until someone
  # re-ran install by hand.
  for old in /usr/local/bin "$HOME/.local/bin"; do
    if [ -f "$old/$LEGACY_BIN" ] && [ -w "$old" ]; then
      rm -f "$old/$LEGACY_BIN" && echo "removed the old $old/$LEGACY_BIN"
    fi
  done
else
  echo "note: agent registration failed (no desktop/systemd session?) - run '$dest/$BIN install' from a login session" >&2
  echo "note: the previous $LEGACY_BIN install was left in place until that succeeds" >&2
fi

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
  # The skill only works if this machine was actually granted read access; a
  # server older than 0.4.0 grants reporting alone, and promising otherwise
  # sends people to an agent that answers every question with an error.
  if "$dest/$BIN" whoami | grep -q "reads this organization"; then
    cat <<TXT

your coding agent can now answer questions about your team's usage; start a
new Claude Code or Codex session and ask something like "what did we spend on
Claude Code last month?". Or ask here:
  $dest/$BIN summary
TXT
  else
    cat <<TXT

note: this machine can report usage but not read it, so the agent skill cannot
answer questions yet. The hototel server needs updating; after that, run:
  $dest/$BIN signin --force
TXT
  fi
else
  cat <<TXT

sign-in did not complete. run this when you are ready:
  $dest/$BIN signin
TXT
fi
