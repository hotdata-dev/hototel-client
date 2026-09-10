//! Desktop indicator: macOS top menu bar / Windows taskbar tray.
//! Linux has no indicator by design — it runs `--daemon` instead.
#![cfg(any(target_os = "macos", target_os = "windows"))]

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::sync;

enum UserEvent {
    Menu(MenuEvent),
    SyncRequested,
    SyncDone(String),
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
}

fn open_config() {
    let path = sync::config_path();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg("-t").arg(&path).spawn();
    #[cfg(target_os = "windows")]
    let _ = Command::new("notepad").arg(&path).spawn();
}

/// Windows tray icons require an image; draw a simple 32x32 up-arrow.
#[cfg(target_os = "windows")]
fn arrow_icon() -> tray_icon::Icon {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    for y in 6..26 {
        let half = (y - 6) * 12 / 20 + 2; // widening triangle
        let cx = N / 2;
        for x in cx.saturating_sub(half)..(cx + half).min(N) {
            let i = (y * N + x) * 4;
            rgba[i] = 255;
            rgba[i + 1] = 255;
            rgba[i + 2] = 255;
            rgba[i + 3] = 255;
        }
    }
    tray_icon::Icon::from_rgba(rgba, N as u32, N as u32).expect("icon")
}

pub fn run() -> ! {
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
                let builder = TrayIconBuilder::new().with_menu(Box::new(menu));
                #[cfg(target_os = "macos")]
                let builder = builder.with_title("\u{23F6}"); // ⏶ in the menu bar
                #[cfg(target_os = "windows")]
                let builder = builder.with_icon(arrow_icon()).with_tooltip("hotusage");
                _tray = Some(builder.build().expect("failed to create tray icon"));
                let _ = sync_proxy.send_event(UserEvent::SyncRequested);
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if Some(e.id()) == sync_id.as_ref() {
                    let _ = sync_proxy.send_event(UserEvent::SyncRequested);
                } else if Some(e.id()) == dash_id.as_ref() {
                    open_url(&sync::load_config().server_url);
                } else if Some(e.id()) == cfg_id.as_ref() {
                    open_config();
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
