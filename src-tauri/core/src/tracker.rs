//! Tracking state machine. Pure logic: clock, store and callbacks are injected.
//!
//! status (derived):
//!   running  - `state.current` exists (an open segment)
//!   paused   - no open segment, but `user_wants_running` (system interrupted: lock/suspend/shutdown/idle)
//!   stopped  - `user_wants_running` is false
//! `user_wants_running` is changed ONLY by `start()`/`stop()`; system events only toggle running <-> paused.
//!
//! Locking rule: every public method is structured as lock -> mutate -> save -> unlock -> callbacks.
//! `on_change`, `on_queue_change`, `get_target` and `is_locked` are never invoked while the state lock is held
//! (they read the state back and the mutex is not reentrant).

use std::sync::Arc;

use parking_lot::Mutex;

use crate::clock::{local_midnight, Clock};
use crate::logger::Logger;
use crate::model::{Current, JsonId, Ms, Op, Segment, State, Target};
use crate::store::{SharedState, Store};

pub type Cb = Arc<dyn Fn() + Send + Sync>;

pub const HEARTBEAT_MS: i64 = 30_000;
const SLEEP_GAP_MS: i64 = 120_000;
const KEEP_SEGMENTS_MS: i64 = 7 * 24 * 3600 * 1000;

/// Default `new_id`: a random uuid v4.
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct TrackerDeps {
    pub store: SharedState,
    pub clock: Arc<dyn Clock>,
    pub get_target: Arc<dyn Fn() -> Option<Target> + Send + Sync>,
    pub is_locked: Arc<dyn Fn() -> bool + Send + Sync>,
    /// After every state change (UI refresh).
    pub on_change: Cb,
    /// The queue got operations (start a sync); must be cheap and non-blocking.
    pub on_queue_change: Cb,
    pub log: Logger,
    /// uuid v4 in production.
    pub new_id: Arc<dyn Fn() -> String + Send + Sync>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Paused,
    Stopped,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrackerStatus {
    pub status: Status,
    pub user_wants_running: bool,
    pub pause_reason: Option<String>,
    pub today_ms: i64,
}

/// `start()` was refused because no project is configured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotConfigured;

impl std::fmt::Display for NotConfigured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not-configured")
    }
}

impl std::error::Error for NotConfigured {}

/// Sum of today's (local day) time over all segments; open segments run until `now`.
pub fn compute_today_ms(segments: &[Segment], now: Ms) -> i64 {
    let day_start = local_midnight(now, 0);
    let day_end = local_midnight(now, 1);
    let mut total = 0;
    for seg in segments {
        let a = seg.start.max(day_start);
        let end = seg.end.unwrap_or(now);
        let b = end.min(day_end).min(now);
        if b > a {
            total += b - a;
        }
    }
    total
}

fn find_seg<'a>(s: &'a State, local_id: &str) -> Option<&'a Segment> {
    s.segments.iter().find(|x| x.local_id == local_id)
}

fn open_segment(s: &mut State, start: Ms, t: &Target, new_id: &(dyn Fn() -> String + Send + Sync)) {
    let local_id = new_id();
    let seg = Segment {
        local_id: local_id.clone(),
        start,
        end: None,
        remote_log_id: None,
        sync_state: "pending".to_string(),
        project_id: t.project_id.clone(),
        task_id: t.task_id.clone(),
        note: t.note.clone(),
        merge: true,
        shift_ms: None,
        recreated: false,
    };
    s.queue.push(Op::Start {
        id: new_id(),
        local_id: local_id.clone(),
        start,
        project_id: seg.project_id.clone(),
        task_id: seg.task_id.clone(),
        note: seg.note.clone(),
    });
    s.segments.push(seg);
    s.current = Some(Current { local_id, start, remote_log_id: None });
    s.pause_reason = None;
    s.last_heartbeat = Some(start);
}

/// Closes the open segment (if any) and queues the stop. Returns whether there was one.
fn close_segment(s: &mut State, end_ms: Ms, new_id: &(dyn Fn() -> String + Send + Sync)) -> bool {
    let Some(cur) = s.current.take() else {
        return false;
    };
    let end = end_ms.max(cur.start);
    if let Some(seg) = s.segments.iter_mut().find(|x| x.local_id == cur.local_id) {
        seg.end = Some(end);
    }
    s.queue.push(Op::Stop { id: new_id(), local_id: cur.local_id, end });
    true
}

