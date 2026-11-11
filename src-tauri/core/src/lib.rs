//! TimeWatch core: pure application logic (no Tauri, no OS APIs).
//!
//! See docs/ARCHITECTURE.md for the module overview and the rules for this crate.

pub mod clock;
pub mod error;
pub mod http;
pub mod logger;
pub mod model;
pub mod store;

pub mod api;
pub mod auth;
pub mod i18n;
pub mod idle_watcher;
pub mod power;
pub mod secrets;
pub mod sync_queue;
pub mod tracker;
