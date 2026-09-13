//! hotusage — usage analytics for AI coding agents.
//!
//! Two jobs, one binary. It parses this machine's Claude Code / Codex /
//! OpenCode history and sends changed sessions to the hotusage server; and it
//! reads the organization's usage back, which is what the installed agent skill
//! calls to answer questions. One sign-in covers both.
//!
//!   hotusage                  macOS/Windows: tray app; Linux: headless daemon
//!   hotusage install          register as a login service + install the skill
//!   hotusage uninstall        undo that
//!   hotusage signin           sign in through the browser (--force to re-authorize)
//!   hotusage signout          revoke this machine's token and forget it
//!   hotusage whoami           who this machine is signed in as, and what it may do
//!   hotusage version          which build this is, and where it lives
//!   hotusage sync             sync now, then exit
//!   hotusage daemon           headless sync loop (any OS)
//!   hotusage dump             print parsed sessions as JSON (debug)
//!   hotusage skill install    (re)write the agent skill file
//!
//!   hotusage summary | users | projects | providers | models | daily | chart
//!            | sessions | session <id> | raw        read the org's usage
//!
//! Desktop indicator exists on macOS (top menu bar) and Windows (taskbar tray)
//! only; Linux runs as a plain daemon under a systemd user unit.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod core;
mod parsers;
mod service;
mod skill;
mod sync;
mod usage;
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

/// Print a whole report to stdout, treating a closed pipe as success.
///
/// `hotusage raw | head` is a normal thing to do, and Rust's `println!` panics
/// when the reader goes away -- a panic message in the middle of an agent's
/// output looks like a real failure. Downstream closing early is not an error
/// of this program's.
fn print_out(text: &str) -> ! {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match out
        .write_all(text.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
    {
        Ok(()) => std::process::exit(0),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("hotusage: cannot write output: {e}");
            std::process::exit(1);
        }
    }
}

