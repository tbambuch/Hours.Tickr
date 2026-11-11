//! Injectable time source and local-calendar helpers.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, TimeZone};

use crate::model::Ms;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> Ms;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> Ms {
        chrono::Utc::now().timestamp_millis()
    }
}

pub fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

/// Manually advanced clock for tests.
pub struct FakeClock(AtomicI64);

impl FakeClock {
    pub fn new(start: Ms) -> Arc<FakeClock> {
        Arc::new(FakeClock(AtomicI64::new(start)))
    }
    pub fn set(&self, ms: Ms) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> Ms {
        self.0.load(Ordering::SeqCst)
    }
}

fn local(ms: Ms) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms).earliest().unwrap_or_else(|| Local.timestamp_millis_opt(0).unwrap())
}

/// Local date of a timestamp as `YYYY-MM-DD`.
pub fn local_date_str(ms: Ms) -> String {
    let d = local(ms);
    format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day())
}

/// Local midnight (ms) of the day containing `ms`, shifted by `plus_days` days.
/// DST-safe: it is computed on the calendar date, not by adding 24 h.
pub fn local_midnight(ms: Ms, plus_days: i64) -> Ms {
    let d = local(ms);
    let date = NaiveDate::from_ymd_opt(d.year(), d.month(), d.day()).unwrap() + chrono::Duration::days(plus_days);
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(t) => t.timestamp_millis(),
        LocalResult::Ambiguous(first, _) => first.timestamp_millis(),
        // Midnight does not exist (DST jump at 00:00): use the first valid instant after it.
        LocalResult::None => Local.from_local_datetime(&(naive + chrono::Duration::hours(1))).earliest().map(|t| t.timestamp_millis() - 3_600_000).unwrap_or(ms),
    }
}

/// Formats a duration as `H:MM`.
pub fn format_hm(ms: i64) -> String {
    let total_min = ms.max(0) / 60_000;
    format!("{}:{:02}", total_min / 60, total_min % 60)
}

/// Local-time timestamp for tests/fixtures: `local_ms(2026, 11, 11, 8, 30, 0)`.
pub fn local_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Ms {
    Local.with_ymd_and_hms(y, mo, d, h, mi, s).earliest().unwrap().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midnight_and_date() {
        let t = local_ms(2026, 11, 11, 15, 30, 12);
        assert_eq!(local_midnight(t, 0), local_ms(2026, 11, 11, 0, 0, 0));
        assert_eq!(local_midnight(t, 1), local_ms(2026, 11, 12, 0, 0, 0));
        assert_eq!(local_date_str(t), "2026-11-11");
    }

    #[test]
    fn hm_format() {
        assert_eq!(format_hm(0), "0:00");
        assert_eq!(format_hm(-5), "0:00");
        assert_eq!(format_hm(3 * 3_600_000 + 7 * 60_000 + 59_000), "3:07");
    }

    #[test]
    fn fake_clock_moves() {
        let c = FakeClock::new(1000);
        c.advance(500);
        assert_eq!(c.now_ms(), 1500);
        c.set(7);
        assert_eq!(c.now_ms(), 7);
    }
}
