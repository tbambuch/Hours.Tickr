//! Persistent data model. The JSON layout is the one written by earlier versions
//! (`state.json` / `settings.json`), so existing files keep working.
//!
//! All timestamps are held in memory as milliseconds since the Unix epoch (`Ms`) and serialized as
//! ISO-8601 UTC strings with millisecond precision (what `Date#toISOString()` produced).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch.
pub type Ms = i64;

pub fn format_iso(ms: Ms) -> String {
    use chrono::{DateTime, SecondsFormat, Utc};
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn parse_iso(s: &str) -> Option<Ms> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

/// serde helper: `Ms` <-> ISO string.
pub mod iso_ms {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Ms, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_iso(*v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Ms, D::Error> {
        let raw = String::deserialize(d)?;
        parse_iso(&raw).ok_or_else(|| serde::de::Error::custom(format!("invalid ISO timestamp: {raw}")))
    }
}

/// serde helper: `Option<Ms>` <-> ISO string or null. An unparsable value becomes `None` (never fails loading).
pub mod iso_ms_opt {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Ms>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(ms) => s.serialize_str(&format_iso(*ms)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Ms>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        Ok(raw.and_then(|r| parse_iso(&r)))
    }
}

/// A My Hours id (project, task, log). The API and the old state files use JSON numbers, but ids that are
/// not numeric are kept as strings. Compare ids with [`JsonId::key`], never with `==` across Num/Str.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum JsonId {
    Num(i64),
    Str(String),
}

impl JsonId {
    /// Canonical string form used for equality ("12" and 12 are the same id).
    pub fn key(&self) -> String {
        match self {
            JsonId::Num(n) => n.to_string(),
            JsonId::Str(s) => s.clone(),
        }
    }

    /// All-digit strings become numbers (like the old `idOrNull`), everything else stays a string.
    pub fn parse(s: &str) -> JsonId {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n) = s.parse::<i64>() {
                return JsonId::Num(n);
            }
        }
        JsonId::Str(s.to_string())
    }

    pub fn same(a: &Option<JsonId>, b: &Option<JsonId>) -> bool {
        a.as_ref().map(|x| x.key()) == b.as_ref().map(|x| x.key())
    }
}

impl std::fmt::Display for JsonId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

/// What a segment is booked on.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Target {
    pub project_id: Option<JsonId>,
    pub task_id: Option<JsonId>,
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Current {
    pub local_id: String,
    #[serde(with = "iso_ms")]
    pub start: Ms,
    #[serde(default)]
    pub remote_log_id: Option<JsonId>,
}

/// One uninterrupted stretch of measuring. `sync_state`: "pending" | "synced" | "failed" | "skipped".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub local_id: String,
    #[serde(with = "iso_ms")]
    pub start: Ms,
    #[serde(default, with = "iso_ms_opt")]
    pub end: Option<Ms>,
    #[serde(default)]
    pub remote_log_id: Option<JsonId>,
    #[serde(default = "pending")]
    pub sync_state: String,
    #[serde(default)]
    pub project_id: Option<JsonId>,
    #[serde(default)]
    pub task_id: Option<JsonId>,
    #[serde(default)]
    pub note: String,
    /// Segments created by older versions have no `merge` flag: one server log per segment.
    #[serde(default)]
    pub merge: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recreated: bool,
}

fn pending() -> String {
    "pending".to_string()
}

impl Segment {
    pub fn target(&self) -> Target {
        Target { project_id: self.project_id.clone(), task_id: self.task_id.clone(), note: self.note.clone() }
    }
}

/// An operation in the persistent sync queue.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum Op {
    Start {
        id: String,
        local_id: String,
        #[serde(with = "iso_ms")]
        start: Ms,
        #[serde(default)]
        project_id: Option<JsonId>,
        #[serde(default)]
        task_id: Option<JsonId>,
        #[serde(default)]
        note: String,
    },
    Stop {
        id: String,
        local_id: String,
        #[serde(with = "iso_ms")]
        end: Ms,
    },
    /// A server log replaced by a newer one of the same day/target.
    Delete { id: String, log_id: JsonId },
}

