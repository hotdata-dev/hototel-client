# hototel client — internals

Everything the [README](../README.md) deliberately leaves out: how the binary is
built, what it registers on each platform, exactly what it puts on the wire, and
the choices that are load-bearing enough to be worth writing down.

## Build and run

```bash
cargo build --release
./target/release/hototel           # macOS/Windows: tray app; Linux: daemon
./target/release/hototel daemon    # headless sync loop (any OS)
./target/release/hototel sync      # one-shot sync
./target/release/hototel dump      # print parsed sessions as JSON (debug)
./target/release/hototel help      # every subcommand
```

`--daemon`, `--once` and `--dump` are still accepted: already-registered
services invoke the binary that way, and a service registration outlives the
release that created it.

Tray menu (macOS/Windows): last-sync status, who this machine is signed in as,
one auth row that reads **Sign In…** or **Sign Out** depending on state (never
both), Sync Now, Open Dashboard, Edit Config, Quit. Which action that row
performs is decided from the config when it is clicked, not from its label —
a `hototel signin` in a terminal can change the state between the menu's
ten-second refresh and the click. The tray icon is the nine-cell mark from the
website, drawn at runtime from the same geometry as the site's `icon.svg`.

An **Update to x.y.z** row appears above Sync Now, and only while this build is
behind the latest release — a background thread checks GitHub ten seconds after
launch and every six hours after that, silently on any failure, so an offline
machine's menu is the one described above. Clicking it starts `hototel update`
in a new session via `setsid(2)`, logging to `~/.hotusage/update.log`: the
update re-registers the login service, which kills the tray, and an updater
still attached to that job would be killed with it — possibly after the
LaunchAgent had been unloaded and before the new binary was in place. Windows
has no scripted install, so there the row opens the releases page instead.

## What it parses

| Provider | Source | Notes |
|---|---|---|
| Claude Code | `~/.claude/projects/*/*.jsonl` | usage deduped by API message id |
| Codex | `~/.codex/sessions/**/*.jsonl` | cumulative `token_count` events, deltas taken; `input_tokens` includes cached |
| OpenCode | `~/.local/share/opencode/opencode.db` | per-message tokens from sqlite |

Cursor and Gemini CLI store no local token counts.

## Service registration

```bash
hototel install      # register + start now, and install the skill
hototel uninstall    # stop + remove, and remove the skill
```

| OS | Mechanism | What runs |
|---|---|---|
| macOS | LaunchAgent `~/Library/LaunchAgents/dev.hotdata.hototel.plist` (RunAtLoad + KeepAlive, log at `~/Library/Logs/hototel.log`) | menu bar app |
| Linux | systemd user unit `~/.config/systemd/user/hototel.service` (Restart=always) | `daemon`, no indicator |
| Windows | `HKCU\…\CurrentVersion\Run` key (a session app — Windows Services cannot show tray icons) | taskbar tray app |

The registration points at the binary's current path: move the binary, re-run
`install`. Installing or uninstalling also retires any registration left under
the old `hotusage` and `hotusage-collector` names, so an upgrade never leaves two daemons
syncing one machine.

## The agent skill

`hototel install` writes `SKILL.md` into `~/.claude/skills/hototel/` and
`~/.codex/skills/hototel/` for whichever agent directories exist.

```bash
hototel skill install     # (re)write it, even where no agent dir exists yet
hototel skill uninstall   # remove it
```

The file is embedded in the binary ([`skill/SKILL.md`](../skill/SKILL.md)) and
the binary's **absolute** path is substituted in, because `~/.local/bin` is
often missing from the environment an agent shells out in. It carries a marker
comment so uninstall removes only what it wrote, never a directory someone
else's files live in.

## Releases

Releases are cut by pushing a `v*` tag matching `Cargo.toml`'s version. Each
carries one archive per platform and nothing else:

| OS | Archive |
|---|---|
| macOS | `…-macos-universal.tar.gz` (Apple silicon + Intel) |
| Linux | `…-linux-x86_64.tar.gz` (glibc 2.35+) |
| Windows | `…-windows-x86_64.zip` |

