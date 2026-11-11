//! Tiny JSON persistence with atomic, synchronous writes (tmp file + rename).
//!
//! `state` is the live value; mutate it under the lock and call `save()`. Writes are synchronous on purpose:
//! the tracker must be able to flush state during an OS shutdown.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::logger::Logger;
use crate::model::{Settings, State};

pub struct Store<T> {
    file: PathBuf,
    pub state: T,
    log: Logger,
}

pub type Shared<T> = Arc<Mutex<Store<T>>>;
pub type SharedState = Shared<State>;
pub type SharedSettings = Shared<Settings>;

impl<T: Serialize + DeserializeOwned + Default + Clone> Store<T> {
    /// A store that has not read anything yet (state = default).
    pub fn new(file: impl Into<PathBuf>, log: Logger) -> Self {
        Store { file: file.into(), state: T::default(), log }
    }

    /// Store with a preset in-memory state that never touches the disk until `save()` is called (tests).
    pub fn with_state(file: impl Into<PathBuf>, state: T, log: Logger) -> Self {
        Store { file: file.into(), state, log }
    }

    pub fn into_shared(self) -> Shared<T> {
        Arc::new(Mutex::new(self))
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    fn name(&self) -> String {
        self.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Loads the file. A missing file keeps the defaults; a corrupt one is moved to `<file>.corrupt`.
    pub fn load(&mut self) {
        let raw = match fs::read_to_string(&self.file) {
            Ok(r) => Some(r),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    self.log.warn(format!("store: cannot read {}: {e}", self.name()));
                }
                None
            }
        };
        if let Some(raw) = raw {
            match serde_json::from_str::<T>(&raw) {
                Ok(v) => self.state = v,
                Err(e) => {
                    self.log.error(format!("store: {} is corrupt ({e}), keeping a copy", self.name()));
                    let mut corrupt = self.file.clone().into_os_string();
                    corrupt.push(".corrupt");
                    let _ = fs::rename(&self.file, PathBuf::from(corrupt));
                    self.state = T::default();
                }
            }
        }
    }

    /// Atomic write (tmp + rename). Never panics; failures are logged.
    pub fn save(&self) {
        let data = match serde_json::to_string_pretty(&self.state) {
            Ok(d) => d,
            Err(e) => {
                self.log.error(format!("store: cannot serialize {}: {e}", self.name()));
                return;
            }
        };
        if let Some(dir) = self.file.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let mut tmp = self.file.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let result = fs::write(&tmp, &data).and_then(|_| {
            fs::rename(&tmp, &self.file).or_else(|_| {
                // Windows may refuse to replace a file that is briefly locked (AV, indexer).
                fs::write(&self.file, &data)
            })
        });
        if let Err(e) = result {
            self.log.error(format!("store: cannot write {}: {e}", self.name()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let mut a: Store<State> = Store::new(&file, Logger::noop());
        a.state.user_wants_running = true;
        a.save();
        assert!(!dir.path().join("state.json.tmp").exists());
        let mut b: Store<State> = Store::new(&file, Logger::noop());
        b.load();
        assert!(b.state.user_wants_running);
    }

    #[test]
    fn corrupt_file_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(&file, "{ not json").unwrap();
        let mut s: Store<Settings> = Store::new(&file, Logger::noop());
        s.load();
        assert_eq!(s.state, Settings::default());
        assert!(dir.path().join("settings.json.corrupt").exists());
    }

    #[test]
    fn missing_file_keeps_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mut s: Store<State> = Store::new(dir.path().join("nope.json"), Logger::noop());
        s.load();
        assert_eq!(s.state, State::default());
    }
}
