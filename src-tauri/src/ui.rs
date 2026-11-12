//! Windows (timer, settings, My Hours login) and the tray icon.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, LogicalSize, Manager, PhysicalPosition, Theme, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use hours_tickr_core::auth::is_callback_url;
use hours_tickr_core::clock::format_hm;
use hours_tickr_core::model::WindowPos;
use hours_tickr_core::tracker::Status;

use crate::app::{Core, THEMES};

const WIN_W: f64 = 180.0;
const WIN_H: f64 = 190.0;
const MARGIN: f64 = 12.0;
const TRAY_ID: &str = "main";
const TIMER: &str = "timer";
const SETTINGS: &str = "settings";
const LOGIN: &str = "login";

const IS_MAC: bool = cfg!(target_os = "macos");

// ---------- timer window ----------

fn theme_of(core: &Core) -> Option<Theme> {
    match core.settings().theme.as_str() {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None,
    }
}

/// Work area (physical pixels) of every monitor.
fn work_areas(app: &AppHandle) -> Vec<(i32, i32, i32, i32)> {
    app.available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let wa = m.work_area();
            (wa.position.x, wa.position.y, wa.size.width as i32, wa.size.height as i32)
        })
        .collect()
}

fn initial_position(core: &Core, scale: f64) -> PhysicalPosition<i32> {
    let (w, h, margin) = ((WIN_W * scale) as i32, (WIN_H * scale) as i32, (MARGIN * scale) as i32);
    if let Some(saved) = core.settings().window_pos {
        let inside = work_areas(&core.app).iter().any(|&(x, y, ww, hh)| saved.x >= x && saved.y >= y && saved.x + w <= x + ww && saved.y + h <= y + hh);
        if inside {
            return PhysicalPosition::new(saved.x, saved.y);
        }
    }
    match core.app.primary_monitor().ok().flatten() {
        Some(m) => {
            let wa = m.work_area();
            PhysicalPosition::new(wa.position.x + wa.size.width as i32 - w - margin, wa.position.y + wa.size.height as i32 - h - margin)
        }
        None => PhysicalPosition::new(100, 100),
    }
}

pub fn create_timer_window(core: &Arc<Core>) -> tauri::Result<()> {
    let app = &core.app;
    let scale = app.primary_monitor().ok().flatten().map_or(1.0, |m| m.scale_factor());
    let win = WebviewWindowBuilder::new(app, TIMER, WebviewUrl::App("timer.html".into()))
        .title(core.tr("win.timer.title"))
        .inner_size(WIN_W, WIN_H)
        .decorations(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .skip_taskbar(true)
        .shadow(true)
        .visible(false)
        .always_on_top(core.settings().always_on_top)
        .theme(theme_of(core))
        .build()?;
    let pos = initial_position(core, scale);
    core.log.info(format!(
        "ui: timer window scale={scale} workAreas={:?} saved={:?} -> position ({}, {})",
        work_areas(app),
        core.settings().window_pos,
        pos.x,
        pos.y
    ));
    let _ = win.set_position(pos);

    let move_gen = Arc::new(AtomicU64::new(0));
    let handler_core = core.clone();
    let handler_win = win.clone();
    win.on_window_event(move |event| match event {
        WindowEvent::CloseRequested { api, .. } => {
            // Closing only hides: the app keeps running in the tray.
            api.prevent_close();
            let _ = handler_win.hide();
            refresh_tray(&handler_core, true);
        }
        WindowEvent::Moved(pos) => {
            let generation = move_gen.fetch_add(1, Ordering::SeqCst) + 1;
            let (core, move_gen, pos) = (handler_core.clone(), move_gen.clone(), *pos);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if move_gen.load(Ordering::SeqCst) != generation {
                    return; // moved again meanwhile
                }
                let mut s = core.settings_store.lock();
                s.state.window_pos = Some(WindowPos { x: pos.x, y: pos.y });
                s.save();
            });
        }
        _ => {}
    });
    let _ = win.show();
    Ok(())
}

pub fn show_timer(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(TIMER) {
        if win.is_minimized().unwrap_or(false) {
            let _ = win.unminimize();
        }
        let _ = win.show();
        let _ = win.set_focus();
    }
    if let Some(core) = app.try_state::<Arc<Core>>() {
        refresh_tray(&core, true);
    }
}

pub fn hide_timer(core: &Core) {
    if let Some(win) = core.app.get_webview_window(TIMER) {
        let _ = win.hide();
    }
    refresh_tray(core, true);
}

fn toggle_timer(app: &AppHandle) {
    let visible = app.get_webview_window(TIMER).and_then(|w| w.is_visible().ok()).unwrap_or(false);
    if visible {
        if let Some(core) = app.try_state::<Arc<Core>>() {
            hide_timer(&core);
        }
    } else {
        show_timer(app);
    }
}

pub fn apply_pinned(core: &Core) {
    let pinned = core.settings().always_on_top;
    for label in [TIMER, SETTINGS] {
        if let Some(win) = core.app.get_webview_window(label) {
            let _ = win.set_always_on_top(pinned);
        }
    }
}

