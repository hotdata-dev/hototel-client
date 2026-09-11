//! `install` / `uninstall`: register the collector as a continuously running
//! background agent using each platform's native mechanism.
//!
//!   macOS    LaunchAgent (~/Library/LaunchAgents/dev.hotdata.hotusage-collector.plist)
//!            -> runs the menu bar app at login, kept alive
//!   Linux    systemd user unit (~/.config/systemd/user/hotusage-collector.service)
//!            -> runs `--daemon` (headless; no desktop indicator on Linux)
//!   Windows  HKCU Run key -> starts the tray app at login (a session app, not a
//!            Windows Service, because services cannot show a tray icon)

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "dev.hotdata.hotusage-collector";

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

#[cfg(target_os = "macos")]
pub fn install() -> Result<String, String> {
    let exe = exe()?;
    // /tmp is shared and pre-creatable by other local accounts; status lines
    // name the signed-in address, so keep them in the user's own log dir
    let logs = crate::parsers::home_dir().join("Library/Logs");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let log = logs.join("hotusage-collector.log");
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
    // reload cleanly if already installed
    let _ = run("launchctl", &["unload", path.to_str().unwrap()]);
    fs::write(&path, plist).map_err(|e| e.to_string())?;
    run("launchctl", &["load", path.to_str().unwrap()])?;
    Ok(format!("installed LaunchAgent {} (menu bar app, starts at login)", path.display()))
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<String, String> {
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
         Description=hotusage collector (AI coding-agent usage)\n\n\
         [Service]\n\
         ExecStart={} --daemon\n\
         Restart=always\n\
         RestartSec=30\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    );
    let dir = crate::parsers::home_dir().join(".config/systemd/user");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("hotusage-collector.service");
    fs::write(&path, unit).map_err(|e| e.to_string())?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", "hotusage-collector"])?;
    // restart, not `enable --now`: start is a no-op on an already-active unit,
    // so an upgrade would keep running the replaced binary until reboot.
    run("systemctl", &["--user", "restart", "hotusage-collector"])?;
    Ok(format!("installed systemd user unit {} (headless daemon, running now)", path.display()))
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<String, String> {
    let path = crate::parsers::home_dir().join(".config/systemd/user/hotusage-collector.service");
    let _ = run("systemctl", &["--user", "disable", "--now", "hotusage-collector"]);
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
    run(
        "reg",
        &[
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "hotusage-collector",
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
    let _ = run(
        "reg",
        &[
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "hotusage-collector",
            "/f",
        ],
    );
    Ok("removed Run-key autostart".into())
}
