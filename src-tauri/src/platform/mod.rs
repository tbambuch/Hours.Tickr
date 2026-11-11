//! OS integration that Tauri does not provide: idle time, screen lock state and power/session events.
//! Everything here only *reports*; the decisions live in
//! `timewatch_core::power` and `timewatch_core::idle_watcher`.

use std::sync::Arc;

use timewatch_core::power::PowerEvents;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows_os;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(windows)]
use windows_os as imp;

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::*;
    pub fn idle_seconds() -> Option<f64> {
        None
    }
    pub fn is_session_locked() -> bool {
        false
    }
    pub fn start_power_listener(_events: Arc<PowerEvents>) {}
}

/// Seconds since the last keyboard/mouse input, or `None` when it cannot be determined.
pub fn idle_seconds() -> Option<f64> {
    imp::idle_seconds()
}

/// True while the screen is locked (lock screen / login window).
pub fn is_session_locked() -> bool {
    imp::is_session_locked()
}

/// Starts a background listener that forwards lock/unlock/suspend/resume/session-end to `events`.
pub fn start_power_listener(events: Arc<PowerEvents>) {
    imp::start_power_listener(events)
}
