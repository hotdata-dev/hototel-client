//! Reading the organization's usage back out of the server.
//!
//! The other half of this binary: `sync` pushes this machine's sessions up,
//! and these subcommands pull the whole org's usage down and summarise it.
//! They exist so a coding agent can answer questions about usage through the
//! skill (see `skill.rs`) without anyone installing a second tool — which is
//! also why the output is aligned text rather than JSON. The agent reads it,
//! and a few hundred rows of table cost a fraction of the same data as JSON.
//!
//! Everything here needs the `read` scope on the token; `sync` needs `ingest`.
//! One sign-in grants both.

use crate::sync::{api_get, load_config};
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Read `null` as the type's default.
///
/// `#[serde(default)]` covers a MISSING key, not a present `null`, and the
/// server sends column values straight through: one session row with a NULL
/// token count would otherwise fail the whole payload and break every read
/// command for the entire organization.
fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// The windows the server serves. Anything else snaps UP to one of these, and
/// the header says so -- a 14-day question must never be answered with 30 days
/// of numbers without saying which it is.
const WINDOWS: [u32; 3] = [7, 30, 90];

#[derive(Debug, Default, Deserialize)]
pub struct Session {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub end: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub requests: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub models: Vec<String>,
    #[serde(rename = "in", default, deserialize_with = "nullable")]
    pub tokens_in: i64,
    #[serde(rename = "out", default, deserialize_with = "nullable")]
    pub tokens_out: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub cr: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub cw: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub cost: f64,
}

#[derive(Debug, Default, Deserialize)]
pub struct Daily {
    #[serde(default, deserialize_with = "nullable")]
    pub s: String,
    #[serde(default, deserialize_with = "nullable")]
    pub d: String,
    #[serde(rename = "in", default, deserialize_with = "nullable")]
    pub tokens_in: i64,
    #[serde(rename = "out", default, deserialize_with = "nullable")]
    pub tokens_out: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub cr: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub cw: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub cin: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub cout: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub ccr: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub ccw: f64,
}

#[derive(Debug, Default, Deserialize)]
pub struct Viewer {
    #[serde(default, deserialize_with = "nullable")]
    pub org: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct Payload {
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub viewer: Viewer,
    #[serde(default)]
    pub sessions: Vec<Session>,
    #[serde(default)]
    pub daily: Vec<Daily>,
}

// serde's rename_all would also rewrite the short keys, so generatedAt is
// mapped on its own.
impl Payload {
    fn from_value(v: serde_json::Value) -> Result<Payload, String> {
        let generated_at = v
            .get("generatedAt")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let mut p: Payload =
            serde_json::from_value(v).map_err(|e| format!("unexpected response shape: {e}"))?;
        p.generated_at = generated_at;
        Ok(p)
    }
}

// ---------------------------------------------------------------------------
// aggregation
// ---------------------------------------------------------------------------
#[derive(Default, Clone)]
struct Agg {
    sessions: i64,
    requests: i64,
    tokens_in: i64,
    tokens_out: i64,
    cache_read: i64,
    cache_write: i64,
    cost: f64,
    users: HashSet<String>,
    last: String,
}

impl Agg {
    fn add(&mut self, s: &Session) {
        self.sessions += 1;
        self.requests += s.requests;
        self.tokens_in += s.tokens_in;
        self.tokens_out += s.tokens_out;
        self.cache_read += s.cr;
        self.cache_write += s.cw;
        self.cost += s.cost;
        if let Some(u) = s.user.as_deref().filter(|u| !u.is_empty()) {
            self.users.insert(u.to_string());
        }
        let end = day_of(s.end.as_deref().unwrap_or(""));
        if end > self.last {
            self.last = end;
        }
    }

