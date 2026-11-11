//! Translates OS power/session events into tracker calls.
//!
//! The platform code calls the event methods from arbitrary OS threads. Delayed actions (resume grace period,
//! session-end cancel) run as tasks on `deps.handle`; they carry a generation number so that
//! `clear_resume` / `clear_session_end` cancel them reliably even when the task is already waking up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::logger::Logger;
use crate::tracker::Tracker;

/// Default grace period after wake-up before measuring resumes (a lock screen usually follows a resume).
pub const RESUME_GRACE_MS: u64 = 5000;
/// Default time after `query_session_end` after which a cancelled shutdown is assumed.
pub const SESSION_END_CANCEL_MS: u64 = 15000;

pub struct PowerDeps {
    pub tracker: Arc<Tracker>,
    pub log: Logger,
    /// True while the session (screen) is locked; checked when the resume grace period ends.
    pub is_session_locked: Arc<dyn Fn() -> bool + Send + Sync>,
    pub on_system_shutdown: Arc<dyn Fn() + Send + Sync>,
    /// For the delayed actions (resume grace, session-end cancel).
    pub handle: tokio::runtime::Handle,
    /// Default [`RESUME_GRACE_MS`].
    pub resume_grace_ms: u64,
    /// Default [`SESSION_END_CANCEL_MS`].
    pub session_end_cancel_ms: u64,
}

struct Task {
    generation: u64,
    handle: JoinHandle<()>,
}

#[derive(Default)]
struct Timers {
    next_generation: u64,
    resume: Option<Task>,
    session_end: Option<Task>,
}

pub struct PowerEvents {
    deps: PowerDeps,
    locked: AtomicBool,
    timers: Mutex<Timers>,
    me: Weak<PowerEvents>,
}

impl PowerEvents {
    pub fn new(deps: PowerDeps) -> Arc<PowerEvents> {
        Arc::new_cyclic(|me| PowerEvents {
            deps,
            locked: AtomicBool::new(false),
            timers: Mutex::new(Timers::default()),
            me: me.clone(),
        })
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }

    pub fn set_locked(&self, v: bool) {
        self.locked.store(v, Ordering::SeqCst);
    }

    /// Fixed at scheduling time (not when the task is first polled), so the delay is exact.
    fn deadline(&self, delay_ms: u64) -> tokio::time::Instant {
        let _guard = self.deps.handle.enter();
        tokio::time::Instant::now() + Duration::from_millis(delay_ms)
    }

    fn clear_resume(&self) {
        if let Some(t) = self.timers.lock().resume.take() {
            t.handle.abort();
        }
    }

    fn clear_session_end(&self) {
        if let Some(t) = self.timers.lock().session_end.take() {
            t.handle.abort();
        }
    }

    pub fn lock_screen(&self) {
        self.deps.log.info("power: lock-screen");
        self.set_locked(true);
        self.clear_resume();
        self.deps.tracker.system_pause("lock", None);
    }

    pub fn unlock_screen(&self) {
        self.deps.log.info("power: unlock-screen");
        self.set_locked(false);
        self.clear_resume();
        self.deps.tracker.system_resume("unlock");
    }

    pub fn suspend(&self) {
        self.deps.log.info("power: suspend");
        self.clear_resume();
        self.deps.tracker.system_pause("suspend", None);
    }

    pub fn resume(&self) {
        self.deps.log.info("power: resume");
        if self.is_locked() {
            return; // wait for unlock-screen
        }
        // After wake-up a lock screen usually follows; give it a moment before resuming.
        let mut timers = self.timers.lock();
        if let Some(old) = timers.resume.take() {
            old.handle.abort();
        }
        timers.next_generation += 1;
        let generation = timers.next_generation;
        let me = self.me.clone();
        let deadline = self.deadline(self.deps.resume_grace_ms);
        let handle = self.deps.handle.spawn(async move {
            tokio::time::sleep_until(deadline).await;
            if let Some(me) = me.upgrade() {
                me.fire_resume(generation);
            }
        });
        timers.resume = Some(Task { generation, handle });
    }

    /// Runs when the resume grace period elapsed. The timers lock stays held, so a concurrent `clear_resume`
    /// either cancels this before it starts or waits until it has finished.
    fn fire_resume(&self, generation: u64) {
        let mut timers = self.timers.lock();
        if timers.resume.as_ref().map(|t| t.generation) != Some(generation) {
            return; // cancelled or superseded
        }
        timers.resume = None;
        if self.is_locked() {
            return;
        }
        if (self.deps.is_session_locked)() {
            self.set_locked(true); // the lock event was missed; unlock-screen will resume
            return;
        }
        self.deps.tracker.system_resume("resume");
    }

