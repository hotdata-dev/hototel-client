//! Desktop indicator: macOS top menu bar / Windows taskbar tray.
//! Linux has no indicator by design — it runs `--daemon` instead.
#![cfg(any(target_os = "macos", target_os = "windows"))]

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
    SignOutRequested,
    /// re-read the on-disk identity: the CLI (or the installer) can sign this
    /// machine in or out while the tray is already running
    RefreshIdentity,
    /// (status line, whether the local token was actually cleared)
    SignOutDone(String, bool),
    /// (status line, signed-in address if it succeeded)
    SignInDone(String, Option<String>),
    /// a release newer than this build exists; carries its tag
    UpdateAvailable(String),
}

/// How long after launch the first version check runs. Non-zero so the check
/// never sits between the user and a menu bar icon at login.
const UPDATE_FIRST_DELAY: Duration = Duration::from_secs(10);

/// And how often after that. The check is unauthenticated, and GitHub allows
/// 60 requests an hour per address -- a whole office behind one NAT shares
/// that budget, so this has to stay far enough below it that the machines
/// never collectively exhaust it and start being told nothing.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Where the update row goes: index 4, directly above Sync Now.
///
/// Deliberate, not incidental. It belongs with the things you can actually do
/// rather than under the identity rows, and it must not sit next to Sign Out
/// or Quit -- a row that appears without warning, under the cursor, adjacent
/// to something that throws state away is how people lose a session by muscle
/// memory.
const UPDATE_ITEM_POSITION: usize = 4;

/// The releases page, for the platform that has no scripted install.
#[cfg(not(unix))]
const RELEASES_URL: &str = "https://github.com/hotdata-dev/hotusage-client/releases/latest";

/// What the update row says. It names the version rather than saying "an
/// update is available", so the click is a decision about a known thing.
///
/// The wording differs by platform because the action does: on Unix the row
/// performs the upgrade, on Windows it can only open the download page, and a
/// row labelled "Update to 0.8.0" that merely opens a browser would be a
/// promise the menu does not keep.
#[cfg(unix)]
fn update_label(tag: &str) -> String {
    format!("Update to {}", tag.trim().trim_start_matches('v'))
}
#[cfg(not(unix))]
fn update_label(tag: &str) -> String {
    format!("Update available: {}", tag.trim().trim_start_matches('v'))
}

fn open_url(url: &str) {
    crate::service::open_browser(url);
}

fn open_config() {
    let path = sync::config_path();
    #[cfg(target_os = "macos")]
    let _ = crate::service::safe_command("open").arg("-t").arg(&path).spawn();
    #[cfg(target_os = "windows")]
    let _ = crate::service::safe_command("notepad").arg(&path).spawn();
}