impl Op {
    pub fn id(&self) -> &str {
        match self {
            Op::Start { id, .. } | Op::Stop { id, .. } | Op::Delete { id, .. } => id,
        }
    }
    /// The segment this operation belongs to (None for deletes).
    pub fn local_id(&self) -> Option<&str> {
        match self {
            Op::Start { local_id, .. } | Op::Stop { local_id, .. } => Some(local_id),
            Op::Delete { .. } => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DayLog {
    #[serde(default)]
    pub remote_log_id: Option<JsonId>,
}

/// `state.json`: everything the tracker and the sync queue need to survive a restart.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub user_wants_running: bool,
    pub current: Option<Current>,
    #[serde(with = "iso_ms_opt")]
    pub last_heartbeat: Option<Ms>,
    /// null | "lock" | "suspend" | "shutdown" | "idle" | "not-configured"
    pub pause_reason: Option<String>,
    pub segments: Vec<Segment>,
    pub queue: Vec<Op>,
    /// groupKey -> the single server log of that day/target.
    pub day_logs: HashMap<String, DayLog>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WindowPos {
    pub x: i32,
    pub y: i32,
}

/// `settings.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub project_id: Option<JsonId>,
    pub project_name: String,
    pub task_id: Option<JsonId>,
    pub task_name: String,
    pub note: String,
    /// Pause measuring after this many minutes without input; 0 = off.
    pub idle_minutes: u32,
    pub sync_interval_min: u32,
    pub open_at_login: bool,
    pub first_run_done: bool,
    pub account_name: String,
    pub account_email: String,
    /// "system" | "cs" | "en"
    pub language: String,
    /// "system" | "light" | "dark"
    pub theme: String,
    pub always_on_top: bool,
    pub window_pos: Option<WindowPos>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            project_id: None,
            project_name: String::new(),
            task_id: None,
            task_name: String::new(),
            note: String::new(),
            idle_minutes: 10,
            sync_interval_min: 5,
            open_at_login: true,
            first_run_done: false,
            account_name: String::new(),
            account_email: String::new(),
            language: "system".into(),
            theme: "system".into(),
            always_on_top: false,
            window_pos: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_roundtrip_keeps_millis() {
        let ms = 1_762_849_740_123;
        assert_eq!(format_iso(ms), "2025-11-11T08:29:00.123Z");
        assert_eq!(parse_iso(&format_iso(ms)), Some(ms));
    }

    #[test]
    fn reads_state_written_by_an_earlier_version() {
        let json = r#"{
          "userWantsRunning": true,
          "current": { "localId": "a", "start": "2026-11-11T08:00:00.000Z", "remoteLogId": 123 },
          "lastHeartbeat": "2026-11-11T08:05:00.000Z",
          "pauseReason": null,
          "segments": [
            { "localId": "a", "start": "2026-11-11T08:00:00.000Z", "end": null, "remoteLogId": 123,
              "syncState": "pending", "projectId": 7, "taskId": null, "note": "", "merge": true },
            { "localId": "old", "start": "2026-11-10T08:00:00.000Z", "end": "2026-11-10T09:00:00.000Z",
              "remoteLogId": "abc", "syncState": "synced", "projectId": 7, "taskId": 9, "note": "x" }
          ],
          "queue": [
            { "id": "1", "type": "start", "localId": "a", "start": "2026-11-11T08:00:00.000Z", "projectId": 7, "taskId": null, "note": "" },
            { "id": "2", "type": "stop", "localId": "old", "end": "2026-11-10T09:00:00.000Z" },
            { "id": "3", "type": "delete", "logId": 55 }
          ],
          "dayLogs": { "2026-11-11|7||": { "remoteLogId": 123 } }
        }"#;
        let st: State = serde_json::from_str(json).unwrap();
        assert!(st.user_wants_running);
        assert_eq!(st.current.as_ref().unwrap().remote_log_id, Some(JsonId::Num(123)));
        assert_eq!(st.segments[1].remote_log_id, Some(JsonId::Str("abc".into())));
        assert!(!st.segments[1].merge, "old segments have no merge flag");
        assert_eq!(st.queue.len(), 3);
        assert_eq!(st.queue[2], Op::Delete { id: "3".into(), log_id: JsonId::Num(55) });
        // and it serializes back to the same shape
        let again: State = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(again, st);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let st: State = serde_json::from_str("{}").unwrap();
        assert_eq!(st, State::default());
        let s: Settings = serde_json::from_str(r#"{"projectId":"5"}"#).unwrap();
        assert_eq!(s.idle_minutes, 10);
        assert_eq!(s.project_id, Some(JsonId::Str("5".into())));
    }

    #[test]
    fn json_id_equality_is_by_key() {
        assert_eq!(JsonId::parse("42"), JsonId::Num(42));
        assert_eq!(JsonId::parse("a1"), JsonId::Str("a1".into()));
        assert!(JsonId::same(&Some(JsonId::Num(5)), &Some(JsonId::Str("5".into()))));
        assert!(JsonId::same(&None, &None));
        assert!(!JsonId::same(&Some(JsonId::Num(5)), &None));
    }
}