    /// The OS announced a shutdown (macOS/Linux).
    pub fn shutdown(&self) {
        self.deps.log.info("power: shutdown");
        self.clear_resume();
        self.deps.tracker.shutdown();
        (self.deps.on_system_shutdown)();
    }

    /// Windows `WM_QUERYENDSESSION`. It may be followed by a cancelled shutdown, so when the app is still alive
    /// afterwards, measuring resumes. The final `session_end` cancels that.
    pub fn query_session_end(&self) {
        self.deps.log.info("power: query-session-end");
        self.deps.tracker.shutdown();
        let mut timers = self.timers.lock();
        if let Some(old) = timers.session_end.take() {
            old.handle.abort();
        }
        timers.next_generation += 1;
        let generation = timers.next_generation;
        let me = self.me.clone();
        let deadline = self.deadline(self.deps.session_end_cancel_ms);
        let handle = self.deps.handle.spawn(async move {
            tokio::time::sleep_until(deadline).await;
            if let Some(me) = me.upgrade() {
                me.fire_session_end_cancel(generation);
            }
        });
        timers.session_end = Some(Task { generation, handle });
    }

    fn fire_session_end_cancel(&self, generation: u64) {
        let mut timers = self.timers.lock();
        if timers.session_end.as_ref().map(|t| t.generation) != Some(generation) {
            return;
        }
        timers.session_end = None;
        if !self.is_locked() {
            self.deps.tracker.system_resume("session-end-cancelled");
        }
    }

    /// Windows `WM_ENDSESSION` (wParam == TRUE): the session really ends.
    pub fn session_end(&self) {
        self.deps.log.info("power: session-end");
        self.clear_session_end();
        self.deps.tracker.shutdown();
        (self.deps.on_system_shutdown)();
    }