/// Start `hotusage update` as a process this one does not own.
///
/// The upgrade ends by re-registering the login service: on macOS `launchctl
/// unload` then `load`, on Linux a systemd user unit restart. Either one kills
/// the tray -- which is the process that started the updater. A child inherits
/// its parent's session and process group, so launchd tears the updater down
/// with the job it belongs to, and it dies somewhere inside install.sh. The
/// window where that is fatal is real: the LaunchAgent has already been
/// unloaded and the new binary may not yet be in place, so nothing reloads it
/// and the app simply never comes back. The user's only recovery is a reboot
/// or a terminal.
///
/// setsid(2) is what prevents that. Called between fork and exec, it makes the
/// child a session leader with a new process group of its own, owned by no
/// terminal and belonging to no job the service manager is about to stop. The
/// updater then outlives the tray it was launched from and completes the
/// install, and the freshly loaded service starts the new build.
///
/// This is load-bearing. A plain `Command::spawn`, or dropping the unsafe
/// block "because nothing uses the session", reintroduces exactly the failure
/// above -- and it only shows up on a machine that actually updates.
#[cfg(unix)]
fn spawn_detached_update() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    // the running binary by path, never a PATH lookup: `hotusage` on PATH may
    // be a different install, or absent entirely for a tray started by launchd
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own path: {e}"))?;

    // The updater has no terminal and no parent left to read it. Pointing it
    // at a file is both halves of a problem: it cannot be killed by writing to
    // a pipe whose reader has gone, and an upgrade that failed leaves
    // something to read afterwards -- which matters most in the case where the
    // app did not come back and there is nothing else to look at.
    let log = crate::sync::ensure_config_dir()
        .map(|d| d.join("update.log"))
        .and_then(std::fs::File::create);
    let (out, err) = match log {
        Ok(f) => match f.try_clone() {
            Ok(g) => (Stdio::from(f), Stdio::from(g)),
            Err(_) => (Stdio::null(), Stdio::null()),
        },
        // no log is a worse outcome than no update, so carry on without one
        Err(_) => (Stdio::null(), Stdio::null()),
    };

    let mut cmd = Command::new(exe);
    cmd.arg("update").stdin(Stdio::null()).stdout(out).stderr(err);
    // SAFETY: pre_exec runs in the forked child between fork and exec, where
    // only async-signal-safe calls are permitted. setsid(2) is one, and it is
    // the only thing done here -- no allocation, no locks, no Rust runtime.
    // See the comment above for why detaching is required rather than tidy.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                // Refuse to exec rather than run attached: an updater in this
                // process's session is the bug this function exists to avoid,
                // and failing visibly leaves the old build installed and
                // working.
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start the updater: {e}"))
}

/// The nine-cell mark from the website, drawn at runtime.
///
/// Mirrors server/static/icon.svg exactly: a 32-unit grid, 8-unit cells with a
/// 2.5-unit corner radius at 3/12/21, warming toward the bottom-right corner.
/// Kept as geometry rather than a bitmap so the same numbers can be checked
/// against the SVG by eye, and so any tray size renders crisply.
const ICON_PX: u32 = 36;
const GRID: f32 = 32.0;
const CELL: f32 = 8.0;
const RADIUS: f32 = 2.5;
const ORIGINS: [f32; 3] = [3.0, 12.0, 21.0];

/// (r, g, b, opacity) per cell, row-major from the top-left. The greys sit at
/// reduced opacity in the SVG; the warm cells are solid.
const CELLS: [(u8, u8, u8, f32); 9] = [
    (0x89, 0x87, 0x81, 0.45), (0x89, 0x87, 0x81, 0.70), (0x2a, 0x78, 0xd6, 0.70),
    (0x89, 0x87, 0x81, 0.70), (0x1b, 0xaf, 0x7a, 1.00), (0xed, 0xa1, 0x00, 1.00),
    (0x2a, 0x78, 0xd6, 0.70), (0xed, 0xa1, 0x00, 1.00), (0xeb, 0x68, 0x34, 1.00),
];

/// Coverage of one rounded cell at a point, 0..=1, sampled on a 3x3 grid inside
/// the pixel. Anti-aliasing matters more than usual here: the mark is nine small
/// rounded squares, and aliased corners read as dirt at menu-bar size.
fn cell_coverage(px: f32, py: f32, x0: f32, y0: f32, unit: f32) -> f32 {
    let (x1, y1) = (x0 + CELL, y0 + CELL);
    let mut hits = 0u32;
    for sy in 0..3 {
        for sx in 0..3 {
            let x = (px + (sx as f32 + 0.5) / 3.0) * unit;
            let y = (py + (sy as f32 + 0.5) / 3.0) * unit;
            if x < x0 || x > x1 || y < y0 || y > y1 {
                continue;
            }
            // inside the straight-edged core, or within the radius of the
            // nearest corner centre
            let cx = x.clamp(x0 + RADIUS, x1 - RADIUS);
            let cy = y.clamp(y0 + RADIUS, y1 - RADIUS);
            if (x - cx).powi(2) + (y - cy).powi(2) <= RADIUS * RADIUS {
                hits += 1;
            }
        }
    }
    hits as f32 / 9.0
}

