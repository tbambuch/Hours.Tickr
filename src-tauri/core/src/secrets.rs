//! Credential storage in the operating system credential store.
//!
//! One JSON blob (`{"method":"oauth"|"apikey","refreshToken"?,"apiKey"?}`) is kept behind a [`SecretBackend`].
//! Production uses the OS credential store (Windows Credential Manager / macOS Keychain / Secret Service) through
//! the `keyring` crate. Credentials saved by earlier versions (encrypted files) are intentionally NOT migrated:
//! the user signs in again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub const KEYRING_SERVICE: &str = "TimeWatch";
pub const KEYRING_ACCOUNT: &str = "auth";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    /// "oauth" | "apikey"
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Where the single credentials blob lives. Errors are plain strings and must never contain the secret.
pub trait SecretBackend: Send + Sync {
    /// Whether secrets can be stored at all (false e.g. on a Linux box without a keyring daemon).
    fn is_available(&self) -> bool;
    /// `Ok(None)` when nothing is stored.
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, v: &str) -> Result<(), String>;
    /// Deleting something that does not exist is not an error.
    fn delete(&self) -> Result<(), String>;
}

// ---------------------------------------------------------------------------------------------------------------
// OS keyring
// ---------------------------------------------------------------------------------------------------------------

/// OS credential store: service "TimeWatch", account "auth".
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringBackend;

impl KeyringBackend {
    pub fn new() -> Self {
        KeyringBackend
    }

    fn entry() -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
    }
}

/// Error text without any secret (keyring errors never carry the password, but keep it generic anyway).
fn describe(e: &keyring::Error) -> String {
    format!("Secure storage error: {e}")
}

impl SecretBackend for KeyringBackend {
    fn is_available(&self) -> bool {
        // Harmless probe: read the (possibly absent) entry. "No storage" style errors mean unavailable.
        match Self::entry().and_then(|e| e.get_password()) {
            Ok(_) => true,
            Err(keyring::Error::NoEntry) | Err(keyring::Error::BadEncoding(_)) | Err(keyring::Error::Ambiguous(_)) => true,
            Err(_) => false,
        }
    }

    fn get(&self) -> Result<Option<String>, String> {
        match Self::entry().and_then(|e| e.get_password()) {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(describe(&e)),
        }
    }

    fn set(&self, v: &str) -> Result<(), String> {
        Self::entry().and_then(|e| e.set_password(v)).map_err(|e| describe(&e))
    }

