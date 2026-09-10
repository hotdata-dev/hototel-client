//! hotusage collector — macOS menu bar agent (Rust port).
//!
//! Sits in the top menu bar, periodically parses this machine's AI
//! coding-agent history (Claude Code, Codex, OpenCode) and sends changed
//! sessions to the central hotusage server.
//!
//!     hotusage-collector           # menu bar app
//!     hotusage-collector --once    # headless one-shot sync
//!
//! Shares ~/.hotusage/collector.json + collector-state.json with the Python
//! collector.

mod core;
mod parsers;
mod sync;

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

enum UserEvent {
    Menu(MenuEvent),
    SyncRequested,
    SyncDone(String),
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

fn main() {
    if std::env::args().any(|a| a == "--dump") {
        run_dump();
    }
    if std::env::args().any(|a| a == "--once") {
        run_once();
    }

    let config = sync::load_config();
    let interval = Duration::from_secs(config.interval_minutes.max(1) * 60);

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory); // no Dock icon
    }

    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    // periodic sync timer (plus one initial sync fired from Init below)
    let timer_proxy = event_loop.create_proxy();
    thread::spawn(move || loop {
        thread::sleep(interval);
        let _ = timer_proxy.send_event(UserEvent::SyncRequested);
    });

    let sync_proxy = event_loop.create_proxy();
    let syncing = Arc::new(AtomicBool::new(false));

    let mut _tray: Option<TrayIcon> = None;
    let mut status_item: Option<MenuItem> = None;
    let mut sync_id = None;
    let mut dash_id = None;
    let mut cfg_id = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                let status = MenuItem::new("Starting...", false, None);
                let user = MenuItem::new(
                    format!("{} on {}", config.user_email, sync::hostname()),
                    false,
                    None,
                );
                let sync_now = MenuItem::new("Sync Now", true, None);
                let dashboard = MenuItem::new("Open Dashboard", true, None);
                let edit_cfg = MenuItem::new("Edit Config", true, None);
                let menu = Menu::new();
                let _ = menu.append_items(&[
                    &status,
                    &user,
                    &PredefinedMenuItem::separator(),
                    &sync_now,
                    &dashboard,
                    &edit_cfg,
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::quit(Some("Quit hotusage")),
                ]);
                sync_id = Some(sync_now.id().clone());
                dash_id = Some(dashboard.id().clone());
                cfg_id = Some(edit_cfg.id().clone());
                status_item = Some(status);
                _tray = Some(
                    TrayIconBuilder::new()
                        .with_title("\u{23F6}") // ⏶
                        .with_menu(Box::new(menu))
                        .build()
                        .expect("failed to create tray icon"),
                );
                let _ = sync_proxy.send_event(UserEvent::SyncRequested);
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if Some(e.id()) == sync_id.as_ref() {
                    let _ = sync_proxy.send_event(UserEvent::SyncRequested);
                } else if Some(e.id()) == dash_id.as_ref() {
                    let url = sync::load_config().server_url;
                    let _ = Command::new("open").arg(url).spawn();
                } else if Some(e.id()) == cfg_id.as_ref() {
                    let _ = Command::new("open").arg("-t").arg(sync::config_path()).spawn();
                }
            }
            Event::UserEvent(UserEvent::SyncRequested) => {
                if !syncing.swap(true, Ordering::SeqCst) {
                    if let Some(item) = &status_item {
                        item.set_text("Syncing...");
                    }
                    let proxy = sync_proxy.clone();
                    let flag = syncing.clone();
                    thread::spawn(move || {
                        let cfg = sync::load_config();
                        let msg = match sync::sync(&cfg) {
                            Ok(m) | Err(m) => m,
                        };
                        flag.store(false, Ordering::SeqCst);
                        let _ = proxy.send_event(UserEvent::SyncDone(msg));
                    });
                }
            }
            Event::UserEvent(UserEvent::SyncDone(msg)) => {
                if let Some(item) = &status_item {
                    let t = chrono::Local::now().format("%H:%M");
                    item.set_text(format!("Last sync {t}: {msg}"));
                }
            }
            _ => {}
        }
    });
}
