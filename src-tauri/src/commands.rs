//! IPC commands called by the two local windows (timer, settings).
//! Every command answers with JSON the UI already understands (`{ ok, error?, ... }`).

use std::sync::Arc;

use serde_json::{json, Value};
use tauri::{State, WebviewWindow};
use timewatch_core::i18n::LANGUAGES;
use timewatch_core::secrets::{Credentials, Secrets};

use crate::app::{id_or_null, Core, THEMES};
use crate::ui;

type Cx<'a> = State<'a, Arc<Core>>;

/// Only the timer and settings windows may call commands (the login window shows remote content).
fn trusted(core: &Core, win: &WebviewWindow) -> Result<(), Value> {
    if ui::is_local_window(win) {
        Ok(())
    } else {
        core.log.warn(format!("ipc: rejected a call from window '{}'", win.label()));
        Err(json!({ "ok": false, "error": core.tr("err.untrusted") }))
    }
}

macro_rules! guard {
    ($core:expr, $win:expr) => {
        if let Err(v) = trusted(&$core, &$win) {
            return v;
        }
    };
}

#[tauri::command]
pub fn i18n_get(core: Cx<'_>, window: WebviewWindow) -> Value {
    guard!(core, window);
    core.i18n_payload()
}

// ---------- timer window ----------

#[tauri::command]
pub fn timer_get_state(core: Cx<'_>, window: WebviewWindow) -> Value {
    guard!(core, window);
    core.build_state()
}

#[tauri::command]
pub fn timer_toggle(core: Cx<'_>, window: WebviewWindow) -> Value {
    guard!(core, window);
    core.toggle_from_ui();
    core.build_state()
}

#[tauri::command]
pub fn timer_set_pinned(core: Cx<'_>, window: WebviewWindow, pinned: bool) -> Value {
    guard!(core, window);
    core.set_pinned(pinned);
    core.build_state()
}

#[tauri::command]
pub fn timer_open_settings(core: Cx<'_>, window: WebviewWindow) -> Value {
    guard!(core, window);
    ui::open_settings(&core);
    json!(true)
}

#[tauri::command]
pub fn timer_hide(core: Cx<'_>, window: WebviewWindow) -> Value {
    guard!(core, window);
    ui::hide_timer(&core);
    json!(true)
}

// ---------- settings window ----------

#[tauri::command]
pub fn settings_get(core: Cx<'_>, window: WebviewWindow) -> Value {
    guard!(core, window);
    let s = core.settings();
    let creds = core.creds.lock().clone();
    let api_key = creds.as_ref().filter(|c| c.method == "apikey").and_then(|c| c.api_key.clone());
    json!({
        "authMethod": creds.as_ref().map(|c| c.method.clone()),
        "accountName": s.account_name,
        "accountEmail": s.account_email,
        "authError": core.queue.status().state == "auth-error",
        "hasKey": api_key.is_some(),
        "keyMask": Secrets::mask(api_key.as_deref()),
        "secureStorage": core.secrets.is_available(),
        "projectId": s.project_id,
        "projectName": s.project_name,
        "taskId": s.task_id,
        "note": s.note,
        "syncIntervalMin": s.sync_interval_min,
        "idleMinutes": s.idle_minutes,
        "openAtLogin": s.open_at_login,
        "language": s.language,
        "theme": s.theme,
    })
}

#[tauri::command]
pub fn settings_set_ui(core: Cx<'_>, window: WebviewWindow, ui: Value) -> Value {
    guard!(core, window);
    let Some(obj) = ui.as_object() else { return json!({ "ok": false, "error": core.tr("err.badData") }) };
    {
        let mut s = core.settings_store.lock();
        if let Some(l) = obj.get("language").and_then(Value::as_str).filter(|l| *l == "system" || LANGUAGES.contains(l)) {
            s.state.language = l.to_string();
        }
        if let Some(t) = obj.get("theme").and_then(Value::as_str).filter(|t| THEMES.contains(t)) {
            s.state.theme = t.to_string();
        }
        s.save();
    }
    core.apply_theme();
    core.apply_language();
    json!({ "ok": true })
}

#[tauri::command]
pub async fn settings_verify_key(core: Cx<'_>, window: WebviewWindow, candidate: String) -> Result<Value, ()> {
    if let Err(v) = trusted(&core, &window) {
        return Ok(v);
    }
    let core: Arc<Core> = core.inner().clone();
    let key = candidate.trim().to_string();
    let key = if key.is_empty() { None } else { Some(key) };
    let has_stored_key = matches!(core.creds.lock().as_ref(), Some(c) if c.method == "apikey");
    if key.is_none() && !has_stored_key {
        return Ok(json!({ "ok": false, "error": core.tr("err.enterKey") }));
    }
    if let Some(k) = &key {
        if k.len() > 512 || k.chars().any(char::is_whitespace) {
            return Ok(json!({ "ok": false, "error": core.tr("err.keyFormat") }));
        }
    }
    let projects = match core.api.get_projects(key.as_deref()).await {
        Ok(p) => p,
        Err(e) => return Ok(json!({ "ok": false, "error": core.user_message(&e) })),
    };
    if let Some(k) = key {
        if !core.secrets.is_available() {
            return Ok(json!({ "ok": false, "error": core.tr("err.noStorageKey") }));
        }
        let next = Credentials { method: "apikey".into(), refresh_token: None, api_key: Some(k) };
        if let Err(e) = core.switch_credentials(next).await {
            core.log.error(format!("settings: cannot store the API key: {e}"));
            return Ok(json!({ "ok": false, "error": core.tr("err.noStorageKey") }));
        }
        core.log.info("settings: API key stored");
        core.after_credentials_changed();
    }
    let masked = core.creds.lock().as_ref().and_then(|c| c.api_key.clone());
    Ok(json!({ "ok": true, "projects": projects, "keyMask": Secrets::mask(masked.as_deref()) }))
}

