//! Provider parsers — 1:1 port of the Python parsers in core.py.
//!
//! Claude Code:  ~/.claude/projects/*/*.jsonl (usage deduped by message.id —
//!               one transcript line per content block repeats the same usage)
//! Codex:        ~/.codex/sessions/**/rollout-*.jsonl (token_count events are
//!               cumulative; deltas keep sums correct; input includes cached)
//! OpenCode:     ~/.local/share/opencode/opencode.db (sqlite)

use crate::core::{rates_claude, rates_openai, truncate_chars, Msg, RawSession};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE")) // Windows
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn home(p: &str) -> PathBuf {
    home_dir().join(p)
}

/// OpenCode's data dir differs by platform; use the first candidate that exists.
fn opencode_db_path() -> Option<PathBuf> {
    let mut candidates = vec![home(".local/share/opencode/opencode.db")];
    for var in ["LOCALAPPDATA", "APPDATA"] {
        if let Ok(base) = std::env::var(var) {
            candidates.push(PathBuf::from(base).join("opencode/opencode.db"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn jstr(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn ji64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------
fn parse_claude_file(path: &Path) -> Option<RawSession> {
    let dirname = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let session_id = path.file_stem()?.to_string_lossy().to_string();
    let content = fs::read_to_string(path).ok()?;

    let mut msgs: HashMap<String, Msg> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut cwd_counts: HashMap<String, u32> = HashMap::new();

    for line in content.lines() {
        if line.contains("\"assistant\"") {
            let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            let m = o.get("message").cloned().unwrap_or(Value::Null);
            let Some(u) = m.get("usage").filter(|u| u.is_object()) else { continue };
            let Some(mid) = m.get("id").and_then(|x| x.as_str()) else { continue };
            if msgs.contains_key(mid) {
                continue;
            }
            let cc = u.get("cache_creation");
            let (cw5, cw1) = match cc.filter(|c| c.is_object()) {
                Some(c) if c.get("ephemeral_5m_input_tokens").is_some()
                    || c.get("ephemeral_1h_input_tokens").is_some() =>
                    (ji64(c, "ephemeral_5m_input_tokens"), ji64(c, "ephemeral_1h_input_tokens")),
                _ => (ji64(u, "cache_creation_input_tokens"), 0),
            };
            msgs.insert(
                mid.to_string(),
                Msg {
                    ts: jstr(&o, "timestamp").unwrap_or_default(),
                    model: jstr(&m, "model").unwrap_or_default(),
                    input: ji64(u, "input_tokens"),
                    output: ji64(u, "output_tokens"),
                    cache_read: ji64(u, "cache_read_input_tokens"),
                    cw5,
                    cw1,
                    ..Default::default()
                },
            );
            order.push(mid.to_string());
            if let Some(cwd) = jstr(&o, "cwd") {
                *cwd_counts.entry(cwd).or_insert(0) += 1;
            }
        } else if title.is_none() && line.contains("\"ai-title\"") {
            let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) == Some("ai-title") {
                title = jstr(&o, "aiTitle");
            }
        } else if first_prompt.is_none() && line.contains("\"user\"") {
            let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) == Some("user") {
                if let Some(content) = o.pointer("/message/content").and_then(|c| c.as_str()) {
                    let t = content.trim();
                    if !t.is_empty() {
                        first_prompt = Some(t.to_string());
                    }
                }
            }
        }
    }

    if msgs.is_empty() {
        return None;
    }
    if title.is_none() {
        title = first_prompt
            .as_deref()
            .and_then(|p| p.lines().next())
            .map(|l| truncate_chars(l, 80));
    }
    let cwd = cwd_counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(c, _)| c)
        .or(Some(dirname));
    Some(RawSession {
        provider: "claude",
        id: session_id,
        cwd,
        title,
        msgs: order.into_iter().filter_map(|k| msgs.remove(&k)).collect(),
    })
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------
fn codex_usage(v: &Value) -> [i64; 3] {
    [
        ji64(v, "input_tokens"),
        ji64(v, "cached_input_tokens"),
        ji64(v, "output_tokens"),
    ]
}

fn parse_codex_file(path: &Path, titles: &HashMap<String, String>) -> Option<RawSession> {
    let content = fs::read_to_string(path).ok()?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut model = String::new();
    let mut prev: Option<[i64; 3]> = None;
    let mut msgs: Vec<Msg> = Vec::new();

    for line in content.lines() {
        if line.contains("\"turn_context\"") {
            let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) == Some("turn_context") {
                if let Some(m) = o.pointer("/payload/model").and_then(|x| x.as_str()) {
                    if !m.is_empty() {
                        model = m.to_string();
                    }
                }
            }
        } else if session_id.is_none() && line.contains("\"session_meta\"") {
            let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
                let p = o.get("payload").cloned().unwrap_or(Value::Null);
                session_id = jstr(&p, "id").or_else(|| jstr(&p, "session_id"));
                cwd = jstr(&p, "cwd");
            }
        } else if line.contains("\"token_count\"") {
            let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
                continue;
            }
            let Some(p) = o.get("payload") else { continue };
            if p.get("type").and_then(|t| t.as_str()) != Some("token_count") {
                continue;
            }
            let Some(info) = p.get("info").filter(|i| i.is_object()) else { continue };
            let tot = codex_usage(info.get("total_token_usage").unwrap_or(&Value::Null));
            let last = codex_usage(info.get("last_token_usage").unwrap_or(&Value::Null));
            let delta = match prev {
                None => tot,
                Some(pv) => {
                    let d = [tot[0] - pv[0], tot[1] - pv[1], tot[2] - pv[2]];
                    if d.iter().any(|x| *x < 0) {
                        last // counter reset
                    } else {
                        d
                    }
                }
            };
            prev = Some(tot);
            let cached = delta[1].max(0);
            msgs.push(Msg {
                ts: jstr(&o, "timestamp").unwrap_or_default(),
                model: model.clone(),
                input: (delta[0] - cached).max(0),
                output: delta[2].max(0),
                cache_read: cached,
                ctx: Some(last[0]),
                ..Default::default()
            });
        } else if first_prompt.is_none() && line.contains("\"user_message\"") {
            let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) == Some("event_msg")
                && o.pointer("/payload/type").and_then(|t| t.as_str()) == Some("user_message")
            {
                if let Some(txt) = o.pointer("/payload/message").and_then(|x| x.as_str()) {
                    let t = txt.trim();
                    if !t.is_empty() {
                        first_prompt = Some(t.to_string());
                    }
                }
            }
        }
    }

    if msgs.is_empty() {
        return None;
    }
    let id = session_id
        .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().to_string());
    let title = titles.get(&id).cloned().or_else(|| {
        first_prompt
            .as_deref()
            .and_then(|p| p.lines().next())
            .map(|l| truncate_chars(l, 80))
    });
    Some(RawSession {
        provider: "codex",
        id,
        cwd,
        title,
        msgs,
    })
}