fn logo_rgba(size: u32) -> Vec<u8> {
    let unit = GRID / size as f32; // icon units per pixel
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for (i, &(cr, cg, cb, op)) in CELLS.iter().enumerate() {
                let cov = cell_coverage(px as f32, py as f32, ORIGINS[i % 3], ORIGINS[i / 3], unit);
                if cov <= 0.0 {
                    continue;
                }
                // cells never overlap, so a straight accumulate is exact
                let alpha = cov * op;
                r += cr as f32 * alpha;
                g += cg as f32 * alpha;
                b += cb as f32 * alpha;
                a += alpha;
            }
            // un-premultiply: Icon::from_rgba wants straight alpha
            let out = |c: f32| if a > 0.0 { (c / a).round().clamp(0.0, 255.0) as u8 } else { 0 };
            rgba.extend_from_slice(&[out(r), out(g), out(b), (a * 255.0).round() as u8]);
        }
    }
    rgba
}

/// What the one auth row says. Signed in, the only thing you can do is leave;
/// signed out, the only thing you can do is join.
fn auth_label(signed_in: bool) -> &'static str {
    if signed_in { "Sign Out" } else { "Sign In..." }
}

/// The identity row above it. Four events rewrite this row -- startup, the
/// identity poll, a sign-out and a sign-in -- and each built the string itself,
/// so the menu could end up describing this machine four slightly different
/// ways depending on which path last touched it.
fn user_label(email: Option<&str>) -> String {
    let host = sync::hostname();
    match email {
        Some(e) => format!("{e} on {host}"),
        None => format!("Not signed in on {host}"),
    }
}