    /// Cancels all pending delayed actions.
    pub fn dispose(&self) {
        self.clear_resume();
        self.clear_session_end();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::clock::{local_ms, FakeClock};
    use crate::tracker::testkit::*;
    use crate::tracker::Status;

    struct Ctx {
        _dir: tempfile::TempDir,
        app: App,
        clock: Arc<FakeClock>,
        pe: Arc<PowerEvents>,
        /// What the OS reports as "session locked" (replaces getSystemIdleState).
        session_locked: Arc<AtomicBool>,
        shutdowns: Arc<AtomicUsize>,
    }

    fn setup(session_locked: bool) -> Ctx {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(local_ms(2025, 6, 10, 9, 0, 0));
        let app = create_app(dir.path(), &clock, Some(default_target()));
        app.tracker.init(false);
        let session_locked = Arc::new(AtomicBool::new(session_locked));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let pe = PowerEvents::new(PowerDeps {
            tracker: app.tracker.clone(),
            log: Logger::noop(),
            is_session_locked: {
                let l = session_locked.clone();
                Arc::new(move || l.load(Ordering::SeqCst))
            },
            on_system_shutdown: {
                let s = shutdowns.clone();
                Arc::new(move || {
                    s.fetch_add(1, Ordering::SeqCst);
                })
            },
            handle: tokio::runtime::Handle::current(),
            resume_grace_ms: RESUME_GRACE_MS,
            session_end_cancel_ms: SESSION_END_CANCEL_MS,
        });
        Ctx { _dir: dir, app, clock, pe, session_locked, shutdowns }
    }

    /// Advances the (paused) tokio clock and lets the woken tasks run.
    async fn advance(ms: u64) {
        tokio::time::advance(Duration::from_millis(ms)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn power_events_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PowerEvents>();
    }

    #[tokio::test(start_paused = true)]
    async fn suspend_and_lock_at_once_resume_unlock_makes_exactly_one_new_segment() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.clock.advance(10 * MIN);
        ctx.pe.lock_screen();
        ctx.pe.suspend(); // duplicate system event
        ctx.pe.lock_screen(); // duplicate
        ctx.clock.advance(60 * MIN);
        ctx.pe.resume(); // the screen is locked: must not resume
        advance(RESUME_GRACE_MS).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        assert_eq!(ctx.app.state().segments.len(), 1);

        ctx.clock.advance(MIN);
        ctx.pe.unlock_screen();
        ctx.pe.unlock_screen(); // duplicate
        advance(RESUME_GRACE_MS).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Running);
        let segs = ctx.app.state().segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs.iter().filter(|s| s.end.is_none()).count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn suspend_then_resume_without_lock_screen_resumes_after_the_grace_period() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.clock.advance(5 * MIN);
        ctx.pe.suspend();
        ctx.clock.advance(30 * MIN);
        ctx.pe.resume();
        assert_eq!(ctx.app.tracker.status().status, Status::Paused); // not immediately
        advance(RESUME_GRACE_MS - 1).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused, "grace period not over yet");
        advance(1).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Running);
        assert_eq!(ctx.app.state().segments.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn resume_while_the_system_reports_locked_does_not_resume() {
        let ctx = setup(true);
        ctx.app.tracker.start().unwrap();
        ctx.clock.advance(5 * MIN);
        ctx.pe.suspend();
        ctx.pe.resume();
        advance(RESUME_GRACE_MS).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        assert!(ctx.pe.is_locked(), "the missed lock event is remembered");
        ctx.pe.unlock_screen();
        assert_eq!(ctx.app.tracker.status().status, Status::Running);
    }

    #[tokio::test(start_paused = true)]
    async fn resume_grace_is_cancelled_by_a_lock_screen_event() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.pe.suspend();
        ctx.pe.resume();
        advance(2000).await;
        ctx.pe.lock_screen();
        advance(10_000).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        assert!(ctx.pe.timers.lock().resume.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_resume_restarts_the_grace_period() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.pe.suspend();
        ctx.pe.resume();
        advance(3000).await;
        ctx.pe.resume();
        advance(3000).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        advance(2000).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Running);
        assert_eq!(ctx.app.state().segments.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_session_lock_is_checked_again_when_the_grace_period_ends() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.pe.suspend();
        ctx.pe.resume();
        ctx.session_locked.store(true, Ordering::SeqCst); // the lock screen appeared during the grace period
        advance(RESUME_GRACE_MS).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        assert!(ctx.pe.is_locked());
    }

    #[tokio::test(start_paused = true)]
    async fn on_system_shutdown_fires_for_shutdown_and_session_end_not_for_query_session_end() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();

        ctx.pe.query_session_end();
        assert_eq!(ctx.shutdowns.load(Ordering::SeqCst), 0);
        assert!(ctx.app.state().current.is_none(), "segment already closed (tracker.shutdown ran first)");
        ctx.pe.session_end();
        assert_eq!(ctx.shutdowns.load(Ordering::SeqCst), 1);
        ctx.pe.shutdown();
        assert_eq!(ctx.shutdowns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn query_session_end_without_session_end_resumes_after_the_cancel_timeout() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.clock.advance(MIN);
        ctx.pe.query_session_end();
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        assert_eq!(ctx.app.state().pause_reason.as_deref(), Some("shutdown"));
        advance(SESSION_END_CANCEL_MS - 1).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        advance(1).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Running, "the shutdown was cancelled");
        assert_eq!(ctx.shutdowns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn session_end_cancels_the_pending_resume() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.pe.query_session_end();
        ctx.pe.session_end();
        advance(SESSION_END_CANCEL_MS * 2).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        assert_eq!(ctx.app.state().segments.len(), 1);
        assert_eq!(ctx.shutdowns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn session_end_cancel_does_not_resume_while_locked() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.pe.query_session_end();
        ctx.pe.set_locked(true);
        advance(SESSION_END_CANCEL_MS).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_event_cancels_a_pending_resume_and_closes_the_segment() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.pe.suspend();
        ctx.pe.resume();
        ctx.pe.shutdown();
        advance(RESUME_GRACE_MS * 2).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
        assert_eq!(ctx.shutdowns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn dispose_cancels_pending_timers() {
        let ctx = setup(false);
        ctx.app.tracker.start().unwrap();
        ctx.pe.suspend();
        ctx.pe.resume();
        ctx.pe.dispose();
        advance(RESUME_GRACE_MS * 2).await;
        assert_eq!(ctx.app.tracker.status().status, Status::Paused);
    }

    #[tokio::test(start_paused = true)]
    async fn set_locked_and_is_locked_roundtrip() {
        let ctx = setup(false);
        assert!(!ctx.pe.is_locked());
        ctx.pe.set_locked(true);
        assert!(ctx.pe.is_locked());
        ctx.pe.lock_screen();
        ctx.pe.unlock_screen();
        assert!(!ctx.pe.is_locked());
    }
}