No `.app`, `.deb` or `.exe` installers to maintain, and the binaries are
unsigned on purpose: `curl` and `tar` do not set the macOS quarantine
attribute, so Gatekeeper never inspects a binary installed that way. A browser
download *is* quarantined — clear it with
`xattr -d com.apple.quarantine hototel`.

`install.sh` always takes the latest release; there is no version to choose. It
verifies the archive against the release's `SHA256SUMS` and refuses to install
on a mismatch.

### `hototel update`

A version check plus a delegation: it compares this build against the latest
GitHub release and, when there is a newer one, runs the same installer. It is
deliberately **not** a self-updater — `install.sh` already unlinks before
writing (a running binary cannot be overwritten on Linux), verifies the
checksum, re-registers the login service and retires the old binary in the
right order. Reimplementing that inside the process would duplicate the parts
most likely to break.

The installer is downloaded to a private `0700` directory and run from a file,
never piped: a pipeline reports the exit status of its right-hand side, so a
failed download through `curl … | sh` would look like a successful update that
installed nothing.

`--check` exits `10` when stale, `0` when current, `1` when the check failed —
so a fleet script can act on it. The tray's update row calls the same
`latest_release` and `is_newer`, so the menu can never offer an upgrade the
command would then refuse.

## Configuration

First run writes `~/.hotusage/collector.json`:

```json
{
  "server_url": "https://hototel.com",
  "token": "<written by Sign In>",
  "user_email": "you@company.com",
  "interval_minutes": 15,
  "scopes": ["ingest", "read"]
}
```

`scopes` records what the server granted. Empty or missing means a token minted
before scopes existed: it reports usage but cannot read it, which is why
`hototel summary` asks such a machine to sign in again.

Sync state (per-session fingerprints, so only changed sessions are re-sent)
lives in `~/.hotusage/collector-state.json`. The filenames predate the rename
from the `hotusage` days and are kept so deployed machines keep their sign-in.

## Security

- `~/.hotusage/` is created `0700`; both files are written `0600`. The config
  holds a bearer token.
- Only `https://` servers are accepted (loopback excepted for local dev), and
  redirects are refused rather than followed — a redirected POST arrives as a
  GET, which once let uploads be silently discarded.
- An upload counts as delivered only when the server acknowledges it. A 2xx
  from something that is not the hototel API does not.
- Helper binaries (`hostname`, `git`, `launchctl`, `open`, `reg`, …) run with a
  fixed system `PATH`, so a writable directory on the user's `PATH` cannot
  hijack a process that runs at login holding a token.

## Pricing

Costs are list-price equivalents computed **entirely on the client**; the server
stores the `cost_*` columns the payload already carries. To reprice a provider,
edit `rates_claude` / `rates_claude_fast` / `rates_openai` in `src/core.rs`.

Anthropic: cache reads at 0.1× input, 5-minute cache writes at 1.25×, 1-hour
writes at 2×, fast mode at its premium rate. OpenAI: cached input discounted per
model. Unknown models price at $0.

Known gap: `usage.service_tier` is not read, so a session on a non-standard tier
(priority, batch) would be priced as standard. Those rates are not published
anywhere this code could cite, and guessing a multiplier would be a silent error
with no way to notice it.

## Wire format

`POST {server_url}/ingest` with `Authorization: Bearer <token>`:

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

The server stamps `user_email`/`hostname` onto stored rows and upserts by
(user, session).

## Reading the organization back

```bash
hototel summary | chart | users | projects | providers | models | daily
hototel sessions [--user X] [--project Y] [--provider claude|codex|opencode]
hototel session <id>
hototel raw
```

All take `--days 7|30|90|all` (default 30) and `--fresh`. A `--days` value the
server does not keep snaps **up** to the next window, and the output says so
rather than quietly answering a wider question.

These need `read` on the token, which `signin` requests alongside `ingest`.
Output is aligned plain text on purpose: it is mostly read by a coding agent,
and a table costs a fraction of the tokens the same numbers cost as JSON.