fn prune(s: &mut State, now: Ms) {
    s.segments.retain(|seg| {
        let Some(end) = seg.end else { return true };
        if seg.sync_state != "synced" && seg.sync_state != "skipped" {
            return true;
        }
        now - end <= KEEP_SEGMENTS_MS
    });
}

fn same_target(seg: &Segment, t: &Target) -> bool {
    JsonId::same(&seg.project_id, &t.project_id) && JsonId::same(&seg.task_id, &t.task_id) && seg.note == t.note
}

pub struct Tracker {
    deps: TrackerDeps,
    /// Time of the previous `tick()` (sleep-gap detection). `None` until the first tick.
    last_tick: Mutex<Option<Ms>>,
}

impl Tracker {
    pub fn new(deps: TrackerDeps) -> Tracker {
        Tracker { deps, last_tick: Mutex::new(None) }
    }

    fn now(&self) -> Ms {
        self.deps.clock.now_ms()
    }

    fn new_id(&self) -> &(dyn Fn() -> String + Send + Sync) {
        &*self.deps.new_id
    }

    /// Prune + atomic save. Called with the state lock held; callbacks are fired separately by `notify`.
    fn commit_locked(&self, g: &mut Store<State>) {
        prune(&mut g.state, self.now());
        g.save();
    }

    /// Fires the callbacks. MUST be called without the state lock.
    fn notify(&self, kick: bool) {
        (self.deps.on_change)();
        if kick {
            (self.deps.on_queue_change)();
        }
    }

    /// Call once at application start. `locked`: the screen is locked right now.
    pub fn init(&self, locked: bool) {
        let now = self.now();
        let wants = self.deps.store.lock().state.user_wants_running;
        let target = if wants { (self.deps.get_target)() } else { None };
        {
            let mut g = self.deps.store.lock();
            if let Some(cur) = g.state.current.clone() {
                // Crash / power loss: the segment was never closed. Close it at the last heartbeat.
                let end = g.state.last_heartbeat.unwrap_or(cur.start).min(now);
                self.deps.log.info(format!(
                    "tracker: recovering unclosed segment, closing at heartbeat {}",
                    crate::model::format_iso(end)
                ));
                close_segment(&mut g.state, end, self.new_id());
            }
            if wants {
                match target {
                    None => {
                        // Keep the user's intent: the key/config may only be temporarily unavailable at boot.
                        self.deps
                            .log
                            .warn("tracker: userWantsRunning but no project configured, staying paused (not-configured)");
                        g.state.pause_reason = Some("not-configured".to_string());
                    }
                    Some(_) if locked => {
                        g.state.pause_reason = Some("lock".to_string());
                        self.deps.log.info("tracker: started while locked, waiting for unlock");
                    }
                    Some(t) => {
                        open_segment(&mut g.state, now, &t, self.new_id());
                        self.deps.log.info("tracker: resumed after application start");
                    }
                }
            }
            self.commit_locked(&mut g);
        }
        self.notify(true);
    }

    pub fn start(&self) -> Result<(), NotConfigured> {
        let Some(target) = (self.deps.get_target)() else {
            return Err(NotConfigured);
        };
        {
            let mut g = self.deps.store.lock();
            g.state.user_wants_running = true;
            if g.state.current.is_none() {
                open_segment(&mut g.state, self.now(), &target, self.new_id());
            }
            self.deps.log.info("tracker: user start");
            self.commit_locked(&mut g);
        }
        self.notify(true);
        Ok(())
    }

    pub fn stop(&self) {
        {
            let mut g = self.deps.store.lock();
            g.state.user_wants_running = false;
            g.state.pause_reason = None;
            close_segment(&mut g.state, self.now(), self.new_id());
            self.deps.log.info("tracker: user stop");
            self.commit_locked(&mut g);
        }
        self.notify(true);
    }

    pub fn toggle(&self) -> Result<(), NotConfigured> {
        if self.status().status == Status::Stopped {
            self.start()
        } else {
            self.stop();
            Ok(())
        }
    }

    /// lock / suspend / idle. Idempotent: does nothing unless a segment is open.
    /// `at_ms` = when the interruption began.
    pub fn system_pause(&self, reason: &str, at_ms: Option<Ms>) -> bool {
        {
            let mut g = self.deps.store.lock();
            if g.state.current.is_none() {
                return false;
            }
            let now = self.now();
            let end = at_ms.map_or(now, |a| a.min(now));
            close_segment(&mut g.state, end, self.new_id());
            g.state.pause_reason = Some(reason.to_string());
            self.deps.log.info(format!("tracker: paused ({reason})"));
            self.commit_locked(&mut g);
        }
        self.notify(true);
        true
    }

