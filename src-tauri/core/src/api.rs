//! Thin My Hours HTTP client.
//!
//! Works for both API key (`ApiKey <k>`) and OAuth (`Bearer <t>`) through [`AuthProvider`]; an explicit
//! `key_override` is used to verify a candidate API key. Credentials never appear in errors or logs.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::error::ApiError;
use crate::http::{Http, HttpRequest};
use crate::logger::Logger;
use crate::model::JsonId;

pub const BASE_URL: &str = "https://api2.myhours.com/api";
const TIMEOUT: Duration = Duration::from_secs(20);

#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// `Authorization` header value; `None` = not signed in.
    async fn auth_header(&self) -> Result<Option<String>, ApiError>;
    /// Called after a 401 with the header that was used. `true` = credentials were renewed, retry once.
    async fn on_unauthorized(&self, used_header: &str) -> Result<bool, ApiError>;
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StartLog {
    pub project_id: Option<JsonId>,
    pub task_id: Option<JsonId>,
    pub note: String,
    pub date: String,
    pub start: String,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InsertLog {
    pub project_id: Option<JsonId>,
    pub task_id: Option<JsonId>,
    pub note: String,
    pub date: String,
    pub start: String,
    pub end: String,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct NamedId {
    pub id: JsonId,
    pub name: String,
}

/// What the sync queue needs from the server (a mock in tests).
#[async_trait]
pub trait LogApi: Send + Sync {
    async fn get_logs(&self, date: &str) -> Result<Value, ApiError>;
    async fn start_new_log(&self, req: StartLog) -> Result<Value, ApiError>;
    async fn stop_timer(&self, log_id: &JsonId, time: &str) -> Result<(), ApiError>;
    async fn insert_log(&self, req: InsertLog) -> Result<Value, ApiError>;
    async fn delete_log(&self, log_id: &JsonId) -> Result<(), ApiError>;
}

// ---- Defensive response parsers (response shapes are not fully documented) ----

fn json_to_id(v: &Value) -> Option<JsonId> {
    match v {
        Value::Number(n) => n.as_i64().map(JsonId::Num).or_else(|| {
            n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15).map(|f| JsonId::Num(f as i64))
        }),
        Value::String(s) => Some(JsonId::Str(s.clone())),
        _ => None,
    }
}

/// JS truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn non_null(v: Option<&Value>) -> Option<&Value> {
    v.filter(|x| !x.is_null())
}

/// Extracts the new log id from a startNewLog / insertlog response.
pub fn parse_log_id(v: &Value) -> Option<JsonId> {
    match v {
        Value::Number(_) => json_to_id(v),
        Value::String(s) => {
            let t = s.trim();
            let t = t.strip_prefix('"').unwrap_or(t);
            let t = t.strip_suffix('"').unwrap_or(t);
            if t.is_empty() {
                None
            } else {
                Some(JsonId::parse(t))
            }
        }
        Value::Object(o) => {
            let nested = |outer: &str, inner: &str| non_null(o.get(outer).and_then(|x| x.get(inner)));
            let found = non_null(o.get("id"))
                .or_else(|| non_null(o.get("logId")))
                .or_else(|| nested("log", "id"))
                .or_else(|| nested("data", "id"))
                .or_else(|| nested("data", "logId"));
            found.and_then(json_to_id)
        }
        _ => None,
    }
}

fn named(v: &Value) -> Option<NamedId> {
    let id = json_to_id(non_null(v.get("id"))?)?;
    let name = v.get("name")?.as_str()?.to_string();
    Some(NamedId { id, name })
}

/// Normalizes the Projects response to `[{id, name}]` (archived projects are left out).
pub fn parse_projects(v: &Value) -> Vec<NamedId> {
    let list = match v {
        Value::Array(a) => Some(a),
        Value::Object(o) => ["projects", "items", "data"].iter().find_map(|k| non_null(o.get(*k))).and_then(|x| x.as_array()),
        _ => None,
    };
    let Some(list) = list else { return Vec::new() };
    list.iter()
        .filter(|p| !truthy(p.get("archived")) && !truthy(p.get("isArchived")))
        .filter_map(named)
        .collect()
}

