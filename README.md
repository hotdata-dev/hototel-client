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

Windows, or a manual install anywhere: download the archive for the machine
from the latest [GitHub Release](https://github.com/hotdata-dev/hotusage-collector/releases),
unpack it, put `hotusage-collector` somewhere on PATH, and run:

```
hotusage-collector install
```

That registers the background agent for the current user (LaunchAgent on macOS,
systemd user unit on Linux, `Run` key on Windows). Releases carry one archive
per platform and nothing else:

| OS | Archive |
|----|---------|
| macOS | `...-macos-universal.tar.gz` (Apple silicon + Intel) |
| Linux | `...-linux-x86_64.tar.gz` (glibc 2.35+) |
| Windows | `...-windows-x86_64.zip` |

There are no `.app`, `.deb` or `.exe` installers to maintain, and the binaries
are unsigned on purpose: `curl` and `tar` do not set the macOS quarantine
attribute, so Gatekeeper never inspects a binary installed this way. A browser
download would be quarantined -- if you fetch an archive by hand in Safari or
Chrome, clear it with `xattr -d com.apple.quarantine hotusage-collector` (or
approve it once under System Settings -> Privacy & Security).

Every CI run also uploads per-OS build artifacts. Releases are cut by pushing a
`v*` tag matching `Cargo.toml`'s version.

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