    /// unlock / resume. Idempotent: only acts when paused and the user wants to run.
    pub fn system_resume(&self, source: &str) -> bool {
        {
            let g = self.deps.store.lock();
            if !g.state.user_wants_running || g.state.current.is_some() {
                return false;
            }
        }
        let Some(target) = (self.deps.get_target)() else {
            return false;
        };
        {
            let mut g = self.deps.store.lock();
            if !g.state.user_wants_running || g.state.current.is_some() {
                return false;
            }
            open_segment(&mut g.state, self.now(), &target, self.new_id());
            self.deps.log.info(format!("tracker: resumed ({source})"));
            self.commit_locked(&mut g);
        }
        self.notify(true);
        true
    }

    /// Session end / OS shutdown. Synchronous: closes the segment and flushes state to disk.
    pub fn shutdown(&self) -> bool {
        let had;
        {
            let mut g = self.deps.store.lock();
            had = g.state.current.is_some();
            close_segment(&mut g.state, self.now(), self.new_id());
            if had {
                g.state.pause_reason = Some("shutdown".to_string());
            }
            self.deps.log.info("tracker: shutdown, segment closed");
            self.commit_locked(&mut g);
        }
        self.notify(true);
        had
    }

    /// User quits the app: close the segment but keep `user_wants_running`.
    pub fn quit(&self) {
        {
            let mut g = self.deps.store.lock();
            close_segment(&mut g.state, self.now(), self.new_id());
            self.deps.log.info("tracker: app quit, segment closed");
            self.commit_locked(&mut g);
        }
        self.notify(true);
    }

    /// Project/task/note changed in settings while running: new segment with the new values.
    pub fn change_target(&self) {
        if self.deps.store.lock().state.current.is_none() {
            return;
        }
        let Some(target) = (self.deps.get_target)() else {
            self.stop();
            return;
        };
        {
            let mut g = self.deps.store.lock();
            let Some(cur) = g.state.current.clone() else {
                return;
            };
            if find_seg(&g.state, &cur.local_id).is_some_and(|seg| same_target(seg, &target)) {
                return;
            }
            let now = self.now();
            close_segment(&mut g.state, now, self.new_id());
            open_segment(&mut g.state, now, &target, self.new_id());
            self.deps.log.info("tracker: target changed, new segment");
            self.commit_locked(&mut g);
        }
        self.notify(true);
    }

    /// Call about once per second: sleep-gap detection, midnight split and heartbeat.
    pub fn tick(&self) {
        let now = self.now();
        let prev_tick = {
            let mut lt = self.last_tick.lock();
            let p = *lt;
            *lt = Some(now);
            p
        };

        // Peek (short lock) to learn which callbacks are needed; they must run outside the lock.
        let (has_current, seg_missing) = {
            let g = self.deps.store.lock();
            match &g.state.current {
                Some(c) => (true, find_seg(&g.state, &c.local_id).is_none()),
                None => (false, false),
            }
        };
        if !has_current {
            return;
        }
        let gap_from = prev_tick.filter(|p| now - p > SLEEP_GAP_MS);
        // Fallback when the open segment is missing from the list (should not happen).
        let fallback = if seg_missing { (self.deps.get_target)() } else { None };
        let locked_now = if gap_from.is_some() { (self.deps.is_locked)() } else { false };

        if let Some(prev) = gap_from {
            let still_open;
            {
                let mut g = self.deps.store.lock();
                let Some(cur) = g.state.current.clone() else {
                    return;
                };
                self.deps.log.info(format!(
                    "tracker: gap of {} s detected (sleep without suspend event)",
                    ((now - prev) as f64 / 1000.0).round() as i64
                ));
                let target =
                    find_seg(&g.state, &cur.local_id).map(|x| x.target()).or_else(|| fallback.clone()).unwrap_or_default();
                close_segment(&mut g.state, prev, self.new_id());
                if locked_now {
                    g.state.pause_reason = Some("lock".to_string());
                } else {
                    open_segment(&mut g.state, now, &target, self.new_id());
                }
                g.state.last_heartbeat = Some(now);
                self.commit_locked(&mut g);
                still_open = g.state.current.is_some();
            }
            self.notify(true);
            if !still_open {
                return;
            }
        }

        let mut g = self.deps.store.lock();
        if g.state.current.is_none() {
            return;
        }
        let mut changed = false;
        while let Some(cur) = g.state.current.clone() {
            let boundary = local_midnight(cur.start, 1);
            if boundary > now || boundary <= cur.start {
                break;
            }
            let target =
                find_seg(&g.state, &cur.local_id).map(|x| x.target()).or_else(|| fallback.clone()).unwrap_or_default();
            close_segment(&mut g.state, boundary - 1, self.new_id());
            open_segment(&mut g.state, boundary, &target, self.new_id());
            self.deps.log.info("tracker: midnight split");
            changed = true;
        }
        if changed {
            g.state.last_heartbeat = Some(now);
            self.commit_locked(&mut g);
            drop(g);
            self.notify(true);
        } else if g.state.last_heartbeat.map_or(true, |h| now - h >= HEARTBEAT_MS) {
            g.state.last_heartbeat = Some(now);
            g.save();
        }
    }