    /// Every token the session moved, cache included -- the same definition
    /// `by_day` uses, so a "tokens" column means one thing everywhere. Cache
    /// reads dominate this for Claude Code, which is why cost, not this, is the
    /// fair way to compare people.
    fn tokens(&self) -> i64 {
        self.tokens_in + self.tokens_out + self.cache_read + self.cache_write
    }
}

fn field<'a>(s: &'a Session, key: &str) -> &'a str {
    let v = match key {
        "user" => s.user.as_deref(),
        "project" => s.project.as_deref(),
        "provider" => s.provider.as_deref(),
        _ => None,
    };
    match v {
        Some(x) if !x.is_empty() => x,
        _ => "(none)",
    }
}

/// Grouped and ordered by cost, descending -- the order every top-N list wants.
fn group_by(sessions: &[Session], key: &str) -> Vec<(String, Agg)> {
    let mut map: HashMap<String, Agg> = HashMap::new();
    for s in sessions {
        map.entry(field(s, key).to_string()).or_default().add(s);
    }
    let mut rows: Vec<(String, Agg)> = map.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.cost
            .partial_cmp(&a.1.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0)) // stable for equal (often zero) costs
    });
    rows
}

struct DayAgg {
    tokens: i64,
    cost: f64,
    sessions: HashSet<String>,
}

/// Day -> totals, oldest first. BTreeMap because the key is an ISO date, which
/// sorts correctly as a string.
fn by_day(daily: &[Daily]) -> Vec<(String, DayAgg)> {
    let mut map: BTreeMap<String, DayAgg> = BTreeMap::new();
    for r in daily {
        let d = day_of(&r.d);
        if d.is_empty() {
            continue;
        }
        let e = map.entry(d).or_insert(DayAgg {
            tokens: 0,
            cost: 0.0,
            sessions: HashSet::new(),
        });
        e.tokens += r.tokens_in + r.tokens_out + r.cr + r.cw;
        e.cost += r.cin + r.cout + r.ccr + r.ccw;
        if !r.s.is_empty() {
            e.sessions.insert(r.s.clone());
        }
    }
    map.into_iter().collect()
}

// ---------------------------------------------------------------------------
// formatting
// ---------------------------------------------------------------------------
fn day_of(s: &str) -> String {
    s.chars().take(10).collect()
}

fn num(n: i64) -> String {
    let f = n as f64;
    for (cut, suffix) in [(1e9, "B"), (1e6, "M"), (1e3, "K")] {
        if f.abs() >= cut {
            return format!("{:.1}{suffix}", f / cut);
        }
    }
    format!("{n}")
}

fn money(n: f64) -> String {
    format!("${n:.2}")
}

/// Left-aligned unless the column holds numbers, which read far better right.
#[derive(Clone, Copy, PartialEq)]
enum Align {
    L,
    R,
}

fn table(headers: &[(&str, Align)], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "  (nothing in this window)".into();
    }
    let mut widths: Vec<usize> = headers.iter().map(|(h, _)| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }
    let pad = |s: &str, w: usize, a: Align| {
        let fill = w.saturating_sub(s.chars().count());
        match a {
            Align::L => format!("{s}{}", " ".repeat(fill)),
            Align::R => format!("{}{s}", " ".repeat(fill)),
        }
    };
    let mut out = String::from("  ");
    out.push_str(
        &headers
            .iter()
            .enumerate()
            .map(|(i, (h, a))| pad(h, widths[i], *a))
            .collect::<Vec<_>>()
            .join("  "),
    );
    out.push_str("\n  ");
    out.push_str(
        &widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  "),
    );
    for r in rows {
        out.push_str("\n  ");
        // trimmed: a left-aligned final column would otherwise pad every row
        // out to its widest cell, and the agent reading this pays for the
        // trailing spaces
        out.push_str(
            r.iter()
                .enumerate()
                // take_while, not index: a row longer than the header list
                // should lose its extra cells, never panic mid-report
                .take_while(|(i, _)| *i < widths.len())
                .map(|(i, c)| pad(c, widths[i], headers[i].1))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end(),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// options
// ---------------------------------------------------------------------------
pub struct Opts {
    pub days: Option<u32>,
    pub asked_days: Option<u32>,
    pub fresh: bool,
    pub limit: usize,
    pub user: Option<String>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub id: Option<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            days: Some(30),
            asked_days: Some(30),
            fresh: false,
            limit: 50,
            user: None,
            project: None,
            provider: None,
            id: None,
        }
    }
}

