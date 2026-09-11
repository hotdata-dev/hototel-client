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
    SignInRequested,
    /// (status line, signed-in address if it succeeded)
    SignInDone(String, Option<String>),
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

/// Flame silhouette, pre-rasterized to a 36x36 alpha mask and baked into the
/// binary. Colorized per platform: black on macOS (a template image, so the
/// system recolors it for dark/light menu bars), white on the Windows tray.
const FLAME_ALPHA: &[u8] = include_bytes!("../assets/flame_alpha_36.bin");
const FLAME_SIZE: u32 = 36;

fn flame_icon(shade: u8) -> tray_icon::Icon {
    let mut rgba = Vec::with_capacity(FLAME_ALPHA.len() * 4);
    for &a in FLAME_ALPHA {
        rgba.extend_from_slice(&[shade, shade, shade, a]);
    }
    tray_icon::Icon::from_rgba(rgba, FLAME_SIZE, FLAME_SIZE).expect("icon")
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
    let signing_in = Arc::new(AtomicBool::new(false));
    let mut status_item: Option<MenuItem> = None;
    let mut user_item: Option<MenuItem> = None;
    let mut signin_id: Option<tray_icon::menu::MenuId> = None;
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
                let sign_in = MenuItem::new("Sign In...", true, None);
                let sync_now = MenuItem::new("Sync Now", true, None);
                let dashboard = MenuItem::new("Open Dashboard", true, None);
                let edit_cfg = MenuItem::new("Edit Config", true, None);
                let menu = Menu::new();
                let _ = menu.append_items(&[
                    &status,
                    &user,
                    &PredefinedMenuItem::separator(),
                    &sign_in,
                    &sync_now,
                    &dashboard,
                    &edit_cfg,
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::quit(Some("Quit hotusage")),
                ]);
                signin_id = Some(sign_in.id().clone());
                sync_id = Some(sync_now.id().clone());
                dash_id = Some(dashboard.id().clone());
                cfg_id = Some(edit_cfg.id().clone());
                status_item = Some(status);
                user_item = Some(user);
                let builder = TrayIconBuilder::new().with_menu(Box::new(menu));
                #[cfg(target_os = "macos")]
                let builder = builder.with_icon(flame_icon(0)).with_icon_as_template(true);
                #[cfg(target_os = "windows")]
                let builder = builder.with_icon(flame_icon(255)).with_tooltip("hotusage");
                _tray = Some(builder.build().expect("failed to create tray icon"));
                let _ = sync_proxy.send_event(UserEvent::SyncRequested);
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if Some(e.id()) == signin_id.as_ref() {
                    let _ = sync_proxy.send_event(UserEvent::SignInRequested);
                } else if Some(e.id()) == sync_id.as_ref() {
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
            Event::UserEvent(UserEvent::SignInRequested) => {
                if !signing_in.swap(true, Ordering::SeqCst) {
                    if let Some(item) = &status_item {
                        item.set_text("Starting sign-in...");
                    }
                    let proxy = sync_proxy.clone();
                    let flag = signing_in.clone();
                    thread::spawn(move || {
                        let server = sync::load_config().server_url;
                        let ev = match sync::signin_start(&server) {
                            Err(e) => UserEvent::SignInDone(e, None),
                            Ok(s) => {
                                // the browser carries the code, and the menu shows
                                // the same one so it can be compared before approving
                                open_url(&s.verification_url);
                                let _ = proxy.send_event(UserEvent::SignInDone(
                                    format!("Approve code {} in your browser", s.user_code),
                                    None,
                                ));
                                match sync::signin_wait(&server, &s) {
                                    Ok(email) => UserEvent::SignInDone(
                                        format!("Signed in as {email}"),
                                        Some(email),
                                    ),
                                    Err(e) => UserEvent::SignInDone(e, None),
                                }
                            }
                        };
                        flag.store(false, Ordering::SeqCst);
                        let _ = proxy.send_event(ev);
                    });
                }
            }
            Event::UserEvent(UserEvent::SignInDone(msg, email)) => {
                if let Some(item) = &status_item {
                    item.set_text(&msg);
                }
                if let Some(email) = email {
                    if let Some(item) = &user_item {
                        item.set_text(format!("{} on {}", email, sync::hostname()));
                    }
                    // a fresh sign-in should show data without waiting a cycle
                    let _ = sync_proxy.send_event(UserEvent::SyncRequested);
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
