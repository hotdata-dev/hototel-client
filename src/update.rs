//! `hototel update` — check for a newer release, and install it by running
//! the same installer the docs tell people to run.
//!
//! Deliberately NOT a self-updater. Replacing a running executable from inside
//! its own process, then restarting the service that supervises it, is a
//! well-known source of subtle breakage — and `install.sh` already solves the
//! hard parts correctly: it unlinks before writing (a running binary cannot be
//! overwritten on Linux), verifies the archive against the release's
//! SHA256SUMS, re-registers the login service, and only then retires the old
//! binary. Duplicating that in Rust would buy nothing and could drift from it.
//!
//! So this command is a version check plus a delegation. The check is the part
//! that did not exist before: nothing told anyone their machine was stale, and
//! a fleet can sit on half a dozen builds without it showing anywhere.
//!
//! The installer is fetched at the tag being installed, never from `main`. What
//! runs here is a shell script executed on every machine that updates, and
//! tip-of-branch is whatever was pushed a minute ago — including a script that
//! assumes a release which has not been cut.

#[cfg(unix)]
use crate::service::safe_command;

/// A directory only this user can read or write, created atomically.
///
/// The installer is downloaded and then EXECUTED, so where it lands is a
/// security decision. A predictable name in a world-writable /tmp lets another
/// local account pre-create the path as a symlink, or swap the file between the
/// download and the run, and get code execution as whoever typed `update`.
///
/// `create` fails if the path exists, and the 0700 mode is applied as the
/// directory is made rather than after -- so an attacker who guesses a name
/// only makes this pick another one, and never owns a directory we then write
/// into. This is what install.sh gets from `mktemp -d`, without a subprocess.
#[cfg(unix)]
fn private_tempdir() -> Result<std::path::PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    let base = std::env::temp_dir();
    for attempt in 0..64u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(attempt);
        let dir = base.join(format!("hototel-{}-{nanos}-{attempt}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not create a temporary directory: {e}")),
        }
    }
    Err("could not create a temporary directory".into())
}

const REPO: &str = "hotdata-dev/hototel-client";

/// The installer as it stood at one tag. Pinned rather than tracking `main`:
/// see the module comment -- this script is executed, and the tag resolved by
/// `latest_release` is the release it is being asked to install.
fn installer_url(tag: &str) -> String {
    format!("https://raw.githubusercontent.com/{REPO}/{tag}/install.sh")
}

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// A release tag as comparable numbers. "v1.2.3" and "1.2.3" both parse.
///
/// String comparison is wrong here and quietly so: "0.10.0" sorts below
/// "0.9.0", so a fleet would be told it was current for an entire minor
/// series.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let mut next = || it.next()?.split(['-', '+']).next()?.parse::<u32>().ok();
    let (a, b) = (next()?, next()?);
    // a two-part tag is a valid release; treat the missing patch as zero
    Some((a, b, next().unwrap_or(0)))
}

/// Is `latest` newer than `current`? Unparseable input means "cannot tell",
/// which must read as "do not claim an upgrade exists".
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

/// The tag GitHub calls latest. `pub(crate)` because the tray polls it too:
/// the version check must be the one this command uses, or the menu could
/// offer an upgrade the command then refuses to perform.
pub fn latest_release() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    // a version check must never hang the command it is gating
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .build()
        .into();
    let mut resp = agent
        .get(&url)
        // GitHub rejects an API request with no User-Agent
        .header("User-Agent", "hototel")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(403) => {
                "GitHub rate-limited the version check; try again shortly".to_string()
            }
            other => format!("could not reach GitHub: {other}"),
        })?;
    let v: serde_json::Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("unreadable response from GitHub: {e}"))?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    if tag.is_empty() {
        return Err("GitHub did not name a latest release".into());
    }
    Ok(tag)
}

/// Hand off to the installer. Unix only: install.sh refuses anything else, and
/// Windows releases are a zip with no scripted install path.
#[cfg(unix)]
fn run_installer(tag: &str) -> Result<(), String> {
    // Download to a file FIRST, then run the file.
    //
    // `curl ... | sh` reports the exit status of the right-hand side, and a
    // shell handed an empty stdin exits 0 -- so a failed download would look
    // like a successful update that installed nothing. A truncated body is
    // worse than that: install.sh removes the old binary before moving the new
    // one into place, so a script cut between those two lines would leave the
    // machine with no binary at all. Curl's own status catches both (it fails
    // on a partial transfer), but only if something reads it.
    let dir = private_tempdir()?;
    let script = dir.join("install.sh");
    let scrub = || {
        let _ = std::fs::remove_dir_all(&dir);
    };
    // --proto governs the URL curl is given; --proto-redir governs where a
    // redirect may send it, and -L follows redirects. Without the second one,
    // "https only" stops at the first hop.
    let dl = safe_command("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "-o",
        ])
        .arg(&script)
        .arg(installer_url(tag))
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !dl.success() {
        scrub();
        return Err(format!(
            "could not download the installer (curl exited {}); nothing was changed",
            dl.code().unwrap_or(-1)
        ));
    }
    // a zero-length or non-script body means something answered that was not
    // the installer -- a captive portal, a proxy error page
    let body = match std::fs::read_to_string(&script) {
        Ok(b) => b,
        Err(e) => {
            scrub();
            return Err(format!("could not read the downloaded installer: {e}"));
        }
    };
    if !body.starts_with("#!") || body.len() < 512 {
        scrub();
        return Err("what downloaded is not the installer; nothing was changed".into());
    }
    println!("hototel: running the installer...\n");
    // The running binary keeps its own inode when install.sh unlinks the path,
    // so replacing it underneath this process is safe -- this command finishes
    // on the old build and the next invocation is the new one.
    let status = safe_command("sh").arg(&script).status();
    scrub();
    let status = status.map_err(|e| format!("could not run the installer: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("the installer did not complete; re-run it by hand".into())
    }
}