pub fn apply_theme(core: &Core) {
    let theme = if THEMES.contains(&core.settings().theme.as_str()) { theme_of(core) } else { None };
    for label in [TIMER, SETTINGS, LOGIN] {
        if let Some(win) = core.app.get_webview_window(label) {
            let _ = win.set_theme(theme);
        }
    }
}

pub fn retitle_windows(core: &Core) {
    if let Some(win) = core.app.get_webview_window(TIMER) {
        let _ = win.set_title(&core.tr("win.timer.title"));
    }
    if let Some(win) = core.app.get_webview_window(SETTINGS) {
        let _ = win.set_title(&core.tr("win.settings.title"));
    }
    if let Some(win) = core.app.get_webview_window(LOGIN) {
        let _ = win.set_title(&core.tr("win.login.title"));
    }
}

// ---------- settings window ----------

/// Creating a webview from the main thread (sync commands, tray/menu handlers) deadlocks on Windows,
/// so the work always happens on the async runtime.
pub fn open_settings(core: &Arc<Core>) {
    let core = core.clone();
    tauri::async_runtime::spawn(async move { open_settings_now(&core) });
}

fn open_settings_now(core: &Arc<Core>) {
    if let Some(win) = core.app.get_webview_window(SETTINGS) {
        core.log.info(format!("ui: settings window exists, show -> {:?}", win.show()));
        let _ = win.set_focus();
        return;
    }
    core.log.info("ui: creating settings window");
    let built = WebviewWindowBuilder::new(&core.app, SETTINGS, WebviewUrl::App("settings.html".into()))
        .title(core.tr("win.settings.title"))
        .inner_size(420.0, 520.0)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .visible(false)
        .always_on_top(core.settings().always_on_top)
        .theme(theme_of(core))
        .build();
    let win = match built {
        Ok(w) => w,
        Err(e) => {
            core.log.error(format!("ui: cannot open settings window: {e}"));
            return;
        }
    };
    // Shown after the first settings_resize (avoids a visible jump); fallback after 1 s.
    let fallback = win.clone();
    let log = core.log.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let visible = fallback.is_visible();
        log.info(format!("ui: settings fallback check, visible = {visible:?}"));
        if !visible.unwrap_or(false) {
            log.info(format!("ui: settings fallback show -> {:?}", fallback.show()));
        }
    });
}

pub fn resize_settings(core: &Core, height: f64) -> bool {
    let Some(win) = core.app.get_webview_window(SETTINGS) else { return false };
    let max = win
        .current_monitor()
        .ok()
        .flatten()
        .map(|m| f64::from(m.work_area().size.height) / m.scale_factor() - 40.0)
        .unwrap_or(900.0);
    let h = height.min(max.max(200.0));
    core.log.info(format!("ui: settings resize to {h}"));
    let _ = win.set_size(LogicalSize::new(420.0, h));
    if !win.is_visible().unwrap_or(true) {
        let _ = win.show();
        let _ = win.set_focus();
    }
    true
}

pub fn close_settings(core: &Core) {
    if let Some(win) = core.app.get_webview_window(SETTINGS) {
        let _ = win.close();
    }
}

// ---------- My Hours login window ----------

pub enum LoginOutcome {
    /// The redirect to https://app.myhours.com was intercepted; carries the full callback URL.
    Callback(String),
    Cancelled,
}

fn login_data_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("login-profile"))
}

/// Shows the My Hours login page and intercepts the OAuth redirect (never reads or scripts the page).
/// The window keeps its own persistent profile, so the next sign-in is usually one click.
pub async fn open_login_window(core: &Arc<Core>, authorize_url: &str) -> LoginOutcome {
    let (tx, rx) = tokio::sync::oneshot::channel::<LoginOutcome>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let send = {
        let tx = tx.clone();
        move |outcome: LoginOutcome| {
            if let Some(sender) = tx.lock().take() {
                let _ = sender.send(outcome);
            }
        }
    };

    let Ok(url) = url::Url::parse(authorize_url) else { return LoginOutcome::Cancelled };
    let nav_send = send.clone();
    let mut builder = WebviewWindowBuilder::new(&core.app, LOGIN, WebviewUrl::External(url))
        .title(core.tr("win.login.title"))
        .inner_size(480.0, 680.0)
        .theme(theme_of(core))
        .on_navigation(move |target| {
            if is_callback_url(target.as_str()) {
                nav_send(LoginOutcome::Callback(target.to_string()));
                return false; // app.myhours.com is never actually loaded
            }
            target.scheme() == "https" || target.as_str() == "about:blank"
        });
    if let Some(dir) = login_data_dir(&core.app) {
        builder = builder.data_directory(dir);
    }
    if let Some(parent) = core.app.get_webview_window(SETTINGS) {
        if let Ok(b) = builder.parent(&parent) {
            builder = b;
        } else {
            return LoginOutcome::Cancelled;
        }
    }
    let win = match builder.build() {
        Ok(w) => w,
        Err(e) => {
            core.log.error(format!("login: cannot open the login window: {e}"));
            return LoginOutcome::Cancelled;
        }
    };
    let close_send = send.clone();
    win.on_window_event(move |event| {
        if matches!(event, WindowEvent::Destroyed) {
            close_send(LoginOutcome::Cancelled);
        }
    });

    let outcome = rx.await.unwrap_or(LoginOutcome::Cancelled);
    let _ = win.destroy();
    outcome
}