#[tauri::command]
pub async fn settings_clear_key(core: Cx<'_>, window: WebviewWindow) -> Result<Value, ()> {
    if let Err(v) = trusted(&core, &window) {
        return Ok(v);
    }
    core.sign_out().await;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
pub async fn settings_logout(core: Cx<'_>, window: WebviewWindow) -> Result<Value, ()> {
    settings_clear_key(core, window).await
}

#[tauri::command]
pub async fn settings_login(core: Cx<'_>, window: WebviewWindow) -> Result<Value, ()> {
    if let Err(v) = trusted(&core, &window) {
        return Ok(v);
    }
    let core: Arc<Core> = core.inner().clone();
    if core.login_in_progress.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(json!({ "ok": false, "error": core.tr("err.loginBusy") }));
    }
    if !core.secrets.is_available() {
        return Ok(json!({ "ok": false, "error": core.tr("err.noStorageLogin") }));
    }
    core.login_in_progress.store(true, std::sync::atomic::Ordering::SeqCst);
    let result = core.login_with_my_hours().await;
    core.login_in_progress.store(false, std::sync::atomic::Ordering::SeqCst);
    Ok(result)
}

#[tauri::command]
pub async fn settings_projects(core: Cx<'_>, window: WebviewWindow) -> Result<Value, ()> {
    if let Err(v) = trusted(&core, &window) {
        return Ok(v);
    }
    if !core.has_credentials() {
        return Ok(json!({ "ok": false, "error": core.tr("err.needLogin") }));
    }
    Ok(match core.api.get_projects(None).await {
        Ok(projects) => json!({ "ok": true, "projects": projects }),
        Err(e) => json!({ "ok": false, "error": core.user_message(&e) }),
    })
}

#[tauri::command]
pub async fn settings_tasks(core: Cx<'_>, window: WebviewWindow, project_id: Value) -> Result<Value, ()> {
    if let Err(v) = trusted(&core, &window) {
        return Ok(v);
    }
    if !core.has_credentials() {
        return Ok(json!({ "ok": false, "error": core.tr("err.needLogin") }));
    }
    let Some(id) = (match &project_id {
        Value::String(_) | Value::Number(_) => id_or_null(&project_id),
        _ => None,
    }) else {
        return Ok(json!({ "ok": false, "error": core.tr("err.badProject") }));
    };
    Ok(match core.api.get_tasks(&id).await {
        Ok(tasks) => json!({ "ok": true, "tasks": tasks }),
        Err(e) => json!({ "ok": false, "error": core.user_message(&e) }),
    })
}

#[tauri::command]
pub fn settings_save(core: Cx<'_>, window: WebviewWindow, data: Value) -> Value {
    guard!(core, window);
    let Some(obj) = data.as_object() else { return json!({ "ok": false, "error": core.tr("err.badData") }) };

    let int = |v: Option<&Value>| v.and_then(|v| v.as_f64()).filter(|n| n.fract() == 0.0).map(|n| n as i64);
    let Some(interval) = int(obj.get("syncIntervalMin")).filter(|n| (1..=60).contains(n)) else {
        return json!({ "ok": false, "error": core.tr("err.badInterval") });
    };
    let idle = match obj.get("idleMinutes") {
        None | Some(Value::Null) => i64::from(core.settings().idle_minutes),
        other => match int(other) {
            Some(n) if (0..=240).contains(&n) => n,
            _ => return json!({ "ok": false, "error": core.tr("err.badIdle") }),
        },
    };
    let text = |key: &str, max: usize| obj.get(key).and_then(Value::as_str).unwrap_or("").chars().take(max).collect::<String>();

    {
        let mut guard = core.settings_store.lock();
        let s = &mut guard.state;
        s.project_id = obj.get("projectId").and_then(id_or_null);
        s.project_name = if s.project_id.is_none() { String::new() } else { text("projectName", 200) };
        s.task_id = if s.project_id.is_none() { None } else { obj.get("taskId").and_then(id_or_null) };
        s.task_name = if s.task_id.is_none() { String::new() } else { text("taskName", 200) };
        s.note = text("note", 500);
        s.sync_interval_min = interval as u32;
        s.idle_minutes = idle as u32;
        s.open_at_login = obj.get("openAtLogin").and_then(Value::as_bool) == Some(true);
        if let Some(l) = obj.get("language").and_then(Value::as_str).filter(|l| *l == "system" || LANGUAGES.contains(l)) {
            s.language = l.to_string();
        }
        if let Some(t) = obj.get("theme").and_then(Value::as_str).filter(|t| THEMES.contains(t)) {
            s.theme = t.to_string();
        }
        guard.save();
    }
    core.apply_theme();
    core.apply_language();
    core.apply_login_item();
    core.queue.set_interval_ms(interval as u64 * 60_000);
    core.tracker.change_target();
    if !core.power.is_locked() {
        core.tracker.system_resume("configured");
    }
    core.push_state();
    json!({ "ok": true })
}

#[tauri::command]
pub fn settings_resize(core: Cx<'_>, window: WebviewWindow, height: f64) -> Value {
    guard!(core, window);
    if !(200.0..=900.0).contains(&height) || height.fract() != 0.0 {
        return json!({ "ok": false, "error": core.tr("err.badHeight") });
    }
    json!({ "ok": ui::resize_settings(&core, height) })
}

#[tauri::command]
pub fn settings_close(core: Cx<'_>, window: WebviewWindow) -> Value {
    guard!(core, window);
    ui::close_settings(&core);
    json!(true)
}
