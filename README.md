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

Grab the latest [GitHub Release](https://github.com/hotdata-dev/hotusage-collector/releases):

| OS | Installer | What it does |
|----|-----------|--------------|
| macOS | `...-macos-universal.app.zip` | unzip, drag to Applications, open (right-click -> Open the first time: unsigned). Then `hotusage-collector install` from the app binary, or use the raw tar.gz + `install` for the LaunchAgent. |
| Windows | `...-windows-x86_64-setup.exe` | per-user install (no admin); registers autostart and launches the tray app |
| Linux | `...-linux-amd64.deb` | `sudo dpkg -i ...`; then per user: `hotusage-collector install` (systemd user daemon) |

Raw binaries (`.tar.gz` / `.zip`) are attached to every release too, and every
CI run uploads per-OS build artifacts. Releases are cut by pushing a `v*` tag
matching `Cargo.toml`'s version.

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
