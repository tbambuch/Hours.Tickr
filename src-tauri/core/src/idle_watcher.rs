//! Pauses measuring when the user has not touched keyboard/mouse for a while, and resumes on activity
//!. The idle time source, threshold and clock are injected. Call `check()` about once
//! a second.
//!
//! When the limit is reached the open segment is closed retroactively at the moment of the LAST input
//! (now - idle time), so the time spent away is not counted. The first input afterwards starts a new segment.

use std::sync::Arc;

use crate::clock::Clock;
use crate::logger::Logger;
use crate::model::{format_iso, Ms};
use crate::tracker::{Status, Tracker};

/// Idle time below this (seconds) means "the user is back".
pub const ACTIVE_BELOW_SEC: f64 = 2.0;

pub struct IdleWatcher {
    tracker: Arc<Tracker>,
    get_idle_seconds: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
    get_threshold_min: Arc<dyn Fn() -> u32 + Send + Sync>,
    clock: Arc<dyn Clock>,
    log: Logger,
}

impl IdleWatcher {
    pub fn new(
        tracker: Arc<Tracker>,
        get_idle_seconds: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
        get_threshold_min: Arc<dyn Fn() -> u32 + Send + Sync>,
        clock: Arc<dyn Clock>,
        log: Logger,
    ) -> IdleWatcher {
        IdleWatcher { tracker, get_idle_seconds, get_threshold_min, clock, log }
    }

    /// Call about once per second.
    pub fn check(&self) {
        let threshold_min = (self.get_threshold_min)();
        let Some(idle) = (self.get_idle_seconds)().filter(|v| v.is_finite()) else {
            return;
        };
        let st = self.tracker.status();

        if st.status == Status::Running {
            if threshold_min > 0 && idle >= f64::from(threshold_min) * 60.0 {
                let since: Ms = (self.clock.now_ms() as f64 - idle * 1000.0).floor() as Ms;
                self.log.info(format!(
                    "idle: no input for {} s, pausing (counted until {})",
                    idle.round() as i64,
                    format_iso(since)
                ));
                self.tracker.system_pause("idle", Some(since));
            }
        } else if st.status == Status::Paused && st.pause_reason.as_deref() == Some("idle") {
            // Resume on activity, or right away when the feature gets switched off while paused.
            if threshold_min == 0 || idle < ACTIVE_BELOW_SEC {
                self.log.info("idle: activity detected, resuming");
                self.tracker.system_resume("activity");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use parking_lot::Mutex;
    use tempfile::TempDir;

    use super::*;
    use crate::clock::{local_ms, FakeClock};
    use crate::tracker::testkit::*;

    struct Ctx {
        _dir: TempDir,
        app: App,
        clock: Arc<FakeClock>,
        idle_sec: Arc<Mutex<Option<f64>>>,
        threshold_min: Arc<AtomicU32>,
        watcher: IdleWatcher,
    }

    impl Ctx {
        fn set_idle(&self, v: f64) {
            *self.idle_sec.lock() = Some(v);
        }
        fn idle(&self) -> f64 {
            self.idle_sec.lock().unwrap_or(0.0)
        }
        /// Lets `sec` seconds pass without input, checking once per second like the application does.
        fn idle_for(&self, sec: u32) {
            for _ in 0..sec {
                self.clock.advance(1000);
                self.set_idle(self.idle() + 1.0);
                self.watcher.check();
            }
        }
    }

    fn setup(threshold_min: u32) -> Ctx {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(local_ms(2025, 6, 10, 9, 0, 0));
        let app = create_app(dir.path(), &clock, Some(default_target()));
        let idle_sec = Arc::new(Mutex::new(Some(0.0)));
        let threshold = Arc::new(AtomicU32::new(threshold_min));
        let clock_dyn: Arc<dyn Clock> = clock.clone();
        let watcher = IdleWatcher::new(
            app.tracker.clone(),
            {
                let i = idle_sec.clone();
                Arc::new(move || *i.lock())
            },
            {
                let t = threshold.clone();
                Arc::new(move || t.load(Ordering::SeqCst))
            },
            clock_dyn,
            Logger::noop(),
        );
        app.tracker.init(false);
        Ctx { _dir: dir, app, clock, idle_sec, threshold_min: threshold, watcher }
    }

    #[test]
    fn no_input_for_the_limit_pauses_and_the_idle_time_is_not_counted() {
        let ctx = setup(10);
        ctx.app.tracker.start().unwrap();
        ctx.clock.advance(5 * MIN); // 5 min of work, then the user leaves
        ctx.set_idle(0.0);
        ctx.idle_for(12 * 60);

        let st = ctx.app.tracker.status();
        assert_eq!(st.status, Status::Paused);
        assert_eq!(st.pause_reason.as_deref(), Some("idle"));
        let seg = &ctx.app.state().segments[0];
        assert_eq!(seg.end.unwrap() - seg.start, 5 * MIN, "segment ends at the last input");
        assert_eq!(st.today_ms, 5 * MIN);
    }

    #[test]
    fn stays_running_while_the_idle_time_is_below_the_limit() {
        let ctx = setup(10);
        ctx.app.tracker.start().unwrap();
        ctx.idle_for(9 * 60);
        assert_eq!(ctx.app.tracker.status().status, Status::Running);
    }

    #[test]
    fn activity_resumes_with_a_new_segment_starting_now() {
        let ctx = setup(10);
        ctx.app.tracker.start().unwrap();
        ctx.idle_for(11 * 60);
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);

        ctx.clock.advance(20 * MIN); // still away
        ctx.set_idle(31.0 * 60.0);
        ctx.watcher.check();
        assert_eq!(ctx.app.tracker.status().status, Status::Paused, "no input yet");

        ctx.set_idle(0.0); // mouse moved
        ctx.watcher.check();
        let st = ctx.app.tracker.status();
        assert_eq!(st.status, Status::Running);
        let segs = ctx.app.state().segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].start, ctx.clock.now_ms());
        assert!(st.user_wants_running);
    }

