//! Windows: idle time via `GetLastInputInfo`; lock/unlock via `WTSRegisterSessionNotification`;
//! suspend/resume via `WM_POWERBROADCAST`; shutdown/logoff via `WM_QUERYENDSESSION` / `WM_ENDSESSION`.
//! A hidden top-level window (message-only windows do not receive the session-end broadcasts) runs the message loop
//! on its own thread.

use std::sync::{Arc, OnceLock};

use hours_tickr_core::power::PowerEvents;
use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSRegisterSessionNotification, WTSSessionInfoEx, NOTIFY_FOR_THIS_SESSION,
    WTSINFOEXW, WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION,
};

// wParam values of WM_WTSSESSION_CHANGE (not exported by the `windows` crate).
const WTS_SESSION_LOCK: u32 = 0x7;
const WTS_SESSION_UNLOCK: u32 = 0x8;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG, PBT_APMRESUMEAUTOMATIC,
    PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, WINDOW_EX_STYLE, WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION,
    WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPED,
};

static EVENTS: OnceLock<Arc<PowerEvents>> = OnceLock::new();

pub fn idle_seconds() -> Option<f64> {
    unsafe {
        let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        if !GetLastInputInfo(&mut info).as_bool() {
            return None;
        }
        let elapsed_ms = GetTickCount().wrapping_sub(info.dwTime);
        Some(f64::from(elapsed_ms) / 1000.0)
    }
}

/// `WTSSessionInfoEx` reports the lock state of the current session.
pub fn is_session_locked() -> bool {
    unsafe {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        if WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), WTS_CURRENT_SESSION, WTSSessionInfoEx, &mut buffer, &mut bytes).is_err() {
            return false;
        }
        let mut locked = false;
        if !buffer.is_null() && bytes as usize >= std::mem::size_of::<WTSINFOEXW>() {
            let info = &*(buffer.0 as *const WTSINFOEXW);
            if info.Level == 1 {
                // WTS_SESSIONSTATE_LOCK = 0, WTS_SESSIONSTATE_UNLOCK = 1 (documented behaviour since Windows 8).
                locked = info.Data.WTSInfoExLevel1.SessionFlags == 0;
            }
        }
        WTSFreeMemory(buffer.0 as *mut _);
        locked
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(events) = EVENTS.get() {
        match msg {
            WM_POWERBROADCAST => {
                match wparam.0 as u32 {
                    PBT_APMSUSPEND => events.suspend(),
                    PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => events.resume(),
                    _ => {}
                }
                return LRESULT(1); // TRUE: request granted
            }
            WM_WTSSESSION_CHANGE => {
                match wparam.0 as u32 {
                    WTS_SESSION_LOCK => events.lock_screen(),
                    WTS_SESSION_UNLOCK => events.unlock_screen(),
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_QUERYENDSESSION => {
                events.query_session_end();
                return LRESULT(1); // never block shutdown
            }
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    events.session_end();
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub fn start_power_listener(events: Arc<PowerEvents>) {
    if EVENTS.set(events).is_err() {
        return; // already started
    }
    std::thread::Builder::new()
        .name("hours-tickr-power".into())
        .spawn(|| unsafe {
            let Ok(hinstance) = GetModuleHandleW(None) else { return };
            let class_name = w!("HoursTickrPowerListener");
            let class = WNDCLASSW { lpfnWndProc: Some(wnd_proc), hInstance: hinstance.into(), lpszClassName: class_name, ..Default::default() };
            if RegisterClassW(&class) == 0 {
                return;
            }
            let Ok(hwnd) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("Hours.Tickr power events"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinstance.into()),
                None,
            ) else {
                return;
            };
            let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .ok();
}