fn run_once() -> ! {
    let config = sync::load_config();
    println!("hotusage: {} -> {}", config.user_email, config.server_url);
    match sync::sync(&config) {
        Ok(msg) => {
            println!("hotusage: {msg}");
            std::process::exit(0);
        }
        Err(msg) => {
            eprintln!("hotusage: {msg}");
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
    print_out(&serde_json::to_string(&out).unwrap_or_default());
}

/// Browser sign-in for machines with no tray: prints the code, opens or prints
/// the approval URL, and waits for the person to approve.
///
/// `--force` re-authorizes an already signed-in machine. That is the upgrade
/// path for an install whose token predates read access: it can report usage
/// but cannot answer questions, and nothing short of a new token fixes that.
fn run_signin(force: bool) -> ! {
    let cfg = sync::load_config();
    // An upgrade or a second run must not force the person to re-authorise --
    // unless the stored token cannot do everything this build needs. A 0.3.x
    // token reports usage but cannot read it, so the skill silently fails; that
    // is worth one approval rather than a confusing half-working install.
    if sync::is_signed_in() && !force && cfg.has_scope("read") {
        println!("hotusage: already signed in as {}", cfg.user_email);
        println!("hotusage: run `signout` first to switch accounts");
        std::process::exit(0);
    }
    if sync::is_signed_in() {
        let why = if cfg.has_scope("read") {
            "--force"
        } else {
            "this machine's access does not yet cover reading your \
             organization's usage"
        };
        println!("hotusage: re-authorizing {} ({why})", cfg.user_email);
    }
    // captured before signin_wait overwrites it, so the credential being
    // replaced can be revoked rather than left live and unheld
    let previous = cfg.token.clone();
    let server = cfg.server_url;
    let s = match sync::signin_start(&server) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hotusage: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "opening your browser to approve this machine.\nif it does not open, \
         visit:\n  {}\n\ncheck the page shows this code: {}\n\nwaiting for approval...",
        s.verification_url, s.user_code
    );
    service::open_browser(&s.verification_url);
    match sync::signin_wait(&server, &s) {
        Ok(email) => {
            println!("signed in as {email}");
            let now = sync::load_config();
            if !previous.is_empty() && previous != now.token {
                // best effort: the new credential works either way, and a stale
                // one the admin page still lists is better than a failed sign-in
                match sync::revoke_token(&server, &previous) {
                    Ok(()) => println!("hotusage: revoked this machine's previous access"),
                    Err(e) => eprintln!(
                        "hotusage: could not revoke the previous token ({e}); \
                         revoke it from the dashboard's Organization page"
                    ),
                }
            }
            if !now.has_scope("read") {
                // worth saying plainly: the sync half works, the skill will not
                println!(
                    "hotusage: this server does not grant read access, so the \
                     skill cannot answer questions about usage (the server needs \
                     updating). Reporting usage works as normal."
                );
            }
            // first data should land now, not in fifteen minutes
            match sync::sync(&sync::load_config()) {
                Ok(msg) => println!("hotusage: {msg}"),
                Err(msg) => eprintln!("hotusage: first sync failed: {msg}"),
            }
        }
        Err(e) => {
            eprintln!("hotusage: {e}");
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}

/// Headless counterpart to the menu's Sign Out.
fn run_signout() -> ! {
    match sync::signout() {
        Ok(msg) => println!("hotusage: {}", msg.to_lowercase()),
        Err(e) => {
            eprintln!("hotusage: {e}");
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}

/// What this binary is, in one line.
///
/// Exists because diagnosing a stale install otherwise means comparing SHA-256
/// sums against release assets -- which is absurd for a tool that upgrades
/// itself, and cost real time the first time a release raced an install.
fn run_version() -> ! {
    print_out(&format!(
        "hotusage {} ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown path".into())
    ));
}

fn run_whoami() -> ! {
    let cfg = sync::load_config();
    if cfg.token.trim().is_empty() {
        println!("hotusage: not signed in - run `hotusage signin`");
        std::process::exit(2);
    }
    let can = if cfg.has_scope("read") {
        "reports usage and reads this organization's usage"
    } else {
        "reports usage only (run `hotusage signin --force` to add read access)"
    };
    println!(
        "{} at {}\n  {can}\n  hotusage {}",
        cfg.user_email,
        cfg.server_url,
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(0);
}

/// The read-only half: everything the agent skill calls.
fn run_usage(cmd: &str, rest: &[String]) -> ! {
    let opts = match usage::parse_opts(rest) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("hotusage: {e}");
            std::process::exit(2);
        }
    };
    let out = match cmd {
        "summary" => usage::summary(&opts),
        "users" => usage::users(&opts),
        "projects" => usage::projects(&opts),
        "providers" | "tools" => usage::providers(&opts),
        "models" => usage::models(&opts),
        "daily" => usage::daily(&opts),
        "chart" => usage::chart(&opts),
        "sessions" => usage::sessions(&opts),
        "session" => usage::session(&opts),
        "raw" => usage::raw(&opts),
        _ => unreachable!("dispatched by main"),
    };
    match out {
        Ok(text) => print_out(&text),
        Err(e) => {
            eprintln!("hotusage: {e}");
            std::process::exit(1);
        }
    }
}

fn run_skill(rest: &[String]) -> ! {
    match rest.first().map(String::as_str).unwrap_or("install") {
        "install" => {
            // asked for by name: install even where the agent directory does
            // not exist yet, which is what makes this different from the
            // opportunistic pass during `install`
            for line in skill::install(true) {
                println!("hotusage: {line}");
            }
        }
        "uninstall" => {
            let done = skill::uninstall();
            if done.is_empty() {
                println!("hotusage: no skill was installed");
            }
            for line in done {
                println!("hotusage: {line}");
            }
        }
        other => {
            eprintln!("hotusage: unknown skill command '{other}' (install, uninstall)");
            std::process::exit(2);
        }
    }
    std::process::exit(0);
}

fn run_daemon() -> ! {
    let config = sync::load_config();
    println!(
        "hotusage daemon: {} -> {} (every {}m)",
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

    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = args.first().cloned().unwrap_or_default();
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    let flag = |name: &str| rest.iter().any(|a| a == name);

    match arg.as_str() {
        "install" => {
            match service::install() {
                Ok(msg) => println!("hotusage: {msg}"),
                Err(msg) => {
                    eprintln!("hotusage: install failed: {msg}");
                    std::process::exit(1);
                }
            }
            // one download installs both halves; silent when neither agent is
            // present, because someone who uses neither did not ask for this
            for line in skill::install(false) {
                println!("hotusage: {line}");
            }
        }
        "uninstall" => {
            match service::uninstall() {
                Ok(msg) => println!("hotusage: {msg}"),
                Err(msg) => {
                    eprintln!("hotusage: uninstall failed: {msg}");
                    std::process::exit(1);
                }
            }
            for line in skill::uninstall() {
                println!("hotusage: {line}");
            }
        }
        "signin" => run_signin(flag("--force")),
        "signout" => run_signout(),
        "whoami" => run_whoami(),
        "version" | "--version" | "-V" => run_version(),
        "skill" => run_skill(&rest),
        // `--once`/`--dump`/`--daemon` are how already-registered services and
        // older docs invoke this, so both spellings stay.
        "sync" | "--once" => run_once(),
        "dump" | "--dump" => run_dump(),
        "daemon" | "--daemon" => run_daemon(),
        "summary" | "users" | "projects" | "providers" | "tools" | "models" | "daily"
        | "chart" | "sessions" | "session" | "raw" => run_usage(&arg, &rest),
        "help" | "--help" | "-h" => print_out(USAGE),
        "" => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            tray::run();
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            run_daemon();
        }
        other => {
            eprintln!("unknown command '{other}'\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    /// install.sh parses `hotusage version` to prove the binary on disk is the
    /// build it just downloaded, with `awk '{print $2}'`. That contract is easy
    /// to break by reformatting this line, and breaking it silently re-opens
    /// the stale-install hole the command exists to close.
    #[test]
    fn the_version_line_stays_parseable_by_the_installer() {
        let line = format!("hotusage {} (/some/path)", env!("CARGO_PKG_VERSION"));
        let second = line.split_whitespace().nth(1).expect("no second field");
        assert_eq!(second, env!("CARGO_PKG_VERSION"));
        // and it is a bare version, not "v1.2.3" -- install.sh compares it to
        // the tag with the leading v stripped
        assert!(!second.starts_with('v'), "{second}");
        assert!(second.split('.').count() >= 2, "{second}");
    }
}

const USAGE: &str = "\
hotusage — usage analytics for AI coding agents

  hotusage                   run in the background (tray on macOS/Windows)
  hotusage install           register as a login service + install the agent skill
  hotusage uninstall         undo that
  hotusage signin [--force]  sign in through the browser
  hotusage signout           revoke this machine's token
  hotusage whoami            who this machine is signed in as, and what it may do
  hotusage version           which build this is, and where it lives
  hotusage sync              sync now, then exit
  hotusage daemon            headless sync loop
  hotusage dump              print parsed sessions as JSON (debug)
  hotusage skill install     (re)write the agent skill file

read your organization's usage (needs read access; `signin` grants it):

  hotusage summary           totals, top people and projects, recent trend
  hotusage users             per-person breakdown
  hotusage projects          per-project breakdown
  hotusage providers         per-tool breakdown
  hotusage models            which models are being used
  hotusage daily             day-by-day tokens and cost
  hotusage chart             the same series as a stacked bar chart
  hotusage sessions          individual sessions (--user/--project/--provider)
  hotusage session <id>      one session, request by request
  hotusage raw               the whole payload as JSON

  options: --days 7|30|90|all   --limit N   --fresh
  chart also: --metric cost|tokens   --height N   --user/--project/--provider";
