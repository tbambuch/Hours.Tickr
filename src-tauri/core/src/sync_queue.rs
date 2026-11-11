//! Persistent, sequential sync queue towards My Hours. No Tauri/OS imports.
//!
//! Operations live in `state.queue`:
//!   `Start { id, local_id, start, project_id, task_id, note }`
//!   `Stop { id, local_id, end }`
//!   `Delete { id, log_id }`   (a server log replaced by a newer one of the same day/target)
//!
//! Concurrency rules: the shared state lock is never held across an `.await` or while calling `on_status`;
//! after every `.await` the segment/operation is looked up again by id, because the tracker mutates the same
//! state at the same time. `process()` is single-flight: concurrent callers coalesce into the running run
//! (which loops again when something arrived meanwhile). The run itself executes as a spawned task on
//! `deps.handle`, so dropping a waiting future (e.g. `flush` timing out) never leaves the queue stuck.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::api::{parse_log_id, parse_logs, InsertLog, LogApi, StartLog};
use crate::clock::{local_date_str, Clock};
use crate::error::{classify_error, ApiError, ErrorKind};
use crate::logger::Logger;
use crate::model::{format_iso, DayLog, JsonId, Ms, Op, Segment, State};
use crate::store::SharedState;

pub const MIN_SEGMENT_MS: i64 = 60_000;
pub const BACKOFF_MS: [u64; 5] = [30_000, 60_000, 120_000, 240_000, 300_000];
const VERIFY_MISSES_BEFORE_RECREATE: u32 = 2;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct SyncStatus {
    /// ok | pending | offline | auth-error | error
    pub state: String,
    pub pending: usize,
    pub detail: String,
}

pub struct SyncQueueDeps {
    pub store: SharedState,
    pub api: Arc<dyn LogApi>,
    pub clock: Arc<dyn Clock>,
    pub log: Logger,
    pub on_status: Arc<dyn Fn(SyncStatus) + Send + Sync>,
    pub interval_ms: u64,
    pub handle: tokio::runtime::Handle,
    pub new_id: Arc<dyn Fn() -> String + Send + Sync>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Net {
    Ok,
    Offline,
    Error,
}

struct Inner {
    auth_error: bool,
    net: Net,
    last_detail: String,
    permanent_error: Option<String>,
    backoff_idx: usize,
    retry: Option<(u64, JoinHandle<()>)>,
    retry_seq: u64,
    periodic: Option<JoinHandle<()>>,
    running: bool,
    again: bool,
    run_seq: u64,
    verify_misses: u32,
    last_status: Option<SyncStatus>,
    interval_ms: u64,
}

pub struct SyncQueue {
    me: Weak<SyncQueue>,
    store: SharedState,
    api: Arc<dyn LogApi>,
    clock: Arc<dyn Clock>,
    log: Logger,
    on_status: Arc<dyn Fn(SyncStatus) + Send + Sync>,
    handle: tokio::runtime::Handle,
    new_id: Arc<dyn Fn() -> String + Send + Sync>,
    inner: Mutex<Inner>,
    /// Highest run id that has finished (waiters of `process()` wait for their run).
    done_tx: watch::Sender<u64>,
}

// ---- Pure helpers on the state (call them under the store lock, never across an await) ----

fn find_seg<'a>(st: &'a State, local_id: &str) -> Option<&'a Segment> {
    st.segments.iter().find(|s| s.local_id == local_id)
}

fn find_seg_mut<'a>(st: &'a mut State, local_id: &str) -> Option<&'a mut Segment> {
    st.segments.iter_mut().find(|s| s.local_id == local_id)
}

fn remove_ops(st: &mut State, pred: impl Fn(&Op) -> bool) {
    st.queue.retain(|o| !pred(o));
}

fn set_sync_state(st: &mut State, local_id: &str, value: &str) {
    if let Some(seg) = find_seg_mut(st, local_id) {
        seg.sync_state = value.to_string();
    }
}

fn set_remote_id(st: &mut State, local_id: &str, id: Option<JsonId>) {
    if let Some(seg) = find_seg_mut(st, local_id) {
        seg.remote_log_id = id.clone();
    }
    if let Some(cur) = st.current.as_mut() {
        if cur.local_id == local_id {
            cur.remote_log_id = id;
        }
    }
}

/// One server log per (local day, project, task, note).
fn group_key(seg: &Segment) -> String {
    let key = |v: &Option<JsonId>| v.as_ref().map(JsonId::key).unwrap_or_default();
    format!("{}|{}|{}|{}", local_date_str(seg.start), key(&seg.project_id), key(&seg.task_id), seg.note)
}

/// Time already worked in `seg`'s group before `seg` started, in ms (merge segments only).
fn prior_ms(st: &State, seg: &Segment) -> i64 {
    if !seg.merge {
        return 0;
    }
    let key = group_key(seg);
    st.segments
        .iter()
        .filter(|o| o.local_id != seg.local_id && o.merge && o.start < seg.start && group_key(o) == key)
        .filter_map(|o| o.end.map(|e| e - o.start))
        .sum()
}

/// `id` is now the group's server log; schedules deletion of the one it replaces.
fn adopt(st: &mut State, new_id: &dyn Fn() -> String, merge: bool, key: &str, id: Option<JsonId>) {
    let Some(id) = id else { return };
    if !merge {
        return;
    }
    let old = st.day_logs.get(key).and_then(|d| d.remote_log_id.clone());
    st.day_logs.insert(key.to_string(), DayLog { remote_log_id: Some(id.clone()) });
    if let Some(old) = old {
        if old.key() != id.key() {
            st.queue.insert(0, Op::Delete { id: new_id(), log_id: old });
        }
    }
}

fn prune_day_logs(st: &mut State, now: Ms) {
    let limit = local_date_str(now - 8 * 24 * 3600 * 1000);
    st.day_logs.retain(|k, _| k.split('|').next().unwrap_or("") >= limit.as_str());
}

/// What a handler needs to know about a segment, copied out so no lock is held while talking to the server.
struct SegSnap {
    start: Ms,
    end: Option<Ms>,
    project_id: Option<JsonId>,
    task_id: Option<JsonId>,
    note: String,
    merge: bool,
    key: String,
    prior: i64,
    shift_ms: Option<i64>,
    remote: Option<JsonId>,
}

fn snapshot(st: &State, local_id: &str) -> Option<SegSnap> {
    let seg = find_seg(st, local_id)?;
    Some(SegSnap {
        start: seg.start,
        end: seg.end,
        project_id: seg.project_id.clone(),
        task_id: seg.task_id.clone(),
        note: seg.note.clone(),
        merge: seg.merge,
        key: group_key(seg),
        prior: prior_ms(st, seg),
        shift_ms: seg.shift_ms,
        remote: seg.remote_log_id.clone(),
    })
}

fn secs(ms: i64) -> i64 {
    (ms as f64 / 1000.0).round() as i64
}

struct RunGuard<'a> {
    q: &'a SyncQueue,
    id: u64,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        {
            let mut i = self.q.inner.lock();
            if i.running && i.run_seq == self.id {
                i.running = false; // only reached when the run was torn down abnormally
            }
        }
        let id = self.id;
        self.q.done_tx.send_modify(|v| {
            if *v < id {
                *v = id;
            }
        });
    }
}

