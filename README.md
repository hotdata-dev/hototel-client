# hotusage-client

The thing you install. One cross-platform Rust binary (`hotusage`) that does
two jobs against a central [hotusage-server](../hotusage-server):

1. **Reports usage.** Parses this machine's local coding-agent history every N
   minutes and uploads only the sessions that changed.
2. **Answers questions about it.** `hotusage summary`, `users`, `projects`,
   `daily` and friends read the whole organization's usage back — and the
   installer drops an agent **skill** into Claude Code and Codex so they can run
   those commands for you ("what did we spend on Claude Code last month?").

One sign-in covers both. Desktop indicator on macOS (top menu bar, flame
template icon) and Windows (taskbar tray); Linux runs headless as a systemd
user daemon.

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
curl -fsSL https://raw.githubusercontent.com/hotdata-dev/hotusage-client/main/install.sh | sh
```

That downloads the right release binary for the machine, puts it on PATH,
registers the background agent (LaunchAgent / systemd user unit), installs the
agent skill for whichever of Claude Code and Codex it finds, then opens your
browser to sign in. Approve the machine and the first sync runs immediately —
there is nothing to edit by hand. Re-running the installer to upgrade leaves an
existing sign-in alone.

The installer always takes the latest release; there is no version to choose.
`hotusage version` prints which build is installed and where it lives, which is
what you want when a machine is behaving unexpectedly.

Upgrading from `hotusage-collector` (0.3.x): the installer removes the old
binary and `hotusage install` retires its LaunchAgent / systemd unit / Run key,
so you do not end up with two daemons. Your existing token keeps working for
reporting; run `hotusage signin --force` once to also grant read access, which
is what the skill needs.

Windows, or a manual install anywhere: download the archive for the machine
from the latest [GitHub Release](https://github.com/hotdata-dev/hotusage-client/releases),
unpack it, put `hotusage` somewhere on PATH, and run:

```
hotusage install
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
Chrome, clear it with `xattr -d com.apple.quarantine hotusage` (or
approve it once under System Settings -> Privacy & Security).

Every CI run also uploads per-OS build artifacts. Releases are cut by pushing a
`v*` tag matching `Cargo.toml`'s version.

## Build & run

```bash
cargo build --release
./target/release/hotusage           # macOS/Windows: tray app; Linux: daemon
./target/release/hotusage daemon    # headless sync loop (any OS)
./target/release/hotusage sync      # one-shot sync
./target/release/hotusage dump      # print parsed sessions as JSON (debug)
./target/release/hotusage help      # every subcommand
```

`--daemon`, `--once` and `--dump` still work: already-registered services
invoke the binary that way.

Tray menu (macOS/Windows): last-sync status, Sign In... / Sign Out (whichever
applies), Sync Now, Open Dashboard, Edit Config, Quit.

## Staying current

```bash
hotusage update           # install the latest release, if there is one
hotusage update --check   # report only; exit 10 when a newer release exists
```

`update` is a version check plus a delegation: it compares this build against
the latest GitHub release and, when there is a newer one, runs the same
installer the one-liner above runs. It is deliberately not a self-updater —
`install.sh` already unlinks before writing (a running binary cannot be
overwritten on Linux), verifies the archive against the release's `SHA256SUMS`,
re-registers the login service and retires the old binary in the right order.
Reimplementing that inside the process would duplicate the hard parts and could
drift from them.

The `--check` exit code is the useful half for a fleet: `10` means stale, `0`
means current, `1` means the check itself failed. Nothing else tells you a
machine is behind.

One bootstrap caveat: a machine older than the release that introduced `update`
has no such command, so it needs the `curl | sh` line once.

## Reading the organization's usage

```bash
hotusage summary                 # totals, top people and projects, recent trend
hotusage users --days 7          # per-person breakdown
hotusage projects                # per-project
hotusage providers               # Claude Code vs Codex vs OpenCode
hotusage models                  # which models, and how many people use each
hotusage daily --days 90         # day-by-day tokens and cost
hotusage sessions --user jane    # individual sessions
hotusage session <id>            # one session, request by request
hotusage raw                     # the whole payload as JSON
```

All of them take `--days 7|30|90|all` (default 30) and `--fresh`. A `--days`
value the server does not keep snaps *up* to the next window, and the output
says so rather than quietly answering a wider question.

