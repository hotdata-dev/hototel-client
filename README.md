# hotusage-collector

macOS menu bar agent (Rust) that collects AI coding-agent token usage and
sends it to a central [hotusage](../hotusage) server. Sits in the top menu bar
(⏶), parses this machine's local history every N minutes, and uploads only
sessions that changed.

Parsed providers:

| Provider | Source | Notes |
|----------|--------|-------|
| Claude Code | `~/.claude/projects/*/*.jsonl` | usage deduped by API message id |
| Codex | `~/.codex/sessions/**/*.jsonl` | cumulative `token_count` events, deltas taken; `input_tokens` includes cached |
| OpenCode | `~/.local/share/opencode/opencode.db` | per-message tokens from sqlite |

Cursor and Gemini CLI store no local token counts, so there is nothing to
collect for them.

## Build & run

```bash
cargo build --release
./target/release/hotusage-collector            # menu bar app
./target/release/hotusage-collector --once     # headless one-shot sync
./target/release/hotusage-collector --dump     # print parsed sessions as JSON (debug)
```

Menu: last-sync status, Sync Now, Open Dashboard, Edit Config, Quit.

## Configuration

First run writes `~/.hotusage/collector.json`:

```json
{
  "server_url": "https://hotusage.internal.example",
  "token": "<shared HOTUSAGE_INGEST_TOKEN>",
  "user_email": "you@company.com",
  "interval_minutes": 15
}
```

`user_email` defaults to `git config user.email`. Sync state (per-session
fingerprints, so only changed sessions are re-sent) lives in
`~/.hotusage/collector-state.json`.

## Start at login

```bash
sed "s|__COLLECTOR__|$HOME/Code/hotusage-collector|" dev.hotdata.hotusage-collector.plist \
  > ~/Library/LaunchAgents/dev.hotdata.hotusage-collector.plist
launchctl load ~/Library/LaunchAgents/dev.hotdata.hotusage-collector.plist
```

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