impl SyncQueue {
    pub fn new(deps: SyncQueueDeps) -> Arc<SyncQueue> {
        let (done_tx, _rx) = watch::channel(0u64);
        Arc::new_cyclic(|me| SyncQueue {
            me: me.clone(),
            store: deps.store,
            api: deps.api,
            clock: deps.clock,
            log: deps.log,
            on_status: deps.on_status,
            handle: deps.handle,
            new_id: deps.new_id,
            inner: Mutex::new(Inner {
                auth_error: false,
                net: Net::Ok,
                last_detail: String::new(),
                permanent_error: None,
                backoff_idx: 0,
                retry: None,
                retry_seq: 0,
                periodic: None,
                running: false,
                again: false,
                run_seq: 0,
                verify_misses: 0,
                last_status: None,
                interval_ms: deps.interval_ms,
            }),
            done_tx,
        })
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.store.lock().state)
    }

    fn save(&self) {
        self.store.lock().save();
    }

    pub fn status(&self) -> SyncStatus {
        let pending = self.store.lock().state.queue.len();
        let i = self.inner.lock();
        let (state, detail) = if i.auth_error {
            ("auth-error", String::new())
        } else if i.net == Net::Offline {
            ("offline", String::new())
        } else if i.net == Net::Error {
            ("error", i.last_detail.clone())
        } else if let Some(p) = &i.permanent_error {
            ("error", p.clone())
        } else if pending > 0 {
            ("pending", String::new())
        } else {
            ("ok", String::new())
        };
        SyncStatus { state: state.to_string(), pending, detail }
    }

    /// Publishes the status when it changed. The callback runs without any lock held.
    fn emit(&self) {
        let s = self.status();
        let changed = {
            let mut i = self.inner.lock();
            if i.last_status.as_ref() == Some(&s) {
                false
            } else {
                i.last_status = Some(s.clone());
                true
            }
        };
        if changed {
            let cb = self.on_status.clone();
            let _ = catch_unwind(AssertUnwindSafe(|| cb(s))); // ignore UI errors
        }
    }

    // ---- handlers ------------------------------------------------------------------------------

    async fn insert_segment(&self, snap: &SegSnap, start: Ms, end: Ms) -> Result<serde_json::Value, ApiError> {
        self.api
            .insert_log(InsertLog {
                project_id: snap.project_id.clone(),
                task_id: snap.task_id.clone(),
                note: snap.note.clone(),
                date: local_date_str(snap.start),
                start: format_iso(start),
                end: format_iso(end),
            })
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_start(
        &self,
        op_id: &str,
        local_id: &str,
        start: Ms,
        project_id: &Option<JsonId>,
        task_id: &Option<JsonId>,
        note: &str,
    ) -> Result<(), ApiError> {
        let snap = { snapshot(&self.store.lock().state, local_id) };
        let Some(snap) = snap else {
            self.with_state(|st| remove_ops(st, |o| o.id() == op_id));
            return Ok(());
        };
        let shift = snap.prior;

        if let Some(end) = snap.end {
            // Segment was closed before its start ever reached the server: collapse start+stop.
            if end - snap.start < MIN_SEGMENT_MS {
                self.log.info(format!("sync: segment {local_id} shorter than 60 s, dropped"));
                self.with_state(|st| {
                    set_sync_state(st, local_id, "skipped");
                    remove_ops(st, |o| o.local_id() == Some(local_id));
                });
                return Ok(());
            }
            let res = self.insert_segment(&snap, snap.start - shift, end).await?;
            self.log.info(format!(
                "sync: insertlog {} .. {} (shifted by {} s)",
                format_iso(snap.start),
                format_iso(end),
                secs(shift)
            ));
            self.with_state(|st| {
                set_sync_state(st, local_id, "synced");
                remove_ops(st, |o| o.local_id() == Some(local_id));
                adopt(st, &*self.new_id, snap.merge, &snap.key, parse_log_id(&res));
            });
            return Ok(());
        }

        let res = self
            .api
            .start_new_log(StartLog {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
                note: note.to_string(),
                date: local_date_str(start),
                start: format_iso(start - shift),
            })
            .await?;
        let id = parse_log_id(&res);
        let now = self.clock.now_ms();
        self.with_state(|st| {
            remove_ops(st, |o| o.id() == op_id);
            if let Some(seg) = find_seg_mut(st, local_id) {
                seg.shift_ms = Some(shift);
            }
            if let Some(id) = &id {
                set_remote_id(st, local_id, Some(id.clone()));
                adopt(st, &*self.new_id, snap.merge, &snap.key, Some(id.clone()));
            }
            prune_day_logs(st, now);
        });
        match id {
            None => self.log.warn("sync: startNewLog succeeded but no log id could be parsed from the response"),
            Some(id) => self.log.info(format!("sync: startNewLog ok, remote id {id} (shifted by {} s)", secs(shift))),
        }
        Ok(())
    }

    async fn handle_stop(&self, op_id: &str, local_id: &str, end: Ms) -> Result<(), ApiError> {
        let snap = { snapshot(&self.store.lock().state, local_id) };
        let Some(snap) = snap else {
            self.with_state(|st| remove_ops(st, |o| o.id() == op_id));
            return Ok(());
        };
        let dur = end - snap.start;
        let shift = snap.shift_ms.unwrap_or(snap.prior);

        if let Some(rid) = snap.remote.clone() {
            match self.api.stop_timer(&rid, &format_iso(end)).await {
                Ok(()) => self.log.info(format!("sync: stoptimer ok, remote id {rid}")),
                Err(err) if err.status == Some(404) => {
                    // The log vanished on the server (deleted): store the group's time as a finished log instead.
                    self.log.warn("sync: stoptimer 404, falling back to insertlog");
                    if dur >= MIN_SEGMENT_MS {
                        let res = self.insert_segment(&snap, snap.start - shift, end).await?;
                        self.with_state(|st| adopt(st, &*self.new_id, snap.merge, &snap.key, parse_log_id(&res)));
                    }
                }
                Err(err) => return Err(err),
            }
            self.with_state(|st| set_sync_state(st, local_id, "synced"));
        } else if dur < MIN_SEGMENT_MS {
            self.with_state(|st| set_sync_state(st, local_id, "skipped"));
            self.log.info(format!("sync: segment {local_id} shorter than 60 s, dropped"));
        } else {
            let res = self.insert_segment(&snap, snap.start - shift, end).await?;
            self.log.info(format!("sync: insertlog {} .. {}", format_iso(snap.start), format_iso(end)));
            self.with_state(|st| {
                set_sync_state(st, local_id, "synced");
                remove_ops(st, |o| o.id() == op_id);
                adopt(st, &*self.new_id, snap.merge, &snap.key, parse_log_id(&res));
            });
            return Ok(());
        }
        self.with_state(|st| remove_ops(st, |o| o.id() == op_id));
        Ok(())
    }

    /// Deletes a replaced group log. Already gone or permanently refused is fine: at worst a duplicate stays.
    async fn handle_delete(&self, op_id: &str, log_id: &JsonId) -> Result<(), ApiError> {
        match self.api.delete_log(log_id).await {
            Ok(()) => self.log.info(format!("sync: deleted replaced log {log_id}")),
            Err(err) if err.status == Some(404) || classify_error(&err) == ErrorKind::Permanent => {
                self.log.warn(format!("sync: could not delete replaced log {log_id} ({}), ignored", err.message));
            }
            Err(err) => return Err(err),
        }
        self.with_state(|st| remove_ops(st, |o| o.id() == op_id));
        Ok(())
    }

    fn fail_operation(&self, op: &Op, err: &ApiError) {
        let detail = if err.detail.is_empty() { err.message.clone() } else { format!("{} - {}", err.message, err.detail) };
        let kind = match op {
            Op::Start { .. } => "start",
            Op::Stop { .. } => "stop",
            Op::Delete { .. } => "delete",
        };
        self.log.error(format!("sync: operation {kind} failed permanently: {detail}"));
        self.with_state(|st| match op.local_id() {
            Some(local_id) => {
                set_sync_state(st, local_id, "failed");
                remove_ops(st, |o| o.local_id() == Some(local_id));
            }
            None => remove_ops(st, |o| o.id() == op.id()),
        });
        self.inner.lock().permanent_error = Some(err.message.clone());
    }

    // ---- retry / run loop ----------------------------------------------------------------------

    fn schedule_retry(&self) {
        let delay = {
            let mut i = self.inner.lock();
            if i.retry.is_some() {
                return;
            }
            let delay = BACKOFF_MS[i.backoff_idx.min(BACKOFF_MS.len() - 1)];
            i.backoff_idx += 1;
            i.retry_seq += 1;
            let token = i.retry_seq;
            let weak = self.me.clone();
            let task = self.handle.spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let Some(q) = weak.upgrade() else { return };
                {
                    let mut i = q.inner.lock();
                    match &i.retry {
                        Some((t, _)) if *t == token => i.retry = None,
                        _ => return, // cleared or replaced meanwhile
                    }
                }
                q.process().await;
            });
            i.retry = Some((token, task));
            delay
        };
        self.log.info(format!("sync: retry in {} s", delay / 1000));
    }

    fn clear_retry(&self) {
        if let Some((_, task)) = self.inner.lock().retry.take() {
            task.abort();
        }
    }

    async fn run_once(&self) {
        if self.inner.lock().auth_error {
            self.emit();
            return;
        }
        loop {
            let op = self.store.lock().state.queue.first().cloned();
            let Some(op) = op else { break };
            let res = match &op {
                Op::Start { id, local_id, start, project_id, task_id, note } => {
                    self.handle_start(id, local_id, *start, project_id, task_id, note).await
                }
                Op::Stop { id, local_id, end } => self.handle_stop(id, local_id, *end).await,
                Op::Delete { id, log_id } => self.handle_delete(id, log_id).await,
            };
            match res {
                Ok(()) => {
                    self.save();
                    self.emit();
                }
                Err(err) => match classify_error(&err) {
                    ErrorKind::Auth => {
                        self.inner.lock().auth_error = true;
                        self.log.error(format!("sync: authentication failed ({}), sync stopped until the key changes", err.status.unwrap_or(0)));
                        self.emit();
                        return;
                    }
                    ErrorKind::Temporary => {
                        {
                            let mut i = self.inner.lock();
                            i.net = if err.network { Net::Offline } else { Net::Error };
                            i.last_detail = err.message.clone();
                        }
                        self.log.warn(format!("sync: temporary failure: {}", err.message));
                        self.schedule_retry();
                        self.emit();
                        return;
                    }
                    ErrorKind::Permanent => {
                        self.fail_operation(&op, &err);
                        self.save();
                        self.emit();
                    }
                },
            }
        }
        {
            let mut i = self.inner.lock();
            i.net = Net::Ok;
            i.backoff_idx = 0;
        }
        self.clear_retry();
        self.emit();
    }

    async fn run_loop(self: Arc<Self>, id: u64) {
        let _guard = RunGuard { q: &self, id };
        loop {
            self.inner.lock().again = false;
            self.run_once().await;
            let mut i = self.inner.lock();
            if !i.again {
                i.running = false;
                break;
            }
        }
    }

    /// Makes sure a run is going to happen after this call; returns the id of the run that will cover it.
    fn begin(&self) -> u64 {
        let id = {
            let mut i = self.inner.lock();
            if i.running {
                i.again = true;
                return i.run_seq;
            }
            i.running = true;
            i.run_seq += 1;
            i.run_seq
        };
        match self.me.upgrade() {
            Some(this) => {
                self.handle.spawn(async move { this.run_loop(id).await });
            }
            None => {
                // Not reachable (`new` returns an Arc); never leave the flag set.
                self.inner.lock().running = false;
                self.done_tx.send_modify(|v| *v = (*v).max(id));
            }
        }
        id
    }

    /// Schedules a run on the runtime and returns immediately. Callable from any thread.
    pub fn kick(&self) {
        self.begin();
    }

    /// Processes the queue now (single-flight; concurrent calls are coalesced).
    pub async fn process(&self) {
        let id = self.begin();
        let mut rx = self.done_tx.subscribe();
        let _ = rx.wait_for(|done| *done >= id).await;
    }

    // ---- verification of the running log -------------------------------------------------------

    fn verify_target(&self) -> Option<(String, JsonId)> {
        let g = self.store.lock();
        let s = &g.state;
        if !s.queue.is_empty() {
            return None;
        }
        let cur = s.current.as_ref()?;
        let rid = cur.remote_log_id.clone()?;
        let seg = find_seg(s, &cur.local_id)?;
        if seg.recreated {
            return None;
        }
        Some((cur.local_id.clone(), rid))
    }

    fn still_current(st: &State, local_id: &str, rid: &JsonId) -> bool {
        st.queue.is_empty()
            && st.current.as_ref().is_some_and(|c| c.local_id == local_id && c.remote_log_id.as_ref().map(JsonId::key) == Some(rid.key()))
    }

    /// Checks that the running segment's log still exists on the server; recreates it if not.
    pub async fn verify_current(&self) {
        if self.inner.lock().auth_error {
            return;
        }
        let Some((local_id, rid)) = self.verify_target() else { return };
        let date = local_date_str(self.clock.now_ms());
        let raw = match self.api.get_logs(&date).await {
            Ok(v) => v,
            Err(err) => {
                if classify_error(&err) == ErrorKind::Auth {
                    self.inner.lock().auth_error = true;
                    self.emit();
                }
                self.log.warn(format!("sync: log verification failed (ignored): {}", err.message));
                return;
            }
        };
        let Some(logs) = parse_logs(&raw) else { return }; // unknown shape: do nothing rather than create duplicates
        // The tracker may have moved on during the request (stopped, new segment, new operations).
        if !Self::still_current(&self.store.lock().state, &local_id, &rid) {
            return;
        }
        let wanted = rid.key();
        let found = logs.iter().any(|l| {
            let id = l.get("id").filter(|v| !v.is_null()).or_else(|| l.get("logId"));
            match id {
                Some(serde_json::Value::String(s)) => *s == wanted,
                Some(serde_json::Value::Number(n)) => n.to_string() == wanted,
                _ => false,
            }
        });
        if found {
            self.inner.lock().verify_misses = 0;
            return;
        }
        let misses = {
            let mut i = self.inner.lock();
            i.verify_misses += 1;
            i.verify_misses
        };
        self.log.warn(format!("sync: running log {rid} not found on server ({misses}x)"));
        if misses < VERIFY_MISSES_BEFORE_RECREATE {
            return;
        }
        self.inner.lock().verify_misses = 0;
        let queued = self.with_state(|st| {
            if !Self::still_current(st, &local_id, &rid) {
                return false;
            }
            let Some(seg) = find_seg_mut(st, &local_id) else { return false };
            seg.recreated = true;
            let op = Op::Start {
                id: (self.new_id)(),
                local_id: seg.local_id.clone(),
                start: seg.start,
                project_id: seg.project_id.clone(),
                task_id: seg.task_id.clone(),
                note: seg.note.clone(),
            };
            set_remote_id(st, &local_id, None);
            st.queue.push(op);
            true
        });
        if queued {
            self.save();
            self.log.warn("sync: recreating the running log on the server");
            self.process().await;
        }
    }

    pub async fn periodic(&self) {
        self.process().await;
        self.verify_current().await;
    }

    // ---- timers and control --------------------------------------------------------------------

    fn start_periodic_locked(&self, i: &mut Inner) {
        if let Some(task) = i.periodic.take() {
            task.abort();
        }
        let period = Duration::from_millis(i.interval_ms.max(1));
        let weak = self.me.clone();
        let first_tick = tokio::time::Instant::now() + period; // fixed now, not when the task is first polled
        i.periodic = Some(self.handle.spawn(async move {
            let mut ticker = tokio::time::interval_at(first_tick, period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let Some(q) = weak.upgrade() else { break };
                q.periodic().await;
            }
        }));
    }

    /// Starts the periodic timer (process + verify every `interval_ms`).
    pub fn start(&self) {
        let mut i = self.inner.lock();
        self.start_periodic_locked(&mut i);
    }

    pub fn stop(&self) {
        {
            let mut i = self.inner.lock();
            if let Some(task) = i.periodic.take() {
                task.abort();
            }
        }
        self.clear_retry();
    }

    pub fn set_interval_ms(&self, ms: u64) {
        let mut i = self.inner.lock();
        i.interval_ms = ms;
        if i.periodic.is_some() {
            self.start_periodic_locked(&mut i);
        }
    }

    /// Call when the API key / login changed: clears the auth-error block and retries.
    pub async fn reset_auth(&self) {
        {
            let mut i = self.inner.lock();
            i.auth_error = false;
            i.net = Net::Ok;
            i.backoff_idx = 0;
        }
        self.clear_retry();
        self.emit();
        self.process().await;
    }

    /// "Sync now" from the UI: also clears a sticky permanent-error warning.
    pub async fn sync_now(&self) {
        self.inner.lock().permanent_error = None;
        self.periodic().await;
    }

    /// Best-effort wait for the queue to drain, at most `timeout_ms`.
    pub async fn flush(&self, timeout_ms: u64) {
        let _ = tokio::time::timeout(Duration::from_millis(timeout_ms), self.process()).await;
    }

    #[cfg(test)]
    fn has_retry_timer(&self) -> bool {
        self.inner.lock().retry.is_some()
    }

    #[cfg(test)]
    fn has_periodic_timer(&self) -> bool {
        self.inner.lock().periodic.is_some()
    }
}

