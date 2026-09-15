//! Provider parsers — 1:1 port of the Python parsers in core.py.
//!
//! Claude Code:  ~/.claude/projects/*/*.jsonl (usage deduped by message.id —
//!               one transcript line per content block repeats the same usage)
//! Codex:        ~/.codex/sessions/**/rollout-*.jsonl (token_count events are
//!               cumulative; deltas keep sums correct; input includes cached)
//! OpenCode:     ~/.local/share/opencode/opencode.db (sqlite)

use crate::core::{prices_as_claude, rates_claude, rates_openai, truncate_chars, Msg, RawSession};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// The longest transcript line that will be parsed, in bytes.
///
/// A JSONL line is the parse unit, so it has to be held whole -- and one line
/// is as long as whatever the person pasted into the prompt that produced it.
/// Streaming the file bounds the file, not the line: `BufRead::split` grows a
/// single Vec until the newline arrives, so a multi-gigabyte paste is an
/// out-of-memory kill of a daemon that runs at login. 8 MB is far above any
/// real message and far below anything that hurts.
const MAX_LINE: usize = 8 * 1024 * 1024;

/// One line's bytes. `None` at EOF or on an I/O error (which would repeat
/// forever if the file were retried); `Some(None)` for a line past MAX_LINE,
/// whose bytes are consumed and discarded without ever being buffered.
fn read_line_capped(reader: &mut impl BufRead) -> Option<Option<Vec<u8>>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut over = false;
    let mut started = false;
    loop {
        let (consumed, eol) = {
            let available = reader.fill_buf().ok()?;
            if available.is_empty() {
                // a final line with no trailing newline still counts
                return started.then(|| (!over).then_some(buf));
            }
            started = true;
            match available.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    if !over {
                        buf.extend_from_slice(&available[..i]);
                    }
                    (i + 1, true)
                }
                None => {
                    if !over {
                        buf.extend_from_slice(available);
                    }
                    (available.len(), false)
                }
            }
        };
        reader.consume(consumed);
        if eol {
            return Some((!over).then_some(buf));
        }
        if buf.len() > MAX_LINE {
            // release what was read: staying bounded is the entire point
            over = true;
            buf = Vec::new();
        }
    }
}

/// Stream a file line by line. Transcript files run to hundreds of MB, and the
/// collector re-reads all of them every cycle for the life of the machine, so
/// they must never be pulled into memory whole. A line of invalid UTF-8, or one
/// past MAX_LINE, is skipped on its own -- losing one message's usage, where
/// abandoning the file would lose the whole session. An I/O error ends the file.
fn lines(path: &Path) -> Option<impl Iterator<Item = String>> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    Some(std::iter::from_fn(move || loop {
        let Some(raw) = read_line_capped(&mut reader)? else {
            continue;
        };
        let Ok(mut s) = String::from_utf8(raw) else {
            continue;
        };
        if s.ends_with('\r') {
            s.pop();
        }
        return Some(s);
    }))
}

