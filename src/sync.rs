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
use std::time::Duration;

fn config_dir() -> PathBuf {
    crate::parsers::home_dir().join(".hotusage")
}

/// The config holds a bearer token, so keep it off other local accounts.
/// Best-effort: a failure to tighten the mode must not stop the collector.
#[cfg(unix)]
fn restrict(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn restrict(_path: &std::path::Path, _mode: u32) {}

fn ensure_config_dir() -> std::io::Result<PathBuf> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    restrict(&dir, 0o700);
    Ok(dir)
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
            server_url: "https://www.hotusage.ai".into(),
            token: String::new(),
            user_email: String::new(),
            interval_minutes: 15,
        }
    }
}

fn cmd_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    crate::service::safe_command(cmd)
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
    let _ = ensure_config_dir();
    let mut cfg: Config = fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if cfg.user_email.is_empty() {
        cfg.user_email = guess_email();
    }
    // Installs created before the default changed point at the apex, which
    // 301s to www; a POST that follows it arrives as a GET (401 on sign-in,
    // a silently discarded upload on sync). Changing the default cannot help
    // those machines, because their config file already exists -- so repair
    // the value itself, on the exact host and nothing else.
    if let Some(fixed) = apex_to_www(&cfg.server_url) {
        cfg.server_url = fixed;
        let _ = save_config(&cfg);
    }
    if !config_path().is_file() {
        let _ = fs::write(config_path(), serde_json::to_string_pretty(&cfg).unwrap());
    }
    restrict(&config_path(), 0o600);
    cfg
}

/// `https://hotusage.ai[/...]` -> `https://www.hotusage.ai[/...]`, or None
/// when nothing needs changing.
fn apex_to_www(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://hotusage.ai")?;
    if rest.is_empty() || rest.starts_with('/') {
        Some(format!("https://www.hotusage.ai{rest}"))
    } else {
        None
    }
}

pub fn save_config(cfg: &Config) -> std::io::Result<()> {
    ensure_config_dir()?;
    let tmp = config_path().with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(cfg).unwrap())?;
    // tighten before the rename, so the token is never briefly world-readable
    restrict(&tmp, 0o600);
    fs::rename(tmp, config_path())
}

/// Marks state written by a build that verifies delivery. Older state may
/// claim sessions were sent that the server never stored (see the redirect
/// note in `sync`), so it is discarded once and everything is re-sent --
/// harmless, because ingest upserts on (user, session).
const STATE_EPOCH: &str = "__verified_delivery_v1";

fn load_state() -> HashMap<String, String> {
    let mut state: HashMap<String, String> = fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if !state.contains_key(STATE_EPOCH) {
        state.clear();
        state.insert(STATE_EPOCH.to_string(), "1".into());
    }
    state
}

fn save_state(state: &HashMap<String, String>) -> std::io::Result<()> {
    ensure_config_dir()?;
    let tmp = state_path().with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string(state).unwrap())?;
    restrict(&tmp, 0o600);
    fs::rename(tmp, state_path())
}

fn fingerprint(b: &Built) -> String {
    format!("{}|{}", b.session.ended_at, b.session.requests)
}

/// Exact loopback-host match, not substring: "localhost.example.net" must not
/// qualify, and "[::1]:8377" must.
fn is_loopback(url: &str) -> bool {
    let host = url
        .split_once("//")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .unwrap_or("");
    let host = if host.starts_with('[') {
        // bracketed IPv6: strip only a port after the closing bracket
        host.split_once(']').map_or(host, |(h, _)| &host[..h.len() + 1])
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(secs)))
        // A redirect turns a POST into a GET, which silently became "success"
        // against the apex domain's 301: the server never saw the data. Fail
        // loudly instead and let the operator fix the URL.
        .max_redirects(0)
        .build()
        .into()
}

/// The token and every session record travel in this request, so refuse to
/// send them in the clear. Loopback is exempt for local development.
fn require_secure(server_url: &str) -> Result<(), String> {
    if server_url.starts_with("https://") || is_loopback(server_url) {
        return Ok(());
    }
    Err(format!(
        "refusing to use {server_url}: set an https:// server_url in {}",
        config_path().display()
    ))
}