/// Removes cookies/storage of the login profile (on sign-out).
pub async fn clear_login_data(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(LOGIN) {
        let _ = win.clear_all_browsing_data();
        let _ = win.destroy();
    }
    // The profile lives in its own folder: once no window uses it, delete it.
    if let Some(dir) = login_data_dir(app) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = std::fs::remove_dir_all(dir);
    }
}

// ---------- tray ----------

fn tray_icon(recording: bool) -> Option<Image<'static>> {
    let bytes: &[u8] = match (IS_MAC, recording) {
        (true, true) => include_bytes!("../icons/tray/trayRecordingTemplate@2x.png"),
        (true, false) => include_bytes!("../icons/tray/trayTemplate@2x.png"),
        (false, true) => include_bytes!("../icons/tray/tray-recording@2x.png"),
        (false, false) => include_bytes!("../icons/tray/tray-idle@2x.png"),
    };
    Image::from_bytes(bytes).ok()
}

fn build_tray_menu(core: &Core) -> tauri::Result<Menu<tauri::Wry>> {
    let app = &core.app;
    let status = core.tracker.status().status;
    let s = core.settings();
    let toggle = MenuItem::with_id(app, "toggle", if status == Status::Stopped { core.tr("tray.start") } else { core.tr("tray.stop") }, true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", core.tr("tray.show"), true, None::<&str>)?;
    let pinned = CheckMenuItem::with_id(app, "pinned", core.tr("tray.pinned"), true, s.always_on_top, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", core.tr("tray.settings"), true, None::<&str>)?;
    let sync = MenuItem::with_id(app, "sync", core.tr("tray.syncNow"), true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "autostart", core.tr("tray.autostart"), true, s.open_at_login, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", core.tr("tray.quit"), true, None::<&str>)?;
    Menu::with_items(app, &[&toggle, &show, &pinned, &settings, &sync, &autostart, &PredefinedMenuItem::separator(app)?, &quit])
}

pub fn create_tray(core: &Arc<Core>) -> tauri::Result<TrayIcon> {
    let menu = build_tray_menu(core)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(IS_MAC)
        .icon_as_template(IS_MAC)
        .on_menu_event(|app, event| {
            let Some(core) = app.try_state::<Arc<Core>>() else { return };
            let core: Arc<Core> = core.inner().clone();
            match event.id().as_ref() {
                "toggle" => {
                    core.toggle_from_ui();
                }
                "show" => show_timer(app),
                "pinned" => {
                    let now = !core.settings().always_on_top;
                    core.set_pinned(now);
                }
                "settings" => open_settings(&core),
                "sync" => {
                    let q = core.queue.clone();
                    tauri::async_runtime::spawn(async move { q.sync_now().await });
                }
                "autostart" => {
                    let now = !core.settings().open_at_login;
                    core.set_open_at_login(now);
                    refresh_tray(&core, true);
                }
                "quit" => core.quit(),
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                if !IS_MAC {
                    toggle_timer(tray.app_handle());
                }
            }
        });
    if let Some(icon) = tray_icon(false) {
        builder = builder.icon(icon);
    }
    let tray = builder.build(&core.app)?;
    refresh_tray(core, true);
    Ok(tray)
}

/// Rebuilds menu, tooltip and icon when something they show changed (`force` skips the change check).
pub fn refresh_tray(core: &Core, force: bool) {
    let Some(tray) = core.app.tray_by_id(TRAY_ID) else { return };
    let t = core.tracker.status();
    let s = core.settings();
    let lang = core.lang();
    let status_key = match t.status {
        Status::Running => "running",
        Status::Paused => "paused",
        Status::Stopped => "stopped",
    };
    let key = format!("{status_key}|{}|{}|{}|{lang}", format_hm(t.today_ms), s.always_on_top, s.open_at_login);
    let icon_changed = {
        let mut last = core.tray_key.lock();
        if !force && *last == key {
            return;
        }
        let changed = last.is_empty() || last.split('|').next() != Some(status_key);
        *last = key;
        changed
    };
    if icon_changed {
        if let Some(icon) = tray_icon(t.status == Status::Running) {
            let _ = tray.set_icon(Some(icon));
            let _ = tray.set_icon_as_template(IS_MAC);
        }
    }
    let label = core.tr(&format!("tray.state.{status_key}"));
    let _ = tray.set_tooltip(Some(core.tr_p("tray.tooltip", &[("time", &format_hm(t.today_ms)), ("state", &label)])));
    if let Ok(menu) = build_tray_menu(core) {
        let _ = tray.set_menu(Some(menu));
    }
}

/// Used by `WebviewWindow` handles that only need to know whether a window is the timer.
pub fn is_local_window(win: &WebviewWindow) -> bool {
    matches!(win.label(), TIMER | SETTINGS)
}