impl Drop for SyncQueue {
    fn drop(&mut self) {
        let i = self.inner.get_mut();
        if let Some((_, task)) = i.retry.take() {
            task.abort();
        }
        if let Some(task) = i.periodic.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {

    // `Driver` stands in for the tracker: it pushes segments and operations into the state exactly as the tracker's
    // `open_segment` / `close_segment` / commit do (and kicks the queue like `on_queue_change`).

    use super::*;
    use crate::clock::{local_ms, local_midnight, FakeClock};
    use crate::model::{Current, Target};
    use crate::store::Store;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::runtime::Handle;

    const MIN: i64 = 60_000;
    const T0_LOCAL: (i32, u32, u32, u32, u32, u32) = (2025, 6, 10, 9, 0, 0);

    fn t0() -> Ms {
        let (y, mo, d, h, mi, s) = T0_LOCAL;
        local_ms(y, mo, d, h, mi, s)
    }

    fn iso(ms: Ms) -> String {
        format_iso(ms)
    }

    // ---- mock API (like createMockApi in test/helpers.js) ----

    #[derive(Clone, Debug)]
    enum Call {
        Start(StartLog),
        Stop { log_id: JsonId, time: String },
        Insert(InsertLog),
        Delete(JsonId),
        GetLogs(String),
    }

    impl Call {
        fn kind(&self) -> &'static str {
            match self {
                Call::Start(_) => "startNewLog",
                Call::Stop { .. } => "stoptimer",
                Call::Insert(_) => "insertlog",
                Call::Delete(_) => "deleteLog",
                Call::GetLogs(_) => "getLogs",
            }
        }
    }

    #[derive(Default)]
    struct MockSt {
        offline: bool,
        fail_status: Option<u16>,
        fail_ops: HashMap<&'static str, u16>,
        calls: Vec<Call>,
        attempts: usize,
        next_id: i64,
        logs: Vec<i64>,
        logs_raw: Option<Value>,
        delay_ms: u64,
        in_flight: usize,
        max_in_flight: usize,
    }

    struct MockApi(Mutex<MockSt>);

    impl MockApi {
        fn new() -> Arc<MockApi> {
            Arc::new(MockApi(Mutex::new(MockSt { next_id: 1000, ..Default::default() })))
        }
        fn set_online(&self, on: bool) {
            self.0.lock().offline = !on;
        }
        fn set_fail_status(&self, s: Option<u16>) {
            self.0.lock().fail_status = s;
        }
        fn set_fail_op(&self, op: &'static str, s: Option<u16>) {
            let mut g = self.0.lock();
            match s {
                Some(s) => g.fail_ops.insert(op, s),
                None => g.fail_ops.remove(op),
            };
        }
        fn set_delay(&self, ms: u64) {
            self.0.lock().delay_ms = ms;
        }
        fn set_logs_raw(&self, v: Option<Value>) {
            self.0.lock().logs_raw = v;
        }
        fn clear_logs(&self) {
            self.0.lock().logs.clear();
        }
        fn log_ids(&self) -> Vec<i64> {
            self.0.lock().logs.clone()
        }
        fn attempts(&self) -> usize {
            self.0.lock().attempts
        }
        fn in_flight(&self) -> usize {
            self.0.lock().in_flight
        }
        fn max_in_flight(&self) -> usize {
            self.0.lock().max_in_flight
        }
        fn calls(&self) -> Vec<Call> {
            self.0.lock().calls.clone()
        }
        fn kinds(&self) -> Vec<&'static str> {
            self.calls().iter().map(Call::kind).collect()
        }
        fn count(&self, kind: &str) -> usize {
            self.calls().iter().filter(|c| c.kind() == kind).count()
        }
        fn starts(&self) -> Vec<StartLog> {
            self.calls().into_iter().filter_map(|c| if let Call::Start(s) = c { Some(s) } else { None }).collect()
        }
        fn stops(&self) -> Vec<(JsonId, String)> {
            self.calls().into_iter().filter_map(|c| if let Call::Stop { log_id, time } = c { Some((log_id, time)) } else { None }).collect()
        }
        fn deletes(&self) -> Vec<JsonId> {
            self.calls().into_iter().filter_map(|c| if let Call::Delete(id) = c { Some(id) } else { None }).collect()
        }
        fn inserts(&self) -> Vec<InsertLog> {
            self.calls().into_iter().filter_map(|c| if let Call::Insert(i) = c { Some(i) } else { None }).collect()
        }

        /// Counts the attempt; the outcome is decided when the request is sent (before the latency).
        async fn enter(&self, op: &'static str) -> Result<(), ApiError> {
            let (delay, result) = {
                let mut s = self.0.lock();
                s.attempts += 1;
                s.in_flight += 1;
                s.max_in_flight = s.max_in_flight.max(s.in_flight);
                let failure = s.fail_status.or_else(|| s.fail_ops.get(op).copied());
                let result = if s.offline {
                    Err(ApiError::network("offline"))
                } else if let Some(st) = failure {
                    Err(ApiError::http(st, format!("HTTP {st}"), ""))
                } else {
                    Ok(())
                };
                (s.delay_ms, result)
            };
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            self.0.lock().in_flight -= 1;
            result
        }
    }