These need **read** access on the token, which `signin` asks for alongside
reporting. Output is aligned plain text on purpose: it is mostly read by a
coding agent, and a table costs a fraction of the same numbers as JSON.

## The agent skill

`hotusage install` also writes `SKILL.md` into `~/.claude/skills/hotusage/` and
`~/.codex/skills/hotusage/` for whichever agent directories exist, so Claude
Code and Codex can answer usage questions by running the commands above. The
file is embedded in the binary ([`skill/SKILL.md`](skill/SKILL.md)) and the
binary's absolute path is substituted in, because `~/.local/bin` is often
missing from the environment an agent shells out in.

```bash
hotusage skill install     # (re)write it, even where no agent dir exists yet
hotusage skill uninstall   # remove it
```

`hotusage uninstall` removes the skill along with the service registration.

## Install as a continuous daemon

```bash
hotusage install      # register + start now, and install the skill
hotusage uninstall    # stop + remove, and remove the skill
```

| OS | Mechanism | What runs |
|----|-----------|-----------|
| macOS | LaunchAgent `~/Library/LaunchAgents/dev.hotdata.hotusage.plist` (RunAtLoad + KeepAlive, log at `~/Library/Logs/hotusage.log`) | menu bar app |
| Linux | systemd user unit `~/.config/systemd/user/hotusage.service` (Restart=always) | `daemon`, no indicator |
| Windows | `HKCU\...\CurrentVersion\Run` key (a session app, since Windows Services cannot show tray icons) | taskbar tray app |

The registration points at the binary's current path - move the binary,
re-run `install`. Installing or uninstalling also retires any registration left
under the old `hotusage-collector` name, so an upgrade never leaves two daemons
syncing one machine. CI (`.github/workflows/build.yml`) builds all three OS
targets and uploads artifacts.

## Sign in

The tray app's **Sign In...** item opens your browser, you log in to hotusage
and confirm that the code on the page matches the one in the menu, and the
collector receives a token bound to your account — no shared secret to copy.
Headless machines (Linux, servers) do the same with:

```bash
hotusage signin           # --force to re-authorize an already signed-in machine
```

which prints the URL and the code and waits for approval. The token is written
to `~/.hotusage/collector.json`; sign in again any time to replace it.

The approval page names what it grants: reporting this machine's usage, and
reading the organization's usage so the skill can answer questions. Both ride
on one token, so there is one approval rather than two. `hotusage whoami` shows
which of them this machine actually holds — a token minted before 0.4.0 reports
only, and `hotusage signin --force` upgrades it.

**Sign Out** (or `hotusage signout`) revokes that token on the server
and clears it locally, so the machine stops reporting. The local half happens
even when the server is unreachable; other machines you signed in stay signed
in, since each holds its own token.

## Security notes

- `~/.hotusage/` is created `0700` and `collector.json` / `collector-state.json`
  are written `0600`: the config holds a bearer token.
- Only `https://` servers are accepted (loopback excepted for local dev), and
  redirects are refused rather than followed — a redirected POST arrives as a
  GET, which once let uploads be silently discarded.
- An upload counts as delivered only when the server acknowledges it; a 2xx
  from something that is not the hotusage API does not.
- Helper binaries (`hostname`, `git`, `launchctl`, `open`, `reg`, ...) are run
  with a fixed system `PATH`, so a writable directory on the user's `PATH`
  cannot hijack a process that runs at login and holds a token.
- `install.sh` verifies the downloaded archive against the release's
  `SHA256SUMS` and refuses to install on a mismatch.

## Configuration

First run writes `~/.hotusage/collector.json`:

```json
{
  "server_url": "https://www.hotusage.ai",
  "token": "<written by Sign In>",
  "user_email": "you@company.com",
  "interval_minutes": 15,
  "scopes": ["ingest", "read"]
}
```

`scopes` records what the server granted this token. An empty or missing value
means a token minted before scopes existed: it reports usage but cannot read
it, which is why `hotusage summary` will ask you to sign in again.

Sign In sets `user_email` and `token` for you; before that `user_email`
falls back to `git config user.email`. Sync state (per-session fingerprints,
so only changed sessions are re-sent) lives in
`~/.hotusage/collector-state.json`. Both files are `0600`.

## What is sent (and what is not)

**All parsing happens locally.** hotusage reads your transcript files on
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