    pub fn status(&self) -> TrackerStatus {
        let g = self.deps.store.lock();
        let s = &g.state;
        let now = self.now();
        let status = if s.current.is_some() {
            Status::Running
        } else if s.user_wants_running {
            Status::Paused
        } else {
            Status::Stopped
        };
        TrackerStatus {
            status,
            user_wants_running: s.user_wants_running,
            pause_reason: s.pause_reason.clone(),
            today_ms: compute_today_ms(&s.segments, now),
        }
    }
}

/// Shared fixtures for the tracker, idle watcher and power tests (fake clock, in-memory store, app wiring).
#[cfg(test)]
pub(crate) mod testkit {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::clock::FakeClock;

    pub const MIN: i64 = 60_000;

    pub fn target(project: i64, task: Option<i64>, note: &str) -> Target {
        Target { project_id: Some(JsonId::Num(project)), task_id: task.map(JsonId::Num), note: note.to_string() }
    }

    /// The default target of the JS helpers: `{ projectId: 7, taskId: 3, note: 'work' }`.
    pub fn default_target() -> Target {
        target(7, Some(3), "work")
    }

    pub struct App {
        pub store: SharedState,
        pub tracker: Arc<Tracker>,
        pub target: Arc<Mutex<Option<Target>>>,
        pub locked: Arc<AtomicBool>,
        pub changes: Arc<AtomicUsize>,
        pub kicks: Arc<AtomicUsize>,
        pub file: PathBuf,
    }

    impl App {
        pub fn set_target(&self, t: Option<Target>) {
            *self.target.lock() = t;
        }
        pub fn state(&self) -> State {
            self.store.lock().state.clone()
        }
        /// The state as currently written on disk.
        pub fn disk(&self) -> serde_json::Value {
            let raw = std::fs::read_to_string(&self.file).expect("state file");
            serde_json::from_str(&raw).expect("valid json")
        }
    }

    /// Wires store + tracker like the application does. `dir` plays the disk: creating a second app on the same
    /// dir simulates a restart. All callbacks read the state back, so a callback fired under the lock deadlocks.
    pub fn create_app(dir: &Path, clock: &Arc<FakeClock>, initial_target: Option<Target>) -> App {
        let file = dir.join("state.json");
        let mut st: Store<State> = Store::new(&file, Logger::noop());
        st.load();
        let store = st.into_shared();
        let target = Arc::new(Mutex::new(initial_target));
        let locked = Arc::new(AtomicBool::new(false));
        let changes = Arc::new(AtomicUsize::new(0));
        let kicks = Arc::new(AtomicUsize::new(0));
        let clock_dyn: Arc<dyn Clock> = clock.clone();
        let tracker = Tracker::new(TrackerDeps {
            store: store.clone(),
            clock: clock_dyn,
            get_target: {
                let (store, target) = (store.clone(), target.clone());
                Arc::new(move || {
                    let _g = store.lock();
                    target.lock().clone()
                })
            },
            is_locked: {
                let (store, locked) = (store.clone(), locked.clone());
                Arc::new(move || {
                    let _g = store.lock();
                    locked.load(Ordering::SeqCst)
                })
            },
            on_change: {
                let (store, changes) = (store.clone(), changes.clone());
                Arc::new(move || {
                    let _g = store.lock();
                    changes.fetch_add(1, Ordering::SeqCst);
                })
            },
            on_queue_change: {
                let (store, kicks) = (store.clone(), kicks.clone());
                Arc::new(move || {
                    let _g = store.lock();
                    kicks.fetch_add(1, Ordering::SeqCst);
                })
            },
            log: Logger::noop(),
            new_id: Arc::new(new_uuid),
        });
        App { store, tracker: Arc::new(tracker), target, locked, changes, kicks, file }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::testkit::*;
    use super::*;
    use crate::clock::{local_ms, FakeClock};

    fn t0() -> Ms {
        local_ms(2025, 6, 10, 9, 0, 0)
    }

    fn setup() -> (tempfile::TempDir, Arc<FakeClock>, App) {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let app = create_app(dir.path(), &clock, Some(default_target()));
        app.tracker.init(false);
        (dir, clock, app)
    }

    #[test]
    fn tracker_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Tracker>();
    }