    #[test]
    fn threshold_0_disables_the_feature_switching_it_off_while_paused_resumes() {
        let ctx = setup(0);
        ctx.app.tracker.start().unwrap();
        ctx.idle_for(3 * 3600);
        assert_eq!(ctx.app.tracker.status().status, Status::Running);

        ctx.threshold_min.store(10, Ordering::SeqCst);
        ctx.watcher.check();
        assert_eq!(ctx.app.tracker.status().pause_reason.as_deref(), Some("idle"));
        ctx.threshold_min.store(0, Ordering::SeqCst);
        ctx.watcher.check();
        assert_eq!(ctx.app.tracker.status().status, Status::Running);
    }

    #[test]
    fn a_stopped_timer_is_never_touched_by_the_watcher_and_stop_clears_the_idle_pause() {
        let ctx = setup(10);
        ctx.idle_for(20 * 60);
        assert_eq!(ctx.app.tracker.status().status, Status::Stopped);

        ctx.app.tracker.start().unwrap();
        ctx.idle_for(11 * 60);
        assert_eq!(ctx.app.tracker.status().pause_reason.as_deref(), Some("idle"));
        ctx.app.tracker.stop();
        ctx.set_idle(0.0);
        ctx.watcher.check();
        let st = ctx.app.tracker.status();
        assert_eq!(st.status, Status::Stopped);
        assert!(!st.user_wants_running);
    }

    #[test]
    fn a_lock_after_an_idle_pause_does_not_break_the_later_unlock() {
        let ctx = setup(10);
        ctx.app.tracker.start().unwrap();
        ctx.idle_for(11 * 60);
        assert!(!ctx.app.tracker.system_pause("lock", None), "nothing open to pause");
        ctx.set_idle(0.0);
        ctx.app.tracker.system_resume("unlock");
        assert_eq!(ctx.app.tracker.status().status, Status::Running);
    }

    #[test]
    fn an_unavailable_or_non_finite_idle_time_is_ignored() {
        let ctx = setup(10);
        ctx.app.tracker.start().unwrap();
        ctx.clock.advance(30 * MIN);
        *ctx.idle_sec.lock() = None;
        ctx.watcher.check();
        ctx.set_idle(f64::NAN);
        ctx.watcher.check();
        ctx.set_idle(f64::INFINITY);
        ctx.watcher.check();
        assert_eq!(ctx.app.tracker.status().status, Status::Running);
    }
}