/// `--days 14` means "at least 14 days"; the server only stores the three
/// windows, so snap up. None is all time.
fn snap(days: u32) -> Option<u32> {
    WINDOWS.into_iter().find(|w| *w >= days)
}

pub fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let value = |i: &mut usize, name: &str| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a {
            "--days" => {
                let raw = value(&mut i, "--days")?;
                if raw.eq_ignore_ascii_case("all") || raw == "0" {
                    o.days = None;
                    o.asked_days = None;
                } else {
                    let n: u32 = raw.parse().map_err(|_| format!("bad --days value: {raw}"))?;
                    o.asked_days = Some(n);
                    o.days = snap(n);
                }
            }
            "--limit" => {
                let raw = value(&mut i, "--limit")?;
                o.limit = raw.parse().map_err(|_| format!("bad --limit value: {raw}"))?;
            }
            "--user" => o.user = Some(value(&mut i, "--user")?),
            "--project" => o.project = Some(value(&mut i, "--project")?),
            "--provider" => o.provider = Some(value(&mut i, "--provider")?),
            "--fresh" => o.fresh = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}'"));
            }
            other => o.id = Some(other.to_string()),
        }
        i += 1;
    }
    Ok(o)
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------
fn fetch(o: &Opts) -> Result<Payload, String> {
    let cfg = load_config();
    let mut path = match o.days {
        Some(d) => format!("/api/data?days={d}"),
        None => "/api/data?days=all".to_string(),
    };
    if o.fresh {
        path.push_str("&fresh=1");
    }
    Payload::from_value(api_get(&cfg, &path, 180)?)
}

fn header(p: &Payload, o: &Opts) -> String {
    let span = match o.days {
        Some(d) => format!("last {d} days"),
        None => "all time".to_string(),
    };
    // when the question's window is not one the server keeps, say so in the
    // output itself: a number presented for "the last two weeks" that actually
    // covers thirty days is worse than no number
    let note = match (o.asked_days, o.days) {
        (Some(asked), Some(served)) if asked != served => {
            format!(" (asked for {asked}; the server keeps 7/30/90 day windows)")
        }
        _ => String::new(),
    };
    let org = if p.viewer.org.is_empty() {
        "your organization"
    } else {
        &p.viewer.org
    };
    format!(
        "{org} — {span}{note}\nas of {}, {} sessions",
        p.generated_at.chars().take(19).collect::<String>(),
        p.sessions.len()
    )
}

fn totals(sessions: &[Session]) -> Agg {
    let mut t = Agg::default();
    for s in sessions {
        t.add(s);
    }
    t
}

