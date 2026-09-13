---
name: hotusage
description: Answer questions about the organization's AI coding-agent usage — how much Claude Code, Codex, or OpenCode is being used, by whom, on which projects, what it costs, and how that is trending. Use for "our usage", "how much are we spending on Claude", "who used the most tokens", "usage by project", "what did we spend last week", "is anyone still on Opus", or any mention of hotusage. Requires a one-time sign-in that grants read-only access to the signed-in person's organization.
---

# hotusage

Queries the usage your team's hotusage clients have already reported: one row
per coding session, with tokens, cost, project, tool and model. The data covers
**your organization only** — the account this machine signed in as.

The command is `{{BIN}}`. It is a single self-contained binary with no
dependencies; it is the same program that reports this machine's usage, so if
usage is being collected here, the command is already installed.

## Signing in

If a command answers `not signed in`, or says this machine can report usage but
not read it, tell the user to run:

```
{{BIN}} signin
```

It prints a URL and a short code; they approve it in the browser while signed in
to hotusage. **Run it in the foreground and let the user act** — it waits for a
human to approve and cannot be completed on their behalf. If they are already
signed in but lack read access (an older install), the command to use is
`{{BIN}} signin --force`.

`{{BIN}} whoami` shows who this machine is signed in as and what it may do.

## Commands

Every reporting command takes `--days 7|30|90` (default 30) or `--days all`, and
`--fresh` to bypass the server's cache.

| Command | Answers |
|---|---|
| `{{BIN}} summary` | Totals, top people, top projects, by-tool split, recent trend. **Start here.** |
| `{{BIN}} users` | Per-person: sessions, requests, tokens, cost, last active. `--limit N` |
| `{{BIN}} projects` | Per-project: sessions, distinct people, tokens, cost. `--limit N` |
| `{{BIN}} providers` | Per-tool: `claude` (Claude Code) vs `codex` vs `opencode`. |
| `{{BIN}} models` | Which models sessions touched, and how many people used each. |
| `{{BIN}} daily` | Day-by-day tokens and cost — the series to read trends from. |
| `{{BIN}} chart` | The same day-by-day series as a stacked ASCII bar chart. `--metric cost\|tokens`, `--height N`, and the `sessions` filters. |
| `{{BIN}} sessions` | Individual sessions. `--user` `--project` `--provider` (substring match; provider is `claude`/`codex`/`opencode`), `--limit N` |
| `{{BIN}} session <id>` | One session request-by-request: context growth and output per turn. |
| `{{BIN}} raw` | The entire payload as JSON. |

## How to use it well

**Reach for `chart` when the question is about shape** — a trend, a spike, "what
does our usage look like", or anything the person would otherwise ask you to
draw. It is one call and renders the same stacked series as the dashboard, so
do not hand-build a chart out of `raw`. It takes `--user`, `--project` and
`--provider` too, which is how you show who or what drove a spike.

**Run `summary` first** for almost any question. It is one call and usually
contains the answer or tells you which breakdown to reach for next.

**Prefer the aggregate commands over `raw`.** `raw` is the whole org's session
list — megabytes of JSON for a real team, and reading it into context to compute
a total you could have asked for is pure waste. Reach for it only when the
question needs a cut none of the other commands make (for example "sessions
longer than an hour"), and even then try `sessions --limit` first.

**Windows are 7, 30, and 90 days.** Anything else snaps *up* to the next one —
`--days 14` answers with 30 days, and the output says so. Do not present a
30-day number as if it were the two weeks the user asked about; say which window
the figures cover.

**`summary` and `daily` count the window differently.** `summary`'s total is
every session that *ended* inside the window, counted whole; `daily` sums only
the per-day rows that fall inside it. A session that started before the window
and ended inside it makes the two disagree at the edges. Neither is wrong —
quote one or the other for a given answer, and don't present them side by side
as if they should match.

**Cost is an estimate**, computed from published per-model rates when the
session was parsed. It is a good relative signal (who, what, which trend) and an
approximation of a real bill. Say "about" when quoting totals; these are not
invoice figures.

**A session can use several models**, and cost is not split among them — so
`models` counts sessions and people, never dollars per model. Do not compute
per-model spend from this data; it is not in there.

**Cache reads dominate token counts** for Claude Code and are much cheaper than
input tokens. When comparing people or projects, cost is the fairer measure;
raw token totals mostly measure how long their sessions ran.

**Absence is ambiguous.** Someone with no rows may not use these tools, or may
simply not have hotusage installed. Say which you know — you cannot tell them
apart from here.

## Interpreting the data

- `tool` / `provider` is exactly `claude` (Claude Code), `codex`, or
  `opencode`. It is **not** spelled claude-code; `--provider` is a substring
  match, so that spelling matches nothing and would read as "nobody uses
  Claude Code" rather than failing loudly.
- `project` is the repository or directory the session ran in.
- `requests` is turns in the session, not API calls.
- `context tokens` is how full the context window got — useful for spotting
  sessions that were fighting the context limit.
- Days with no usage are absent from `daily` and from `chart` rather than
  present as zero. Under a filter this is more noticeable: a person's chart
  skips the days they did not work, so the x-axis can have gaps.
- `chart` bars narrow automatically so a 30-day window still fits a terminal.
  A series too small to fill one row simply does not appear — say so rather
  than reporting it as zero.

## Privacy

hotusage uploads **derived usage only** — token counts, costs, timings, project
and model names. Prompts, code and transcript contents are never sent, so they
are not here and cannot be retrieved. If the user asks what someone was actually
working on, the honest answer is the project name and nothing more.

Usage is per-person and visible to everyone in the organization. When answering
comparative questions ("who spent the most"), report the numbers plainly, but do
not editorialize about individuals' productivity — session counts and token
totals measure neither effort nor output.
