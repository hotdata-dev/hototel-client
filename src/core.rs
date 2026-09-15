//! Shared types, pricing, and session building — a 1:1 port of core.py.
//! The wire row structs serialize to exactly the field names the hototel
//! server's /ingest endpoint expects.

use chrono::{DateTime, Local};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// One normalized API request/message.
#[derive(Debug, Clone, Default)]
pub struct Msg {
    pub ts: String, // ISO-8601 with Z
    pub model: String,
    pub input: i64, // uncached input tokens
    pub output: i64,
    pub cache_read: i64,
    pub cw5: i64, // 5-minute cache writes (Anthropic)
    pub cw1: i64, // 1-hour cache writes (Anthropic)
    /// Claude Code's fast mode, which bills at a premium -- double the standard
    /// rate on the models that offer it. Priced wrong, a fast-mode session
    /// reads as half what it actually costs, and nothing in the numbers hints
    /// at why.
    pub fast: bool,
    pub ctx: Option<i64>,        // context override (Codex); None = in+cr+cw
    pub costs: Option<[f64; 4]>, // precomputed (in, out, cache read, cache write)
    /// Who actually served this request, when that is not the tool that logged
    /// it. OpenCode is a router: the session's provider id is `opencode`, but
    /// every message records the vendor it was sent to (`anthropic`, `openai`,
    /// ...), and pricing has to follow the vendor. Without it the tool id was
    /// all `msg_costs` could see, so an Anthropic model run through OpenCode
    /// was looked up in the OpenAI table and priced at exactly zero.
    pub vendor: Option<String>,
}

/// The provider ids the parsers emit, and the exact strings that reach the
/// server, the dashboard and `--provider`. Named here so the skill file can be
/// tested against them: documenting "claude-code" (which nothing emits) made
/// `--provider claude-code` match nothing and read as "nobody uses Claude
/// Code" -- a confidently wrong answer, which is worse than an error.
pub const PROVIDERS: [&str; 3] = [CLAUDE, CODEX, OPENCODE];
pub const CLAUDE: &str = "claude";
pub const CODEX: &str = "codex";
pub const OPENCODE: &str = "opencode";

/// A session before building: parser output, merged across files by (provider, id).
pub struct RawSession {
    pub provider: &'static str,
    pub id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub msgs: Vec<Msg>,
}

