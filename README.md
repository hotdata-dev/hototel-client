# hotusage-collector

Cross-platform background agent (Rust) that collects AI coding-agent token
usage and sends it to a central [hotusage](../hotusage) server. Parses this
machine's local history every N minutes and uploads only sessions that changed.

Desktop indicator on macOS (top menu bar, flame template icon) and Windows (taskbar tray);
Linux runs headless as a systemd user daemon.

Parsed providers:

| Provider | Source | Notes |
|----------|--------|-------|
| Claude Code | `~/.claude/projects/*/*.jsonl` | usage deduped by API message id |
| Codex | `~/.codex/sessions/**/*.jsonl` | cumulative `token_count` events, deltas taken; `input_tokens` includes cached |
| OpenCode | `~/.local/share/opencode/opencode.db` | per-message tokens from sqlite |

Cursor and Gemini CLI store no local token counts, so there is nothing to
collect for them.

## Install

**macOS and Linux, one line:**

```bash
curl -fsSL https://raw.githubusercontent.com/hotdata-dev/hotusage-collector/main/install.sh | sh
```

That downloads the right release binary for the machine, puts it on PATH,
registers the background agent (LaunchAgent / systemd user unit), and tells you
to fill in `~/.hotusage/collector.json`.

Or grab an installer from the latest
[GitHub Release](https://github.com/hotdata-dev/hotusage-collector/releases):

| OS | Installer | What it does |
|----|-----------|--------------|
| macOS | `...-macos-universal.app.zip` | unzip, drag to Applications, launch. Releases built with signing credentials are Developer ID signed and notarized, so Gatekeeper lets them open normally; an unsigned release (see below) is refused on first launch and needs System Settings -> Privacy & Security -> Open Anyway. Then `hotusage-collector install` from the app binary, or use the raw tar.gz + `install` for the LaunchAgent. |
| Windows | `...-windows-x86_64-setup.exe` | per-user install (no admin); registers autostart and launches the tray app |
| Linux | `...-linux-amd64.deb` | `sudo dpkg -i ...`; then per user: `hotusage-collector install` (systemd user daemon) |

Raw binaries (`.tar.gz` / `.zip`) are attached to every release too, and every
CI run uploads per-OS build artifacts. Releases are cut by pushing a `v*` tag
matching `Cargo.toml`'s version.

### macOS code signing

The release workflow signs and notarizes the macOS binary and `.app` when these
repository secrets are present, and ships unsigned builds (with a CI warning)
when none of them are — so a fork or a pre-certificate release still works.
Setting the certificate without the notarization key fails the build, because a
signed-but-un-notarized download is refused by Gatekeeper just like an unsigned
one:

| Secret | What it is |
|--------|------------|
| `MACOS_CERT_P12` | base64 of a **Developer ID Application** certificate exported as `.p12` (requires the $99/yr Apple Developer Program; a free account cannot issue one) |
| `MACOS_CERT_PASSWORD` | the password set when exporting that `.p12` |
| `MACOS_SIGN_IDENTITY` | the identity string, e.g. `Developer ID Application: HotData Inc (TEAMID)` |
| `APPLE_API_KEY_P8` | base64 of an App Store Connect API key (`.p8`) with the Developer role, used for notarization |
| `APPLE_API_KEY_ID` | that key's ID |
| `APPLE_API_ISSUER_ID` | the issuer UUID from App Store Connect |

Producing the first three, once enrolled:

```bash
# in Keychain Access: request a cert from a CA, upload the CSR at
# developer.apple.com -> Certificates -> Developer ID Application, download and
# double-click the .cer, then export the resulting identity as cert.p12
base64 -i cert.p12 | pbcopy          # -> MACOS_CERT_P12
security find-identity -v -p codesigning   # -> MACOS_SIGN_IDENTITY
```

Notarization staples a ticket to the `.app`, so it verifies offline. A bare CLI
executable cannot carry a stapled ticket; the `.tar.gz` binary is signed and
notarized but checked online the first time it runs. `install.sh` also clears
the quarantine attribute, which covers unsigned releases and offline machines.

Windows SmartScreen is a separate problem needing its own (OV/EV) certificate;
the `.exe` is currently unsigned.

## Build & run

```bash
cargo build --release
./target/release/hotusage-collector            # macOS/Windows: tray app; Linux: daemon
./target/release/hotusage-collector --daemon   # headless sync loop (any OS)
./target/release/hotusage-collector --once     # one-shot sync
./target/release/hotusage-collector --dump     # print parsed sessions as JSON (debug)
```

Tray menu (macOS/Windows): last-sync status, Sync Now, Open Dashboard,
Edit Config, Quit.

## Install as a continuous daemon

```bash
hotusage-collector install      # register + start now
hotusage-collector uninstall    # stop + remove
```

| OS | Mechanism | What runs |
|----|-----------|-----------|
| macOS | LaunchAgent `~/Library/LaunchAgents/dev.hotdata.hotusage-collector.plist` (RunAtLoad + KeepAlive, log at /tmp/hotusage-collector.log) | menu bar app |
| Linux | systemd user unit `~/.config/systemd/user/hotusage-collector.service` (Restart=always) | `--daemon`, no indicator |
| Windows | `HKCU\...\CurrentVersion\Run` key (a session app, since Windows Services cannot show tray icons) | taskbar tray app |

The registration points at the binary's current path - move the binary,
re-run `install`. CI (`.github/workflows/build.yml`) builds all three OS
targets and uploads artifacts.

## Configuration

First run writes `~/.hotusage/collector.json`:

```json
{
  "server_url": "https://hotusage.ai",
  "token": "<shared HOTUSAGE_INGEST_TOKEN>",
  "user_email": "you@company.com",
  "interval_minutes": 15
}
```

`user_email` defaults to `git config user.email`. Sync state (per-session
fingerprints, so only changed sessions are re-sent) lives in
`~/.hotusage/collector-state.json`.

## What is sent (and what is not)

**All parsing happens locally.** The collector reads your transcript files on
disk, extracts usage numbers, and sends only the derived rows below. Transcript
files are never uploaded, and message bodies, assistant responses, tool calls,
tool results, code, and diffs are never transmitted.

Sent per session:

| Field | Notes |
|-------|-------|
| `session_id`, `provider` | ids only |
| `project`, `cwd` | working directory basename and **full local path** |
| `title` | the agent's own session title. When the agent recorded none, the fallback is the **first line of your first prompt, truncated to 80 characters** |
| `started_at`, `ended_at`, `requests` | timing and request count |
| `models` | model ids used |
| `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens` | counts |
| `cost_*` | list-price estimates derived from those counts |
| `peak_context_tokens` | largest single-request context |

Sent per request: `session_id`, `provider`, `seq`, `ts`, `context_tokens`,
`output_tokens`. Sent per day: `session_id`, `provider`, `day`, the same token
counts and cost estimates.

So the only fields that can carry text you typed are `title` (a summary, or up
to 80 characters of a first prompt) and `cwd` (which reveals directory and user
names). Everything else is counts, timestamps, and identifiers.

## Wire format

`POST {server_url}/ingest` with `Authorization: Bearer <token>` and JSON body:

```json
{
  "schema": 1,
  "user_email": "...", "hostname": "...",
  "sessions": [ { "session_id", "provider", "project", "cwd", "title",
                  "started_at", "ended_at", "requests", "models",
                  "input_tokens", "output_tokens", "cache_read_tokens",
                  "cache_write_tokens", "cost_*", "peak_context_tokens" } ],
  "requests": [ { "session_id", "provider", "seq", "ts",
                  "context_tokens", "output_tokens" } ],
  "daily":    [ { "session_id", "provider", "day", "..._tokens", "cost_*" } ]
}
```

Costs are estimates at provider API list prices; the server stamps
`user_email`/`hostname` onto stored rows and upserts by (user, session).

A Python reference implementation with identical behavior (verified
field-for-field) lives in the hotusage repo under `collector/`.