    #[async_trait::async_trait]
    impl LogApi for MockApi {
        async fn get_logs(&self, date: &str) -> Result<Value, ApiError> {
            self.enter("getLogs").await?;
            let mut s = self.0.lock();
            s.calls.push(Call::GetLogs(date.to_string()));
            Ok(s.logs_raw.clone().unwrap_or_else(|| Value::Array(s.logs.iter().map(|id| json!({ "id": id })).collect())))
        }
        async fn start_new_log(&self, req: StartLog) -> Result<Value, ApiError> {
            self.enter("startNewLog").await?;
            let mut s = self.0.lock();
            let id = s.next_id;
            s.next_id += 1;
            s.calls.push(Call::Start(req));
            s.logs.push(id);
            Ok(json!({ "id": id }))
        }
        async fn stop_timer(&self, log_id: &JsonId, time: &str) -> Result<(), ApiError> {
            self.enter("stoptimer").await?;
            self.0.lock().calls.push(Call::Stop { log_id: log_id.clone(), time: time.to_string() });
            Ok(())
        }
        async fn insert_log(&self, req: InsertLog) -> Result<Value, ApiError> {
            self.enter("insertlog").await?;
            let mut s = self.0.lock();
            let id = s.next_id;
            s.next_id += 1;
            s.calls.push(Call::Insert(req));
            s.logs.push(id);
            Ok(json!({ "id": id }))
        }
        async fn delete_log(&self, log_id: &JsonId) -> Result<(), ApiError> {
            self.enter("deleteLog").await?;
            let mut s = self.0.lock();
            s.calls.push(Call::Delete(log_id.clone()));
            s.logs.retain(|l| JsonId::Num(*l).key() != log_id.key());
            Ok(())
        }
    }

    // ---- stand-in for the tracker: writes segments/operations exactly like tracker.js ----

    struct Driver {
        store: SharedState,
        clock: Arc<FakeClock>,
        queue: Arc<SyncQueue>,
        target: Mutex<Target>,
    }

    fn uid() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn open_segment(st: &mut State, start: Ms, t: &Target) -> String {
        let local_id = uid();
        st.segments.push(Segment {
            local_id: local_id.clone(),
            start,
            end: None,
            remote_log_id: None,
            sync_state: "pending".into(),
            project_id: t.project_id.clone(),
            task_id: t.task_id.clone(),
            note: t.note.clone(),
            merge: true,
            shift_ms: None,
            recreated: false,
        });
        st.current = Some(Current { local_id: local_id.clone(), start, remote_log_id: None });
        st.pause_reason = None;
        st.last_heartbeat = Some(start);
        st.queue.push(Op::Start {
            id: uid(),
            local_id: local_id.clone(),
            start,
            project_id: t.project_id.clone(),
            task_id: t.task_id.clone(),
            note: t.note.clone(),
        });
        local_id
    }

    fn close_segment(st: &mut State, end_ms: Ms) {
        let Some(cur) = st.current.take() else { return };
        let end = end_ms.max(cur.start);
        if let Some(seg) = find_seg_mut(st, &cur.local_id) {
            seg.end = Some(end);
        }
        st.queue.push(Op::Stop { id: uid(), local_id: cur.local_id, end });
    }

