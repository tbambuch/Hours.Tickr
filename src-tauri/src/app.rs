//! Application core: wires the `timewatch-core` pieces together (credentials, sync queue, tracker, power events).
//! Windows/tray live in `ui.rs`, IPC commands in `commands.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use timewatch_core::api::{AuthProvider, LogApi, MyHoursClient};
use timewatch_core::auth::{self, AuthDeps, AuthManager};
use timewatch_core::clock::{system_clock, Clock};
use timewatch_core::error::{classify_error, ApiError, ErrorKind};
use timewatch_core::http::{Http, ReqwestHttp};
use timewatch_core::i18n;
use timewatch_core::idle_watcher::IdleWatcher;
use timewatch_core::logger::Logger;
use timewatch_core::model::{JsonId, Settings, State, Target};
use timewatch_core::power::{PowerDeps, PowerEvents};
use timewatch_core::secrets::{Credentials, KeyringBackend, Secrets};
use timewatch_core::store::{SharedSettings, Store};
use timewatch_core::sync_queue::{SyncQueue, SyncQueueDeps};
use timewatch_core::tracker::{Status, Tracker, TrackerDeps};

use crate::{platform, ui};

pub const THEMES: [&str; 3] = ["system", "light", "dark"];

pub struct Core {
    pub app: AppHandle,
    pub log: Logger,
    pub settings_store: SharedSettings,
    pub secrets: Arc<Secrets>,
    pub creds: Arc<Mutex<Option<Credentials>>>,
    pub auth: Arc<AuthManager>,
    pub api: Arc<MyHoursClient>,
    pub queue: Arc<SyncQueue>,
    pub tracker: Arc<Tracker>,
    pub power: Arc<PowerEvents>,
    pub lang: Mutex<String>,
    pub login_in_progress: Arc<AtomicBool>,
    pub pending_refresh_token: Arc<Mutex<Option<String>>>,
    pub quitting: AtomicBool,
    pub final_quit: Arc<AtomicBool>,
    pub tray_key: Mutex<String>,
}

/// Adapts the stored credentials (OAuth or API key) to what the API client needs.
struct CredsAuth {
    creds: Arc<Mutex<Option<Credentials>>>,
    auth: Arc<AuthManager>,
}

#[async_trait]
impl AuthProvider for CredsAuth {
    async fn auth_header(&self) -> Result<Option<String>, ApiError> {
        let creds = self.creds.lock().clone();
        match creds {
            None => Ok(None),
            Some(c) if c.method == "oauth" => Ok(Some(self.auth.auth_header().await?)),
            Some(c) => Ok(c.api_key.map(|k| format!("ApiKey {k}"))),
        }
    }

    async fn on_unauthorized(&self, used_header: &str) -> Result<bool, ApiError> {
        let oauth = matches!(self.creds.lock().as_ref(), Some(c) if c.method == "oauth");
        if oauth {
            self.auth.handle_unauthorized(used_header).await
        } else {
            Ok(false)
        }
    }
}

fn has_credentials(creds: &Option<Credentials>) -> bool {
    match creds {
        None => false,
        Some(c) if c.method == "oauth" => c.refresh_token.as_deref().is_some_and(|t| !t.is_empty()),
        Some(c) => c.api_key.as_deref().is_some_and(|k| !k.is_empty()),
    }
}

fn target_from(creds: &Mutex<Option<Credentials>>, settings: &SharedSettings) -> Option<Target> {
    if !has_credentials(&creds.lock()) {
        return None;
    }
    let s = settings.lock();
    let project = s.state.project_id.clone()?;
    if project.key().is_empty() {
        return None;
    }
    Some(Target { project_id: Some(project), task_id: s.state.task_id.clone(), note: s.state.note.clone() })
}

/// `null` / `""` -> None, digits -> number, anything else -> string (same as the old `idOrNull`).
pub fn id_or_null(v: &Value) -> Option<JsonId> {
    match v {
        Value::Null => None,
        Value::Number(n) => n.as_i64().map(JsonId::Num),
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(JsonId::parse(s)),
        _ => None,
    }
}