    #[test]
    fn start_lock_unlock_stop_makes_two_segments_and_user_wants_running_false() {
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        clock.advance(10 * MIN);
        app.tracker.system_pause("lock", None);
        assert_eq!(app.tracker.status().status, Status::Paused);
        clock.advance(5 * MIN);
        app.tracker.system_resume("unlock");
        assert_eq!(app.tracker.status().status, Status::Running);
        clock.advance(20 * MIN);
        app.tracker.stop();

        let s = app.state();
        assert_eq!(s.segments.len(), 2);
        assert!(!s.user_wants_running);
        assert!(s.current.is_none());
        assert_eq!(s.segments[0].end.unwrap() - s.segments[0].start, 10 * MIN);
        assert_eq!(s.segments[1].end.unwrap() - s.segments[1].start, 20 * MIN);
        assert_eq!(app.tracker.status().status, Status::Stopped);
        // every segment queued a start and a stop for the sync queue, in order
        let kinds: Vec<&str> = s
            .queue
            .iter()
            .map(|o| match o {
                Op::Start { .. } => "start",
                Op::Stop { .. } => "stop",
                Op::Delete { .. } => "delete",
            })
            .collect();
        assert_eq!(kinds, ["start", "stop", "start", "stop"]);
        assert!(app.kicks.load(Ordering::SeqCst) >= 4, "the queue is kicked after every change");
        assert!(s.segments.iter().all(|x| x.sync_state == "pending" && x.merge));
        assert_eq!(s.segments[0].project_id, Some(JsonId::Num(7)));
        assert_eq!(s.segments[0].task_id, Some(JsonId::Num(3)));
        assert_eq!(s.segments[0].note, "work");
    }

    #[test]
    fn user_stop_while_paused_is_not_undone_by_unlock() {
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        clock.advance(5 * MIN);
        app.tracker.system_pause("lock", None);
        app.tracker.stop();
        assert!(!app.tracker.system_resume("unlock"));
        assert_eq!(app.tracker.status().status, Status::Stopped);
    }

    #[test]
    fn crash_unclosed_segment_closed_at_last_heartbeat_new_one_opened_if_user_wants_running() {
        let (dir, clock, app1) = setup();
        app1.tracker.start().unwrap();
        clock.advance(10 * MIN);
        app1.tracker.tick(); // heartbeat written at T0 + 10 min
        let heartbeat = clock.now_ms();
        clock.advance(10 * 1000); // ... then power loss, no further writes
        let before_crash = app1.disk();
        assert!(!before_crash["current"].is_null(), "segment still open on disk");

        clock.set(t0() + 3 * 3600 * 1000); // the PC comes back 3 h later
        let app2 = create_app(dir.path(), &clock, Some(default_target()));
        app2.tracker.init(false);
        let s = app2.state();
        assert_eq!(s.segments.len(), 2);
        assert_eq!(s.segments[0].end, Some(heartbeat));
        assert_eq!(s.segments[1].start, clock.now_ms());
        assert_eq!(s.segments[1].end, None);
        assert!(s.current.is_some());
        assert_eq!(app2.tracker.status().status, Status::Running);
        // the crash-recovered segment reaches the server with the heartbeat end time: the stop op carries it
        let stop = s.queue.iter().find_map(|o| match o {
            Op::Stop { local_id, end, .. } if *local_id == s.segments[0].local_id => Some(*end),
            _ => None,
        });
        assert_eq!(stop, Some(heartbeat));
        // exactly one start op belongs to the new segment
        let starts_new =
            s.queue.iter().filter(|o| matches!(o, Op::Start { local_id, .. } if *local_id == s.segments[1].local_id)).count();
        assert_eq!(starts_new, 1);
    }