/// Count a file this process could not open, and pass the failure on.
///
/// A file that cannot be read parses to nothing, which is indistinguishable
/// from a file with no usage in it -- so a permissions problem over the whole
/// transcript directory reported "up to date (0 sessions)" and looked like a
/// healthy, idle machine. The count is what lets `sync` say otherwise.
fn count_unreadable<T>(opened: Option<T>, unreadable: &mut usize) -> Option<T> {
    if opened.is_none() {
        *unreadable += 1;
    }
    opened
}

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
fn parse_claude_file(path: &Path, unreadable: &mut usize) -> Option<RawSession> {
    let dirname = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let session_id = path.file_stem()?.to_string_lossy().to_string();

    let mut msgs: HashMap<String, Msg> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut cwd_counts: HashMap<String, u32> = HashMap::new();

    for line in count_unreadable(lines(path), unreadable)? {
        if line.contains("\"assistant\"") {
            let Ok(o) = serde_json::from_str::<Value>(&line) else { continue };
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
                    // absent on transcripts written before fast mode existed,
                    // which is the same thing as standard
                    fast: jstr(u, "speed").as_deref() == Some("fast"),
                    ..Default::default()
                },
            );
            order.push(mid.to_string());
            if let Some(cwd) = jstr(&o, "cwd") {
                *cwd_counts.entry(cwd).or_insert(0) += 1;
            }
        } else if title.is_none() && line.contains("\"ai-title\"") {
            let Ok(o) = serde_json::from_str::<Value>(&line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) == Some("ai-title") {
                title = jstr(&o, "aiTitle");
            }
        } else if first_prompt.is_none() && line.contains("\"user\"") {
            let Ok(o) = serde_json::from_str::<Value>(&line) else { continue };
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
        provider: crate::core::CLAUDE,
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

fn parse_codex_file(
    path: &Path,
    titles: &HashMap<String, String>,
    unreadable: &mut usize,
) -> Option<RawSession> {
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut model = String::new();
    let mut prev: Option<[i64; 3]> = None;
    let mut msgs: Vec<Msg> = Vec::new();

    for line in count_unreadable(lines(path), unreadable)? {
        if line.contains("\"turn_context\"") {
            let Ok(o) = serde_json::from_str::<Value>(&line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) == Some("turn_context") {
                if let Some(m) = o.pointer("/payload/model").and_then(|x| x.as_str()) {
                    if !m.is_empty() {
                        model = m.to_string();
                    }
                }
            }
        } else if session_id.is_none() && line.contains("\"session_meta\"") {
            let Ok(o) = serde_json::from_str::<Value>(&line) else { continue };
            if o.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
                let p = o.get("payload").cloned().unwrap_or(Value::Null);
                session_id = jstr(&p, "id").or_else(|| jstr(&p, "session_id"));
                cwd = jstr(&p, "cwd");
            }
        } else if line.contains("\"token_count\"") {
            let Ok(o) = serde_json::from_str::<Value>(&line) else { continue };
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
            let Ok(o) = serde_json::from_str::<Value>(&line) else { continue };
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
        provider: crate::core::CODEX,
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
                // opencode routes to a vendor; pricing follows the vendor, not
                // the fact that opencode is what wrote the row
                vendor: Some(provider_id.clone()),
                ..Default::default()
            };
            // our tables know anthropic/openai models; otherwise fall back to
            // opencode's own computed cost, bucketed under output
            let known = if prices_as_claude(&provider_id, &model) {
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
                provider: crate::core::OPENCODE,
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

/// Everything this machine's history yielded, plus what it could not read.
pub struct Scan {
    pub sessions: Vec<RawSession>,
    /// Transcript files that could not be opened. Reported rather than
    /// swallowed: silently, they are the same as no usage at all.
    pub unreadable: usize,
}

pub fn scan_all() -> Scan {
    let mut raws: Vec<RawSession> = Vec::new();
    let mut unreadable = 0usize;

    for p in jsonl_files_at_depth(&home(".claude/projects"), 1) {
        if let Some(r) = parse_claude_file(&p, &mut unreadable) {
            raws.push(r);
        }
    }
    let titles = load_codex_titles();
    for p in jsonl_files_at_depth(&home(".codex/sessions"), 3) {
        if let Some(r) = parse_codex_file(&p, &titles, &mut unreadable) {
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
    Scan {
        sessions: merged.into_values().collect(),
        unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transcript file on disk, in a directory of its own so the Claude
    /// parser's project-name fallback (the parent directory) is predictable.
    fn fixture(name: &str, lines: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hototel-parse-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("fixture dir");
        let path = dir.join(format!("{name}.jsonl"));
        fs::write(&path, format!("{}\n", lines.join("\n"))).expect("fixture file");
        path
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }

    fn claude(path: &Path) -> RawSession {
        let mut unreadable = 0;
        let s = parse_claude_file(path, &mut unreadable).expect("a session");
        assert_eq!(unreadable, 0, "the fixture must be readable");
        s
    }

    fn codex(path: &Path) -> RawSession {
        let mut unreadable = 0;
        let s =
            parse_codex_file(path, &HashMap::new(), &mut unreadable).expect("a session");
        assert_eq!(unreadable, 0, "the fixture must be readable");
        s
    }

    #[test]
    fn claude_counts_one_message_id_once_however_often_it_appears() {
        // Claude Code writes one transcript line per content block, and every
        // one of them repeats the SAME usage object. Counted per line, a long
        // assistant turn multiplies its own tokens by however many blocks it
        // happened to produce.
        let line = r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","cwd":"/w/api","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":3}}}"#;
        let path = fixture("dedupe", &[line, line, line]);
        let s = claude(&path);
        assert_eq!(s.msgs.len(), 1, "one id must count once");
        assert_eq!(s.msgs[0].input, 10);
        assert_eq!(s.msgs[0].output, 5);
        assert_eq!(s.cwd.as_deref(), Some("/w/api"));
        cleanup(&path);
    }

    #[test]
    fn claude_splits_cache_writes_by_ttl_and_falls_back_to_the_old_field() {
        // The two TTLs bill differently (1.25x and 2x input), so they cannot be
        // summed into one number. Transcripts predating the split carry only
        // `cache_creation_input_tokens`, which is the 5-minute product -- and
        // an object with neither ephemeral key has to take that path too, or
        // real cache writes vanish.
        let split = fixture(
            "ttl",
            &[r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":700,"ephemeral_1h_input_tokens":900}}}}"#],
        );
        let s = claude(&split);
        assert_eq!((s.msgs[0].cw5, s.msgs[0].cw1), (700, 900));
        cleanup(&split);

        let legacy = fixture(
            "legacy-ttl",
            &[r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":500}}}"#],
        );
        let s = claude(&legacy);
        assert_eq!((s.msgs[0].cw5, s.msgs[0].cw1), (500, 0), "legacy is 5m");
        cleanup(&legacy);

        let empty_object = fixture(
            "ttl-empty",
            &[r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1,"cache_creation":{},"cache_creation_input_tokens":400}}}"#],
        );
        let s = claude(&empty_object);
        assert_eq!((s.msgs[0].cw5, s.msgs[0].cw1), (400, 0));
        cleanup(&empty_object);
    }

    #[test]
    fn codex_token_counts_are_cumulative_so_only_the_delta_is_new_usage() {
        // Codex reports a running total on every event. Summed as if each were
        // a fresh measurement, a twenty-turn session reports roughly ten times
        // the tokens it used.
        let path = fixture(
            "codex",
            &[
                r#"{"type":"session_meta","payload":{"id":"sess-1","cwd":"/w/api"}}"#,
                r#"{"type":"turn_context","payload":{"model":"gpt-5"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-09-12T10:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20},"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20}}}}"#,
                r#"{"type":"event_msg","timestamp":"2026-09-12T10:05:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":100,"output_tokens":50},"last_token_usage":{"input_tokens":200,"cached_input_tokens":60,"output_tokens":30}}}}"#,
            ],
        );
        let s = codex(&path);
        assert_eq!(s.id, "sess-1");
        assert_eq!(s.cwd.as_deref(), Some("/w/api"));
        assert_eq!(s.msgs.len(), 2);
        assert_eq!(s.msgs[0].model, "gpt-5");
        // first event: the total IS the delta
        assert_eq!((s.msgs[0].input, s.msgs[0].cache_read, s.msgs[0].output), (60, 40, 20));
        // second: 300-100 input of which 100-40 cached, 50-20 output
        assert_eq!((s.msgs[1].input, s.msgs[1].cache_read, s.msgs[1].output), (140, 60, 30));
        cleanup(&path);
    }

    #[test]
    fn a_codex_counter_reset_falls_back_to_the_last_turn_rather_than_going_negative() {
        // A resumed rollout can restart the running total. Subtracting then
        // yields a negative delta, which would either cancel out real usage or
        // clamp the turn to zero; the event's own last_token_usage is the only
        // trustworthy number at that point.
        let path = fixture(
            "codex-reset",
            &[
                r#"{"type":"session_meta","payload":{"id":"sess-2","cwd":"/w/api"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-09-12T10:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":100,"output_tokens":90},"last_token_usage":{"input_tokens":500,"cached_input_tokens":100,"output_tokens":90}}}}"#,
                r#"{"type":"event_msg","timestamp":"2026-09-12T10:05:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":5,"output_tokens":2},"last_token_usage":{"input_tokens":7,"cached_input_tokens":3,"output_tokens":1}}}}"#,
            ],
        );
        let s = codex(&path);
        assert_eq!(s.msgs.len(), 2);
        assert_eq!((s.msgs[1].input, s.msgs[1].cache_read, s.msgs[1].output), (4, 3, 1));
        cleanup(&path);
    }

    #[test]
    fn a_session_with_no_recorded_title_is_named_by_its_first_prompt_line() {
        // This is the fallback the privacy section of SKILL.md has to describe:
        // with no ai-title, the title IS prompt text -- one line of it, capped.
        let long = "x".repeat(200);
        let user = format!(
            r#"{{"type":"user","message":{{"content":"{long}\nsecond line"}}}}"#
        );
        let path = fixture(
            "title",
            &[
                &user,
                r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        let s = claude(&path);
        let title = s.title.expect("a title");
        assert_eq!(title.chars().count(), 80, "capped at 80 chars: {title:?}");
        assert!(!title.contains("second line"), "only the first line");
        cleanup(&path);

        // an ai-title, where one exists, wins over the prompt
        let titled = fixture(
            "ai-title",
            &[
                r#"{"type":"ai-title","aiTitle":"Refactor the ingest endpoint"}"#,
                r#"{"type":"user","message":{"content":"do the thing"}}"#,
                r#"{"type":"assistant","timestamp":"2026-09-12T10:00:00Z","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        assert_eq!(
            claude(&titled).title.as_deref(),
            Some("Refactor the ingest endpoint")
        );
        cleanup(&titled);
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_counted_rather_than_read_as_empty() {
        // the difference between "this machine is idle" and "I could not look"
        let missing = std::env::temp_dir().join("hototel-does-not-exist.jsonl");
        let mut unreadable = 0;
        assert!(parse_claude_file(&missing, &mut unreadable).is_none());
        assert_eq!(unreadable, 1);
        assert!(parse_codex_file(&missing, &HashMap::new(), &mut unreadable).is_none());
        assert_eq!(unreadable, 2);
    }

    #[test]
    fn an_enormous_single_line_is_skipped_instead_of_being_buffered() {
        // A pasted file makes one JSONL line arbitrarily long, and the line is
        // the parse unit -- so an unbounded read is an out-of-memory kill of a
        // daemon that runs at login, triggered by nothing but a big paste.
        let dir = std::env::temp_dir().join(format!("hototel-bigline-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.jsonl");
        fs::write(&path, "x".repeat(9 * 1024 * 1024)).unwrap();
        assert_eq!(lines(&path).unwrap().count(), 0, "the huge line must be dropped");

        // and a normal file is completely unaffected
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(
            lines(&path).unwrap().collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
        // including a line that runs past the reader's buffer without a newline
        fs::write(&path, format!("{}\nshort", "y".repeat(200_000))).unwrap();
        let got: Vec<String> = lines(&path).unwrap().collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].len(), 200_000);
        assert_eq!(got[1], "short");
        let _ = fs::remove_dir_all(&dir);
    }
}