// ---------------------------------------------------------------------------
// subcommands
// ---------------------------------------------------------------------------
pub fn summary(o: &Opts) -> Result<String, String> {
    let p = fetch(o)?;
    let mut out = header(&p, o);
    if p.sessions.is_empty() {
        out.push_str("\n\n  no usage reported in this window");
        return Ok(out);
    }
    let t = totals(&p.sessions);
    out.push_str(&format!(
        "\n\n  {} people, {} sessions, {} requests\n  {} in / {} out / {} cache read / {} cache write\n  {} total\n",
        t.users.len(),
        t.sessions,
        t.requests,
        num(t.tokens_in),
        num(t.tokens_out),
        num(t.cache_read),
        num(t.cache_write),
        money(t.cost)
    ));

    out.push_str("\nTop people by cost\n");
    out.push_str(&table(
        &[
            ("person", Align::L),
            ("sessions", Align::R),
            ("tokens", Align::R),
            ("cost", Align::R),
        ],
        &group_by(&p.sessions, "user")
            .iter()
            .take(10)
            .map(|(k, g)| {
                vec![
                    k.clone(),
                    g.sessions.to_string(),
                    num(g.tokens()),
                    money(g.cost),
                ]
            })
            .collect::<Vec<_>>(),
    ));

    out.push_str("\n\nTop projects by cost\n");
    out.push_str(&table(
        &[
            ("project", Align::L),
            ("sessions", Align::R),
            ("people", Align::R),
            ("cost", Align::R),
        ],
        &group_by(&p.sessions, "project")
            .iter()
            .take(10)
            .map(|(k, g)| {
                vec![
                    k.clone(),
                    g.sessions.to_string(),
                    g.users.len().to_string(),
                    money(g.cost),
                ]
            })
            .collect::<Vec<_>>(),
    ));

    out.push_str("\n\nBy tool\n");
    out.push_str(&table(
        &[
            ("tool", Align::L),
            ("sessions", Align::R),
            ("people", Align::R),
            ("cost", Align::R),
        ],
        &group_by(&p.sessions, "provider")
            .iter()
            .map(|(k, g)| {
                vec![
                    k.clone(),
                    g.sessions.to_string(),
                    g.users.len().to_string(),
                    money(g.cost),
                ]
            })
            .collect::<Vec<_>>(),
    ));

    let days = by_day(&p.daily);
    if !days.is_empty() {
        let recent: Vec<_> = days.iter().rev().take(14).rev().collect();
        out.push_str(&format!("\n\nLast {} days with usage\n", recent.len()));
        out.push_str(&table(
            &[("day", Align::L), ("tokens", Align::R), ("cost", Align::R)],
            &recent
                .iter()
                .map(|(d, v)| vec![d.clone(), num(v.tokens), money(v.cost)])
                .collect::<Vec<_>>(),
        ));
    }
    Ok(out)
}

pub fn users(o: &Opts) -> Result<String, String> {
    let p = fetch(o)?;
    Ok(format!(
        "{}\n\n{}",
        header(&p, o),
        table(
            &[
                ("person", Align::L),
                ("sessions", Align::R),
                ("requests", Align::R),
                ("in", Align::R),
                ("out", Align::R),
                ("cache read", Align::R),
                ("cost", Align::R),
                ("last active", Align::L),
            ],
            &group_by(&p.sessions, "user")
                .iter()
                .take(o.limit)
                .map(|(k, g)| vec![
                    k.clone(),
                    g.sessions.to_string(),
                    g.requests.to_string(),
                    num(g.tokens_in),
                    num(g.tokens_out),
                    num(g.cache_read),
                    money(g.cost),
                    g.last.clone(),
                ])
                .collect::<Vec<_>>(),
        )
    ))
}

pub fn projects(o: &Opts) -> Result<String, String> {
    let p = fetch(o)?;
    Ok(format!(
        "{}\n\n{}",
        header(&p, o),
        table(
            &[
                ("project", Align::L),
                ("sessions", Align::R),
                ("people", Align::R),
                ("tokens", Align::R),
                ("cost", Align::R),
                ("last active", Align::L),
            ],
            &group_by(&p.sessions, "project")
                .iter()
                .take(o.limit)
                .map(|(k, g)| vec![
                    k.clone(),
                    g.sessions.to_string(),
                    g.users.len().to_string(),
                    num(g.tokens()),
                    money(g.cost),
                    g.last.clone(),
                ])
                .collect::<Vec<_>>(),
        )
    ))
}

pub fn providers(o: &Opts) -> Result<String, String> {
    let p = fetch(o)?;
    Ok(format!(
        "{}\n\n{}",
        header(&p, o),
        table(
            &[
                ("tool", Align::L),
                ("sessions", Align::R),
                ("people", Align::R),
                ("requests", Align::R),
                ("tokens", Align::R),
                ("cost", Align::R),
                ("last active", Align::L),
            ],
            &group_by(&p.sessions, "provider")
                .iter()
                .map(|(k, g)| vec![
                    k.clone(),
                    g.sessions.to_string(),
                    g.users.len().to_string(),
                    g.requests.to_string(),
                    num(g.tokens()),
                    money(g.cost),
                    g.last.clone(),
                ])
                .collect::<Vec<_>>(),
        )
    ))
}

