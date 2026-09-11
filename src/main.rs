//! hotusage collector — background agent for AI coding-agent usage.
//!
//! Parses this machine's Claude Code / Codex / OpenCode history and sends
//! changed sessions to the central hotusage server, continuously.
//!
//!   hotusage-collector              macOS/Windows: tray app; Linux: headless daemon
//!   hotusage-collector --daemon     headless sync loop (any OS)
//!   hotusage-collector --once       one-shot sync, then exit
//!   hotusage-collector --dump       print parsed sessions as JSON (debug)
//!   hotusage-collector signin       sign in through the browser (Linux/headless;
//!                                   the tray app has a Sign In... menu item)
//!   hotusage-collector install      register as a login/background service
//!   hotusage-collector uninstall    remove that registration
//!
//! Desktop indicator exists on macOS (top menu bar) and Windows (taskbar tray)
//! only; Linux runs as a plain daemon under a systemd user unit.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod core;
mod parsers;
mod service;
mod sync;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod tray;

use std::thread;
use std::time::Duration;

/// With windows_subsystem="windows" there is no console by default; reattach
/// to the parent's console so --once/--dump/install still print in a terminal.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn run_once() -> ! {
    let config = sync::load_config();
    println!(
        "hotusage collector: {} -> {}",
        config.user_email, config.server_url
    );
    match sync::sync(&config) {
        Ok(msg) => {
            println!("hotusage collector: {msg}");
            std::process::exit(0);
        }
        Err(msg) => {
            eprintln!("hotusage collector: {msg}");
            std::process::exit(1);
        }
    }
}

fn run_dump() -> ! {
    // parity/debug: print every built session as JSON (ignores sync state)
    let built: Vec<core::Built> = parsers::scan_all()
        .into_iter()
        .filter_map(core::build_session)
        .collect();
    let out = serde_json::json!({
        "sessions": built.iter().map(|b| &b.session).collect::<Vec<_>>(),
        "requests": built.iter().flat_map(|b| &b.requests).collect::<Vec<_>>(),
        "daily": built.iter().flat_map(|b| &b.daily).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string(&out).unwrap());
    std::process::exit(0);
}

/// Browser sign-in for machines with no tray: prints the code, opens or prints
/// the approval URL, and waits for the person to approve.
fn run_signin() -> ! {
    let server = sync::load_config().server_url;
    let s = match sync::signin_start(&server) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hotusage collector: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "open this in a browser and check the code matches:\n  {}\n  code: {}\n\nwaiting for approval...",
        s.verification_url, s.user_code
    );
    match sync::signin_wait(&server, &s) {
        Ok(email) => println!("signed in as {email}"),
        Err(e) => {
            eprintln!("hotusage collector: {e}");
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}

fn run_daemon() -> ! {
    let config = sync::load_config();
    println!(
        "hotusage collector daemon: {} -> {} (every {}m)",
        config.user_email, config.server_url, config.interval_minutes
    );
    loop {
        let cfg = sync::load_config();
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        match sync::sync(&cfg) {
            Ok(msg) => println!("[{stamp}] {msg}"),
            Err(msg) => eprintln!("[{stamp}] {msg}"),
        }
        thread::sleep(Duration::from_secs(cfg.interval_minutes.max(1) * 60));
    }
}

fn main() {
    #[cfg(windows)]
    attach_console();

    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "install" => match service::install() {
            Ok(msg) => println!("hotusage collector: {msg}"),
            Err(msg) => {
                eprintln!("hotusage collector: install failed: {msg}");
                std::process::exit(1);
            }
        },
        "uninstall" => match service::uninstall() {
            Ok(msg) => println!("hotusage collector: {msg}"),
            Err(msg) => {
                eprintln!("hotusage collector: uninstall failed: {msg}");
                std::process::exit(1);
            }
        },
        "signin" => run_signin(),
        "--once" => run_once(),
        "--dump" => run_dump(),
        "--daemon" => run_daemon(),
        "" => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            tray::run();
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            run_daemon();
        }
        other => {
            eprintln!(
                "unknown argument '{other}'\n\
                 usage: hotusage-collector [--once | --dump | --daemon | signin | install | uninstall]"
            );
            std::process::exit(2);
        }
    }
}
