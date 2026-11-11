//! Rotating file logger with credential redaction.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use regex::Regex;

const MAX_BYTES: u64 = 1024 * 1024;

/// Removes anything that looks like credentials from a log line.
pub fn sanitize(text: &str) -> String {
    use std::sync::OnceLock;
    static RES: OnceLock<[(Regex, &'static str); 4]> = OnceLock::new();
    let res = RES.get_or_init(|| {
        [
            (Regex::new(r#"(?i)ApiKey\s+[^\s"',;]+"#).unwrap(), "ApiKey [redacted]"),
            (Regex::new(r#"(?i)Bearer\s+[^\s"',;]+"#).unwrap(), "Bearer [redacted]"),
            (Regex::new(r#"(?i)\b(access_token|refresh_token|id_token|code_verifier|code)=[^&\s"']+"#).unwrap(), "$1=[redacted]"),
            (Regex::new(r#"(?i)(authorization["']?\s*[:=]\s*)[^\r\n]+"#).unwrap(), "$1[redacted]"),
        ]
    });
    let mut out = text.to_string();
    for (re, rep) in res.iter() {
        out = re.replace_all(&out, *rep).into_owned();
    }
    out
}

enum Sink {
    None,
    Memory(Arc<Mutex<Vec<String>>>),
    File { path: PathBuf, size: Mutex<u64>, max: u64 },
}

/// Cheap to clone; all clones write to the same sink.
#[derive(Clone)]
pub struct Logger(Arc<Sink>);

impl Logger {
    /// Logger that discards everything.
    pub fn noop() -> Logger {
        Logger(Arc::new(Sink::None))
    }

    /// Logger that keeps (sanitized) lines in memory; returns the buffer for assertions.
    pub fn memory() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        (Logger(Arc::new(Sink::Memory(buf.clone()))), buf)
    }

    /// `dir/timewatch.log`, rotated to `timewatch.log.1` above 1 MB.
    pub fn file(dir: &Path) -> Logger {
        Logger::file_with_limit(dir, MAX_BYTES)
    }

    pub fn file_with_limit(dir: &Path, max: u64) -> Logger {
        let _ = fs::create_dir_all(dir);
        let path = dir.join("timewatch.log");
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Logger(Arc::new(Sink::File { path, size: Mutex::new(size), max }))
    }

    pub fn path(&self) -> Option<PathBuf> {
        match &*self.0 {
            Sink::File { path, .. } => Some(path.clone()),
            _ => None,
        }
    }

    fn write(&self, level: &str, msg: &str) {
        let msg = sanitize(msg);
        match &*self.0 {
            Sink::None => {}
            Sink::Memory(buf) => buf.lock().push(format!("{level} {msg}")),
            Sink::File { path, size, max } => {
                let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let line = format!("{ts} {:<5} {msg}\n", level.to_uppercase());
                let mut size = size.lock();
                if *size + line.len() as u64 > *max {
                    let old = path.with_extension("log.1");
                    let _ = fs::remove_file(&old);
                    let _ = fs::rename(path, &old);
                    *size = 0;
                }
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                    if f.write_all(line.as_bytes()).is_ok() {
                        *size += line.len() as u64;
                    }
                }
            }
        }
    }

    pub fn info(&self, msg: impl AsRef<str>) {
        self.write("info", msg.as_ref());
    }
    pub fn warn(&self, msg: impl AsRef<str>) {
        self.write("warn", msg.as_ref());
    }
    pub fn error(&self, msg: impl AsRef<str>) {
        self.write("error", msg.as_ref());
    }
    /// Level given by name ("info" | "warn" | "error"); anything else is info.
    pub fn log(&self, level: &str, msg: impl AsRef<str>) {
        self.write(match level { "warn" => "warn", "error" => "error", _ => "info" }, msg.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials() {
        assert_eq!(sanitize("Authorization: Bearer abc.def"), "Authorization: [redacted]");
        assert_eq!(sanitize("x ApiKey SECRET123 y"), "x ApiKey [redacted] y");
        assert_eq!(sanitize("https://a/cb?code=XYZ&state=1"), "https://a/cb?code=[redacted]&state=1");
        assert_eq!(sanitize("refresh_token=RT1"), "refresh_token=[redacted]");
    }

    #[test]
    fn memory_logger_sanitizes() {
        let (log, buf) = Logger::memory();
        log.info("got Bearer TOPSECRET here");
        assert!(!buf.lock()[0].contains("TOPSECRET"));
    }

    #[test]
    fn file_logger_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let log = Logger::file_with_limit(dir.path(), 200);
        for i in 0..20 {
            log.info(format!("line number {i} with some padding text"));
        }
        assert!(dir.path().join("timewatch.log").exists());
        assert!(dir.path().join("timewatch.log.1").exists());
    }
}