    impl Driver {
        fn target(&self) -> Target {
            self.target.lock().clone()
        }
        fn set_task(&self, task: i64) {
            self.target.lock().task_id = Some(JsonId::Num(task));
        }
        fn commit(&self) {
            self.store.lock().save();
            self.queue.kick(); // onQueueChange
        }
        fn start(&self) {
            let t = self.target();
            let now = self.clock.now_ms();
            self.with(|s| {
                s.user_wants_running = true;
                if s.current.is_none() {
                    open_segment(s, now, &t);
                }
            });
        }
        fn stop(&self) {
            let now = self.clock.now_ms();
            self.with(|s| {
                s.user_wants_running = false;
                s.pause_reason = None;
                close_segment(s, now);
            });
        }
        fn system_pause(&self, reason: &str) {
            let now = self.clock.now_ms();
            self.with(|s| {
                if s.current.is_none() {
                    return;
                }
                close_segment(s, now);
                s.pause_reason = Some(reason.to_string());
            });
        }
        fn system_resume(&self) {
            let t = self.target();
            let now = self.clock.now_ms();
            self.with(|s| {
                if s.user_wants_running && s.current.is_none() {
                    open_segment(s, now, &t);
                }
            });
        }
        /// Crash recovery + resume after application start (`tracker.init`).
        fn init(&self) {
            let t = self.target();
            let now = self.clock.now_ms();
            self.with(|s| {
                if let Some(cur) = s.current.clone() {
                    let end = s.last_heartbeat.unwrap_or(cur.start).min(now);
                    close_segment(s, end);
                }
                if s.user_wants_running {
                    open_segment(s, now, &t);
                }
            });
        }
        /// The midnight loop of `tracker.tick`.
        fn tick_midnight(&self) {
            let now = self.clock.now_ms();
            self.with(|s| {
                while let Some(cur) = s.current.clone() {
                    let boundary = local_midnight(cur.start, 1);
                    if boundary > now {
                        break;
                    }
                    let t = find_seg(s, &cur.local_id).map(|g| g.target()).unwrap_or_default();
                    close_segment(s, boundary - 1);
                    open_segment(s, boundary, &t);
                }
            });
        }
        fn heartbeat(&self) {
            let now = self.clock.now_ms();
            let mut g = self.store.lock();
            g.state.last_heartbeat = Some(now);
            g.save();
        }
        fn with(&self, f: impl FnOnce(&mut State)) {
            f(&mut self.store.lock().state);
            self.commit();
        }
    }

    struct App {
        store: SharedState,
        queue: Arc<SyncQueue>,
        api: Arc<MockApi>,
        drv: Driver,
        statuses: Arc<Mutex<Vec<String>>>,
    }

    impl App {
        fn state(&self) -> State {
            self.store.lock().state.clone()
        }
        fn status(&self) -> SyncStatus {
            self.queue.status()
        }
        fn push_op(&self, op: Op) {
            self.store.lock().state.queue.push(op);
        }
    }

    fn deps(store: &SharedState, api: &Arc<MockApi>, clock: &Arc<FakeClock>, on_status: Arc<dyn Fn(SyncStatus) + Send + Sync>) -> SyncQueueDeps {
        SyncQueueDeps {
            store: store.clone(),
            api: api.clone(),
            clock: clock.clone(),
            log: Logger::noop(),
            on_status,
            interval_ms: 300_000,
            handle: Handle::current(),
            new_id: Arc::new(uid),
        }
    }

    /// Wires store + driver + queue like main does. `dir` persists across "restarts".
    fn build_with(dir: &Path, clock: &Arc<FakeClock>, api: Arc<MockApi>) -> App {
        let mut st: Store<State> = Store::new(dir.join("state.json"), Logger::noop());
        st.load();
        let store = st.into_shared();
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let rec = statuses.clone();
        let queue = SyncQueue::new(deps(&store, &api, clock, Arc::new(move |s| rec.lock().push(s.state))));
        let drv = Driver {
            store: store.clone(),
            clock: clock.clone(),
            queue: queue.clone(),
            target: Mutex::new(Target { project_id: Some(JsonId::Num(7)), task_id: Some(JsonId::Num(3)), note: "work".into() }),
        };
        App { store, queue, api, drv, statuses }
    }

    fn build(dir: &tempfile::TempDir, clock: &Arc<FakeClock>) -> App {
        build_with(dir.path(), clock, MockApi::new())
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    async fn advance(ms: u64) {
        tokio::time::advance(Duration::from_millis(ms)).await;
        settle().await;
    }

    async fn wait_in_flight(api: &MockApi, n: usize) {
        for _ in 0..200 {
            if api.in_flight() == n {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("api never reached {n} requests in flight");
    }

    fn seg(local_id: &str, start: Ms, end: Option<Ms>, project: i64, task: Option<i64>, note: &str, merge: bool) -> Segment {
        Segment {
            local_id: local_id.into(),
            start,
            end,
            remote_log_id: None,
            sync_state: "pending".into(),
            project_id: Some(JsonId::Num(project)),
            task_id: task.map(JsonId::Num),
            note: note.into(),
            merge,
            shift_ms: None,
            recreated: false,
        }
    }

    fn delete_op(id: &str, log: i64) -> Op {
        Op::Delete { id: id.into(), log_id: JsonId::Num(log) }
    }

    // ================= test/syncQueue.test.js =================

    #[tokio::test(start_paused = true)]
    async fn online_start_sends_start_new_log_stop_sends_stoptimer() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        let start = &a.api.starts()[0];
        assert_eq!(a.api.kinds()[0], "startNewLog");
        assert_eq!(start.project_id, Some(JsonId::Num(7)));
        assert_eq!(start.task_id, Some(JsonId::Num(3)));
        assert_eq!(start.date, "2025-06-10");
        assert_eq!(start.start, iso(t0()));
        assert_eq!(start.note, "work");
        assert_eq!(a.state().current.unwrap().remote_log_id, Some(JsonId::Num(1000)));

        clock.advance(15 * MIN);
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.api.kinds()[1], "stoptimer");
        let (log_id, time) = &a.api.stops()[0];
        assert_eq!(*log_id, JsonId::Num(1000));
        assert_eq!(*time, iso(t0() + 15 * MIN));
        assert_eq!(a.status().state, "ok");
    }

    #[tokio::test(start_paused = true)]
    async fn offline_start_and_stop_become_one_insertlog_after_the_network_returns() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_online(false);
        a.drv.start();
        a.queue.process().await;
        assert_eq!(a.status().state, "offline");

        clock.advance(25 * MIN);
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.status().state, "offline");
        assert_eq!(a.status().pending, 2);
        assert!(a.api.calls().is_empty());