#[cfg(not(unix))]
fn run_installer(_tag: &str) -> Result<(), String> {
    Err(format!(
        "no scripted install on this platform -- download the latest archive from \
         https://github.com/{REPO}/releases/latest and unzip it over the current \
         binary"
    ))
}

/// `check_only` reports and stops; `force` reinstalls even when current.
pub fn run(check_only: bool, force: bool) -> i32 {
    let latest = match latest_release() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("hototel: {e}");
            eprintln!("hototel: installed version is {CURRENT}");
            return 1;
        }
    };
    if parse_version(&latest).is_none() {
        // saying "you are on the latest" when the comparison never happened is
        // the failure this command exists to prevent
        eprintln!("hototel: cannot read the latest release tag ({latest})");
        eprintln!("hototel: installed version is {CURRENT}");
        return 1;
    }
    let newer = is_newer(CURRENT, &latest);
    if !newer && !force {
        // say which is installed either way: "up to date" alone is the kind of
        // message people stop believing after it is once wrong
        println!("hototel {CURRENT} is the latest release ({latest})");
        return 0;
    }
    if newer {
        println!("hototel {CURRENT} is installed; {latest} is available");
    } else {
        println!("hototel {CURRENT} is current; reinstalling anyway (--force)");
    }
    if check_only {
        println!("hototel: run `hototel update` to install it");
        // non-zero so a script or a fleet check can act on "this box is stale"
        return if newer { 10 } else { 0 };
    }
    match run_installer(&latest) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("hototel: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_not_as_strings() {
        // the bug this exists to avoid: "0.10.0" < "0.9.0" as text, which would
        // report a whole minor series as up to date
        assert!(is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("0.6.1", "0.6.2"));
        assert!(is_newer("0.6.1", "1.0.0"));
        assert!(!is_newer("0.6.1", "0.6.1"));
        assert!(!is_newer("0.6.1", "0.6.0"));
    }

    #[test]
    fn tags_parse_with_or_without_a_leading_v() {
        assert_eq!(parse_version("v0.6.1"), Some((0, 6, 1)));
        assert_eq!(parse_version("0.6.1"), Some((0, 6, 1)));
        assert_eq!(parse_version(" v1.2 "), Some((1, 2, 0)));
        assert_eq!(parse_version("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("nonsense"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn an_unreadable_version_never_claims_an_upgrade() {
        // a garbled tag must not be reported as newer -- that would send every
        // machine into a reinstall loop
        assert!(!is_newer("0.6.1", "nonsense"));
        assert!(!is_newer("nonsense", "0.6.1"));
        assert!(!is_newer("", ""));
    }

    #[cfg(unix)]
    #[test]
    fn the_download_directory_is_private_and_unpredictable() {
        use std::os::unix::fs::PermissionsExt;
        let a = private_tempdir().expect("a");
        let b = private_tempdir().expect("b");
        // only this user may read, write or enter it: the file inside is
        // executed, so another local account must not be able to reach it
        let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "mode {mode:o}");
        assert_ne!(a, b, "two calls must not collide");
        // an attacker who guesses a name gets a different one chosen, never a
        // directory they own and we then write into
        assert!(std::fs::DirBuilder::new().create(&a).is_err(), "not atomic");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn a_downloaded_body_that_is_not_a_script_is_rejected() {
        // the guard run_installer applies before executing anything: a proxy
        // error page or a captive portal answers 200 with HTML
        let looks_ok = |b: &str| b.starts_with("#!") && b.len() >= 512;
        assert!(!looks_ok("<html>nope</html>"));
        assert!(!looks_ok(""));
        assert!(!looks_ok("#!/bin/sh\necho hi\n")); // too short to be install.sh
        let real = std::fs::read_to_string("install.sh").expect("install.sh");
        assert!(looks_ok(&real), "the real installer must pass its own guard");
    }

    #[test]
    fn the_installer_is_fetched_at_the_release_being_installed() {
        // it used to fetch main: whatever was pushed to the branch ran on every
        // machine that updated, not the script the release was cut from
        let url = installer_url("v0.7.0");
        assert!(url.ends_with("/v0.7.0/install.sh"), "{url}");
        assert!(!url.contains("/main/"), "{url}");
        assert!(url.starts_with("https://raw.githubusercontent.com/"), "{url}");
    }

    #[test]
    fn the_current_version_is_the_crate_version() {
        assert_eq!(CURRENT, env!("CARGO_PKG_VERSION"));
        assert!(parse_version(CURRENT).is_some(), "{CURRENT} does not parse");
    }
}
