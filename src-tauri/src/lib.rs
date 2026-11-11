//! TimeWatch: tray time tracker that syncs to My Hours (Tauri 2).

mod app;
mod commands;
mod platform;
mod ui;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::{Manager, RunEvent};
use tauri_plugin_autostart::MacosLauncher;

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| ui::show_timer(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .invoke_handler(tauri::generate_handler![
            commands::i18n_get,
            commands::timer_get_state,
            commands::timer_toggle,
            commands::timer_set_pinned,
            commands::timer_open_settings,
            commands::timer_hide,
            commands::settings_get,
            commands::settings_set_ui,
            commands::settings_verify_key,
            commands::settings_clear_key,
            commands::settings_login,
            commands::settings_logout,
            commands::settings_projects,
            commands::settings_tasks,
            commands::settings_save,
            commands::settings_resize,
            commands::settings_close,
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory); // menu-bar app, no Dock icon

            let core = app::Core::init(app.handle())?;
            app.manage(core.clone());
            core.apply_theme();
            core.start();
            ui::create_timer_window(&core)?;
            // The tray must outlive setup: Tauri keeps it alive by id.
            let _ = ui::create_tray(&core)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building TimeWatch");

    app.run(|handle, event| match event {
        RunEvent::ExitRequested { api, code, .. } => {
            let Some(core) = handle.try_state::<Arc<app::Core>>() else { return };
            if core.final_quit.load(Ordering::SeqCst) {
                return; // our own shutdown path: let it exit
            }
            api.prevent_exit(); // tray app: closing the last window must not quit
            if code.is_some() {
                core.inner().clone().quit();
            }
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => ui::show_timer(handle),
        _ => {}
    });
}