    #[test]
    fn crash_with_user_wants_running_false_opens_nothing() {
        let (dir, clock, app1) = setup();
        app1.tracker.start().unwrap();
        clock.advance(5 * MIN);
        app1.tracker.tick();
        // tamper: the user had stopped but the crash happened before the close (state says wants=false + current)
        let mut j = app1.disk();
        j["userWantsRunning"] = serde_json::Value::Bool(false);
        std::fs::write(dir.path().join("state.json"), serde_json::to_string(&j).unwrap()).unwrap();
        clock.advance(MIN);
        let app2 = create_app(dir.path(), &clock, Some(default_target()));
        app2.tracker.init(false);
        assert!(app2.state().current.is_none());
        assert_eq!(app2.tracker.status().status, Status::Stopped);
    }

    #[test]
    fn shutdown_then_restart_of_the_app_continues_with_a_new_segment() {
        let (dir, clock, app1) = setup();
        app1.tracker.start().unwrap();
        clock.advance(30 * MIN);
        assert!(app1.tracker.shutdown());
        let j = app1.disk();
        assert!(j["current"].is_null());
        assert_eq!(j["pauseReason"], "shutdown");
        assert_eq!(j["userWantsRunning"], true);

        clock.advance(12 * 3600 * 1000);
        let app2 = create_app(dir.path(), &clock, Some(default_target()));
        app2.tracker.init(false);
        assert_eq!(app2.tracker.status().status, Status::Running);
        let s = app2.state();
        assert_eq!(s.segments.len(), 2);
        assert_eq!(s.segments[0].end, Some(t0() + 30 * MIN));
        assert_eq!(s.segments[1].start, clock.now_ms());
    }

    #[test]
    fn shutdown_without_open_segment_reports_false_and_keeps_pause_reason() {
        let (_d, _clock, app) = setup();
        assert!(!app.tracker.shutdown());
        assert_eq!(app.tracker.status().pause_reason, None);
    }

    #[test]
    fn user_quit_keeps_user_wants_running_so_the_next_start_resumes() {
        let (dir, clock, app1) = setup();
        app1.tracker.start().unwrap();
        clock.advance(5 * MIN);
        app1.tracker.quit();
        assert!(app1.state().user_wants_running);
        assert!(app1.state().current.is_none());
        let app2 = create_app(dir.path(), &clock, Some(default_target()));
        app2.tracker.init(false);
        assert_eq!(app2.tracker.status().status, Status::Running);
    }

    #[test]
    fn midnight_crossing_while_running_makes_two_segments() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(local_ms(2025, 6, 10, 23, 59, 0));
        let app = create_app(dir.path(), &clock, Some(default_target()));
        app.tracker.init(false);
        app.tracker.start().unwrap();
        clock.set(local_ms(2025, 6, 11, 0, 0, 30));
        app.tracker.tick();