        a.api.set_online(true);
        a.queue.process().await;
        assert_eq!(a.api.kinds(), vec!["insertlog"]);
        let ins = &a.api.inserts()[0];
        assert_eq!(ins.start, iso(t0()));
        assert_eq!(ins.end, iso(t0() + 25 * MIN));
        assert_eq!(ins.date, "2025-06-10");
        assert!(a.state().queue.is_empty());
        assert_eq!(a.status().state, "ok");
        assert_eq!(a.state().segments[0].sync_state, "synced");
    }

    #[tokio::test(start_paused = true)]
    async fn offline_queue_survives_an_app_restart_and_is_flushed_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let app1 = build(&dir, &clock);
        app1.api.set_online(false);
        app1.drv.start();
        clock.advance(10 * MIN);
        app1.drv.stop();
        app1.queue.process().await;
        assert_eq!(app1.status().pending, 2);

        let app2 = build(&dir, &clock);
        assert_eq!(app2.state().queue.len(), 2, "queue was read back from disk");
        app2.queue.process().await;
        assert_eq!(app2.api.kinds(), vec!["insertlog"]);
    }

    #[tokio::test(start_paused = true)]
    async fn segment_shorter_than_60_s_is_never_sent_when_its_start_was_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_online(false);
        a.drv.start();
        a.queue.process().await;
        clock.advance(45 * 1000);
        a.drv.stop();
        a.api.set_online(true);
        a.queue.process().await;
        assert!(a.api.calls().is_empty());
        assert!(a.state().queue.is_empty());
        assert_eq!(a.state().segments[0].sync_state, "skipped");
        assert_eq!(a.status().state, "ok");
    }

    #[tokio::test(start_paused = true)]
    async fn segment_of_exactly_60_s_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_online(false);
        a.drv.start();
        a.queue.process().await;
        clock.advance(60 * 1000);
        a.drv.stop();
        a.api.set_online(true);
        a.queue.process().await;
        assert_eq!(a.api.kinds(), vec!["insertlog"]);
    }

    #[tokio::test(start_paused = true)]
    async fn short_segment_whose_start_was_already_sent_still_gets_a_normal_stop() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        clock.advance(20 * 1000);
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.api.kinds(), vec!["startNewLog", "stoptimer"]);
    }

    #[tokio::test(start_paused = true)]
    async fn unauthorized_is_auth_error_and_no_further_attempts_until_reset() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_fail_status(Some(401));
        a.drv.start();
        a.queue.process().await;
        assert_eq!(a.status().state, "auth-error");
        assert_eq!(a.api.attempts(), 1);

        clock.advance(10 * MIN);
        a.drv.stop();
        a.queue.process().await;
        a.queue.process().await;
        a.queue.periodic().await;
        assert_eq!(a.api.attempts(), 1, "no retries while auth-error");
        assert!(!a.queue.has_retry_timer(), "no retry timer scheduled");
        assert_eq!(a.status().state, "auth-error");
        assert_eq!(a.state().queue.len(), 2, "operations are kept");

        // key replaced -> sync resumes
        a.api.set_fail_status(None);
        a.queue.reset_auth().await;
        assert_eq!(a.status().state, "ok");
        assert_eq!(a.api.kinds(), vec!["insertlog"]);
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_is_retried_and_permanent_4xx_marks_the_segment_failed() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_fail_status(Some(503));
        a.drv.start();
        a.queue.process().await;
        let st = a.status();
        assert_eq!(st.state, "error");
        assert_eq!(st.detail, "HTTP 503");
        assert!(a.queue.has_retry_timer(), "retry scheduled");
        assert_eq!(a.state().queue.len(), 1, "operation kept");

        a.api.set_fail_status(Some(400));
        a.queue.process().await;
        assert!(a.state().queue.is_empty(), "permanent error drops the operation");
        assert_eq!(a.state().segments[0].sync_state, "failed");
        let st = a.status();
        assert_eq!(st.state, "error");
        assert_eq!(st.detail, "HTTP 400");
        assert!(!a.queue.has_retry_timer());
    }

    #[tokio::test(start_paused = true)]
    async fn running_log_deleted_on_the_server_is_recreated_with_the_original_start() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        clock.advance(5 * MIN);

        a.queue.verify_current().await; // exists -> nothing
        assert_eq!(a.api.count("startNewLog"), 1);
        assert!(matches!(a.api.calls().last(), Some(Call::GetLogs(d)) if d == "2025-06-10"), "checks today's logs");

        a.api.clear_logs(); // deleted on the web
        a.queue.verify_current().await; // first miss: only noted
        assert_eq!(a.api.count("startNewLog"), 1);
        a.queue.verify_current().await; // second miss: recreate
        assert_eq!(a.api.count("startNewLog"), 2);
        assert_eq!(a.api.starts()[1].start, iso(t0()));
        assert_eq!(a.state().current.unwrap().remote_log_id, Some(JsonId::Num(1001)));
        assert!(a.state().segments[0].recreated);

        // a third check does not recreate again (already recreated once)
        a.api.clear_logs();
        a.queue.verify_current().await;
        a.queue.verify_current().await;
        assert_eq!(a.api.count("startNewLog"), 2);

        // stop goes to the new log
        clock.advance(5 * MIN);
        a.drv.stop();
        a.queue.process().await;
        let (log_id, _) = a.api.stops().pop().unwrap();
        assert_eq!(log_id, JsonId::Num(1001));
    }

    #[tokio::test(start_paused = true)]
    async fn failures_of_the_log_verification_never_break_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        a.api.set_fail_op("getLogs", Some(500));
        a.queue.verify_current().await;
        assert!(a.state().current.is_some());
        assert_eq!(a.status().state, "ok");
        // an auth failure of the check does block syncing, like any other 401
        a.api.set_fail_op("getLogs", Some(401));
        a.queue.verify_current().await;
        assert_eq!(a.status().state, "auth-error");
        assert!(a.state().current.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn verification_ignores_unknown_shapes_and_resets_misses_when_found() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;

        a.api.set_logs_raw(Some(json!({ "nope": 1 })));
        for _ in 0..3 {
            a.queue.verify_current().await;
        }
        assert_eq!(a.api.count("startNewLog"), 1, "unknown shape never recreates");

        a.api.set_logs_raw(Some(json!({ "logs": [] })));
        a.queue.verify_current().await; // miss 1
        a.api.set_logs_raw(Some(json!([{ "logId": "1000" }]))); // found (string id, other key)
        a.queue.verify_current().await; // counter reset
        a.api.set_logs_raw(Some(json!({ "logs": [] })));
        a.queue.verify_current().await; // miss 1 again
        assert_eq!(a.api.count("startNewLog"), 1);
        a.queue.verify_current().await; // miss 2
        assert_eq!(a.api.count("startNewLog"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn verification_is_skipped_while_operations_are_pending() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        let before = a.api.count("getLogs");
        a.push_op(delete_op("x", 5));
        a.queue.verify_current().await;
        assert_eq!(a.api.count("getLogs"), before);
    }

    #[tokio::test(start_paused = true)]
    async fn stoptimer_404_falls_back_to_insertlog() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        clock.advance(10 * MIN);
        a.api.set_fail_op("stoptimer", Some(404));
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.api.count("insertlog"), 1);
        assert_eq!(a.api.inserts()[0].end, iso(t0() + 10 * MIN));
        assert_eq!(a.state().segments[0].sync_state, "synced");
        // the vanished log is still scheduled for deletion; it is already gone, so that is harmless
        assert!(a.state().queue.is_empty());
        assert_eq!(a.api.count("deleteLog"), 1);
    }

    // ---- one server log per day and target ----

    #[tokio::test(start_paused = true)]
    async fn several_runs_of_one_day_end_up_as_one_server_log_with_the_total_time() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);

        a.drv.start(); // run 1: 09:00 - 09:30 (30 min)
        a.queue.process().await;
        clock.advance(30 * MIN);
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.api.log_ids().len(), 1);

        clock.advance(20 * MIN); // pause 20 min
        a.drv.start(); // run 2: 09:50 - 10:05 (15 min)
        a.queue.process().await;
        // the new running log starts shifted back by the 30 min already worked
        assert_eq!(a.api.starts()[1].start, iso(t0() + 50 * MIN - 30 * MIN));
        clock.advance(15 * MIN);
        a.drv.stop();
        a.queue.process().await;

        clock.advance(5 * MIN);
        a.drv.start(); // run 3: 10:10 - 10:15 (5 min)
        a.queue.process().await;
        clock.advance(5 * MIN);
        a.drv.stop();
        a.queue.process().await;

        assert_eq!(a.api.log_ids().len(), 1, "exactly one log remains on the server");
        assert_eq!(a.api.count("deleteLog"), 2, "each replaced log was deleted");
        assert_eq!(a.api.deletes(), vec![JsonId::Num(1000), JsonId::Num(1001)], "oldest replaced log first");
        let last_start = a.api.starts().pop().unwrap();
        let (last_log, last_time) = a.api.stops().pop().unwrap();
        assert_eq!(last_log, JsonId::Num(a.api.log_ids()[0]));
        // duration of the remaining log = 30 + 15 + 5 minutes
        let dur = crate::model::parse_iso(&last_time).unwrap() - crate::model::parse_iso(&last_start.start).unwrap();
        assert_eq!(dur, 50 * MIN);
        assert!(a.state().queue.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn lock_unlock_cycles_also_stay_in_one_server_log() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        for _ in 0..3 {
            clock.advance(10 * MIN);
            a.drv.system_pause("lock");
            a.queue.process().await;
            clock.advance(3 * MIN);
            a.drv.system_resume();
            a.queue.process().await;
        }
        clock.advance(10 * MIN);
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.state().segments.len(), 4);
        assert_eq!(a.api.log_ids().len(), 1);
        let last_start = a.api.starts().pop().unwrap();
        let (_, last_time) = a.api.stops().pop().unwrap();
        let dur = crate::model::parse_iso(&last_time).unwrap() - crate::model::parse_iso(&last_start.start).unwrap();
        assert_eq!(dur, 40 * MIN);
    }

    #[tokio::test(start_paused = true)]
    async fn offline_runs_of_one_day_are_merged_into_one_insertlog_and_the_stale_one_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_online(false);
        a.drv.start();
        clock.advance(20 * MIN);
        a.drv.stop();
        clock.advance(10 * MIN);
        a.drv.start();
        clock.advance(15 * MIN);
        a.drv.stop();
        a.queue.process().await;
        a.api.set_online(true);
        a.queue.process().await;
        assert_eq!(a.api.log_ids().len(), 1);
        let inserts = a.api.inserts();
        assert_eq!(inserts.len(), 2);
        let dur = |i: &InsertLog| crate::model::parse_iso(&i.end).unwrap() - crate::model::parse_iso(&i.start).unwrap();
        assert_eq!(dur(&inserts[0]), 20 * MIN);
        assert_eq!(dur(&inserts[1]), 35 * MIN);
        assert!(a.state().queue.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_different_day_or_target_gets_its_own_server_log() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        clock.advance(10 * MIN);
        a.drv.stop();
        a.queue.process().await;

        a.drv.set_task(4); // other task, same day
        a.drv.start();
        a.queue.process().await;
        clock.advance(10 * MIN);
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.api.log_ids().len(), 2);
        assert_eq!(a.api.count("deleteLog"), 0);

        clock.advance(24 * 60 * MIN); // next day, original task
        a.drv.set_task(3);
        a.drv.start();
        a.queue.process().await;
        let start = a.api.starts().pop().unwrap();
        assert_eq!(start.start, iso(clock.now_ms()), "a new day starts without a shift");
        clock.advance(10 * MIN);
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.api.log_ids().len(), 3);
        assert_eq!(a.api.count("deleteLog"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_delete_is_retried_and_never_creates_another_log() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        clock.advance(10 * MIN);
        a.drv.stop();
        a.queue.process().await;
        clock.advance(5 * MIN);
        a.api.set_fail_op("deleteLog", Some(503));
        a.drv.start();
        a.queue.process().await;
        let q = a.state().queue;
        assert_eq!(q.len(), 1, "only the delete is left in the queue");
        assert!(matches!(q[0], Op::Delete { .. }));
        assert_eq!(a.api.count("startNewLog"), 2, "no extra log was created while delete fails");
        a.api.set_fail_op("deleteLog", None);
        a.queue.process().await;
        assert!(a.state().queue.is_empty());
        assert_eq!(a.api.count("deleteLog"), 1);
        clock.advance(10 * MIN);
        a.drv.stop();
        a.queue.process().await;
        assert_eq!(a.api.log_ids().len(), 1);
        assert_eq!(a.api.count("startNewLog"), 2);
        assert!(a.state().queue.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn segments_from_older_versions_without_merge_keep_one_log_per_segment() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        clock.advance(10 * MIN);
        a.drv.stop();
        a.queue.process().await;
        {
            let mut g = a.store.lock();
            g.state.segments[0].merge = false;
            g.state.day_logs.clear();
        }
        clock.advance(5 * MIN);
        a.drv.start();
        a.queue.process().await;
        assert_eq!(a.api.starts()[1].start, iso(clock.now_ms()), "legacy segments do not shift the new log");
        assert_eq!(a.api.count("deleteLog"), 0);
    }

    // ================= parts of test/tracker.test.js that need the queue =================

    #[tokio::test(start_paused = true)]
    async fn start_lock_unlock_stop_syncs_two_segments_as_start_plus_stop_each() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        clock.advance(10 * MIN);
        a.drv.system_pause("lock");
        a.queue.process().await;
        clock.advance(5 * MIN);
        a.drv.system_resume();
        a.queue.process().await;
        assert!(a.state().current.is_some());
        clock.advance(20 * MIN);
        a.drv.stop();
        a.queue.process().await;

        let s = a.state();
        assert_eq!(s.segments.len(), 2);
        assert!(s.queue.is_empty());
        assert_eq!(a.api.count("startNewLog"), 2);
        assert_eq!(a.api.count("stoptimer"), 2);
        assert!(s.segments.iter().all(|x| x.sync_state == "synced"));
    }

    #[tokio::test(start_paused = true)]
    async fn crash_recovered_segment_reaches_the_server_with_the_heartbeat_end_time() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let app1 = build(&dir, &clock);
        app1.drv.start();
        app1.queue.process().await; // start reached the server before the crash
        assert!(app1.state().current.unwrap().remote_log_id.is_some());
        clock.advance(10 * MIN);
        app1.drv.heartbeat();
        let heartbeat = clock.now_ms();
        clock.advance(10 * 1000); // ... then power loss, no further writes

        clock.set(t0() + 3 * 3600 * 1000); // PC comes back 3 h later
        let app2 = build(&dir, &clock);
        app2.drv.init();
        let s = app2.state();
        assert_eq!(s.segments.len(), 2);
        assert_eq!(s.segments[0].end, Some(heartbeat));
        assert_eq!(s.segments[1].start, clock.now_ms());
        assert_eq!(s.segments[1].end, None);
        assert!(s.current.is_some());
        app2.queue.process().await;
        let (_, time) = &app2.api.stops()[0];
        assert_eq!(*time, iso(heartbeat));
        assert_eq!(app2.api.count("startNewLog"), 1); // the new segment
    }

    #[tokio::test(start_paused = true)]
    async fn midnight_split_syncs_two_logs_on_different_days_without_a_shift() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(local_ms(2025, 6, 10, 23, 59, 0));
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        clock.set(local_ms(2025, 6, 11, 0, 0, 30));
        a.drv.tick_midnight();
        a.queue.process().await;

        let boundary = local_ms(2025, 6, 11, 0, 0, 0);
        let segs = a.state().segments;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].end, Some(boundary - 1));
        let starts = a.api.starts();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0].date, "2025-06-10");
        assert_eq!(starts[1].date, "2025-06-11");
        assert_eq!(starts[1].start, iso(boundary), "the new day starts without a shift");
        let (log_id, time) = &a.api.stops()[0];
        assert_eq!(*log_id, JsonId::Num(1000));
        assert_eq!(*time, iso(boundary - 1));
        assert_eq!(a.api.log_ids().len(), 2);
        assert_eq!(a.api.count("deleteLog"), 0);
        assert_eq!(a.state().current.unwrap().remote_log_id, Some(JsonId::Num(1001)));
        assert!(a.state().queue.is_empty());
    }

    // ================= behaviour specific to the Rust implementation =================

    #[tokio::test(start_paused = true)]
    async fn concurrent_process_calls_do_not_run_two_operations_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_delay(100);
        for n in 1..=3 {
            a.push_op(delete_op(&format!("d{n}"), n));
        }
        tokio::join!(a.queue.process(), a.queue.process(), a.queue.process());
        assert_eq!(a.api.max_in_flight(), 1);
        assert_eq!(a.api.count("deleteLog"), 3);
        assert!(a.state().queue.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_that_arrives_while_processing_is_picked_up_by_the_again_flag() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_delay(100);
        a.api.set_online(false);
        a.push_op(delete_op("d1", 1));
        a.queue.kick();
        wait_in_flight(&a.api, 1).await; // the first request (already sent while offline) is in flight
        a.api.set_online(true);
        a.push_op(delete_op("d2", 2));
        a.queue.kick(); // marks `again`
        a.queue.process().await;
        // without `again` the run would have ended at the offline failure and waited for the 30 s retry
        assert!(a.state().queue.is_empty());
        assert_eq!(a.api.count("deleteLog"), 2);
        assert!(!a.queue.has_retry_timer());
        assert_eq!(a.status().state, "ok");
    }

    #[tokio::test(start_paused = true)]
    async fn tracker_changes_during_a_request_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_delay(100);
        a.drv.start();
        wait_in_flight(&a.api, 1).await; // startNewLog in flight
        clock.advance(5 * MIN);
        a.drv.stop(); // closes the segment while the start is on its way
        a.queue.process().await;
        assert_eq!(a.api.kinds(), vec!["startNewLog", "stoptimer"], "the stop knows the remote id of the start");
        let s = a.state();
        assert_eq!(s.segments[0].remote_log_id, Some(JsonId::Num(1000)));
        assert_eq!(s.segments[0].sync_state, "synced");
        assert!(s.queue.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn kick_works_from_a_plain_thread_and_needs_no_await() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.push_op(delete_op("d1", 1));
        let q = a.queue.clone();
        std::thread::spawn(move || q.kick()).join().unwrap();
        settle().await;
        assert!(a.state().queue.is_empty());
        assert_eq!(a.api.count("deleteLog"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_waits_at_most_the_timeout_and_never_blocks_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_delay(1000);
        a.push_op(delete_op("d1", 1));
        let t = tokio::time::Instant::now();
        a.queue.flush(100).await;
        assert_eq!(t.elapsed(), Duration::from_millis(100));
        assert_eq!(a.state().queue.len(), 1, "still in flight");
        // the abandoned wait did not wedge the single-flight gate
        a.queue.process().await;
        assert!(a.state().queue.is_empty());
        // a drained queue returns long before the timeout
        let t = tokio::time::Instant::now();
        a.queue.flush(10_000).await;
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_is_30_60_120_240_300_300_seconds_and_success_resets_it() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_fail_status(Some(503));
        a.push_op(delete_op("d1", 1));
        a.queue.process().await;
        assert_eq!(a.api.attempts(), 1);
        let mut expected = 1;
        for delay in [30u64, 60, 120, 240, 300, 300] {
            advance(delay * 1000 - 1000).await;
            assert_eq!(a.api.attempts(), expected, "too early for the {delay} s retry");
            advance(1000).await;
            expected += 1;
            assert_eq!(a.api.attempts(), expected, "retry after {delay} s");
            assert_eq!(a.status().state, "error");
        }
        a.api.set_fail_status(None);
        advance(300_000).await;
        assert!(a.state().queue.is_empty());
        assert_eq!(a.status().state, "ok");
        assert!(!a.queue.has_retry_timer());

        // after the success the schedule starts from 30 s again
        a.api.set_fail_status(Some(503));
        a.push_op(delete_op("d2", 2));
        let before = a.api.attempts();
        a.queue.process().await;
        advance(29_000).await;
        assert_eq!(a.api.attempts(), before + 1);
        advance(1000).await;
        assert_eq!(a.api.attempts(), before + 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_retry_is_not_duplicated_by_manual_processing() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_online(false);
        a.push_op(delete_op("d1", 1));
        a.queue.process().await;
        a.queue.process().await;
        a.queue.process().await;
        assert_eq!(a.api.attempts(), 3);
        // one timer, still the first step of the schedule
        advance(30_000).await;
        assert_eq!(a.api.attempts(), 4);
        advance(59_000).await;
        assert_eq!(a.api.attempts(), 4);
        advance(1_000).await;
        assert_eq!(a.api.attempts(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_cancels_the_retry_timer() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_online(false);
        a.push_op(delete_op("d1", 1));
        a.queue.process().await;
        assert!(a.queue.has_retry_timer());
        a.queue.stop();
        assert!(!a.queue.has_retry_timer());
        advance(600_000).await;
        assert_eq!(a.api.attempts(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_timer_processes_and_follows_interval_changes() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.queue.set_interval_ms(1000);
        assert!(!a.queue.has_periodic_timer(), "setting the interval does not start the timer");
        a.queue.start();
        a.push_op(delete_op("d1", 1));
        advance(999).await;
        assert_eq!(a.api.count("deleteLog"), 0);
        advance(1).await;
        assert_eq!(a.api.count("deleteLog"), 1);

        a.push_op(delete_op("d2", 2));
        a.queue.set_interval_ms(5000); // restarts the timer with the new period
        advance(4999).await;
        assert_eq!(a.api.count("deleteLog"), 1);
        advance(1).await;
        assert_eq!(a.api.count("deleteLog"), 2);

        a.queue.stop();
        assert!(!a.queue.has_periodic_timer());
        a.push_op(delete_op("d3", 3));
        advance(60_000).await;
        assert_eq!(a.api.count("deleteLog"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_verifies_the_running_log() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.drv.start();
        a.queue.process().await;
        a.queue.set_interval_ms(1000);
        a.queue.start();
        a.api.clear_logs();
        advance(1000).await; // miss 1
        assert_eq!(a.api.count("startNewLog"), 1);
        advance(1000).await; // miss 2 -> recreated
        assert_eq!(a.api.count("startNewLog"), 2);
        a.queue.stop();
    }

    #[tokio::test(start_paused = true)]
    async fn sync_now_clears_the_sticky_permanent_error() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_fail_status(Some(400));
        a.drv.start();
        a.queue.process().await;
        assert_eq!(a.status().state, "error");
        a.api.set_fail_status(None);
        a.queue.process().await;
        assert_eq!(a.status().state, "error", "sticky: a later successful run does not clear it");
        a.queue.sync_now().await;
        assert_eq!(a.status().state, "ok");
    }

    #[tokio::test(start_paused = true)]
    async fn auth_error_is_sticky_even_for_sync_now_until_reset_auth() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.api.set_fail_status(Some(403));
        a.push_op(delete_op("d1", 1));
        a.queue.process().await;
        assert_eq!(a.status().state, "auth-error");
        a.api.set_fail_status(None);
        a.queue.sync_now().await;
        assert_eq!(a.status().state, "auth-error");
        a.queue.reset_auth().await;
        assert_eq!(a.status().state, "ok");
        assert_eq!(a.api.count("deleteLog"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_delete_that_is_gone_or_refused_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        for status in [404, 400] {
            a.api.set_fail_op("deleteLog", Some(status));
            a.push_op(delete_op("d", 1));
            a.queue.process().await;
            assert!(a.state().queue.is_empty());
            assert_eq!(a.status().state, "ok", "status {status} must not raise a warning");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn operations_of_vanished_segments_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        a.push_op(Op::Start { id: "s".into(), local_id: "gone".into(), start: t0(), project_id: None, task_id: None, note: String::new() });
        a.push_op(Op::Stop { id: "e".into(), local_id: "gone".into(), end: t0() + 5 * MIN });
        a.queue.process().await;
        assert!(a.state().queue.is_empty());
        assert!(a.api.calls().is_empty());
        assert_eq!(a.status().state, "ok");
    }

    #[tokio::test(start_paused = true)]
    async fn status_reports_pending_and_is_published_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        assert_eq!(a.status(), SyncStatus { state: "ok".into(), pending: 0, detail: String::new() });
        a.push_op(delete_op("d1", 1));
        a.push_op(delete_op("d2", 2));
        assert_eq!(a.status(), SyncStatus { state: "pending".into(), pending: 2, detail: String::new() });
        a.queue.process().await;
        let seen = a.statuses.lock().clone();
        assert_eq!(seen, vec!["pending", "ok"], "pending after the first op, ok at the end, no repeats");
        a.queue.process().await;
        assert_eq!(a.statuses.lock().len(), 2, "unchanged status is not published again");
    }

    #[tokio::test(start_paused = true)]
    async fn status_callback_runs_without_the_state_lock_and_may_panic() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let api = MockApi::new();
        let mut st: Store<State> = Store::new(dir.path().join("state.json"), Logger::noop());
        st.load();
        let store = st.into_shared();
        let reads = Arc::new(AtomicUsize::new(0));
        let (s2, r2) = (store.clone(), reads.clone());
        let queue = SyncQueue::new(deps(
            &store,
            &api,
            &clock,
            Arc::new(move |_s| {
                // would deadlock if the queue called us while holding the (non-reentrant) state lock
                let _len = s2.lock().state.queue.len();
                r2.fetch_add(1, Ordering::SeqCst);
                if r2.load(Ordering::SeqCst) == 1 {
                    panic!("UI blew up");
                }
            }),
        ));
        store.lock().state.queue.push(delete_op("d1", 1));
        store.lock().state.queue.push(delete_op("d2", 2));
        queue.process().await;
        assert!(store.lock().state.queue.is_empty(), "a panicking callback does not break syncing");
        assert!(reads.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn group_key_and_prior_ms() {
        let day = t0();
        let a = seg("a", day, Some(day + 30 * MIN), 7, Some(3), "work", true);
        let b = seg("b", day + 50 * MIN, Some(day + 65 * MIN), 7, Some(3), "work", true);
        let c = seg("c", day + 70 * MIN, None, 7, Some(3), "work", true);
        let other_note = seg("d", day + 10 * MIN, Some(day + 20 * MIN), 7, Some(3), "other", true);
        let other_task = seg("e", day + 10 * MIN, Some(day + 20 * MIN), 7, None, "work", true);
        let legacy = seg("f", day + 10 * MIN, Some(day + 20 * MIN), 7, Some(3), "work", false);
        let yesterday = seg("g", day - 24 * 60 * MIN, Some(day - 23 * 60 * MIN), 7, Some(3), "work", true);
        let st = State { segments: vec![a.clone(), b.clone(), c.clone(), other_note, other_task, legacy.clone(), yesterday], ..State::default() };
        assert_eq!(group_key(&a), "2025-06-10|7|3|work");
        assert_eq!(group_key(&seg("x", day, None, 7, None, "", true)), "2025-06-10|7||");
        assert_eq!(prior_ms(&st, &a), 0);
        assert_eq!(prior_ms(&st, &b), 30 * MIN);
        assert_eq!(prior_ms(&st, &c), 45 * MIN, "only finished merge segments of the same day/target count");
        assert_eq!(prior_ms(&st, &legacy), 0, "legacy segments never shift");
    }

    #[tokio::test(start_paused = true)]
    async fn old_day_logs_are_pruned_after_a_start() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(t0());
        let a = build(&dir, &clock);
        {
            let mut g = a.store.lock();
            for key in ["2025-05-01|7|3|work", "2025-06-01|7|3|work", "2025-06-05|7|3|work"] {
                g.state.day_logs.insert(key.into(), DayLog { remote_log_id: Some(JsonId::Num(1)) });
            }
        }
        a.drv.start();
        a.queue.process().await;
        let keys: Vec<String> = {
            let mut k: Vec<_> = a.state().day_logs.keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys, vec!["2025-06-05|7|3|work", "2025-06-10|7|3|work"]);
    }
}