const TASK_CONTAINER_KEYS: [&str; 4] = ["tasks", "incompletedTasks", "incompleteTasks", "openTasks"];

/// Normalizes the (undocumented) tasklist response to `[{id, name}]`.
/// Accepts an array of tasks, or task lists with nested task arrays, at any depth.
pub fn parse_task_list(v: &Value) -> Vec<NamedId> {
    fn walk(node: &Value, out: &mut Vec<NamedId>, seen: &mut HashSet<String>) {
        match node {
            Value::Array(a) => a.iter().for_each(|n| walk(n, out, seen)),
            Value::Object(o) => {
                let mut had_container = false;
                for key in TASK_CONTAINER_KEYS {
                    if let Some(arr) = o.get(key).filter(|x| x.is_array()) {
                        had_container = true;
                        walk(arr, out, seen);
                    }
                }
                if had_container {
                    return; // the container itself (a task list) is not a task
                }
                if let Some(task) = named(node) {
                    if truthy(o.get("completed")) || truthy(o.get("archived")) || truthy(o.get("isArchived")) {
                        return;
                    }
                    if seen.insert(task.id.key()) {
                        out.push(task);
                    }
                    return;
                }
                // Unknown wrapper object: look into array-valued properties.
                for val in o.values().filter(|x| x.is_array()) {
                    walk(val, out, seen);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out, &mut HashSet::new());
    out
}

/// The log objects of a getLogs response, or `None` if the shape is not recognized.
pub fn parse_logs(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Array(a) => Some(a.clone()),
        Value::Object(o) => ["logs", "items", "data", "result"].iter().find_map(|k| o.get(*k).and_then(|x| x.as_array()).cloned()),
        _ => None,
    }
}

/// `encodeURIComponent`.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub struct MyHoursClient {
    http: Arc<dyn Http>,
    auth: Arc<dyn AuthProvider>,
    log: Logger,
}

impl MyHoursClient {
    pub fn new(http: Arc<dyn Http>, auth: Arc<dyn AuthProvider>, log: Logger) -> MyHoursClient {
        MyHoursClient { http, auth, log }
    }

    async fn get_header(&self) -> Result<String, ApiError> {
        match self.auth.auth_header().await? {
            Some(h) if !h.is_empty() => Ok(h),
            _ => Err(ApiError::not_signed_in()),
        }
    }

    async fn request(
        &self,
        method: &'static str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        key: Option<&str>,
    ) -> Result<Value, ApiError> {
        let key = key.filter(|k| !k.is_empty());
        let header = match key {
            Some(k) => format!("ApiKey {k}"),
            None => self.get_header().await?,
        };
        match self.request_once(method, path, query, body, &header).await {
            Err(err) if err.status == Some(401) && key.is_none() => {
                // a failing token refresh (expired login / temporary) propagates as its own error
                if self.auth.on_unauthorized(&header).await? {
                    self.log.info("api: retrying after 401 with renewed credentials");
                    let header = self.get_header().await?;
                    return self.request_once(method, path, query, body, &header).await;
                }
                Err(err)
            }
            other => other,
        }
    }

    async fn request_once(
        &self,
        method: &'static str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        auth_header: &str,
    ) -> Result<Value, ApiError> {
        let mut url = format!("{BASE_URL}{path}");
        if !query.is_empty() {
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in query {
                ser.append_pair(k, v);
            }
            url.push('?');
            url.push_str(&ser.finish());
        }
        let mut headers = vec![
            ("Authorization".to_string(), auth_header.to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            ("api-version".to_string(), "1.0".to_string()),
        ];
        let mut raw_body = None;
        if let Some(b) = body {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
            raw_body = serde_json::to_string(b).ok();
        }
        let res = self
            .http
            .send(HttpRequest { method, url, headers, body: raw_body, timeout: TIMEOUT })
            .await
            .map_err(|e| ApiError::network(format!("Network error ({method} {path}): {}", e.message)))?;

        if !res.ok() {
            return Err(ApiError::http(res.status, format!("HTTP {} ({method} {path})", res.status), res.body.chars().take(300).collect::<String>()));
        }
        if res.body.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&res.body).unwrap_or(Value::String(res.body)))
    }

    /// `key_override` = verification of a candidate API key (the auth provider is not consulted).
    pub async fn get_projects(&self, key_override: Option<&str>) -> Result<Vec<NamedId>, ApiError> {
        Ok(parse_projects(&self.request("GET", "/Projects", &[], None, key_override).await?))
    }

    pub async fn get_tasks(&self, project_id: &JsonId) -> Result<Vec<NamedId>, ApiError> {
        let path = format!("/Projects/{}/tasklist", encode_component(&project_id.key()));
        Ok(parse_task_list(&self.request("GET", &path, &[], None, None).await?))
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, ApiError> {
        self.request("POST", path, &[], Some(&body), None).await
    }
}

#[async_trait]
impl LogApi for MyHoursClient {
    async fn get_logs(&self, date: &str) -> Result<Value, ApiError> {
        self.request("GET", "/Logs", &[("date", date), ("step", "100")], None, None).await
    }

    async fn start_new_log(&self, req: StartLog) -> Result<Value, ApiError> {
        // NOTE: `date` format (YYYY-MM-DD vs. full ISO) is the one place to adjust if the API answers 400.
        self.post("/Logs/startNewLog", serde_json::to_value(&req).unwrap_or(Value::Null)).await
    }

    async fn stop_timer(&self, log_id: &JsonId, time: &str) -> Result<(), ApiError> {
        let body = serde_json::json!({ "logId": log_id, "time": time });
        self.post("/Logs/stoptimer", body).await.map(|_| ())
    }

    async fn insert_log(&self, req: InsertLog) -> Result<Value, ApiError> {
        self.post("/Logs/insertlog", serde_json::to_value(&req).unwrap_or(Value::Null)).await
    }

    async fn delete_log(&self, log_id: &JsonId) -> Result<(), ApiError> {
        let path = format!("/Logs/{}", encode_component(&log_id.key()));
        self.request("DELETE", &path, &[], None, None).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpError, HttpResponse};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---- parsers ----

    #[test]
    fn parse_log_id_is_defensive() {
        assert_eq!(parse_log_id(&json!({"id": 5})), Some(JsonId::Num(5)));
        assert_eq!(parse_log_id(&json!({"logId": 6})), Some(JsonId::Num(6)));
        assert_eq!(parse_log_id(&json!(7)), Some(JsonId::Num(7)));
        assert_eq!(parse_log_id(&json!("8")), Some(JsonId::Num(8)));
        assert_eq!(parse_log_id(&json!("\"9\"")), Some(JsonId::Num(9)));
        assert_eq!(parse_log_id(&json!("  abc ")), Some(JsonId::Str("abc".into())));
        assert_eq!(parse_log_id(&json!({"log": {"id": 9}})), Some(JsonId::Num(9)));
        assert_eq!(parse_log_id(&json!({"data": {"id": 10}})), Some(JsonId::Num(10)));
        assert_eq!(parse_log_id(&json!({"data": {"logId": 11}})), Some(JsonId::Num(11)));
        assert_eq!(parse_log_id(&json!({"id": null, "logId": 12})), Some(JsonId::Num(12)));
        assert_eq!(parse_log_id(&json!({"id": "x1"})), Some(JsonId::Str("x1".into())));
        assert_eq!(parse_log_id(&Value::Null), None);
        assert_eq!(parse_log_id(&json!({})), None);
        assert_eq!(parse_log_id(&json!("")), None);
        assert_eq!(parse_log_id(&json!([1])), None);
    }

    #[test]
    fn parse_projects_filters_and_normalizes() {
        let n = |id, name: &str| NamedId { id: JsonId::Num(id), name: name.into() };
        assert_eq!(parse_projects(&json!([{"id": 1, "name": "A", "x": 1}, {"id": 2, "name": "B", "archived": true}])), vec![n(1, "A")]);
        assert_eq!(parse_projects(&json!({"projects": [{"id": 1, "name": "A"}]})), vec![n(1, "A")]);
        assert_eq!(parse_projects(&json!({"items": [{"id": 1, "name": "A", "isArchived": true}, {"id": 3, "name": "C"}]})), vec![n(3, "C")]);
        assert_eq!(parse_projects(&json!({"data": [{"id": 4, "name": "D"}]})), vec![n(4, "D")]);
        // entries without id / string name are skipped
        assert_eq!(parse_projects(&json!([{"name": "x"}, {"id": 1}, {"id": null, "name": "n"}, null, 5])), vec![]);
        assert_eq!(parse_projects(&Value::Null), vec![]);
        assert_eq!(parse_projects(&json!({"projects": "nope"})), vec![]);
    }

    #[test]
    fn parse_task_list_handles_many_shapes() {
        let n = |id, name: &str| NamedId { id: JsonId::Num(id), name: name.into() };
        assert_eq!(parse_task_list(&json!([{"id": 1, "name": "T1"}, {"id": 2, "name": "T2"}])), vec![n(1, "T1"), n(2, "T2")]);
        assert_eq!(
            parse_task_list(&json!([
                {"id": 10, "name": "List", "tasks": [{"id": 1, "name": "T1"}], "incompletedTasks": [{"id": 2, "name": "T2"}, {"id": 1, "name": "T1"}]},
                {"id": 11, "name": "Empty list", "tasks": []}
            ])),
            vec![n(1, "T1"), n(2, "T2")]
        );
        assert_eq!(parse_task_list(&json!({"tasks": [{"id": 3, "name": "X"}]})), vec![n(3, "X")]);
        assert_eq!(parse_task_list(&json!({"wrapper": {"deep": [{"id": 4, "name": "Y"}]}, "list": [{"id": 5, "name": "Z"}]})), vec![n(5, "Z")]);
        assert_eq!(parse_task_list(&json!([{"id": 1, "name": "done", "completed": true}, {"id": 2, "name": "arch", "isArchived": true}, {"id": 3, "name": "ok"}])), vec![n(3, "ok")]);
        assert_eq!(parse_task_list(&Value::Null), vec![]);
        assert_eq!(parse_task_list(&json!("str")), vec![]);
    }

    #[test]
    fn parse_logs_shapes() {
        assert_eq!(parse_logs(&json!([{"id": 1}])), Some(vec![json!({"id": 1})]));
        assert_eq!(parse_logs(&json!({"logs": [{"id": 2}]})), Some(vec![json!({"id": 2})]));
        assert_eq!(parse_logs(&json!({"items": []})), Some(vec![]));
        assert_eq!(parse_logs(&json!({"result": [{"id": 3}]})), Some(vec![json!({"id": 3})]));
        assert_eq!(parse_logs(&json!({"nope": 1})), None);
        assert_eq!(parse_logs(&Value::Null), None);
        assert_eq!(parse_logs(&json!("x")), None);
    }

    #[test]
    fn encode_component_matches_js() {
        assert_eq!(encode_component("abc-_.!~*'()123"), "abc-_.!~*'()123");
        assert_eq!(encode_component("a b/c?d&é"), "a%20b%2Fc%3Fd%26%C3%A9");
    }

    // ---- client with a mock Http ----

    #[derive(Default)]
    struct MockHttp {
        script: Mutex<VecDeque<Result<HttpResponse, HttpError>>>,
        sent: Mutex<Vec<HttpRequest>>,
    }

    impl MockHttp {
        fn with(items: Vec<Result<HttpResponse, HttpError>>) -> Arc<MockHttp> {
            Arc::new(MockHttp { script: Mutex::new(items.into()), sent: Mutex::new(Vec::new()) })
        }
        fn sent(&self) -> Vec<HttpRequest> {
            self.sent.lock().clone()
        }
    }

    #[async_trait]
    impl Http for MockHttp {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.sent.lock().push(req);
            self.script.lock().pop_front().unwrap_or(Err(HttpError { message: "script exhausted".into() }))
        }
    }

    fn resp(status: u16, body: &str) -> Result<HttpResponse, HttpError> {
        Ok(HttpResponse { status, body: body.into() })
    }

    struct MockAuth {
        header: Mutex<Option<String>>,
        /// header returned after a successful on_unauthorized
        renewed: String,
        renew: Result<bool, ApiError>,
        header_calls: AtomicUsize,
        unauthorized_with: Mutex<Vec<String>>,
    }

    impl MockAuth {
        fn new(header: &str, renewed: &str, renew: Result<bool, ApiError>) -> Arc<MockAuth> {
            Arc::new(MockAuth {
                header: Mutex::new(Some(header.into())),
                renewed: renewed.into(),
                renew,
                header_calls: AtomicUsize::new(0),
                unauthorized_with: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AuthProvider for MockAuth {
        async fn auth_header(&self) -> Result<Option<String>, ApiError> {
            self.header_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.header.lock().clone())
        }
        async fn on_unauthorized(&self, used: &str) -> Result<bool, ApiError> {
            self.unauthorized_with.lock().push(used.to_string());
            if let Ok(true) = self.renew {
                *self.header.lock() = Some(self.renewed.clone());
            }
            self.renew.clone()
        }
    }

    fn client(http: &Arc<MockHttp>, auth: &Arc<MockAuth>) -> MyHoursClient {
        MyHoursClient::new(http.clone(), auth.clone(), Logger::noop())
    }

    fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
        req.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    #[tokio::test]
    async fn get_request_has_exact_headers_url_and_timeout() {
        let http = MockHttp::with(vec![resp(200, r#"[{"id":1}]"#)]);
        let auth = MockAuth::new("ApiKey K1", "", Ok(false));
        let res = client(&http, &auth).get_logs("2025-06-10").await.unwrap();
        assert_eq!(res, json!([{"id": 1}]));
        let sent = http.sent();
        assert_eq!(sent.len(), 1);
        let r = &sent[0];
        assert_eq!(r.method, "GET");
        assert_eq!(r.url, "https://api2.myhours.com/api/Logs?date=2025-06-10&step=100");
        assert_eq!(r.timeout, Duration::from_secs(20));
        assert_eq!(header(r, "Authorization"), Some("ApiKey K1"));
        assert_eq!(header(r, "Accept"), Some("application/json"));
        assert_eq!(header(r, "api-version"), Some("1.0"));
        assert_eq!(header(r, "Content-Type"), None);
        assert_eq!(r.headers.len(), 3);
        assert!(r.body.is_none());
    }

    #[tokio::test]
    async fn post_bodies_match_the_js_client() {
        let http = MockHttp::with(vec![resp(200, r#"{"id":1000}"#), resp(200, ""), resp(200, r#"{"id":1001}"#), resp(204, "")]);
        let auth = MockAuth::new("Bearer T", "", Ok(false));
        let c = client(&http, &auth);
        let start = StartLog { project_id: Some(JsonId::Num(7)), task_id: None, note: String::new(), date: "2025-06-10".into(), start: "2025-06-10T07:00:00.000Z".into() };
        assert_eq!(parse_log_id(&c.start_new_log(start).await.unwrap()), Some(JsonId::Num(1000)));
        c.stop_timer(&JsonId::Num(1000), "2025-06-10T07:30:00.000Z").await.unwrap();
        let ins = InsertLog {
            project_id: Some(JsonId::Num(7)),
            task_id: Some(JsonId::Num(3)),
            note: "work".into(),
            date: "2025-06-10".into(),
            start: "a".into(),
            end: "b".into(),
        };
        c.insert_log(ins).await.unwrap();
        c.delete_log(&JsonId::Str("a/b".into())).await.unwrap();

        let sent = http.sent();
        let body = |i: usize| serde_json::from_str::<Value>(sent[i].body.as_deref().unwrap()).unwrap();
        assert_eq!(sent[0].url, "https://api2.myhours.com/api/Logs/startNewLog");
        assert_eq!(sent[0].method, "POST");
        assert_eq!(header(&sent[0], "Content-Type"), Some("application/json"));
        assert_eq!(body(0), json!({"projectId": 7, "taskId": null, "note": "", "date": "2025-06-10", "start": "2025-06-10T07:00:00.000Z"}));
        assert_eq!(sent[1].url, "https://api2.myhours.com/api/Logs/stoptimer");
        assert_eq!(body(1), json!({"logId": 1000, "time": "2025-06-10T07:30:00.000Z"}));
        assert_eq!(sent[2].url, "https://api2.myhours.com/api/Logs/insertlog");
        assert_eq!(body(2), json!({"projectId": 7, "taskId": 3, "note": "work", "date": "2025-06-10", "start": "a", "end": "b"}));
        assert_eq!(sent[3].method, "DELETE");
        assert_eq!(sent[3].url, "https://api2.myhours.com/api/Logs/a%2Fb");
        assert!(sent[3].body.is_none());
        assert_eq!(header(&sent[3], "Content-Type"), None);
    }

    #[tokio::test]
    async fn unauthorized_refreshes_and_retries_once() {
        let http = MockHttp::with(vec![resp(401, "expired"), resp(200, r#"[{"id":1,"name":"P"}]"#)]);
        let auth = MockAuth::new("Bearer old", "Bearer new", Ok(true));
        let projects = client(&http, &auth).get_projects(None).await.unwrap();
        assert_eq!(projects, vec![NamedId { id: JsonId::Num(1), name: "P".into() }]);
        let sent = http.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(header(&sent[0], "Authorization"), Some("Bearer old"));
        assert_eq!(header(&sent[1], "Authorization"), Some("Bearer new"));
        assert_eq!(*auth.unauthorized_with.lock(), vec!["Bearer old".to_string()]);
    }

    #[tokio::test]
    async fn unauthorized_twice_gives_up_after_one_retry() {
        let http = MockHttp::with(vec![resp(401, "a"), resp(401, "b"), resp(200, "[]")]);
        let auth = MockAuth::new("Bearer old", "Bearer new", Ok(true));
        let err = client(&http, &auth).get_projects(None).await.unwrap_err();
        assert_eq!(err.status, Some(401));
        assert_eq!(err.detail, "b");
        assert_eq!(http.sent().len(), 2, "exactly one retry");
        assert_eq!(auth.unauthorized_with.lock().len(), 1);
    }

    #[tokio::test]
    async fn unauthorized_without_renewal_returns_the_401() {
        let http = MockHttp::with(vec![resp(401, "nope")]);
        let auth = MockAuth::new("Bearer old", "", Ok(false));
        let err = client(&http, &auth).get_logs("2025-06-10").await.unwrap_err();
        assert_eq!(err.status, Some(401));
        assert_eq!(http.sent().len(), 1);
    }

    #[tokio::test]
    async fn failing_renewal_propagates_its_own_error() {
        let http = MockHttp::with(vec![resp(401, "nope")]);
        let auth = MockAuth::new("Bearer old", "", Err(ApiError::network("token endpoint down")));
        let err = client(&http, &auth).get_logs("2025-06-10").await.unwrap_err();
        assert!(err.network);
        assert_eq!(err.message, "token endpoint down");
    }

    #[tokio::test]
    async fn key_override_bypasses_the_auth_provider() {
        let http = MockHttp::with(vec![resp(200, r#"[{"id":2,"name":"Q"}]"#), resp(401, "bad key")]);
        let auth = MockAuth::new("Bearer should-not-be-used", "x", Ok(true));
        let c = client(&http, &auth);
        let p = c.get_projects(Some("CANDIDATE")).await.unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(header(&http.sent()[0], "Authorization"), Some("ApiKey CANDIDATE"));
        // a 401 with a candidate key is final: no refresh, no retry
        let err = c.get_projects(Some("BAD")).await.unwrap_err();
        assert_eq!(err.status, Some(401));
        assert_eq!(http.sent().len(), 2);
        assert_eq!(auth.header_calls.load(Ordering::SeqCst), 0);
        assert!(auth.unauthorized_with.lock().is_empty());
    }

    #[tokio::test]
    async fn not_signed_in_fails_without_a_request() {
        let http = MockHttp::with(vec![]);
        let auth = MockAuth::new("x", "", Ok(false));
        *auth.header.lock() = None;
        let err = client(&http, &auth).get_logs("2025-06-10").await.unwrap_err();
        assert_eq!(err.status, Some(401));
        assert_eq!(err.message, "Not signed in");
        assert!(http.sent().is_empty());
    }

    #[tokio::test]
    async fn network_error_is_marked_as_network() {
        let http = MockHttp::with(vec![Err(HttpError { message: "connection refused".into() })]);
        let auth = MockAuth::new("Bearer t", "", Ok(false));
        let err = client(&http, &auth).get_logs("2025-06-10").await.unwrap_err();
        assert!(err.network);
        assert_eq!(err.status, None);
        assert_eq!(err.message, "Network error (GET /Logs): connection refused");
    }

    #[tokio::test]
    async fn non_2xx_carries_status_and_truncated_detail() {
        let long = "x".repeat(500);
        let http = MockHttp::with(vec![resp(500, &long), resp(400, "é".repeat(400).as_str())]);
        let auth = MockAuth::new("Bearer t", "", Ok(false));
        let c = client(&http, &auth);
        let err = c.get_logs("d").await.unwrap_err();
        assert_eq!(err.status, Some(500));
        assert!(!err.network);
        assert_eq!(err.message, "HTTP 500 (GET /Logs)");
        assert_eq!(err.detail.len(), 300);
        let err = c.stop_timer(&JsonId::Num(1), "t").await.unwrap_err();
        assert_eq!(err.message, "HTTP 400 (POST /Logs/stoptimer)");
        assert_eq!(err.detail.chars().count(), 300, "truncated by characters, never inside one");
    }

    #[tokio::test]
    async fn bodies_empty_json_and_plain_text() {
        let http = MockHttp::with(vec![resp(200, ""), resp(200, "  "), resp(200, "42"), resp(200, "just text")]);
        let auth = MockAuth::new("Bearer t", "", Ok(false));
        let c = client(&http, &auth);
        assert_eq!(c.get_logs("d").await.unwrap(), Value::Null);
        // whitespace is not valid JSON -> returned as a string, like JSON.parse failing in JS
        assert_eq!(c.get_logs("d").await.unwrap(), Value::String("  ".into()));
        assert_eq!(c.get_logs("d").await.unwrap(), json!(42));
        assert_eq!(c.get_logs("d").await.unwrap(), Value::String("just text".into()));
    }

    #[tokio::test]
    async fn get_tasks_uses_the_encoded_tasklist_path() {
        let http = MockHttp::with(vec![resp(200, r#"[{"id":1,"name":"T1"}]"#)]);
        let auth = MockAuth::new("Bearer t", "", Ok(false));
        let tasks = client(&http, &auth).get_tasks(&JsonId::Str("p 1".into())).await.unwrap();
        assert_eq!(tasks, vec![NamedId { id: JsonId::Num(1), name: "T1".into() }]);
        assert_eq!(http.sent()[0].url, "https://api2.myhours.com/api/Projects/p%201/tasklist");
    }
}