/// The approval URL is opened through the platform shell (`cmd /C start` on
/// Windows), so a hostile or compromised server must not be able to smuggle a
/// command into it: accept only https, or http on a loopback host for local
/// development, and nothing containing shell metacharacters.
fn validated_url(url: &str) -> Result<String, String> {
    let bad = |c: char| "&|;<>^\"'`$(){}[]\\ \t\r\n".contains(c);
    if url.chars().any(bad) {
        return Err("server returned an unusable approval URL".into());
    }
    let ok = url.starts_with("https://") || (url.starts_with("http://") && is_loopback(url));
    if !ok {
        return Err("server returned a non-https approval URL".into());
    }
    Ok(url.to_string())
}

/// A sign-in attempt: the code to show the person, and where to approve it.
pub struct SignIn {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Ask the server to start a sign-in. The caller opens `verification_url` and
/// then polls; nothing is stored until the person approves in the browser.
pub fn signin_start(server_url: &str) -> Result<SignIn, String> {
    require_secure(server_url)?;
    let url = format!("{}/api/device/start", server_url.trim_end_matches('/'));
    let mut resp = agent(30)
        .post(&url)
        .send_json(serde_json::json!({ "hostname": hostname() }))
        .map_err(|e| format!("could not reach {server_url}: {e}"))?;
    let v: serde_json::Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("bad response from server: {e}"))?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let device_code = get("device_code");
    if device_code.is_empty() {
        return Err("server did not start a sign-in".into());
    }
    Ok(SignIn {
        device_code,
        user_code: get("user_code"),
        verification_url: validated_url(&get("verification_url"))?,
        // clamped: an absurd expiry would pin the poll thread (and the
        // "signing in" flag that blocks another attempt) open for days
        interval: v.get("interval").and_then(|x| x.as_u64()).unwrap_or(3).clamp(1, 60),
        expires_in: v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(600).clamp(30, 900),
    })
}

/// Block until the person approves (or the request expires), then persist the
/// token and the address it is bound to. Returns the signed-in address.
pub fn signin_wait(server_url: &str, s: &SignIn) -> Result<String, String> {
    require_secure(server_url)?;
    let url = format!("{}/api/device/poll", server_url.trim_end_matches('/'));
    let deadline = std::time::Instant::now() + Duration::from_secs(s.expires_in);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(s.interval));
        let resp = agent(30)
            .post(&url)
            .send_json(serde_json::json!({ "device_code": s.device_code }));
        let mut resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(400)) => {
                return Err("this sign-in request expired - try again".into())
            }
            // a transient network blip should not abandon the whole attempt
            Err(_) => continue,
        };
        let v: serde_json::Value = match resp.body_mut().read_json() {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("status").and_then(|x| x.as_str()).unwrap_or("") {
            "approved" => {
                let email = v.get("user_email").and_then(|x| x.as_str()).unwrap_or("");
                let token = v.get("token").and_then(|x| x.as_str()).unwrap_or("");
                if email.is_empty() || token.is_empty() {
                    return Err("server approved the sign-in without a token".into());
                }
                let mut cfg = load_config();
                cfg.server_url = server_url.trim_end_matches('/').to_string();
                cfg.user_email = email.to_string();
                cfg.token = token.to_string();
                save_config(&cfg).map_err(|e| format!("could not save config: {e}"))?;
                return Ok(email.to_string());
            }
            "expired" => return Err("this sign-in request expired - try again".into()),
            _ => {}
        }
    }
    Err("sign-in timed out - try again".into())
}

/// True when this collector holds a credential (so the menu can offer Sign
/// Out rather than Sign In).
pub fn is_signed_in() -> bool {
    !load_config().token.trim().is_empty()
}