pub fn models(o: &Opts) -> Result<String, String> {
    let p = fetch(o)?;
    // A session can touch several models and the cost is not split among them,
    // so this counts sessions and people rather than pretending to attribute
    // dollars per model.
    let mut seen: HashMap<String, (i64, HashSet<String>, String)> = HashMap::new();
    for s in &p.sessions {
        for m in &s.models {
            if m.is_empty() {
                continue;
            }
            let e = seen
                .entry(m.clone())
                .or_insert((0, HashSet::new(), String::new()));
            e.0 += 1;
            if let Some(u) = s.user.as_deref().filter(|u| !u.is_empty()) {
                e.1.insert(u.to_string());
            }
            let d = day_of(s.end.as_deref().unwrap_or(""));
            if d > e.2 {
                e.2 = d;
            }
        }
    }
    let mut rows: Vec<_> = seen.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(&b.0)));
    Ok(format!(
        "{}\n\n{}\n\n  (cost is not split per model: one session can use several)",
        header(&p, o),
        table(
            &[
                ("model", Align::L),
                ("sessions", Align::R),
                ("people", Align::R),
                ("last used", Align::L),
            ],
            &rows
                .iter()
                .map(|(m, (n, users, last))| vec![
                    m.clone(),
                    n.to_string(),
                    users.len().to_string(),
                    last.clone(),
                ])
                .collect::<Vec<_>>(),
        )
    ))
}

pub fn daily(o: &Opts) -> Result<String, String> {
    let p = fetch(o)?;
    let days = by_day(&p.daily);
    let total: f64 = days.iter().map(|(_, v)| v.cost).sum();
    let mut out = format!(
        "{}\n\n{}",
        header(&p, o),
        table(
            &[
                ("day", Align::L),
                ("sessions", Align::R),
                ("tokens", Align::R),
                ("cost", Align::R),
            ],
            &days
                .iter()
                .map(|(d, v)| vec![
                    d.clone(),
                    v.sessions.len().to_string(),
                    num(v.tokens),
                    money(v.cost),
                ])
                .collect::<Vec<_>>(),
        )
    );
    if !days.is_empty() {
        out.push_str(&format!(
            "\n\n  {} across {} days with usage",
            money(total),
            days.len()
        ));
    }
    Ok(out)
}

pub fn sessions(o: &Opts) -> Result<String, String> {
    let p = fetch(o)?;
    let matches = |want: &Option<String>, got: &str| match want {
        Some(w) => got.to_lowercase().contains(&w.to_lowercase()),
        None => true,
    };
    let rows: Vec<&Session> = p
        .sessions
        .iter()
        .filter(|s| {
            matches(&o.user, field(s, "user"))
                && matches(&o.project, field(s, "project"))
                && matches(&o.provider, field(s, "provider"))
        })
        .collect();
    let mut out = format!(
        "{}\n{} matching sessions\n\n{}",
        header(&p, o),
        rows.len(),
        table(
            &[
                ("day", Align::L),
                ("person", Align::L),
                ("tool", Align::L),
                ("project", Align::L),
                ("requests", Align::R),
                ("tokens", Align::R),
                ("cost", Align::R),
                ("session id", Align::L),
            ],
            &rows
                .iter()
                .take(o.limit)
                .map(|s| vec![
                    day_of(s.end.as_deref().unwrap_or("")),
                    field(s, "user").to_string(),
                    field(s, "provider").to_string(),
                    field(s, "project").chars().take(28).collect(),
                    s.requests.to_string(),
                    num(s.tokens_in + s.tokens_out + s.cr + s.cw),
                    money(s.cost),
                    s.id.clone(),
                ])
                .collect::<Vec<_>>(),
        )
    );
    if rows.len() > o.limit {
        out.push_str(&format!(
            "\n\n  showing {} of {}; raise it with --limit",
            o.limit,
            rows.len()
        ));
    }
    Ok(out)
}