        let segs = app.state().segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].end, Some(local_ms(2025, 6, 10, 23, 59, 59) + 999));
        assert_eq!(segs[1].start, local_ms(2025, 6, 11, 0, 0, 0));
        assert_eq!(segs[1].end, None);
        assert_eq!(app.tracker.status().status, Status::Running);
        // today's total counts only the new day
        assert_eq!(app.tracker.status().today_ms, 30 * 1000);
    }

    #[test]
    fn midnight_split_keeps_the_project_and_task_of_the_running_segment() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(local_ms(2025, 6, 10, 23, 58, 0));
        let app = create_app(dir.path(), &clock, Some(default_target()));
        app.tracker.init(false);
        app.tracker.start().unwrap();
        clock.set(local_ms(2025, 6, 11, 0, 1, 0));
        app.tracker.tick();
        let segs = app.state().segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].project_id, segs[0].project_id);
        assert_eq!(segs[1].task_id, segs[0].task_id);
        assert_eq!(segs[1].note, segs[0].note);
    }

    #[test]
    fn midnight_split_uses_the_segment_target_not_the_current_settings() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(local_ms(2025, 6, 10, 23, 58, 0));
        let app = create_app(dir.path(), &clock, Some(default_target()));
        app.tracker.init(false);
        app.tracker.start().unwrap();
        app.set_target(Some(target(99, None, "other"))); // settings changed but change_target() not yet called
        clock.set(local_ms(2025, 6, 11, 0, 1, 0));
        app.tracker.tick();
        let segs = app.state().segments;
        assert_eq!(segs[1].project_id, Some(JsonId::Num(7)));
    }

    fn seg_at(start: Ms, end: Option<Ms>) -> Segment {
        Segment {
            local_id: "x".into(),
            start,
            end,
            remote_log_id: None,
            sync_state: "pending".into(),
            project_id: None,
            task_id: None,
            note: String::new(),
            merge: true,
            shift_ms: None,
            recreated: false,
        }
    }

    #[test]
    fn compute_today_ms_sums_only_today_and_includes_the_running_segment() {
        let now = local_ms(2025, 6, 10, 15, 0, 0);
        let segments = vec![
            // yesterday, fully outside
            seg_at(local_ms(2025, 6, 9, 10, 0, 0), Some(local_ms(2025, 6, 9, 12, 0, 0))),
            // crossing midnight: only the part after 00:00 counts (30 min)
            seg_at(local_ms(2025, 6, 9, 23, 30, 0), Some(local_ms(2025, 6, 10, 0, 30, 0))),
            // today 1 h
            seg_at(local_ms(2025, 6, 10, 9, 0, 0), Some(local_ms(2025, 6, 10, 10, 0, 0))),
            // running since 14:00 -> 1 h until now
            seg_at(local_ms(2025, 6, 10, 14, 0, 0), None),
        ];
        assert_eq!(compute_today_ms(&segments, now), (30 + 60 + 60) * MIN);
        assert_eq!(compute_today_ms(&[], now), 0);
    }

    #[test]
    fn start_without_a_configured_project_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let app = create_app(dir.path(), &clock, None);
        app.tracker.init(false);
        assert_eq!(app.tracker.start(), Err(NotConfigured));
        assert_eq!(app.tracker.toggle(), Err(NotConfigured));
        assert_eq!(app.tracker.status().status, Status::Stopped);
    }

    #[test]
    fn changing_the_target_while_running_closes_the_segment_and_opens_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let app = create_app(dir.path(), &clock, Some(target(1, None, "")));
        app.tracker.init(false);
        app.tracker.start().unwrap();
        clock.advance(5 * MIN);
        app.set_target(Some(target(2, None, "")));
        app.tracker.change_target();
        let segs = app.state().segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].project_id, Some(JsonId::Num(1)));
        assert_eq!(segs[1].project_id, Some(JsonId::Num(2)));
        app.tracker.change_target(); // unchanged -> no-op
        assert_eq!(app.state().segments.len(), 2);
    }

    #[test]
    fn changing_the_target_to_none_stops_the_timer() {
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        clock.advance(MIN);
        app.set_target(None);
        app.tracker.change_target();
        assert_eq!(app.tracker.status().status, Status::Stopped);
        assert!(!app.state().user_wants_running);
    }

    #[test]
    fn old_synced_segments_older_than_7_days_are_pruned_unsynced_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let old = t0() - 10 * 24 * 3600 * 1000;
        let iso = |ms: Ms| crate::model::format_iso(ms);
        let json = serde_json::json!({
            "userWantsRunning": false,
            "current": null,
            "lastHeartbeat": null,
            "pauseReason": null,
            "queue": [],
            "segments": [
                { "localId": "a", "start": iso(old), "end": iso(old + MIN), "syncState": "synced" },
                { "localId": "b", "start": iso(old), "end": iso(old + MIN), "syncState": "failed" },
                { "localId": "c", "start": iso(old), "end": iso(old + MIN), "syncState": "skipped" },
                { "localId": "d", "start": iso(t0() - MIN), "end": iso(t0()), "syncState": "synced" }
            ]
        });
        std::fs::write(dir.path().join("state.json"), json.to_string()).unwrap();
        let app = create_app(dir.path(), &clock, Some(default_target()));
        app.tracker.init(false);
        let ids: Vec<String> = app.state().segments.into_iter().map(|s| s.local_id).collect();
        assert_eq!(ids, ["b", "d"]);
    }

    #[test]
    fn tick_sleep_gap_while_unlocked_closes_at_last_tick_and_opens_a_new_segment_now() {
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        clock.advance(1000);
        app.tracker.tick();
        let last_tick = clock.now_ms();
        clock.advance(3 * 3600 * 1000); // frozen for 3 h without a suspend event
        app.tracker.tick();
        let segs = app.state().segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].end, Some(last_tick));
        assert_eq!(segs[1].start, clock.now_ms());
        assert_eq!(app.tracker.status().status, Status::Running);
    }

    #[test]
    fn tick_sleep_gap_while_locked_pauses_without_a_new_segment() {
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        clock.advance(1000);
        app.tracker.tick();
        let last_tick = clock.now_ms();
        clock.advance(2 * 3600 * 1000);
        app.locked.store(true, Ordering::SeqCst);
        app.tracker.tick();
        let s = app.state();
        assert_eq!(s.segments.len(), 1);
        assert_eq!(s.segments[0].end, Some(last_tick));
        assert_eq!(s.pause_reason.as_deref(), Some("lock"));
        assert_eq!(app.tracker.status().status, Status::Paused);
        assert!(app.tracker.system_resume("unlock"));
        assert_eq!(app.state().segments.len(), 2);
    }

    #[test]
    fn tick_normal_one_second_ticks_and_the_first_tick_after_init_change_nothing() {
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        clock.advance(10 * MIN); // time passed before the first tick: no gap known
        app.tracker.tick();
        for _ in 0..5 {
            clock.advance(1000);
            app.tracker.tick();
        }
        assert_eq!(app.state().segments.len(), 1);
        assert_eq!(app.tracker.status().status, Status::Running);
    }

    #[test]
    fn tick_writes_a_heartbeat_every_30_seconds_only() {
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        let first = app.state().last_heartbeat;
        assert_eq!(first, Some(t0()));
        clock.advance(10_000);
        app.tracker.tick();
        assert_eq!(app.state().last_heartbeat, first, "less than HEARTBEAT_MS since the last write");
        clock.advance(HEARTBEAT_MS);
        app.tracker.tick();
        assert_eq!(app.state().last_heartbeat, Some(clock.now_ms()));
        assert_eq!(app.disk()["lastHeartbeat"], crate::model::format_iso(clock.now_ms()));
    }

    #[test]
    fn init_without_a_target_keeps_user_wants_running_and_stays_paused_then_system_resume_starts() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let app1 = create_app(dir.path(), &clock, Some(default_target()));
        app1.tracker.init(false);
        app1.tracker.start().unwrap();
        clock.advance(5 * MIN);
        app1.tracker.quit();

        clock.advance(MIN);
        let app2 = create_app(dir.path(), &clock, None);
        app2.tracker.init(false);
        assert!(app2.state().user_wants_running);
        assert_eq!(app2.state().pause_reason.as_deref(), Some("not-configured"));
        assert_eq!(app2.tracker.status().status, Status::Paused);
        assert!(!app2.tracker.system_resume("configured"));

        app2.set_target(Some(target(1, None, "")));
        assert!(app2.tracker.system_resume("configured"));
        assert_eq!(app2.tracker.status().status, Status::Running);
        assert_eq!(app2.state().pause_reason, None);
    }

    #[test]
    fn init_while_locked_waits_for_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let app1 = create_app(dir.path(), &clock, Some(default_target()));
        app1.tracker.init(false);
        app1.tracker.start().unwrap();
        app1.tracker.quit();
        let app2 = create_app(dir.path(), &clock, Some(default_target()));
        app2.tracker.init(true);
        assert_eq!(app2.tracker.status().status, Status::Paused);
        assert_eq!(app2.state().pause_reason.as_deref(), Some("lock"));
        assert!(app2.tracker.system_resume("unlock"));
    }

    #[test]
    fn system_pause_at_a_past_moment_closes_there_and_never_in_the_future() {
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        clock.advance(10 * MIN);
        assert!(app.tracker.system_pause("idle", Some(t0() + 4 * MIN)));
        assert_eq!(app.state().segments[0].end, Some(t0() + 4 * MIN));
        assert!(!app.tracker.system_pause("idle", None), "idempotent");
        assert!(app.tracker.system_resume("activity"));
        assert!(app.tracker.system_pause("lock", Some(clock.now_ms() + 99 * MIN)));
        assert_eq!(app.state().segments[1].end, Some(clock.now_ms()));
    }

    #[test]
    fn toggle_starts_and_stops() {
        let (_d, _clock, app) = setup();
        app.tracker.toggle().unwrap();
        assert_eq!(app.tracker.status().status, Status::Running);
        app.tracker.toggle().unwrap();
        assert_eq!(app.tracker.status().status, Status::Stopped);
    }

    #[test]
    fn callbacks_run_without_the_state_lock() {
        // the testkit callbacks lock the state; reaching this point means none of them deadlocked
        let (_d, clock, app) = setup();
        app.tracker.start().unwrap();
        clock.advance(MIN);
        app.tracker.system_pause("lock", None);
        app.tracker.system_resume("unlock");
        app.tracker.change_target();
        app.tracker.shutdown();
        assert!(app.changes.load(Ordering::SeqCst) >= 5);
    }

    #[test]
    fn new_uuid_is_unique_and_well_formed() {
        let a = new_uuid();
        assert_eq!(a.len(), 36);
        assert_ne!(a, new_uuid());
    }
}