#[derive(Debug, Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub provider: String,
    pub project: String,
    pub cwd: String,
    pub title: String,
    pub started_at: String,
    pub ended_at: String,
    pub requests: i64,
    pub models: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
    pub cost_cache_write: f64,
    pub cost_total: f64,
    pub peak_context_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct RequestRow {
    pub session_id: String,
    pub provider: String,
    pub seq: i64,
    pub ts: String,
    pub context_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct DailyRow {
    pub session_id: String,
    pub provider: String,
    pub day: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
    pub cost_cache_write: f64,
}

pub struct Built {
    pub session: SessionRow,
    pub requests: Vec<RequestRow>,
    pub daily: Vec<DailyRow>,
}

// ---------------------------------------------------------------------------
// Pricing (USD per MTok, provider list prices as of Sep 2026). Estimates only.
// ---------------------------------------------------------------------------

/// Fast-mode rates, where they are published.
///
/// Fast mode exists on Claude Opus 5 and Opus 4.8 only. Opus 5 is documented at
/// $10/$50 per MTok, double its standard $5/$25; Opus 4.8 shares Opus 5's
/// standard pricing and its fast rate is assumed to match rather than
/// documented -- if that assumption is ever contradicted, this is the line to
/// change. Any other model falls through to standard rates: a model that does
/// not offer fast mode cannot have been billed for it.
fn rates_claude_fast(model: &str) -> Option<(f64, f64)> {
    let m = model.to_lowercase();
    if m.contains("opus-5") || m.contains("opus-4-8") {
        Some((10.0, 50.0))
    } else {
        None
    }
}

/// (input, output). Cache: read 0.1x input, 5m write 1.25x, 1h write 2x.
/// Known limit: `usage.service_tier` is not read.
///
/// Claude Code records it, and a non-standard tier (priority, batch) bills at
/// different rates -- so a session run on one would be priced wrong here. It is
/// deliberately not handled: those rates are not published anywhere this code
/// could cite, and guessing a multiplier would be the same silent-drift bug
/// that ignoring `speed` was, with no way to notice it. Every message in the
/// corpus this was written against is `service_tier: "standard"`. If that ever
/// stops being true, price it from a documented rate or not at all.
pub fn rates_claude(model: &str) -> (f64, f64) {
    let m = model.to_lowercase();
    if m.contains("fable") || m.contains("mythos") {
        (10.0, 50.0)
    } else if m.contains("opus-4-1") || m.contains("opus-4-2025") || m.contains("claude-3-opus") {
        (15.0, 75.0)
    } else if m.contains("opus") {
        (5.0, 25.0)
    } else if m.contains("sonnet") {
        (3.0, 15.0)
    } else if m.contains("haiku-3-5") || m.contains("haiku-3.5") {
        (0.8, 4.0)
    } else if m.contains("haiku-3") {
        (0.25, 1.25)
    } else if m.contains("haiku") {
        (1.0, 5.0)
    } else {
        (0.0, 0.0) // <synthetic>, unknown
    }
}

/// (prefix, input, cached input, output) — most specific prefixes first.
const OPENAI_RATES: &[(&str, f64, f64, f64)] = &[
    ("gpt-5.6-sol", 5.0, 0.5, 30.0),
    ("gpt-5.6-terra", 2.0, 0.2, 12.0),
    ("gpt-5.6-luna", 0.2, 0.02, 1.2),
    ("gpt-6-astra", 10.0, 1.0, 50.0),
    ("astra", 10.0, 1.0, 50.0),
    ("gpt-5-mini", 0.25, 0.025, 2.0),
    ("gpt-5-nano", 0.05, 0.005, 0.4),
    ("gpt-5", 1.25, 0.125, 10.0),
    ("codex-mini", 1.5, 0.375, 6.0),
    ("o3", 2.0, 0.5, 8.0),
    ("o4-mini", 1.1, 0.275, 4.4),
    ("gpt-4.1", 2.0, 0.5, 8.0),
    ("gpt-4o", 2.5, 1.25, 10.0),
];

pub fn rates_openai(model: &str) -> (f64, f64, f64) {
    let m = model.to_lowercase();
    for (prefix, rin, rcache, rout) in OPENAI_RATES {
        if m.starts_with(prefix) {
            return (*rin, *rcache, *rout);
        }
    }
    (0.0, 0.0, 0.0)
}

/// Does this (provider, model) pair belong on the Anthropic rate table?
///
/// One predicate, two callers: pricing here and the parsers' decision about
/// whether a model is one we can price at all. They were written separately and
/// drifted -- the parser accepted an OpenCode session whose `providerID` was
/// "anthropic" as known, threw away OpenCode's own computed cost for it, and
/// then priced it through the OpenAI table, which returns zero for every
/// Anthropic model name. Real tokens, $0.00, no error anywhere. Any future
/// vendor spelling has to be added here and takes both sites with it.
pub fn prices_as_claude(provider: &str, model: &str) -> bool {
    let p = provider.to_lowercase();
    p == CLAUDE || p.contains("anthropic") || model.to_lowercase().contains("claude")
}

/// Cost split by token type: (input, output, cache read, cache write).
fn msg_costs(provider: &str, m: &Msg) -> [f64; 4] {
    if let Some(c) = m.costs {
        return c;
    }
    // the vendor that served the request outranks the tool that logged it
    let provider = m.vendor.as_deref().unwrap_or(provider);
    if prices_as_claude(provider, &m.model) {
        let (rin, rout) = match m.fast.then(|| rates_claude_fast(&m.model)).flatten() {
            Some(fast) => fast,
            None => rates_claude(&m.model),
        };
        [
            m.input as f64 * rin / 1e6,
            m.output as f64 * rout / 1e6,
            m.cache_read as f64 * rin * 0.1 / 1e6,
            (m.cw5 as f64 * 1.25 + m.cw1 as f64 * 2.0) * rin / 1e6,
        ]
    } else {
        // OpenAI-style: cached reads discounted, cache writes not billed
        let (rin, rcache, rout) = rates_openai(&m.model);
        [
            m.input as f64 * rin / 1e6,
            m.output as f64 * rout / 1e6,
            m.cache_read as f64 * rcache / 1e6,
            0.0,
        ]
    }
}

fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

fn local_day(ts: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

pub fn project_name(cwd: Option<&str>, fallback: &str) -> String {
    if let Some(c) = cwd {
        if let Some(base) = c.trim_end_matches('/').rsplit('/').next() {
            if !base.is_empty() {
                return base.to_string();
            }
        }
    }
    fallback.to_string()
}

pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[derive(Default)]
struct DayAgg {
    tok: [i64; 4],  // in, out, cr, cw
    cost: [f64; 4], // cin, cout, ccr, ccw
}

pub fn build_session(raw: RawSession) -> Option<Built> {
    if raw.msgs.is_empty() {
        return None;
    }
    let mut msgs = raw.msgs;
    msgs.sort_by(|a, b| a.ts.cmp(&b.ts));

    let mut tot = [0i64; 4]; // in, out, cr, cw
    let mut costs = [0f64; 4];
    let mut peak_ctx = 0i64;
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut daily: BTreeMap<String, DayAgg> = BTreeMap::new();
    let mut requests: Vec<RequestRow> = Vec::with_capacity(msgs.len());

    for (i, m) in msgs.iter().enumerate() {
        let cw = m.cw5 + m.cw1;
        let ctx = m.ctx.unwrap_or(m.input + m.cache_read + cw);
        let c = msg_costs(raw.provider, m);
        tot[0] += m.input;
        tot[1] += m.output;
        tot[2] += m.cache_read;
        tot[3] += cw;
        for k in 0..4 {
            costs[k] += c[k];
        }
        peak_ctx = peak_ctx.max(ctx);
        if !m.model.is_empty() && m.model != "<synthetic>" {
            models.insert(m.model.clone());
        }
        if let Some(day) = local_day(&m.ts) {
            let agg = daily.entry(day).or_default();
            agg.tok[0] += m.input;
            agg.tok[1] += m.output;
            agg.tok[2] += m.cache_read;
            agg.tok[3] += cw;
            for k in 0..4 {
                agg.cost[k] += c[k];
            }
        }
        requests.push(RequestRow {
            session_id: raw.id.clone(),
            provider: raw.provider.to_string(),
            seq: (i + 1) as i64,
            ts: m.ts.clone(),
            context_tokens: ctx,
            output_tokens: m.output,
        });
    }

    let cwd = raw.cwd.as_deref();
    let session = SessionRow {
        session_id: raw.id.clone(),
        provider: raw.provider.to_string(),
        project: project_name(cwd, "?"),
        cwd: cwd.unwrap_or("?").to_string(),
        title: raw.title.unwrap_or_else(|| "(untitled session)".to_string()),
        started_at: msgs[0].ts.clone(),
        ended_at: msgs[msgs.len() - 1].ts.clone(),
        requests: msgs.len() as i64,
        models: models.into_iter().collect::<Vec<_>>().join(","),
        input_tokens: tot[0],
        output_tokens: tot[1],
        cache_read_tokens: tot[2],
        cache_write_tokens: tot[3],
        cost_input: round4(costs[0]),
        cost_output: round4(costs[1]),
        cost_cache_read: round4(costs[2]),
        cost_cache_write: round4(costs[3]),
        cost_total: round4(costs.iter().sum()),
        peak_context_tokens: peak_ctx,
    };

    let daily_rows = daily
        .into_iter()
        .map(|(day, a)| DailyRow {
            session_id: raw.id.clone(),
            provider: raw.provider.to_string(),
            day,
            input_tokens: a.tok[0],
            output_tokens: a.tok[1],
            cache_read_tokens: a.tok[2],
            cache_write_tokens: a.tok[3],
            cost_input: round4(a.cost[0]),
            cost_output: round4(a.cost[1]),
            cost_cache_read: round4(a.cost[2]),
            cost_cache_write: round4(a.cost[3]),
        })
        .collect();

    Some(Built {
        session,
        requests,
        daily: daily_rows,
    })
}

#[cfg(test)]
mod cost_tests {
    use super::*;

    fn msg(model: &str, fast: bool) -> Msg {
        Msg {
            model: model.into(),
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cw1: 1_000_000,
            fast,
            ..Default::default()
        }
    }

    #[test]
    fn fast_mode_bills_at_the_premium_rate() {
        // Opus 5: $5/$25 standard, $10/$50 fast. Priced as standard, a fast
        // session reads as half what it cost.
        let std = msg_costs("claude", &msg("claude-opus-5", false));
        let fast = msg_costs("claude", &msg("claude-opus-5", true));
        assert_eq!(std[0], 5.0);
        assert_eq!(std[1], 25.0);
        assert_eq!(fast[0], 10.0, "fast input");
        assert_eq!(fast[1], 50.0, "fast output");
        // cache read (0.1x) and 1h cache write (2x) scale with the input rate
        assert!((fast[2] - 1.0).abs() < 1e-9, "{:?}", fast);
        assert!((fast[3] - 20.0).abs() < 1e-9, "{:?}", fast);
        assert!(fast.iter().sum::<f64>() > std.iter().sum::<f64>() * 1.99);
    }

    #[test]
    fn fast_mode_applies_only_where_it_exists() {
        // Fast mode is Opus 5 / Opus 4.8 only. A `speed` value on anything else
        // must not invent a premium that was never billed.
        assert_eq!(rates_claude_fast("claude-opus-4-8"), Some((10.0, 50.0)));
        assert_eq!(rates_claude_fast("claude-opus-4-7"), None);
        assert_eq!(rates_claude_fast("claude-sonnet-5"), None);
        assert_eq!(rates_claude_fast("claude-fable-5"), None);
        let sonnet_std = msg_costs("claude", &msg("claude-sonnet-5", false));
        let sonnet_fast = msg_costs("claude", &msg("claude-sonnet-5", true));
        assert_eq!(sonnet_std, sonnet_fast, "unsupported model must not change");
    }

    #[test]
    fn standard_pricing_is_unchanged_by_the_new_field() {
        // the whole existing corpus is speed=standard; nothing may shift
        for model in ["claude-opus-5", "claude-fable-5", "claude-sonnet-5"] {
            let m = msg(model, false);
            let (rin, rout) = rates_claude(model);
            let c = msg_costs("claude", &m);
            assert_eq!(c[0], rin, "{model} input");
            assert_eq!(c[1], rout, "{model} output");
        }
    }

    #[test]
    fn an_anthropic_model_routed_through_opencode_is_not_priced_at_zero() {
        // The verified bug: OpenCode logs provider "opencode" for the session
        // and "anthropic" for the message. The parser called the model known
        // (so OpenCode's own cost was discarded) and pricing then looked
        // "opus-4-5" up in the OpenAI table, which knows no Anthropic name --
        // three million real tokens reported as $0.00, with nothing to notice.
        let m = Msg {
            model: "opus-4-5".into(),
            vendor: Some("anthropic".into()),
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        };
        let c = msg_costs(OPENCODE, &m);
        assert!(c.iter().sum::<f64>() > 0.0, "priced to nothing: {c:?}");
        assert_eq!(c[0], 5.0, "opus input rate");
        assert_eq!(c[1], 25.0, "opus output rate");
    }

    #[test]
    fn the_pricing_predicate_covers_every_spelling_both_sites_see() {
        // the parser matched on providerID and pricing matched on the session
        // provider; one predicate now answers for both
        assert!(prices_as_claude(CLAUDE, "opus-4-5"));
        assert!(prices_as_claude("anthropic", "opus-4-5"));
        assert!(prices_as_claude("Anthropic", "opus-4-5"));
        assert!(prices_as_claude(OPENCODE, "claude-sonnet-5"));
        assert!(!prices_as_claude(OPENCODE, "gpt-5"));
        assert!(!prices_as_claude(CODEX, "gpt-5"));
    }

    #[test]
    fn a_message_with_no_vendor_still_prices_by_its_tool() {
        // the Claude Code and Codex parsers set no vendor, so the session's own
        // provider has to keep deciding for them
        let claude = msg_costs(CLAUDE, &msg("claude-opus-5", false));
        assert_eq!(claude[0], 5.0);
        let codex = msg_costs(CODEX, &msg("gpt-5", false));
        assert_eq!(codex[0], 1.25, "gpt-5 input rate");
    }

    #[test]
    fn cache_writes_keep_their_separate_ttl_rates() {
        // 1h writes cost 2x input, 5m writes 1.25x -- 91% of real traffic is 1h
        let mut m = msg("claude-opus-5", false);
        m.cw1 = 0;
        m.cw5 = 1_000_000;
        let five = msg_costs("claude", &m);
        let mut m2 = msg("claude-opus-5", false);
        m2.cw5 = 0;
        let hour = msg_costs("claude", &m2);
        assert!((five[3] - 6.25).abs() < 1e-9, "{:?}", five);
        assert!((hour[3] - 10.0).abs() < 1e-9, "{:?}", hour);
    }
}