pub fn session(o: &Opts) -> Result<String, String> {
    let id = o
        .id
        .as_deref()
        .ok_or("usage: hotusage session <session-id>")?;
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || id.len() > 80
    {
        return Err("that does not look like a session id".into());
    }
    let cfg = load_config();
    let v = api_get(&cfg, &format!("/api/session/{id}"), 60)?;
    let detail = v
        .get("detail")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let cwd = v.get("cwd").and_then(|c| c.as_str()).unwrap_or("");
    let mut out = format!(
        "session {id}\n  cwd: {cwd}\n  {} requests",
        detail.len()
    );
    if detail.is_empty() {
        return Ok(out);
    }
    let get = |r: &serde_json::Value, k: &str| r.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let peak = detail.iter().map(|r| get(r, "ctx")).max().unwrap_or(0);
    out.push_str("\n\n");
    out.push_str(&table(
        &[
            ("when", Align::L),
            ("context tokens", Align::R),
            ("output tokens", Align::R),
        ],
        &detail
            .iter()
            .take(o.limit)
            .map(|r| {
                vec![
                    r.get("t")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(19)
                        .collect(),
                    num(get(r, "ctx")),
                    num(get(r, "out")),
                ]
            })
            .collect::<Vec<_>>(),
    ));
    out.push_str(&format!("\n\n  peak context {} tokens", num(peak)));
    if detail.len() > o.limit {
        // never truncate silently: a capped list read as the whole session
        // would make "when did the context blow up" answerable and wrong
        out.push_str(&format!(
            "\n  showing the first {} of {} requests; raise it with --limit",
            o.limit,
            detail.len()
        ));
    }
    Ok(out)
}