fn system_languages() -> Vec<String> {
    sys_locale::get_locales().collect()
}

/// Earlier versions kept their files in `<appData>/TimeWatch`; copy them over on the first start of this version.
fn migrate_from_previous_version(data_dir: &Path, log: &Logger) {
    let Some(parent) = data_dir.parent() else { return };
    let old: PathBuf = parent.join("TimeWatch");
    for name in ["state.json", "settings.json"] {
        let (from, to) = (old.join(name), data_dir.join(name));
        if from.is_file() && !to.exists() {
            match std::fs::copy(&from, &to) {
                Ok(_) => log.info(format!("migration: copied {name} from the previous version")),
                Err(e) => log.warn(format!("migration: cannot copy {name}: {e}")),
            }
        }
    }
}

impl Core {
    pub fn init(app: &AppHandle) -> Result<Arc<Core>, Box<dyn std::error::Error>> {
        let data_dir = app.path().app_data_dir()?;
        std::fs::create_dir_all(&data_dir)?;
        let log = Logger::file(&data_dir.join("logs"));
        log.info(format!(
            "app: starting TimeWatch {} ({}), data={}",
            app.package_info().version,
            std::env::consts::OS,
            data_dir.display()
        ));
        migrate_from_previous_version(&data_dir, &log);

        let mut st = Store::<State>::new(data_dir.join("state.json"), log.clone());
        st.load();
        let state_store = st.into_shared();
        let mut se = Store::<Settings>::new(data_dir.join("settings.json"), log.clone());
        se.load();
        let settings_store = se.into_shared();

        let secrets = Arc::new(Secrets::new(Arc::new(KeyringBackend::new())));
        let creds = Arc::new(Mutex::new(secrets.load_auth()));
        let login_in_progress = Arc::new(AtomicBool::new(false));
        let pending_refresh_token = Arc::new(Mutex::new(None::<String>));
        let final_quit = Arc::new(AtomicBool::new(false));

        let clock: Arc<dyn Clock> = system_clock();
        let http: Arc<dyn Http> = Arc::new(ReqwestHttp::new());
        let new_id: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(uuid_v4);
        let handle = tauri::async_runtime::handle().inner().clone();

        let auth = Arc::new(AuthManager::new(AuthDeps {
            http: http.clone(),
            clock: clock.clone(),
            get_refresh_token: {
                let creds = creds.clone();
                Arc::new(move || creds.lock().as_ref().and_then(|c| c.refresh_token.clone()))
            },
            save_refresh_token: {
                let (creds, secrets, pending, busy, log) =
                    (creds.clone(), secrets.clone(), pending_refresh_token.clone(), login_in_progress.clone(), log.clone());
                Arc::new(move |token: String| {
                    // Refresh tokens rotate: always persist the newest one.
                    if busy.load(Ordering::SeqCst) {
                        *pending.lock() = Some(token); // initial login: stored once the exchange is complete
                    } else {
                        let mut guard = creds.lock();
                        if let Some(c) = guard.as_mut().filter(|c| c.method == "oauth") {
                            c.refresh_token = Some(token);
                            if let Err(e) = secrets.save_auth(c) {
                                log.error(format!("auth: cannot persist rotated refresh token: {e}"));
                            }
                        }
                    }
                })
            },
            log: log.clone(),
        }));

        let api = Arc::new(MyHoursClient::new(http, Arc::new(CredsAuth { creds: creds.clone(), auth: auth.clone() }), log.clone()));

        let weak_app = app.clone();
        let queue = SyncQueue::new(SyncQueueDeps {
            store: state_store.clone(),
            api: api.clone() as Arc<dyn LogApi>,
            clock: clock.clone(),
            log: log.clone(),
            on_status: Arc::new(move |_| push_state_of(&weak_app)),
            interval_ms: u64::from(settings_store.lock().state.sync_interval_min.max(1)) * 60_000,
            handle: handle.clone(),
            new_id: new_id.clone(),
        });

        let tracker = Arc::new(Tracker::new(TrackerDeps {
            store: state_store.clone(),
            clock: clock.clone(),
            get_target: {
                let (creds, settings) = (creds.clone(), settings_store.clone());
                Arc::new(move || target_from(&creds, &settings))
            },
            is_locked: Arc::new(platform::is_session_locked),
            on_change: {
                let a = app.clone();
                Arc::new(move || push_state_of(&a))
            },
            on_queue_change: {
                let q = queue.clone();
                Arc::new(move || q.kick())
            },
            log: log.clone(),
            new_id,
        }));

        let power = PowerEvents::new(PowerDeps {
            tracker: tracker.clone(),
            log: log.clone(),
            is_session_locked: Arc::new(platform::is_session_locked),
            on_system_shutdown: {
                // State is already persisted by tracker.shutdown(); never block the OS shutdown/logout.
                let (q, fq) = (queue.clone(), final_quit.clone());
                Arc::new(move || {
                    fq.store(true, Ordering::SeqCst);
                    q.kick();
                })
            },
            handle,
            resume_grace_ms: 5000,
            session_end_cancel_ms: 15000,
        });

        Ok(Arc::new(Core {
            app: app.clone(),
            log,
            settings_store,
            secrets,
            creds,
            auth,
            api,
            queue,
            tracker,
            power,
            lang: Mutex::new("en".into()),
            login_in_progress,
            pending_refresh_token,
            quitting: AtomicBool::new(false),
            final_quit,
            tray_key: Mutex::new(String::new()),
        }))
    }