    fn delete(&self) -> Result<(), String> {
        match Self::entry().and_then(|e| e.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(describe(&e)),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// In-memory backend (tests)
// ---------------------------------------------------------------------------------------------------------------

/// In-memory backend for tests, with a switch to simulate unavailable secure storage.
pub struct MemoryBackend {
    value: Mutex<Option<String>>,
    available: AtomicBool,
}

impl MemoryBackend {
    pub fn new() -> Self {
        MemoryBackend { value: Mutex::new(None), available: AtomicBool::new(true) }
    }

    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::SeqCst);
    }

    /// The raw stored blob (for assertions).
    pub fn raw(&self) -> Option<String> {
        self.value.lock().clone()
    }

    fn check(&self) -> Result<(), String> {
        if self.available.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("Secure storage is not available.".to_string())
        }
    }
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretBackend for MemoryBackend {
    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }
    fn get(&self) -> Result<Option<String>, String> {
        self.check()?;
        Ok(self.value.lock().clone())
    }
    fn set(&self, v: &str) -> Result<(), String> {
        self.check()?;
        *self.value.lock() = Some(v.to_string());
        Ok(())
    }
    fn delete(&self) -> Result<(), String> {
        self.check()?;
        *self.value.lock() = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------------------------------------------

/// Only one auth method is stored at a time; saving replaces whatever was there.
pub struct Secrets {
    backend: Arc<dyn SecretBackend>,
}

impl Secrets {
    pub fn new(b: Arc<dyn SecretBackend>) -> Secrets {
        Secrets { backend: b }
    }

    pub fn is_available(&self) -> bool {
        self.backend.is_available()
    }

    /// Returns `None` when nothing is stored or the stored blob is unusable (corrupt, unknown method, backend error).
    pub fn load_auth(&self) -> Option<Credentials> {
        let raw = self.backend.get().ok().flatten()?;
        let creds: Credentials = serde_json::from_str(&raw).ok()?;
        matches!(creds.method.as_str(), "oauth" | "apikey").then_some(creds)
    }

    pub fn save_auth(&self, c: &Credentials) -> Result<(), String> {
        if !self.backend.is_available() {
            return Err("Secure storage is not available.".to_string());
        }
        let json = serde_json::to_string(c).map_err(|_| "Could not serialize credentials.".to_string())?;
        self.backend.set(&json)
    }

    /// Best effort: failures are ignored.
    pub fn clear_auth(&self) {
        let _ = self.backend.delete();
    }

    /// Masked form for the UI (`••••` + last 4 chars), never the full key. Empty for `None` / empty.
    pub fn mask(key: Option<&str>) -> String {
        match key {
            Some(k) if !k.is_empty() => {
                let chars: Vec<char> = k.chars().collect();
                let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
                format!("••••{tail}")
            }
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets() -> (Secrets, Arc<MemoryBackend>) {
        let b = Arc::new(MemoryBackend::new());
        (Secrets::new(b.clone()), b)
    }

    fn oauth(rt: &str) -> Credentials {
        Credentials { method: "oauth".into(), refresh_token: Some(rt.into()), api_key: None }
    }

    #[test]
    fn round_trip_clear_and_replace() {
        let (s, b) = secrets();
        assert!(s.is_available());
        assert_eq!(s.load_auth(), None);
        s.save_auth(&oauth("R")).unwrap();
        assert_eq!(s.load_auth(), Some(oauth("R")));
        // saving another method replaces the previous one
        let key = Credentials { method: "apikey".into(), refresh_token: None, api_key: Some("LEGACYKEY1234".into()) };
        s.save_auth(&key).unwrap();
        assert_eq!(s.load_auth(), Some(key));
        s.clear_auth();
        assert_eq!(s.load_auth(), None);
        assert_eq!(b.raw(), None);
        s.clear_auth(); // clearing nothing is fine
    }

    #[test]
    fn stored_blob_is_camel_case_json_without_nulls() {
        let (s, b) = secrets();
        s.save_auth(&oauth("R")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&b.raw().unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "oauth", "refreshToken": "R" }));
        s.save_auth(&Credentials { method: "apikey".into(), refresh_token: None, api_key: Some("K".into()) }).unwrap();
        let v: serde_json::Value = serde_json::from_str(&b.raw().unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({ "method": "apikey", "apiKey": "K" }));
    }

    #[test]
    fn loads_blob_written_by_hand() {
        let (s, b) = secrets();
        b.set(r#"{"method":"apikey","apiKey":"LEGACYKEY1234"}"#).unwrap();
        assert_eq!(s.load_auth(), Some(Credentials { method: "apikey".into(), refresh_token: None, api_key: Some("LEGACYKEY1234".into()) }));
    }

    #[test]
    fn legacy_bare_key_garbage_and_unknown_method_load_as_none() {
        let (s, b) = secrets();
        // the legacy format (bare API key) is intentionally not migrated
        b.set("LEGACYKEY1234").unwrap();
        assert_eq!(s.load_auth(), None);
        for bad in ["", "{", "null", "[]", "42", r#"{"method":"basic"}"#, r#"{"refreshToken":"R"}"#, r#"{"method":7}"#, r#"{"method":"oauth","refreshToken":5}"#] {
            b.set(bad).unwrap();
            assert_eq!(s.load_auth(), None, "{bad}");
        }
    }

    #[test]
    fn unavailable_storage() {
        let (s, b) = secrets();
        s.save_auth(&oauth("R")).unwrap();
        b.set_available(false);
        assert!(!s.is_available());
        let e = s.save_auth(&oauth("X")).unwrap_err();
        assert!(!e.contains('X') || e.contains("not available"));
        assert_eq!(s.load_auth(), None); // never panics
        s.clear_auth();
        b.set_available(true);
        assert_eq!(s.load_auth(), Some(oauth("R"))); // nothing was overwritten or deleted
    }

    #[test]
    fn mask_shows_only_last_four() {
        assert_eq!(Secrets::mask(Some("ABCDEFGH1234")), "••••1234");
        assert_eq!(Secrets::mask(Some("abc")), "••••abc");
        assert_eq!(Secrets::mask(Some("")), "");
        assert_eq!(Secrets::mask(None), "");
        assert_eq!(Secrets::mask(Some("xxxxxxžščř")), "••••žščř");
    }

    /// Touches the real OS credential store: `cargo test -- --ignored keyring`.
    #[test]
    #[ignore]
    fn keyring_backend_round_trip() {
        let s = Secrets::new(Arc::new(KeyringBackend::new()));
        if !s.is_available() {
            eprintln!("OS keyring not available, skipping");
            return;
        }
        // Do not clobber real credentials of a developer machine that has TimeWatch installed.
        let before = KeyringBackend::new().get().unwrap();
        let c = oauth("KEYRING-TEST-TOKEN");
        s.save_auth(&c).unwrap();
        assert_eq!(s.load_auth(), Some(c));
        s.clear_auth();
        assert_eq!(s.load_auth(), None);
        if let Some(prev) = before {
            KeyringBackend::new().set(&prev).unwrap();
        }
    }
}
