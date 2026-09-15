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

/// ~/.hotusage, created 0700. Public because it is also where the detached
/// updater's log goes: that file records an upgrade that killed the process
/// which started it, so it cannot live anywhere the next run would not look.
pub fn ensure_config_dir() -> std::io::Result<PathBuf> {
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
    /// What the stored token may do, as the server granted it. Empty means a
    /// token minted before scopes existed, which is ingest-only -- so an
    /// existing install keeps syncing and is simply told to sign in again
    /// before it can answer questions.
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server_url: "https://hototel.com".into(),
            token: String::new(),
            user_email: String::new(),
            interval_minutes: 15,
            scopes: Vec::new(),
        }
    }
}

/// Scopes this client asks for: report this machine's usage, and read the org
/// back so the installed skill can answer questions. One approval, both jobs.
pub const WANTED_SCOPES: &str = "ingest,read";

impl Config {
    pub fn has_scope(&self, scope: &str) -> bool {
        if self.scopes.is_empty() {
            return scope == "ingest"; // pre-scopes tokens are ingest-only
        }
        self.scopes.iter().any(|s| s == scope)
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
    // The service moved from hotusage.ai to hototel.com (Sep 2026; the old
    // domain still serves during the transition, but is being retired).
    // Changing the default cannot help existing machines, because their
    // config file already exists -- so repair the value itself, on the exact
    // legacy hosts and nothing else. Same token, same backend: only the host
    // changes. (This replaces the earlier apex-to-www repair, which existed
    // because the old apex once 301'd POSTs into GETs.)
    if let Some(fixed) = legacy_to_hototel(&cfg.server_url) {
        cfg.server_url = fixed;
        let _ = save_config(&cfg);
    }
    if !config_path().is_file() {
        // save_config writes atomically and tightens the mode before the
        // rename, so the file is never briefly visible with open permissions
        let _ = save_config(&cfg);
    }
    restrict(&config_path(), 0o600);
    cfg
}

/// `https://hotusage.ai[/...]` or `https://www.hotusage.ai[/...]` ->
/// `https://hototel.com[/...]`, or None when nothing needs changing.
fn legacy_to_hototel(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://www.hotusage.ai")
        .or_else(|| url.strip_prefix("https://hotusage.ai"))?;
    if rest.is_empty() || rest.starts_with('/') {
        Some(format!("https://hototel.com{rest}"))
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

/// How a session is named in the sync state file. Built in one place because
/// the two sites that need it -- deciding what changed, and recording what was
/// delivered -- must agree exactly: a difference between them would re-send
/// every session forever while claiming each one had been stored.
fn state_key(b: &Built) -> String {
    format!("{}:{}", b.session.provider, b.session.session_id)
}

/// Exact loopback-host match, not substring: "localhost.example.net" must not
/// qualify, and "[::1]:8377" must.
fn is_loopback(url: &str) -> bool {
    let host = url
        .split_once("//")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    // userinfo can disguise the real host ("localhost:x@evil.example" parses
    // to host "localhost" below), and no legitimate loopback URL needs it
    if host.contains('@') {
        return false;
    }
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
    /// What the server said it will grant. A server predating scopes echoes
    /// nothing, which means ingest-only -- reported, not refused, because
    /// reporting usage is still the client's main job.
    pub granted: Vec<String>,
}

/// Ask the server to start a sign-in. The caller opens `verification_url` and
/// then polls; nothing is stored until the person approves in the browser.
pub fn signin_start(server_url: &str) -> Result<SignIn, String> {
    require_secure(server_url)?;
    let url = format!("{}/api/device/start", server_url.trim_end_matches('/'));
    let mut resp = agent(30)
        .post(&url)
        .send_json(serde_json::json!({
            "hostname": hostname(),
            "scope": WANTED_SCOPES,
        }))
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
    let granted: Vec<String> = get("scope")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Ok(SignIn {
        device_code,
        user_code: get("user_code"),
        verification_url: validated_url(&get("verification_url"))?,
        granted,
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
    // Kept so the timeout can say what actually went wrong. Every failure here
    // is retried, which is right for a blip -- but a server answering 500, an
    // expired certificate or an unresolvable host fails identically forever,
    // and reporting only "timed out" after fifteen silent minutes sends people
    // looking at the browser tab instead of at the network.
    let mut last_error: Option<String> = None;
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
            Err(e) => {
                last_error = Some(e.to_string());
                continue;
            }
        };
        let v: serde_json::Value = match resp.body_mut().read_json() {
            Ok(v) => v,
            Err(e) => {
                last_error = Some(format!("unreadable response: {e}"));
                continue;
            }
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
                // what the server said it granted at /api/device/start; an
                // older server says nothing, and has_scope reads that as
                // ingest-only rather than assuming what was asked for
                cfg.scopes = s.granted.clone();
                save_config(&cfg).map_err(|e| format!("could not save config: {e}"))?;
                return Ok(email.to_string());
            }
            "expired" => return Err("this sign-in request expired - try again".into()),
            _ => {}
        }
    }
    Err(match last_error {
        Some(e) => format!("sign-in timed out - last error: {e}"),
        None => "sign-in timed out - try again".to_string(),
    })
}

/// True when this collector holds a credential (so the menu can offer Sign
/// Out rather than Sign In).
pub fn is_signed_in() -> bool {
    !load_config().token.trim().is_empty()
}

/// An authenticated GET against the server's read-only API, for the `usage`
/// subcommands. Kept here beside `sync` so the token, the secure-transport
/// rule and the no-redirect agent have exactly one home.
pub fn api_get(cfg: &Config, path: &str, timeout: u64) -> Result<serde_json::Value, String> {
    if cfg.token.trim().is_empty() {
        return Err("not signed in - run `hototel signin`".into());
    }
    if !cfg.has_scope("read") {
        // the token works, it just was not granted this; say what to do rather
        // than letting the server answer with a bare 401
        return Err("this machine is signed in to report usage but not to read it - \
                    run `hototel signin --force` to grant read access"
            .into());
    }
    require_secure(&cfg.server_url)?;
    let url = format!("{}{}", cfg.server_url.trim_end_matches('/'), path);
    let resp = agent(timeout)
        .get(&url)
        .header("Authorization", &format!("Bearer {}", cfg.token))
        .call();
    match resp {
        Ok(mut r) => r
            .body_mut()
            .read_json()
            .map_err(|e| format!("unreadable response from the server: {e}")),
        Err(ureq::Error::StatusCode(401)) => Err("this machine's access was revoked - \
                                                  run `hototel signin` again"
            .into()),
        Err(ureq::Error::StatusCode(403)) => {
            Err("this machine is not allowed to read that".into())
        }
        Err(ureq::Error::StatusCode(404)) => Err("not found".into()),
        Err(ureq::Error::StatusCode(409)) => {
            Err("this account does not belong to an organization yet".into())
        }
        Err(ureq::Error::RedirectFailed) => Err(format!(
            "{} redirected the request - point server_url at the API host itself",
            cfg.server_url
        )),
        Err(ureq::Error::StatusCode(code)) => Err(format!("server error {code}")),
        Err(e) => Err(format!("could not reach {}: {e}", cfg.server_url)),
    }
}

/// Revoke one specific token server-side, leaving local state alone.
///
/// Split out so a re-authorization can retire the credential it replaces:
/// `signin --force` overwrites the stored token, and without this the old one
/// stays live forever with nobody holding it -- only an org admin could revoke
/// it, and the admin page would show two rows for one machine.
pub fn revoke_token(server_url: &str, token: &str) -> Result<(), String> {
    if token.trim().is_empty() {
        return Ok(());
    }
    require_secure(server_url)?;
    let url = format!("{}/api/collector/signout", server_url.trim_end_matches('/'));
    agent(30)
        .post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .send_empty()
        .map(|_| ())
        .map_err(|e| e.to_string())
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
    // the scopes described the token just discarded; leaving them would make
    // has_scope() answer for a credential that no longer exists
    next.scopes = Vec::new();
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
                    `hototel signin`)"
            .to_string());
    }
    require_secure(&config.server_url)?;
    let scan = scan_all();
    let built: Vec<Built> = scan
        .sessions
        .into_iter()
        .filter_map(build_session)
        .collect();
    let total = built.len();
    // An unreadable transcript parses to nothing, so a directory this process
    // has lost permission to read reports a clean, idle machine. Say it in the
    // status line: "up to date (0 sessions)" is the one answer that must never
    // mean "I could not look".
    let skipped = match scan.unreadable {
        0 => String::new(),
        n => format!(" ({n} files unreadable)"),
    };
    let mut state = load_state();

    let changed: Vec<&Built> = built
        .iter()
        .filter(|b| state.get(&state_key(b)) != Some(&fingerprint(b)))
        .collect();
    if changed.is_empty() {
        return Ok(format!("up to date ({total} sessions){skipped}"));
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
                              check server_url points at the hototel API"
                    .to_string())?;
            if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                return Err(format!(
                    "server rejected the upload: {}",
                    body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error")
                ));
            }
            for b in &changed {
                state.insert(state_key(b), fingerprint(b));
            }
            save_state(&state).map_err(|e| format!("failed to save state: {e}"))?;
            Ok(format!(
                "sent {} changed of {total} sessions{skipped}",
                changed.len()
            ))
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
    use super::{is_loopback, legacy_to_hototel, validated_url};

    #[test]
    fn legacy_hosts_are_repaired_but_nothing_else_is() {
        assert_eq!(legacy_to_hototel("https://hotusage.ai").as_deref(),
                   Some("https://hototel.com"));
        assert_eq!(legacy_to_hototel("https://hotusage.ai/").as_deref(),
                   Some("https://hototel.com/"));
        assert_eq!(legacy_to_hototel("https://www.hotusage.ai").as_deref(),
                   Some("https://hototel.com"));
        assert_eq!(legacy_to_hototel("https://www.hotusage.ai/x").as_deref(),
                   Some("https://hototel.com/x"));
        assert!(legacy_to_hototel("https://hototel.com").is_none());
        assert!(legacy_to_hototel("https://hotusage.ai.evil.example").is_none());
        assert!(legacy_to_hototel("https://www.hotusage.ai.evil.example").is_none());
        assert!(legacy_to_hototel("https://internal.example").is_none());
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
        // userinfo must not disguise the real host
        assert!(!is_loopback("http://localhost:x@evil.example"));
        assert!(!is_loopback("http://127.0.0.1@evil.example/"));
        assert!(!is_loopback("http://user@localhost")); // fail closed: no userinfo at all
    }
}
