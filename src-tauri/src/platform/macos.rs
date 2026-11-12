//! macOS: idle time via `CGEventSourceSecondsSinceLastEventType`; screen lock via the session dictionary
//! (`CGSSessionScreenIsLocked`), polled once a second. Sleep/wake is detected by the tracker's tick-gap check
//! (a gap > 2 min without a suspend event closes the segment at the last tick), logout/shutdown by the app exit handler.
//!
//! NOTE: written without access to a Mac (built and tested only by CI); verify on a real machine.

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFGetTypeID, CFRelease};
use core_foundation_sys::number::{CFBooleanGetTypeID, CFBooleanGetValue, CFBooleanRef};
use core_foundation_sys::dictionary::{CFDictionaryGetValueIfPresent, CFDictionaryRef};
use hours_tickr_core::power::PowerEvents;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
}

// kCGEventSourceStateCombinedSessionState = 0, kCGAnyInputEventType = ~0
const COMBINED_SESSION_STATE: i32 = 0;
const ANY_INPUT_EVENT_TYPE: u32 = u32::MAX;

pub fn idle_seconds() -> Option<f64> {
    let s = unsafe { CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT_TYPE) };
    if s.is_finite() && s >= 0.0 {
        Some(s)
    } else {
        None
    }
}

pub fn is_session_locked() -> bool {
    unsafe {
        let dict = CGSessionCopyCurrentDictionary();
        if dict.is_null() {
            return false;
        }
        let key = CFString::new("CGSSessionScreenIsLocked");
        let mut value: *const c_void = std::ptr::null();
        let found = CFDictionaryGetValueIfPresent(dict, key.as_concrete_TypeRef() as *const c_void, &mut value) != 0;
        let locked = found
            && !value.is_null()
            && CFGetTypeID(value) == CFBooleanGetTypeID()
            && CFBooleanGetValue(value as CFBooleanRef);
        CFRelease(dict as *const c_void);
        locked
    }
}

pub fn start_power_listener(events: Arc<PowerEvents>) {
    std::thread::Builder::new()
        .name("hours-tickr-session".into())
        .spawn(move || {
            let mut was_locked = is_session_locked();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let locked = is_session_locked();
                if locked != was_locked {
                    was_locked = locked;
                    if locked {
                        events.lock_screen();
                    } else {
                        events.unlock_screen();
                    }
                }
            }
        })
        .ok();
}