    /// Starts everything that runs on its own: OS listeners, the tracker, the sync timer and the 1 s tick.
    pub fn start(self: &Arc<Self>) {
        {
            let mut s = self.settings_store.lock();
            if !s.state.first_run_done {
                s.state.first_run_done = true;
                s.state.open_at_login = true;
                s.save();
            }
        }
        self.apply_login_item();
        *self.lang.lock() = self.resolve_lang();

        platform::start_power_listener(self.power.clone());
        let locked = platform::is_session_locked();
        self.power.set_locked(locked);
        self.tracker.init(locked);
        self.queue.start();
        self.queue.kick();

        let idle = IdleWatcher::new(
            self.tracker.clone(),
            Arc::new(platform::idle_seconds),
            {
                let settings = self.settings_store.clone();
                Arc::new(move || settings.lock().state.idle_minutes)
            },
            system_clock(),
            self.log.clone(),
        );
        let core = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                core.tracker.tick();
                idle.check();
                core.push_state();
            }
        });
    }

    // ---------- helpers ----------

    pub fn tr(&self, key: &str) -> String {
        i18n::translate(&self.lang.lock(), key, &[])
    }
    pub fn tr_p(&self, key: &str, params: &[(&str, &str)]) -> String {
        i18n::translate(&self.lang.lock(), key, params)
    }
    pub fn lang(&self) -> String {
        self.lang.lock().clone()
    }

    pub fn settings(&self) -> Settings {
        self.settings_store.lock().state.clone()
    }

    pub fn has_credentials(&self) -> bool {
        has_credentials(&self.creds.lock())
    }

    pub fn auth_method(&self) -> Option<String> {
        self.creds.lock().as_ref().map(|c| c.method.clone())
    }

    pub fn is_configured(&self) -> bool {
        target_from(&self.creds, &self.settings_store).is_some()
    }

    pub fn resolve_lang(&self) -> String {
        i18n::resolve_language(&self.settings().language, &system_languages()).to_string()
    }

    pub fn build_state(&self) -> Value {
        let t = self.tracker.status();
        let s = self.settings();
        json!({
            "status": match t.status { Status::Running => "running", Status::Paused => "paused", Status::Stopped => "stopped" },
            "pauseReason": t.pause_reason,
            "todayMs": t.today_ms,
            "configured": self.is_configured(),
            "projectName": s.project_name,
            "pinned": s.always_on_top,
            "authMethod": self.auth_method(),
            "sync": self.queue.status(),
        })
    }

    pub fn i18n_payload(&self) -> Value {
        let lang = self.lang();
        json!({ "lang": lang, "dict": i18n::dictionary(&lang) })
    }

    pub fn push_state(&self) {
        let _ = self.app.emit("state", self.build_state());
        ui::refresh_tray(self, false);
    }

    /// Maps an API/auth failure to a message in the active language.
    pub fn user_message(&self, err: &ApiError) -> String {
        if classify_error(err) == ErrorKind::Auth {
            return self.tr(if self.auth_method().as_deref() == Some("oauth") { "err.loginExpired" } else { "err.invalidKey" });
        }
        if err.network {
            return self.tr("err.network");
        }
        match err.status {
            Some(status) => self.tr_p("err.apiStatus", &[("status", &status.to_string())]),
            None => self.tr("err.api"),
        }
    }

    pub fn apply_login_item(&self) {
        use tauri_plugin_autostart::ManagerExt;
        let enabled = self.settings().open_at_login;
        if cfg!(debug_assertions) {
            self.log.info(format!("autostart: skipped in development (wanted: {enabled})"));
            return;
        }
        let manager = self.app.autolaunch();
        let result = if enabled { manager.enable() } else { manager.disable() };
        if let Err(e) = result {
            self.log.error(format!("autostart: cannot set login item: {e}"));
        }
    }

    pub fn apply_theme(&self) {
        ui::apply_theme(self);
    }

    /// Recomputes the language; on change retitles windows, rebuilds the tray and tells the windows.
    pub fn apply_language(&self) {
        let next = self.resolve_lang();
        let changed = {
            let mut lang = self.lang.lock();
            let changed = *lang != next;
            *lang = next.clone();
            changed
        };
        ui::retitle_windows(self);
        ui::refresh_tray(self, true);
        if changed {
            let _ = self.app.emit("i18n", self.i18n_payload());
            self.log.info(format!("language: {next}"));
        }
    }

    pub fn toggle_from_ui(self: &Arc<Self>) -> Value {
        if !self.is_configured() {
            ui::open_settings(self);
            return json!({ "ok": false, "reason": "not-configured" });
        }
        match self.tracker.toggle() {
            Ok(()) => json!({ "ok": true }),
            Err(_) => json!({ "ok": false, "reason": "not-configured" }),
        }
    }

    pub fn set_pinned(&self, pinned: bool) {
        {
            let mut s = self.settings_store.lock();
            s.state.always_on_top = pinned;
            s.save();
        }
        ui::apply_pinned(self);
        self.push_state();
        ui::refresh_tray(self, true);
    }

    pub fn set_open_at_login(&self, enabled: bool) {
        {
            let mut s = self.settings_store.lock();
            s.state.open_at_login = enabled;
            s.save();
        }
        self.apply_login_item();
    }

    // ---------- credentials ----------

    fn clear_account_info(&self) {
        let mut s = self.settings_store.lock();
        s.state.account_name.clear();
        s.state.account_email.clear();
        s.save();
    }

    /// Replaces stored credentials (only one method is active); revokes a previous OAuth session.
    pub async fn switch_credentials(&self, next: Credentials) -> Result<(), String> {
        if self.auth_method().as_deref() == Some("oauth") && next.method != "oauth" {
            self.auth.revoke().await;
        }
        self.secrets.save_auth(&next)?;
        let is_oauth = next.method == "oauth";
        *self.creds.lock() = Some(next);
        self.auth.reset();
        if !is_oauth {
            self.clear_account_info();
        }
        Ok(())
    }

    pub async fn sign_out(&self) {
        if self.auth_method().as_deref() == Some("oauth") {
            self.auth.revoke().await;
        }
        self.auth.reset();
        self.secrets.clear_auth();
        *self.creds.lock() = None;
        self.clear_account_info();
        ui::clear_login_data(&self.app).await;
        if self.tracker.status().status != Status::Stopped {
            self.tracker.stop();
        }
        self.push_state();
    }

    /// After new credentials were stored: unblock the queue and start measuring if the user wanted that.
    pub fn after_credentials_changed(self: &Arc<Self>) {
        let q = self.queue.clone();
        tauri::async_runtime::spawn(async move { q.reset_auth().await });
        if !self.power.is_locked() {
            self.tracker.system_resume("configured");
        }
        self.push_state();
    }

    pub async fn login_with_my_hours(self: &Arc<Self>) -> Value {
        let pkce = auth::create_pkce();
        let state = auth::random_token();
        let url = auth::build_authorize_url(&pkce.challenge, &state, &auth::random_token());
        self.log.info("login: opening the My Hours login window");
        let outcome = ui::open_login_window(self, &url).await;
        let callback = match outcome {
            ui::LoginOutcome::Cancelled => {
                self.log.info("login: cancelled by user");
                return json!({ "ok": false, "cancelled": true });
            }
            ui::LoginOutcome::Callback(url) => url,
        };
        let code = match auth::parse_callback(&callback, &state) {
            Ok(code) => code,
            Err(e) => {
                self.log.warn(format!("login: callback error: {}", e.key()));
                let detail = if let auth::CallbackError::Failed(d) = &e { d.clone() } else { String::new() };
                return json!({ "ok": false, "error": self.tr_p(&format!("login.err.{}", e.key()), &[("detail", &detail)]) });
            }
        };
        *self.pending_refresh_token.lock() = None;
        let id_token = match self.auth.exchange_code(&code, &pkce.verifier).await {
            Ok(t) => t,
            Err(e) => {
                self.log.warn(format!("login: code exchange failed ({:?})", e.status));
                let msg = if e.network { self.user_message(&e) } else { self.tr("err.loginFailed") };
                return json!({ "ok": false, "error": msg });
            }
        };
        let Some(refresh_token) = self.pending_refresh_token.lock().take() else {
            return json!({ "ok": false, "error": self.tr("err.noRefresh") });
        };
        let new_creds = Credentials { method: "oauth".into(), refresh_token: Some(refresh_token), api_key: None };
        if let Err(e) = self.secrets.save_auth(&new_creds) {
            self.log.error(format!("login: cannot store the sign-in: {e}"));
            return json!({ "ok": false, "error": self.tr("err.noStorageLogin") });
        }
        *self.creds.lock() = Some(new_creds);
        let who = id_token.map(|t| auth::parse_id_token(&t)).unwrap_or(auth::IdentityInfo { name: String::new(), email: String::new() });
        {
            let mut s = self.settings_store.lock();
            s.state.account_name = who.name.clone();
            s.state.account_email = who.email.clone();
            s.save();
        }
        self.log.info("login: signed in");
        self.after_credentials_changed();
        json!({ "ok": true, "accountName": who.name, "accountEmail": who.email })
    }

    // ---------- quitting ----------

    /// "Quit": closes the open segment (the user's intent is kept), waits briefly for the sync, then exits.
    pub fn quit(self: &Arc<Self>) {
        if self.quitting.swap(true, Ordering::SeqCst) {
            return;
        }
        self.log.info("app: quitting");
        self.tracker.quit();
        let core = self.clone();
        tauri::async_runtime::spawn(async move {
            core.queue.flush(3000).await;
            core.final_quit.store(true, Ordering::SeqCst);
            core.app.exit(0);
        });
    }
}

fn uuid_v4() -> String {
    timewatch_core::tracker::new_uuid()
}

/// `push_state` from a place that only holds an `AppHandle` (callbacks created before `Core` exists).
pub fn push_state_of(app: &AppHandle) {
    if let Some(core) = app.try_state::<Arc<Core>>() {
        core.push_state();
    }
}
