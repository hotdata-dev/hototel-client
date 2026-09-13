//! `install` / `uninstall`: register hotusage as a continuously running
//! background agent using each platform's native mechanism.
//!
//!   macOS    LaunchAgent (~/Library/LaunchAgents/dev.hotdata.hotusage.plist)
//!            -> runs the menu bar app at login, kept alive
//!   Linux    systemd user unit (~/.config/systemd/user/hotusage.service)
//!            -> runs `daemon` (headless; no desktop indicator on Linux)
//!   Windows  HKCU Run key -> starts the tray app at login (a session app, not a
//!            Windows Service, because services cannot show a tray icon)
//!
//! These names changed when `hotusage-collector` became `hotusage`. A
//! registration under the old name points at a binary the new installer has
//! replaced or removed, and two registrations would mean two daemons syncing
//! the same machine -- so every install and uninstall sweeps the old one away
//! first. `remove_legacy` is best-effort throughout: a machine that never had
//! the old name must not fail to install because of it.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

// Each platform registers under exactly one of these, so the others would be
// dead code there; cfg keeps the build warning-free without allow(dead_code).
#[cfg(target_os = "macos")]
const LABEL: &str = "dev.hotdata.hotusage";
#[cfg(target_os = "macos")]
const LEGACY_LABEL: &str = "dev.hotdata.hotusage-collector";

#[cfg(target_os = "linux")]
const UNIT: &str = "hotusage";
#[cfg(target_os = "linux")]
const LEGACY_UNIT: &str = "hotusage-collector";

#[cfg(target_os = "windows")]
const RUN_VALUE: &str = "hotusage";
#[cfg(target_os = "windows")]
const LEGACY_RUN_VALUE: &str = "hotusage-collector";

/// A `Command` that cannot be hijacked by a writable directory on `PATH`.
/// This process runs at login and holds a bearer token, so every helper it
/// shells out to (hostname, git, launchctl, reg, open, ...) is resolved
/// against a fixed system PATH instead of the inherited one.
pub fn safe_command(program: &str) -> Command {
    let mut c = Command::new(program);
    #[cfg(unix)]
    c.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin");
    #[cfg(windows)]
    {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        c.env("PATH", format!(r"{root}\System32;{root}"));
    }
    c
}

fn exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve own path: {e}"))
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = safe_command(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Best-effort browser launch. Silent on failure by design: every caller
/// prints the URL as well, which is all a headless box can offer anyway.
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = safe_command("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = safe_command("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = safe_command("cmd").args(["/C", "start", "", url]).spawn();
}

/// Retire any registration left by the `hotusage-collector` name. Best effort:
/// nothing here may fail an install, because a machine that never ran the old
/// name has nothing to clean and must not be punished for it.
#[cfg(target_os = "macos")]
fn remove_legacy() {
    let path =
        crate::parsers::home_dir().join(format!("Library/LaunchAgents/{LEGACY_LABEL}.plist"));
    if path.is_file() {
        if let Some(p) = path.to_str() {
            let _ = run("launchctl", &["unload", p]);
        }
        let _ = fs::remove_file(&path);
        println!("hotusage: removed the old LaunchAgent {LEGACY_LABEL}");
    }
}

#[cfg(target_os = "linux")]
fn remove_legacy() {
    let path = crate::parsers::home_dir()
        .join(format!(".config/systemd/user/{LEGACY_UNIT}.service"));
    if path.is_file() {
        let _ = run("systemctl", &["--user", "disable", "--now", LEGACY_UNIT]);
        let _ = fs::remove_file(&path);
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        println!("hotusage: removed the old systemd unit {LEGACY_UNIT}.service");
    }
}

#[cfg(target_os = "windows")]
fn remove_legacy() {
    // `reg delete` fails when the value is absent, which is the common case;
    // the result is deliberately ignored rather than reported
    let _ = run(
        "reg",
        &[
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            LEGACY_RUN_VALUE,
            "/f",
        ],
    );
}

#[cfg(target_os = "macos")]
pub fn install() -> Result<String, String> {
    let exe = exe()?;
    // /tmp is shared and pre-creatable by other local accounts; status lines
    // name the signed-in address, so keep them in the user's own log dir
    let logs = crate::parsers::home_dir().join("Library/Logs");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let log = logs.join("hotusage.log");
    let log = log.display();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe.display()
    );
    let path = crate::parsers::home_dir().join(format!("Library/LaunchAgents/{LABEL}.plist"));
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    remove_legacy();
    // reload cleanly if already installed
    let _ = run("launchctl", &["unload", path.to_str().unwrap()]);
    fs::write(&path, plist).map_err(|e| e.to_string())?;
    run("launchctl", &["load", path.to_str().unwrap()])?;
    Ok(format!("installed LaunchAgent {} (menu bar app, starts at login)", path.display()))
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<String, String> {
    remove_legacy();
    let path = crate::parsers::home_dir().join(format!("Library/LaunchAgents/{LABEL}.plist"));
    if path.is_file() {
        let _ = run("launchctl", &["unload", path.to_str().unwrap()]);
        fs::remove_file(&path).map_err(|e| e.to_string())?;
        Ok(format!("removed {}", path.display()))
    } else {
        Ok("not installed".into())
    }
}

#[cfg(target_os = "linux")]
pub fn install() -> Result<String, String> {
    let exe = exe()?;
    let unit = format!(
        "[Unit]\n\
         Description=hotusage (AI coding-agent usage)\n\n\
         [Service]\n\
         ExecStart={} daemon\n\
         Restart=always\n\
         RestartSec=30\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    );
    let dir = crate::parsers::home_dir().join(".config/systemd/user");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{UNIT}.service"));
    remove_legacy();
    fs::write(&path, unit).map_err(|e| e.to_string())?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", UNIT])?;
    // restart, not `enable --now`: start is a no-op on an already-active unit,
    // so an upgrade would keep running the replaced binary until reboot.
    run("systemctl", &["--user", "restart", UNIT])?;
    Ok(format!("installed systemd user unit {} (headless daemon, running now)", path.display()))
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<String, String> {
    remove_legacy();
    let path = crate::parsers::home_dir()
        .join(format!(".config/systemd/user/{UNIT}.service"));
    let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
    if path.is_file() {
        fs::remove_file(&path).map_err(|e| e.to_string())?;
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        Ok(format!("removed {}", path.display()))
    } else {
        Ok("not installed".into())
    }
}

#[cfg(target_os = "windows")]
pub fn install() -> Result<String, String> {
    let exe = exe()?;
    remove_legacy();
    run(
        "reg",
        &[
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            RUN_VALUE,
            "/t",
            "REG_SZ",
            "/d",
            &format!("\"{}\"", exe.display()),
            "/f",
        ],
    )?;
    Ok("installed Run-key autostart (tray app, starts at login)".into())
}

#[cfg(target_os = "windows")]
pub fn uninstall() -> Result<String, String> {
    remove_legacy();
    let _ = run(
        "reg",
        &[
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            RUN_VALUE,
            "/f",
        ],
    );
    Ok("removed Run-key autostart".into())
}