fn load_codex_titles() -> HashMap<String, String> {
    let mut titles = HashMap::new();
    if let Ok(content) = fs::read_to_string(home(".codex/session_index.jsonl")) {
        for line in content.lines() {
            if let Ok(o) = serde_json::from_str::<Value>(line) {
                if let (Some(id), Some(name)) = (jstr(&o, "id"), jstr(&o, "thread_name")) {
                    if !name.is_empty() {
                        titles.insert(id, name);
                    }
                }
            }
        }
    }
    titles
}

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------
fn parse_opencode() -> Vec<RawSession> {
    let mut out = Vec::new();
    let Some(db_path) = opencode_db_path() else {
        return out;
    };
    let Ok(con) = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return out;
    };
    let sessions: Vec<(String, Option<String>, Option<String>)> = {
        let Ok(mut stmt) =
            con.prepare("SELECT id, title, directory FROM session WHERE parent_id IS NULL")
        else {
            return out;
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };
    for (sid, title, directory) in sessions {
        let datas: Vec<String> = {
            let Ok(mut stmt) =
                con.prepare("SELECT data FROM message WHERE session_id = ? ORDER BY time_created")
            else {
                continue;
            };
            stmt.query_map([&sid], |r| r.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        };
        let mut msgs = Vec::new();
        for data in datas {
            let Ok(d) = serde_json::from_str::<Value>(&data) else { continue };
            if d.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            let Some(tk) = d.get("tokens").filter(|t| t.is_object()) else { continue };
            let Some(created) = d.pointer("/time/created").and_then(|x| x.as_i64()) else {
                continue;
            };
            let cache = tk.get("cache").cloned().unwrap_or(Value::Null);
            let model = jstr(&d, "modelID").unwrap_or_default();
            let provider_id = jstr(&d, "providerID").unwrap_or_default().to_lowercase();
            let mut m = Msg {
                ts: ms_to_iso(created),
                model: model.clone(),
                input: ji64(tk, "input"),
                output: ji64(tk, "output") + ji64(tk, "reasoning"),
                cache_read: ji64(&cache, "read"),
                cw5: ji64(&cache, "write"),
                ..Default::default()
            };
            // our tables know anthropic/openai models; otherwise fall back to
            // opencode's own computed cost, bucketed under output
            let known = if provider_id.contains("anthropic") || model.to_lowercase().contains("claude") {
                rates_claude(&model) != (0.0, 0.0)
            } else {
                rates_openai(&model) != (0.0, 0.0, 0.0)
            };
            if !known {
                if let Some(cost) = d.get("cost").and_then(|c| c.as_f64()) {
                    if cost > 0.0 {
                        m.costs = Some([0.0, cost, 0.0, 0.0]);
                    }
                }
            }
            msgs.push(m);
        }
        if !msgs.is_empty() {
            out.push(RawSession {
                provider: "opencode",
                id: sid,
                cwd: directory,
                title,
                msgs,
            });
        }
    }
    out
}

fn ms_to_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Scan all providers on this machine, merging duplicate (provider, id)
// sessions (e.g. resumed Codex rollouts spanning files).
// ---------------------------------------------------------------------------
fn jsonl_files_at_depth(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for d in &dirs {
            if let Ok(entries) = fs::read_dir(d) {
                for e in entries.flatten() {
                    if e.path().is_dir() {
                        next.push(e.path());
                    }
                }
            }
        }
        dirs = next;
    }
    let mut files = Vec::new();
    for d in &dirs {
        if let Ok(entries) = fs::read_dir(d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
                    files.push(p);
                }
            }
        }
    }
    files
}

pub fn scan_all() -> Vec<RawSession> {
    let mut raws: Vec<RawSession> = Vec::new();

    for p in jsonl_files_at_depth(&home(".claude/projects"), 1) {
        if let Some(r) = parse_claude_file(&p) {
            raws.push(r);
        }
    }
    let titles = load_codex_titles();
    for p in jsonl_files_at_depth(&home(".codex/sessions"), 3) {
        if let Some(r) = parse_codex_file(&p, &titles) {
            raws.push(r);
        }
    }
    raws.extend(parse_opencode());

    // merge by (provider, id)
    let mut merged: HashMap<(String, String), RawSession> = HashMap::new();
    for r in raws {
        let key = (r.provider.to_string(), r.id.clone());
        match merged.get_mut(&key) {
            Some(prev) => {
                if prev.cwd.is_none() {
                    prev.cwd = r.cwd;
                }
                if prev.title.is_none() {
                    prev.title = r.title;
                }
                prev.msgs.extend(r.msgs);
            }
            None => {
                merged.insert(key, r);
            }
        }
    }
    merged.into_values().collect()
}
