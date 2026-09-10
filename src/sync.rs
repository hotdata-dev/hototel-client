//! Config, fingerprint state, and the sync operation (scan -> diff -> POST).
//! Shares ~/.hotusage/collector.json and collector-state.json with the Python
//! collector, so either implementation can take over from the other.

use crate::core::{build_session, Built};
use crate::parsers::scan_all;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn config_dir() -> PathBuf {
    crate::parsers::home_dir().join(".hotusage")
}

pub fn config_path() -> PathBuf {
    config_dir().join("collector.json")
}

fn state_path() -> PathBuf {
    config_dir().join("collector-state.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server_url: String,
    pub token: String,
    pub user_email: String,
    pub interval_minutes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server_url: "https://hotusage.ai".into(),
            token: String::new(),
            user_email: String::new(),
            interval_minutes: 15,
        }
    }
}

fn cmd_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn hostname() -> String {
    cmd_stdout("hostname", &[])
        .or_else(|| std::env::var("COMPUTERNAME").ok()) // Windows fallback
        .unwrap_or_else(|| "unknown-host".into())
}

fn guess_email() -> String {
    cmd_stdout("git", &["config", "user.email"]).unwrap_or_else(|| {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME")) // Windows
            .unwrap_or_else(|_| "unknown".into());
        format!("{user}@{}", hostname())
    })
}

pub fn load_config() -> Config {
    let _ = fs::create_dir_all(config_dir());
    let mut cfg: Config = fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if cfg.user_email.is_empty() {
        cfg.user_email = guess_email();
    }
    if !config_path().is_file() {
        let _ = fs::write(config_path(), serde_json::to_string_pretty(&cfg).unwrap());
    }
    cfg
}

fn load_state() -> HashMap<String, String> {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(state: &HashMap<String, String>) -> std::io::Result<()> {
    fs::create_dir_all(config_dir())?;
    let tmp = state_path().with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string(state).unwrap())?;
    fs::rename(tmp, state_path())
}

fn fingerprint(b: &Built) -> String {
    format!("{}|{}", b.session.ended_at, b.session.requests)
}

/// Parse local history, send changed sessions to the server.
/// Returns a short human status string, Err(status) on failure.
pub fn sync(config: &Config) -> Result<String, String> {
    let built: Vec<Built> = scan_all().into_iter().filter_map(build_session).collect();
    let total = built.len();
    let mut state = load_state();

    let changed: Vec<&Built> = built
        .iter()
        .filter(|b| {
            let key = format!("{}:{}", b.session.provider, b.session.session_id);
            state.get(&key) != Some(&fingerprint(b))
        })
        .collect();
    if changed.is_empty() {
        return Ok(format!("up to date ({total} sessions)"));
    }

    let payload = json!({
        "schema": 1,
        "user_email": config.user_email,
        "hostname": hostname(),
        "sessions": changed.iter().map(|b| &b.session).collect::<Vec<_>>(),
        "requests": changed.iter().flat_map(|b| &b.requests).collect::<Vec<_>>(),
        "daily": changed.iter().flat_map(|b| &b.daily).collect::<Vec<_>>(),
    });

    let url = format!("{}/ingest", config.server_url.trim_end_matches('/'));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into();
    let resp = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", config.token))
        .send_json(&payload);
    match resp {
        Ok(_) => {
            for b in &changed {
                let key = format!("{}:{}", b.session.provider, b.session.session_id);
                state.insert(key, fingerprint(b));
            }
            save_state(&state).map_err(|e| format!("failed to save state: {e}"))?;
            Ok(format!("sent {} changed of {total} sessions", changed.len()))
        }
        Err(ureq::Error::StatusCode(code)) => Err(format!("server error {code}")),
        Err(e) => Err(format!("failed: {e}")),
    }
}