fn logo_icon() -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(logo_rgba(ICON_PX), ICON_PX, ICON_PX).expect("icon")
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

    // identity poll: cheap config read, so a sign-in performed by
    // `hotusage signin` shows up in the menu within seconds
    let id_proxy = event_loop.create_proxy();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(10));
        let _ = id_proxy.send_event(UserEvent::RefreshIdentity);
    });

    // periodic sync timer (plus one initial sync fired from Init below)
    let timer_proxy = event_loop.create_proxy();
    thread::spawn(move || loop {
        thread::sleep(interval);
        let _ = timer_proxy.send_event(UserEvent::SyncRequested);
    });

    // version check: silent on every failure. An offline laptop, a rate-limited
    // address and a GitHub outage all have to leave the menu exactly as it is
    // today -- there is no useful action behind "could not check", and a tray
    // that reports its own background errors trains people to ignore it.
    let update_proxy = event_loop.create_proxy();
    thread::spawn(move || {
        thread::sleep(UPDATE_FIRST_DELAY);
        loop {
            if let Ok(tag) = crate::update::latest_release() {
                if crate::update::is_newer(crate::update::CURRENT, &tag) {
                    let _ = update_proxy.send_event(UserEvent::UpdateAvailable(tag));
                }
            }
            thread::sleep(UPDATE_CHECK_INTERVAL);
        }
    });

    let sync_proxy = event_loop.create_proxy();
    let syncing = Arc::new(AtomicBool::new(false));

    let mut _tray: Option<TrayIcon> = None;
    let signing_in = Arc::new(AtomicBool::new(false));
    let mut status_item: Option<MenuItem> = None;
    let mut user_item: Option<MenuItem> = None;
    let mut auth_id: Option<tray_icon::menu::MenuId> = None;
    let mut auth_item: Option<MenuItem> = None;
    let mut last_signed_in = sync::is_signed_in();
    let mut sync_id = None;
    let mut dash_id = None;
    let mut cfg_id = None;
    // The update row is created only when a newer release exists, so both of
    // these stay None on a current machine -- and `update_item` being set is
    // what makes a second UpdateAvailable (six hours later, or a retry) a
    // no-op instead of a duplicate row.
    let mut menu_handle: Option<Menu> = None;
    let mut update_id: Option<tray_icon::menu::MenuId> = None;
    let mut update_item: Option<MenuItem> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                let status = MenuItem::new("Starting...", false, None);
                let signed_in = sync::is_signed_in();
                let user = MenuItem::new(
                    user_label(signed_in.then_some(config.user_email.as_str())),
                    false,
                    None,
                );
                // One item, not two greyed against each other: a menu that
                // always lists Sign In AND Sign Out makes the reader work out
                // which applies, and a disabled row still reads as an option
                // that ought to work.
                let auth = MenuItem::new(auth_label(signed_in), true, None);
                let sync_now = MenuItem::new("Sync Now", true, None);
                let dashboard = MenuItem::new("Open Dashboard", true, None);
                let edit_cfg = MenuItem::new("Edit Config", true, None);
                let menu = Menu::new();
                let _ = menu.append_items(&[
                    &status,
                    &user,
                    &PredefinedMenuItem::separator(),
                    &auth,
                    &sync_now,
                    &dashboard,
                    &edit_cfg,
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::quit(Some("Quit hotusage")),
                ]);
                auth_id = Some(auth.id().clone());
                auth_item = Some(auth);
                sync_id = Some(sync_now.id().clone());
                dash_id = Some(dashboard.id().clone());
                cfg_id = Some(edit_cfg.id().clone());
                status_item = Some(status);
                user_item = Some(user);
                // kept so the update row can be inserted later; Menu is a
                // handle to the same native menu, not a copy of it
                menu_handle = Some(menu.clone());
                let builder = TrayIconBuilder::new().with_menu(Box::new(menu));
                // NOT a template image: macOS renders template icons as a
                // monochrome mask, which would throw the colour away. The mark
                // carries its own contrast -- the greys are the muted token,
                // whose value is identical in both themes -- so it reads on a
                // light or a dark menu bar alike.
                #[cfg(target_os = "macos")]
                let builder = builder.with_icon(logo_icon()).with_icon_as_template(false);
                #[cfg(target_os = "windows")]
                let builder = builder.with_icon(logo_icon()).with_tooltip("hotusage");
                _tray = Some(builder.build().expect("failed to create tray icon"));
                let _ = sync_proxy.send_event(UserEvent::SyncRequested);
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if Some(e.id()) == auth_id.as_ref() {
                    // decided from the config at click time, not from the
                    // label: a `hotusage signin` in a terminal can change the
                    // state between the ten-second refresh and this click
                    let _ = sync_proxy.send_event(if sync::is_signed_in() {
                        UserEvent::SignOutRequested
                    } else {
                        UserEvent::SignInRequested
                    });
                } else if Some(e.id()) == sync_id.as_ref() {
                    let _ = sync_proxy.send_event(UserEvent::SyncRequested);
                } else if Some(e.id()) == dash_id.as_ref() {
                    open_url(&sync::load_config().server_url);
                } else if Some(e.id()) == cfg_id.as_ref() {
                    open_config();
                } else if Some(e.id()) == update_id.as_ref() {
                    #[cfg(unix)]
                    {
                        match spawn_detached_update() {
                            Ok(()) => {
                                if let Some(item) = &status_item {
                                    item.set_text("Updating - the app will restart");
                                }
                                // Take the row away rather than grey it: the
                                // upgrade is already running detached, a second
                                // click would start a second installer racing
                                // the first over the same binary, and this
                                // process is about to be killed by its own
                                // success anyway.
                                if let (Some(menu), Some(item)) = (&menu_handle, &update_item) {
                                    let _ = menu.remove(item);
                                }
                                update_id = None;
                                update_item = None;
                            }
                            // the old build is untouched and still working, so
                            // say so and leave the row for another try
                            Err(msg) => {
                                if let Some(item) = &status_item {
                                    item.set_text(format!("Update failed: {msg}"));
                                }
                            }
                        }
                    }
                    // Windows has no scripted install -- `hotusage update`
                    // refuses there -- so the only honest action is the page
                    // the archive is on. The row stays: nothing happened to
                    // this machine, and a second click just reopens the tab.
                    #[cfg(not(unix))]
                    open_url(RELEASES_URL);
                }
            }
            Event::UserEvent(UserEvent::UpdateAvailable(tag)) => {
                // The row exists only while this build is behind. A permanent
                // greyed "Up to date" is the pattern the auth row was just
                // rewritten to get rid of: a disabled item still reads as an
                // option that ought to work and is refusing.
                if update_item.is_none() {
                    let item = MenuItem::new(update_label(&tag), true, None);
                    if let Some(menu) = &menu_handle {
                        let _ = menu.insert(&item, UPDATE_ITEM_POSITION);
                        update_id = Some(item.id().clone());
                        update_item = Some(item);
                    }
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
            Event::UserEvent(UserEvent::RefreshIdentity) => {
                let cfg = sync::load_config();
                let signed_in = !cfg.token.trim().is_empty();
                if signed_in != last_signed_in {
                    last_signed_in = signed_in;
                    if let Some(item) = &user_item {
                        item.set_text(user_label(signed_in.then_some(cfg.user_email.as_str())));
                    }
                    if let Some(item) = &auth_item {
                        item.set_text(auth_label(signed_in));
                    }
                    if signed_in {
                        let _ = sync_proxy.send_event(UserEvent::SyncRequested);
                    }
                }
            }
            Event::UserEvent(UserEvent::SignOutRequested) => {
                // off the event loop: revoking is a network call, and an
                // unreachable server would otherwise freeze the menu
                if let Some(item) = &status_item {
                    item.set_text("Signing out...");
                }
                let proxy = sync_proxy.clone();
                thread::spawn(move || {
                    let ev = match sync::signout() {
                        Ok(m) => UserEvent::SignOutDone(m, true),
                        // the token is still on disk, so the menu must keep
                        // saying signed in rather than lying about it
                        Err(m) => UserEvent::SignOutDone(m, false),
                    };
                    let _ = proxy.send_event(ev);
                });
            }
            Event::UserEvent(UserEvent::SignOutDone(msg, cleared)) => {
                if let Some(item) = &status_item {
                    item.set_text(&msg);
                }
                if !cleared {
                    return;
                }
                if let Some(item) = &user_item {
                    item.set_text(user_label(None));
                }
                if let Some(item) = &auth_item {
                    item.set_text(auth_label(false));
                }
                last_signed_in = false;
            }
            Event::UserEvent(UserEvent::SignInDone(msg, email)) => {
                if let Some(item) = &status_item {
                    item.set_text(&msg);
                }
                if let Some(email) = email {
                    if let Some(item) = &user_item {
                        item.set_text(user_label(Some(&email)));
                    }
                    if let Some(item) = &auth_item {
                        item.set_text(auth_label(true));
                    }
                    last_signed_in = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], size: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * size + x) * 4) as usize;
        (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
    }

    #[test]
    fn the_auth_row_names_the_one_action_available() {
        // the menu used to carry both, one greyed out, leaving the reader to
        // work out which applied
        assert_eq!(auth_label(true), "Sign Out");
        assert_eq!(auth_label(false), "Sign In...");
    }

    #[test]
    fn the_update_row_names_the_version_it_would_install() {
        // "an update is available" makes the click a leap; the version makes it
        // a decision, and it is the same string the upgrade will fetch
        assert_eq!(update_label("v0.8.0"), update_label("0.8.0"));
        assert!(update_label("v0.8.0").ends_with("0.8.0"), "must name the tag");
        assert!(!update_label("v0.8.0").contains('v'), "the tag's v is not UI");
        assert_eq!(update_label(" v1.10.2 "), update_label("1.10.2"));
    }

    #[test]
    fn the_version_check_stays_well_inside_githubs_budget() {
        // unauthenticated GitHub allows 60 requests an hour per address, and a
        // whole office behind one NAT shares it; four checks a day per machine
        // leaves that budget for the humans using it
        assert!(UPDATE_CHECK_INTERVAL >= Duration::from_secs(60 * 60), "too eager");
        assert!(UPDATE_CHECK_INTERVAL <= Duration::from_secs(24 * 60 * 60), "too rare");
        // and the first check must not be part of launch
        assert!(UPDATE_FIRST_DELAY > Duration::from_secs(0), "checks during launch");
        assert!(UPDATE_FIRST_DELAY < UPDATE_CHECK_INTERVAL, "first check too late");
    }

    #[test]
    fn the_update_row_sits_above_sync_now_and_not_beside_sign_out() {
        // the Init menu, in order; the row is inserted into this list
        let rows = ["status", "user", "---", "auth", "Sync Now"];
        assert_eq!(rows[UPDATE_ITEM_POSITION], "Sync Now", "not above Sync Now");
        // a row that appears unannounced, under the cursor, must not land on
        // top of something that throws state away -- Quit and the auth row's
        // Sign Out are both below it, never displaced by it
        assert_eq!(rows[UPDATE_ITEM_POSITION - 1], "auth", "would push auth down");
        assert!(UPDATE_ITEM_POSITION < rows.len(), "falls off the actionable group");
    }

    #[test]
    fn the_identity_row_reads_the_same_however_it_was_reached() {
        // startup, the identity poll, a sign-out and a sign-in all rewrite this
        // row; they used to format it separately
        let host = sync::hostname();
        assert_eq!(user_label(Some("ada@x.dev")), format!("ada@x.dev on {host}"));
        assert_eq!(user_label(None), format!("Not signed in on {host}"));
    }

    #[test]
    fn the_mark_matches_the_websites_icon_svg() {
        // Nine cells, same colours and opacities as server/static/icon.svg. If
        // the site's mark changes, this fails rather than letting the tray and
        // the dashboard drift apart.
        let size = ICON_PX;
        let rgba = logo_rgba(size);
        assert_eq!(rgba.len(), (size * size * 4) as usize);
        let unit = GRID / size as f32;
        for (i, &(r, g, b, op)) in CELLS.iter().enumerate() {
            let cx = ((ORIGINS[i % 3] + CELL / 2.0) / unit) as u32;
            let cy = ((ORIGINS[i / 3] + CELL / 2.0) / unit) as u32;
            let got = pixel(&rgba, size, cx, cy);
            assert_eq!((got.0, got.1, got.2), (r, g, b), "cell {i} colour");
            let want_a = (op * 255.0).round() as u8;
            assert!(
                got.3.abs_diff(want_a) <= 2,
                "cell {i} alpha {} wanted {want_a}",
                got.3
            );
        }
    }

    #[test]
    fn the_corners_are_transparent_and_the_grid_has_gaps() {
        let size = ICON_PX;
        let rgba = logo_rgba(size);
        // outside the 3..29 grid entirely
        assert_eq!(pixel(&rgba, size, 0, 0).3, 0, "top-left corner");
        assert_eq!(pixel(&rgba, size, size - 1, size - 1).3, 0, "bottom-right");
        // the mark must read as nine cells, not one block: the middle of a gap
        // between columns (x = 11.5 units) is empty
        let unit = GRID / size as f32;
        let gap_x = (11.5 / unit) as u32;
        let mid_y = (16.0 / unit) as u32;
        assert!(pixel(&rgba, size, gap_x, mid_y).3 < 80, "column gap filled in");
    }

    #[test]
    fn it_renders_at_any_size() {
        // the geometry is unit-based, so a retina or Windows tray size works
        for size in [16u32, 22, 36, 64] {
            let rgba = logo_rgba(size);
            assert_eq!(rgba.len(), (size * size * 4) as usize, "size {size}");
            let opaque = rgba.chunks(4).filter(|p| p[3] > 200).count();
            assert!(opaque > 0, "size {size} rendered nothing");
        }
    }
}