/// Revoke this collector's token server-side, then forget it locally. The
/// local half happens even if the server cannot be reached -- the point of
/// signing out is that this machine stops reporting.
pub fn signout() -> Result<String, String> {
    let cfg = load_config();
    if cfg.token.trim().is_empty() {
        return Err("not signed in".into());
    }
    require_secure(&cfg.server_url)?;
    let url = format!("{}/api/collector/signout", cfg.server_url.trim_end_matches('/'));
    let remote = agent(30)
        .post(&url)
        .header("Authorization", &format!("Bearer {}", cfg.token))
        .send_empty();
    let mut next = cfg.clone();
    next.token = String::new();
    save_config(&next).map_err(|e| format!("could not clear the local token: {e}"))?;
    match remote {
        Ok(_) => Ok("Signed out".into()),
        // the credential is gone from this machine either way; say which half
        // did not happen so a server-side problem is not read as offline
        Err(ureq::Error::StatusCode(code)) => {
            Ok(format!("Signed out locally (server said {code})"))
        }
        Err(_) => Ok("Signed out locally (server unreachable)".into()),
    }
}

/// Parse local history, send changed sessions to the server.
/// Returns a short human status string, Err(status) on failure.
pub fn sync(config: &Config) -> Result<String, String> {
    // Refuse to ship local session metadata to a remote server without a
    // token: with the production default URL, a fresh unconfigured install
    // must stay silent until the user sets `token` (loopback dev servers are
    // exempt - the server's dev mode accepts tokenless ingest locally).
    if config.token.trim().is_empty() && !is_loopback(&config.server_url) {
        return Err("not signed in: use Sign In... in the menu (or run \
                    `hotusage-collector signin`)"
            .to_string());
    }
    require_secure(&config.server_url)?;
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
    let resp = agent(120)
        .post(&url)
        .header("Authorization", &format!("Bearer {}", config.token))
        .send_json(&payload);
    // A 2xx alone is not proof of delivery: following the apex 301 produced a
    // 200 from the login page while the data went nowhere, and those sessions
    // were then marked sent forever. Require the server's own acknowledgement.
    match resp {
        Ok(mut r) => {
            let body: serde_json::Value = r
                .body_mut()
                .read_json()
                .map_err(|_| "server did not return an ingest response - \
                              check server_url points at the hotusage API"
                    .to_string())?;
            if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                return Err(format!(
                    "server rejected the upload: {}",
                    body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error")
                ));
            }
            for b in &changed {
                let key = format!("{}:{}", b.session.provider, b.session.session_id);
                state.insert(key, fingerprint(b));
            }
            save_state(&state).map_err(|e| format!("failed to save state: {e}"))?;
            Ok(format!("sent {} changed of {total} sessions", changed.len()))
        }
        Err(ureq::Error::RedirectFailed) => Err(format!(
            "{} redirected the upload - point server_url at the API host itself",
            config.server_url
        )),
        Err(ureq::Error::StatusCode(code)) => Err(format!("server error {code}")),
        Err(e) => Err(format!("failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{apex_to_www, is_loopback, validated_url};

    #[test]
    fn apex_is_repaired_but_nothing_else_is() {
        assert_eq!(apex_to_www("https://hotusage.ai").as_deref(),
                   Some("https://www.hotusage.ai"));
        assert_eq!(apex_to_www("https://hotusage.ai/").as_deref(),
                   Some("https://www.hotusage.ai/"));
        assert!(apex_to_www("https://www.hotusage.ai").is_none());
        assert!(apex_to_www("https://hotusage.ai.evil.example").is_none());
        assert!(apex_to_www("https://internal.example").is_none());
    }

    #[test]
    fn approval_url_must_be_safe() {
        assert!(validated_url("https://hotusage.ai/device?code=AB12-CD34").is_ok());
        assert!(validated_url("http://127.0.0.1:8378/device?code=AB12-CD34").is_ok());
        // plain http off-loopback, and anything a shell could split on
        assert!(validated_url("http://evil.example/x").is_err());
        assert!(validated_url("http://x&calc.exe").is_err());
        assert!(validated_url("https://h/a b").is_err());
        assert!(validated_url("").is_err());
    }

    #[test]
    fn loopback_hosts_are_exact_matches() {
        assert!(is_loopback("http://127.0.0.1:8377"));
        assert!(is_loopback("http://localhost:8377/x"));
        assert!(is_loopback("http://localhost"));
        assert!(is_loopback("http://[::1]:8377"));
        assert!(is_loopback("http://[::1]"));
        assert!(!is_loopback("https://hotusage.ai"));
        assert!(!is_loopback("https://localhost.example.net"));
        assert!(!is_loopback("https://127.0.0.1.example.net:443"));
    }
}