pub fn raw(o: &Opts) -> Result<String, String> {
    let cfg = load_config();
    let mut path = match o.days {
        Some(d) => format!("/api/data?days={d}"),
        None => "/api/data?days=all".to_string(),
    };
    if o.fresh {
        path.push_str("&fresh=1");
    }
    let v = api_get(&cfg, &path, 180)?;
    Ok(serde_json::to_string(&v).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, user: &str, provider: &str, project: &str, cost: f64, end: &str) -> Session {
        Session {
            id: id.into(),
            user: Some(user.into()),
            provider: Some(provider.into()),
            project: Some(project.into()),
            end: Some(end.into()),
            requests: 10,
            models: vec!["claude-sonnet-5".into()],
            tokens_in: 1000,
            tokens_out: 2000,
            cr: 3000,
            cw: 400,
            cost,
        }
    }

    fn fixture() -> Vec<Session> {
        vec![
            s("s1", "ada@x.dev", "claude-code", "api", 12.50, "2026-09-12T09:00:00+00:00"),
            s("s2", "ada@x.dev", "claude-code", "web", 3.25, "2026-09-11T09:00:00+00:00"),
            s("s3", "bob@x.dev", "codex", "api", 7.00, "2026-09-13T09:00:00+00:00"),
            s("s4", "bob@x.dev", "claude-code", "api", 1.00, "2026-09-10T09:00:00+00:00"),
        ]
    }

    #[test]
    fn totals_cover_every_session() {
        let t = totals(&fixture());
        assert_eq!(t.sessions, 4);
        assert_eq!(t.requests, 40);
        assert_eq!(t.users.len(), 2);
        assert!((t.cost - 23.75).abs() < 1e-9, "{}", t.cost);
    }

    #[test]
    fn groups_are_ordered_by_cost() {
        let by_user = group_by(&fixture(), "user");
        assert_eq!(by_user[0].0, "ada@x.dev"); // 15.75 beats bob's 8.00
        assert!((by_user[0].1.cost - 15.75).abs() < 1e-9);

        let by_project = group_by(&fixture(), "project");
        assert_eq!(by_project[0].0, "api");
        assert_eq!(by_project[0].1.sessions, 3);
        // distinct people, not session count
        assert_eq!(by_project[0].1.users.len(), 2);
    }

    #[test]
    fn a_missing_field_groups_as_none_rather_than_vanishing() {
        let mut only = vec![s("s1", "ada@x.dev", "claude-code", "api", 1.0, "2026-09-12")];
        only[0].project = None;
        let rows = group_by(&only, "project");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "(none)");
        assert_eq!(rows[0].1.sessions, 1);
    }

    #[test]
    fn daily_sums_all_four_cost_columns() {
        let rows = by_day(&[
            Daily {
                s: "s1".into(),
                d: "2026-09-12".into(),
                cin: 4.0,
                cout: 5.0,
                ccr: 3.0,
                ccw: 0.5,
                tokens_in: 1000,
                tokens_out: 2000,
                cr: 3000,
                cw: 400,
            },
            Daily {
                s: "s2".into(),
                d: "2026-09-11".into(),
                cin: 1.0,
                cout: 2.0,
                ccr: 0.25,
                ..Default::default()
            },
        ]);
        // oldest first
        assert_eq!(rows[0].0, "2026-09-11");
        assert!((rows[1].1.cost - 12.5).abs() < 1e-9, "{}", rows[1].1.cost);
        assert_eq!(rows[1].1.tokens, 6400);
    }

    #[test]
    fn windows_snap_up_so_a_number_is_never_quietly_wider() {
        assert_eq!(snap(7), Some(7));
        assert_eq!(snap(14), Some(30)); // must not silently mean 7
        assert_eq!(snap(30), Some(30));
        assert_eq!(snap(365), None); // wider than 90 is all time
    }

    #[test]
    fn options_parse() {
        let o = parse_opts(&["--days".into(), "14".into(), "--limit".into(), "5".into()]).unwrap();
        assert_eq!(o.asked_days, Some(14));
        assert_eq!(o.days, Some(30));
        assert_eq!(o.limit, 5);

        let all = parse_opts(&["--days".into(), "all".into()]).unwrap();
        assert_eq!(all.days, None);
        assert_eq!(parse_opts(&["--days".into(), "0".into()]).unwrap().days, None);

        assert!(parse_opts(&["--days".into()]).is_err());
        assert!(parse_opts(&["--days".into(), "soon".into()]).is_err());
        assert!(parse_opts(&["--nope".into()]).is_err());

        let one = parse_opts(&["sess-1".into()]).unwrap();
        assert_eq!(one.id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn numbers_are_compact_and_costs_are_exact() {
        assert_eq!(num(999), "999");
        assert_eq!(num(1500), "1.5K");
        assert_eq!(num(2_400_000), "2.4M");
        assert_eq!(money(23.75), "$23.75");
        assert_eq!(money(0.0), "$0.00");
    }

    #[test]
    fn a_table_pads_to_its_widest_cell() {
        let t = table(
            &[("a", Align::L), ("n", Align::R)],
            &[vec!["xx".into(), "1".into()], vec!["y".into(), "100".into()]],
        );
        // widths: col0 = 2 ("xx"), col1 = 3 ("100"); two spaces between columns
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "  a     n");
        assert_eq!(lines[1], "  --  ---");
        assert_eq!(lines[2], "  xx    1"); // numbers right-aligned
        assert_eq!(lines[3], "  y   100");
        // a left-aligned last column must not leave the row padded
        let trailing = table(
            &[("n", Align::R), ("note", Align::L)],
            &[vec!["1".into(), "x".into()], vec!["2".into(), "longer".into()]],
        );
        assert_eq!(trailing.lines().nth(2).unwrap(), "  1  x");
    }

    #[test]
    fn an_empty_result_says_so() {
        assert!(table(&[("a", Align::L)], &[]).contains("nothing in this window"));
    }

    #[test]
    fn the_payload_maps_the_servers_short_keys() {
        let v = serde_json::json!({
            "generatedAt": "2026-09-13T10:00:00+00:00",
            "viewer": {"email": "ada@x.dev", "org": "Acme Inc"},
            "sessions": [{
                "id": "s1", "user": "ada@x.dev", "provider": "claude-code",
                "project": "api", "end": "2026-09-12T09:00:00+00:00",
                "requests": 10, "models": ["claude-sonnet-5"],
                "in": 1000, "out": 2000, "cr": 3000, "cw": 400, "cost": 12.5
            }],
            "daily": [{"s": "s1", "d": "2026-09-12", "in": 1, "out": 2,
                       "cin": 0.5, "cout": 0.25, "ccr": 0.0, "ccw": 0.0}]
        });
        let p = Payload::from_value(v).unwrap();
        assert_eq!(p.generated_at, "2026-09-13T10:00:00+00:00");
        assert_eq!(p.viewer.org, "Acme Inc");
        assert_eq!(p.sessions[0].tokens_in, 1000);
        assert_eq!(p.sessions[0].tokens_out, 2000);
        assert_eq!(p.sessions[0].cr, 3000);
        assert!((p.sessions[0].cost - 12.5).abs() < 1e-9);
        assert_eq!(p.daily[0].d, "2026-09-12");
        assert!((p.daily[0].cin - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_null_column_reads_as_zero_rather_than_failing_the_payload() {
        // one NULL anywhere used to fail the whole response, which broke every
        // read command for the entire organization
        let v = serde_json::json!({
            "generatedAt": "x", "viewer": {"org": "Acme"},
            "sessions": [{"id": "s1", "user": "ada@x.dev", "project": null,
                          "requests": null, "in": null, "out": 5, "cr": null,
                          "cw": null, "cost": null, "models": null}],
            "daily": [{"s": "s1", "d": "2026-09-12", "in": null, "out": null,
                       "cr": null, "cw": null, "cin": null, "cout": null,
                       "ccr": null, "ccw": null}]
        });
        let p = Payload::from_value(v).expect("nulls must not fail the payload");
        assert_eq!(p.sessions[0].requests, 0);
        assert_eq!(p.sessions[0].tokens_in, 0);
        assert_eq!(p.sessions[0].tokens_out, 5);
        assert!((p.sessions[0].cost - 0.0).abs() < 1e-9);
        assert!(p.sessions[0].models.is_empty());
        // and the whole pipeline still runs over it
        let t = totals(&p.sessions);
        assert_eq!(t.sessions, 1);
        assert_eq!(by_day(&p.daily)[0].1.tokens, 0);
        // a null project groups as "(none)", not as a parse failure
        assert_eq!(group_by(&p.sessions, "project")[0].0, "(none)");
    }

    #[test]
    fn tokens_means_the_same_thing_everywhere() {
        // per-group and per-day totals must agree, or a report contradicts
        // itself and an agent invents a reason why
        let g = &group_by(&fixture(), "user")[0].1;
        assert_eq!(g.tokens(), g.tokens_in + g.tokens_out + g.cache_read + g.cache_write);
        let one = vec![s("s1", "a@x.dev", "claude-code", "api", 1.0, "2026-09-12")];
        let per_session = totals(&one).tokens();
        let per_day = by_day(&[Daily {
            s: "s1".into(), d: "2026-09-12".into(),
            tokens_in: 1000, tokens_out: 2000, cr: 3000, cw: 400,
            ..Default::default()
        }])[0].1.tokens;
        assert_eq!(per_session, per_day, "session and daily totals must agree");
    }

    #[test]
    fn a_row_longer_than_its_headers_truncates_rather_than_panicking() {
        let t = table(
            &[("a", Align::L)],
            &[vec!["x".into(), "extra".into(), "more".into()]],
        );
        assert_eq!(t.lines().nth(2).unwrap(), "  x");
    }

    #[test]
    fn an_unknown_field_does_not_break_parsing() {
        // the server may grow the payload; an old client must keep working
        let v = serde_json::json!({
            "generatedAt": "x", "viewer": {"email": "a", "org": "b"},
            "sessions": [{"id": "s1", "brandNewField": 42}], "daily": []
        });
        let p = Payload::from_value(v).unwrap();
        assert_eq!(p.sessions[0].id, "s1");
        assert_eq!(p.sessions[0].requests, 0);
    }
}
