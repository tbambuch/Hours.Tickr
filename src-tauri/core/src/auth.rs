//! OAuth (OIDC Authorization Code + PKCE) against login.myhours.com.
//! The login window lives elsewhere; this module only builds URLs, parses callbacks and manages tokens.
//! Unofficial: uses the public client_id of the My Hours web application.
//! Tokens, authorization codes and verifiers must never be logged or put into error messages.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::clock::Clock;
use crate::error::ApiError;
use crate::http::{Http, HttpRequest};
use crate::logger::Logger;
use crate::model::Ms;

pub const AUTHORIZE_URL: &str = "https://login.myhours.com/connect/authorize";
pub const TOKEN_URL: &str = "https://login.myhours.com/connect/token";
pub const REVOCATION_URL: &str = "https://login.myhours.com/connect/revocation";
pub const CLIENT_ID: &str = "pkce_client";
pub const REDIRECT_URI: &str = "https://app.myhours.com";
pub const SCOPE: &str = "openid profile api offline_access";
pub const APP_ID: &str = "F45FF764-794C-4652-972E-1287F90C7B81";

const REFRESH_MARGIN_MS: i64 = 60_000;
const DEFAULT_TTL_SECS: f64 = 300.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------------------------
// PKCE and URLs
// ---------------------------------------------------------------------------------------------------------------

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    if getrandom::fill(&mut buf).is_err() {
        // The OS RNG failing is practically impossible; never panic in the login path. Best-effort fallback:
        // hash of clock + address entropy + counter, expanded to N bytes.
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut filled = 0;
        while filled < N {
            let mut h = Sha256::new();
            h.update(chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).to_le_bytes());
            h.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
            h.update(std::process::id().to_le_bytes());
            h.update(uuid::Uuid::new_v4().as_bytes());
            let digest = h.finalize();
            let n = (N - filled).min(digest.len());
            buf[filled..filled + n].copy_from_slice(&digest[..n]);
            filled += n;
        }
    }
    buf
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Verifier = base64url(48 random bytes) (64 chars), challenge = base64url(SHA-256(verifier)).
pub fn create_pkce() -> Pkce {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes::<48>());
    let challenge = challenge_for(&verifier);
    Pkce { verifier, challenge }
}

/// Random URL-safe token (32 chars) for `state` / `nonce`.
pub fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<24>())
}

pub fn build_authorize_url(challenge: &str, state: &str, nonce: &str) -> String {
    // AUTHORIZE_URL is a constant, valid URL; fall back to the plain string rather than panic.
    let mut u = match Url::parse(AUTHORIZE_URL) {
        Ok(u) => u,
        Err(_) => return AUTHORIZE_URL.to_string(),
    };
    u.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("nonce", nonce),
        ("AppId", APP_ID),
    ]);
    u.to_string()
}

/// True if `url` is the OAuth redirect target (`https://app.myhours.com[/...]`).
pub fn is_callback_url(url: &str) -> bool {
    match Url::parse(url) {
        Ok(u) => u.scheme() == "https" && u.host_str() == Some("app.myhours.com") && u.port().is_none(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Callback parsing
// ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallbackError {
    Invalid,
    Denied,
    /// Provider error code, sanitized to `[\w.-]`, at most 60 chars. Contains no secrets.
    Failed(String),
    State,
    NoCode,
}

impl CallbackError {
    pub fn key(&self) -> &'static str {
        match self {
            CallbackError::Invalid => "invalid",
            CallbackError::Denied => "denied",
            CallbackError::Failed(_) => "failed",
            CallbackError::State => "state",
            CallbackError::NoCode => "nocode",
        }
    }
}

fn parse_form(s: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(s.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

fn has(params: &[(String, String)], key: &str) -> bool {
    params.iter().any(|(k, _)| k == key)
}

fn get<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Parses a callback URL (query, or fragment as a fallback) or a form_post body string.
/// Returns the authorization code, or a [`CallbackError`] (never containing secrets).
pub fn parse_callback(url_or_body: &str, expected_state: &str) -> Result<String, CallbackError> {
    let is_url = url_or_body.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("http:"))
        || url_or_body.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("https:"));
    let params: Vec<(String, String)> = if is_url {
        let u = Url::parse(url_or_body).map_err(|_| CallbackError::Invalid)?;
        let mut p: Vec<(String, String)> = u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        if !has(&p, "code") && !has(&p, "error") {
            if let Some(frag) = u.fragment().filter(|f| !f.is_empty()) {
                p = parse_form(frag);
            }
        }
        p
    } else {
        parse_form(url_or_body.strip_prefix('?').unwrap_or(url_or_body))
    };
    if let Some(e) = get(&params, "error") {
        let detail: String = e.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.' || *c == '-').take(60).collect();
        return Err(if detail == "access_denied" { CallbackError::Denied } else { CallbackError::Failed(detail) });
    }
    match get(&params, "state") {
        Some(s) if !s.is_empty() && s == expected_state => {}
        _ => return Err(CallbackError::State),
    }
    match get(&params, "code") {
        Some(c) if !c.is_empty() => Ok(c.to_string()),
        _ => Err(CallbackError::NoCode),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// id_token (display only)
// ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentityInfo {
    pub name: String,
    pub email: String,
}

fn str_claim(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Decodes the id_token payload for display only (the signature is NOT verified).
pub fn parse_id_token(id_token: &str) -> IdentityInfo {
    let decode = || -> Option<IdentityInfo> {
        let part = id_token.split('.').nth(1)?;
        let bytes = URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok()?;
        let payload: Value = serde_json::from_slice(&bytes).ok()?;
        if !payload.is_object() {
            return None;
        }
        let name = str_claim(&payload, "name")
            .or_else(|| {
                let joined = [str_claim(&payload, "given_name"), str_claim(&payload, "family_name")].into_iter().flatten().collect::<Vec<_>>().join(" ");
                (!joined.is_empty()).then_some(joined)
            })
            .or_else(|| str_claim(&payload, "preferred_username"))
            .unwrap_or_default();
        Some(IdentityInfo { name, email: str_claim(&payload, "email").unwrap_or_default() })
    };
    decode().unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------------------------
// Token manager
// ---------------------------------------------------------------------------------------------------------------

pub struct AuthDeps {
    pub http: Arc<dyn Http>,
    pub clock: Arc<dyn Clock>,
    /// Persisted refresh token (stored encrypted by the caller).
    pub get_refresh_token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// Called when the server rotates the refresh token.
    pub save_refresh_token: Arc<dyn Fn(String) + Send + Sync>,
    pub log: Logger,
}

#[derive(Default)]
struct Tokens {
    access: Option<String>,
    expires_at: Ms,
}

/// Outcome of the most recent refresh, shared with callers that were waiting for it.
struct Flight {
    generation: u64,
    last: Result<(), ApiError>,
}

/// Resets the "refresh in progress" flag even if the refreshing future is dropped.
struct RefreshingGuard<'a>(&'a AtomicBool);

impl Drop for RefreshingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The access token is kept in memory only. Refreshes are single-flight: callers that arrive while a refresh is
/// running wait for it and share its result (success or error) instead of sending another token request.
pub struct AuthManager {
    deps: AuthDeps,
    tokens: Mutex<Tokens>,
    /// Held for the whole duration of a token refresh (the single flight).
    gate: tokio::sync::Mutex<()>,
    refreshing: AtomicBool,
    flight: Mutex<Flight>,
}

fn form_body(params: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(params.iter().copied()).finish()
}

fn sanitize_error_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.' || *c == '-').take(60).collect()
}

impl AuthManager {
    pub fn new(deps: AuthDeps) -> AuthManager {
        AuthManager {
            deps,
            tokens: Mutex::new(Tokens::default()),
            gate: tokio::sync::Mutex::new(()),
            refreshing: AtomicBool::new(false),
            flight: Mutex::new(Flight { generation: 0, last: Ok(()) }),
        }
    }

    async fn token_request(&self, params: &[(&str, &str)]) -> Result<Value, ApiError> {
        let req = HttpRequest {
            method: "POST",
            url: TOKEN_URL.to_string(),
            headers: vec![
                ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            body: Some(form_body(params)),
            timeout: REQUEST_TIMEOUT,
        };
        let res = match self.deps.http.send(req).await {
            Ok(r) => r,
            Err(_) => return Err(ApiError::network("Network error contacting login server")),
        };
        let json: Option<Value> = serde_json::from_str(&res.body).ok();
        if !res.ok() {
            let code = json.as_ref().and_then(|j| j.get("error")).and_then(Value::as_str).map(sanitize_error_code).unwrap_or_default();
            let permanent = matches!(res.status, 400 | 401 | 403);
            if permanent {
                self.deps.log.warn(format!(
                    "auth: token endpoint rejected the request (HTTP {}{})",
                    res.status,
                    if code.is_empty() { String::new() } else { format!(", {code}") }
                ));
                let what = if code.is_empty() { format!("HTTP {}", res.status) } else { code };
                let mut err = ApiError::http(401, format!("Login expired ({what})"), "");
                err.expired = true;
                return Err(err);
            }
            self.deps.log.warn(format!("auth: token endpoint temporary failure (HTTP {})", res.status));
            return Err(ApiError::http(res.status, format!("Login server error (HTTP {})", res.status), ""));
        }
        match json {
            Some(j) if j.get("access_token").is_some_and(Value::is_string) => Ok(j),
            _ => Err(ApiError::http(502, "Login server returned an unexpected response", "")),
        }
    }

    fn apply_tokens(&self, json: &Value) {
        let access = json.get("access_token").and_then(Value::as_str).unwrap_or_default().to_string();
        let ttl = match json.get("expires_in") {
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
        .filter(|t| t.is_finite())
        .unwrap_or(DEFAULT_TTL_SECS);
        {
            let mut t = self.tokens.lock();
            t.access = Some(access);
            t.expires_at = self.deps.clock.now_ms() + (ttl * 1000.0) as i64;
        }
        if let Some(rt) = json.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            (self.deps.save_refresh_token)(rt.to_string()); // rotation
        }
    }

    /// Exchanges the authorization code. Returns the id_token if the server sent one.
    pub async fn exchange_code(&self, code: &str, verifier: &str) -> Result<Option<String>, ApiError> {
        let json = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", REDIRECT_URI),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier),
            ])
            .await?;
        self.apply_tokens(&json);
        self.deps.log.info("auth: code exchanged, signed in");
        Ok(json.get("id_token").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string))
    }

    fn generation(&self) -> u64 {
        self.flight.lock().generation
    }

    async fn do_refresh(&self) -> Result<(), ApiError> {
        let rt = match (self.deps.get_refresh_token)() {
            Some(rt) if !rt.is_empty() => rt,
            _ => return Err(ApiError::not_signed_in()),
        };
        let json = self.token_request(&[("grant_type", "refresh_token"), ("refresh_token", &rt), ("client_id", CLIENT_ID)]).await?;
        self.apply_tokens(&json);
        self.deps.log.info("auth: access token refreshed");
        Ok(())
    }

    /// Single-flight refresh. `gen_before` is the flight generation observed before the caller decided to refresh:
    /// if a refresh finished since then, its result is shared instead of sending a new token request.
    async fn refresh_since(&self, gen_before: u64) -> Result<(), ApiError> {
        let _gate = self.gate.lock().await;
        {
            let f = self.flight.lock();
            if f.generation != gen_before {
                return f.last.clone();
            }
        }
        self.refreshing.store(true, Ordering::SeqCst);
        let _flag = RefreshingGuard(&self.refreshing);
        let res = self.do_refresh().await;
        let mut f = self.flight.lock();
        f.generation += 1;
        f.last = res.clone();
        res
    }

    pub async fn refresh(&self) -> Result<(), ApiError> {
        let gen_before = self.generation();
        self.refresh_since(gen_before).await
    }

    /// `"Bearer <token>"`; refreshes first when there is no token or fewer than 60 s remain.
    pub async fn auth_header(&self) -> Result<String, ApiError> {
        let gen_before = self.generation();
        let needs_refresh = {
            let t = self.tokens.lock();
            t.access.is_none() || t.expires_at - self.deps.clock.now_ms() < REFRESH_MARGIN_MS
        };
        if needs_refresh {
            self.refresh_since(gen_before).await?;
        }
        match &self.tokens.lock().access {
            Some(a) => Ok(format!("Bearer {a}")),
            None => Err(ApiError::not_signed_in()),
        }
    }

    /// Called on HTTP 401 with the header that was sent. `Ok(true)` = a (new) token is available, retry once.
    pub async fn handle_unauthorized(&self, used_header: &str) -> Result<bool, ApiError> {
        let gen_before = self.generation();
        if self.refreshing.load(Ordering::SeqCst) {
            self.refresh_since(gen_before).await?; // waits for the running refresh and shares its result
            return Ok(true);
        }
        {
            let mut t = self.tokens.lock();
            if let Some(access) = &t.access {
                if !used_header.is_empty() && used_header != format!("Bearer {access}") {
                    return Ok(true); // already refreshed by someone else
                }
            }
            t.access = None;
        }
        self.refresh_since(gen_before).await?;
        Ok(true)
    }

    /// Best-effort revocation of the refresh token, then forget the in-memory tokens.
    pub async fn revoke(&self) {
        let rt = (self.deps.get_refresh_token)();
        self.reset();
        let rt = match rt {
            Some(rt) if !rt.is_empty() => rt,
            _ => return,
        };
        let req = HttpRequest {
            method: "POST",
            url: REVOCATION_URL.to_string(),
            headers: vec![("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string())],
            body: Some(form_body(&[("token", &rt), ("token_type_hint", "refresh_token"), ("client_id", CLIENT_ID)])),
            timeout: REQUEST_TIMEOUT,
        };
        if self.deps.http.send(req).await.is_err() {
            self.deps.log.warn("auth: token revocation failed (ignored)");
        }
    }

    /// Forgets the in-memory access token (e.g. on logout or method switch).
    pub fn reset(&self) {
        let mut t = self.tokens.lock();
        t.access = None;
        t.expires_at = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;

    use super::*;
    use crate::clock::FakeClock;
    use crate::error::{classify_error, ErrorKind};
    use crate::http::{HttpError, HttpResponse};
    use crate::logger::sanitize;

    const API_URL: &str = "https://api2.myhours.com/api/Projects";

    fn resp(status: u16, body: Value) -> Result<HttpResponse, HttpError> {
        Ok(HttpResponse { status, body: if body.is_null() { String::new() } else { body.to_string() } })
    }
    fn resp_text(status: u16, body: &str) -> Result<HttpResponse, HttpError> {
        Ok(HttpResponse { status, body: body.to_string() })
    }
    fn net_err() -> Result<HttpResponse, HttpError> {
        Err(HttpError { message: "ECONNRESET".into() })
    }

    type Handler = Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, HttpError> + Send + Sync>;

    /// Mock `Http`: records requests, answers through a handler, optionally delays each response.
    struct MockHttp {
        calls: Mutex<Vec<HttpRequest>>,
        handler: Handler,
        delay: Duration,
    }

    impl MockHttp {
        fn new(handler: impl Fn(&HttpRequest) -> Result<HttpResponse, HttpError> + Send + Sync + 'static) -> Arc<MockHttp> {
            Arc::new(MockHttp { calls: Mutex::new(Vec::new()), handler: Box::new(handler), delay: Duration::ZERO })
        }
        fn delayed(delay: Duration, handler: impl Fn(&HttpRequest) -> Result<HttpResponse, HttpError> + Send + Sync + 'static) -> Arc<MockHttp> {
            Arc::new(MockHttp { calls: Mutex::new(Vec::new()), handler: Box::new(handler), delay })
        }
        fn calls(&self) -> Vec<HttpRequest> {
            self.calls.lock().clone()
        }
        fn calls_to(&self, url: &str) -> Vec<HttpRequest> {
            self.calls().into_iter().filter(|c| c.url == url).collect()
        }
    }

    #[async_trait]
    impl Http for MockHttp {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.calls.lock().push(req.clone());
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            (self.handler)(&req)
        }
    }

    fn form(req: &HttpRequest) -> HashMap<String, String> {
        url::form_urlencoded::parse(req.body.clone().unwrap_or_default().as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
    }
    fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
        req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    struct Fixture {
        auth: Arc<AuthManager>,
        store: Arc<Mutex<Option<String>>>,
        clock: Arc<FakeClock>,
        http: Arc<MockHttp>,
        logs: Arc<Mutex<Vec<String>>>,
    }

    fn fixture(http: Arc<MockHttp>) -> Fixture {
        fixture_with(http, Some("RT0"))
    }

    fn fixture_with(http: Arc<MockHttp>, refresh_token: Option<&str>) -> Fixture {
        let store = Arc::new(Mutex::new(refresh_token.map(str::to_string)));
        let clock = FakeClock::new(1_000_000);
        let (log, logs) = Logger::memory();
        let (s1, s2) = (store.clone(), store.clone());
        let auth = Arc::new(AuthManager::new(AuthDeps {
            http: http.clone(),
            clock: clock.clone(),
            get_refresh_token: Arc::new(move || s1.lock().clone()),
            save_refresh_token: Arc::new(move |t| *s2.lock() = Some(t)),
            log,
        }));
        Fixture { auth, store, clock, http, logs }
    }

    /// Minimal stand-in for the API client's 401 handling (GET with Authorization, one retry after refresh).
    async fn api_get(auth: &AuthManager, http: &MockHttp) -> Result<u16, ApiError> {
        let mut hdr = auth.auth_header().await?;
        let mut attempt = 0;
        loop {
            let req = HttpRequest {
                method: "GET",
                url: API_URL.to_string(),
                headers: vec![("Authorization".into(), hdr.clone())],
                body: None,
                timeout: Duration::from_secs(5),
            };
            let res = http.send(req).await.map_err(|e| ApiError::network(e.message))?;
            if res.status == 401 && attempt == 0 {
                attempt += 1;
                if auth.handle_unauthorized(&hdr).await? {
                    hdr = auth.auth_header().await?;
                    continue;
                }
            }
            return Ok(res.status);
        }
    }

    fn token_ok(at: &str) -> Result<HttpResponse, HttpError> {
        resp(200, serde_json::json!({ "access_token": at, "expires_in": 3600 }))
    }

    #[test]
    fn pkce_verifier_and_challenge() {
        let p = create_pkce();
        assert_eq!(p.verifier.len(), 64);
        assert!(p.verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(p.challenge, URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes())));
        assert_eq!(p.challenge.len(), 43);
        assert_ne!(create_pkce().verifier, p.verifier);
        // RFC 7636 appendix B test vector
        assert_eq!(challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(random_token().len(), 32);
        assert_ne!(random_token(), random_token());
    }

    #[test]
    fn authorize_url_contains_all_required_parameters() {
        let url = Url::parse(&build_authorize_url("CH", "ST", "NO")).unwrap();
        assert_eq!(format!("{}://{}{}", url.scheme(), url.host_str().unwrap(), url.path()), "https://login.myhours.com/connect/authorize");
        let p: HashMap<_, _> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        assert_eq!(p["response_type"], "code");
        assert_eq!(p["client_id"], "pkce_client");
        assert_eq!(p["redirect_uri"], "https://app.myhours.com");
        assert_eq!(p["scope"], "openid profile api offline_access");
        assert_eq!(p["code_challenge"], "CH");
        assert_eq!(p["code_challenge_method"], "S256");
        assert_eq!(p["state"], "ST");
        assert_eq!(p["nonce"], "NO");
        assert_eq!(p["AppId"], "F45FF764-794C-4652-972E-1287F90C7B81");
        assert_eq!(p.len(), 9);
        assert_eq!(CLIENT_ID, "pkce_client");
        assert_eq!(REDIRECT_URI, "https://app.myhours.com");
        assert_eq!(SCOPE, "openid profile api offline_access");
        assert_eq!(APP_ID, "F45FF764-794C-4652-972E-1287F90C7B81");
    }

    #[test]
    fn callback_parsing_success_state_denied_fragment_and_form_post() {
        assert_eq!(parse_callback("https://app.myhours.com/?code=abc&state=S1&scope=x", "S1"), Ok("abc".to_string()));
        assert_eq!(parse_callback("https://app.myhours.com/?code=abc&state=EVIL", "S1"), Err(CallbackError::State));
        assert_eq!(parse_callback("https://app.myhours.com/?code=abc", "S1"), Err(CallbackError::State));
        assert_eq!(parse_callback("https://app.myhours.com/?error=access_denied&state=S1", "S1"), Err(CallbackError::Denied));
        assert_eq!(parse_callback("https://app.myhours.com/?error=server_error", "S1"), Err(CallbackError::Failed("server_error".into())));
        assert_eq!(parse_callback("https://app.myhours.com/#code=frag&state=S1", "S1"), Ok("frag".to_string()));
        assert_eq!(parse_callback("code=posted&state=S1&scope=x", "S1"), Ok("posted".to_string())); // form_post body
        assert_eq!(parse_callback("https://app.myhours.com/?state=S1", "S1"), Err(CallbackError::NoCode));
        assert_eq!(parse_callback("https://", "S1"), Err(CallbackError::Invalid));
        assert!(is_callback_url("https://app.myhours.com/?code=1"));
        assert!(is_callback_url("https://app.myhours.com"));
        assert!(!is_callback_url("https://evil.example/?code=1"));
        assert!(!is_callback_url("https://app.myhours.com.evil.example/"));
        assert!(!is_callback_url("http://app.myhours.com/"));
        assert!(!is_callback_url("not a url"));
    }

    #[test]
    fn callback_error_detail_is_sanitized_and_keys() {
        let nasty = format!("https://app.myhours.com/?error={}", "a%20b%3Cscript%3E.-_".to_string() + &"x".repeat(100));
        match parse_callback(&nasty, "S1") {
            Err(CallbackError::Failed(d)) => {
                assert!(d.len() <= 60);
                assert!(d.starts_with("abscript.-_x"));
                assert!(d.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(CallbackError::Invalid.key(), "invalid");
        assert_eq!(CallbackError::Denied.key(), "denied");
        assert_eq!(CallbackError::Failed("x".into()).key(), "failed");
        assert_eq!(CallbackError::State.key(), "state");
        assert_eq!(CallbackError::NoCode.key(), "nocode");
    }

    #[tokio::test]
    async fn code_exchange_sends_form_body_and_keeps_refresh_token() {
        let http = MockHttp::new(|_| resp(200, serde_json::json!({ "access_token": "AT1", "expires_in": 3600, "refresh_token": "RT1", "id_token": "x.y.z" })));
        let f = fixture(http);
        let id = f.auth.exchange_code("CODE", "VERIFIER").await.unwrap();
        let c = &f.http.calls()[0];
        assert_eq!(c.url, TOKEN_URL);
        assert_eq!(c.method, "POST");
        assert_eq!(header(c, "Content-Type"), Some("application/x-www-form-urlencoded"));
        let body = form(c);
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code"], "CODE");
        assert_eq!(body["redirect_uri"], "https://app.myhours.com");
        assert_eq!(body["client_id"], "pkce_client");
        assert_eq!(body["code_verifier"], "VERIFIER");
        assert_eq!(f.store.lock().as_deref(), Some("RT1"));
        assert_eq!(id.as_deref(), Some("x.y.z"));
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer AT1");
        assert_eq!(f.http.calls().len(), 1);
    }

    #[tokio::test]
    async fn code_exchange_without_id_token_and_with_bad_response() {
        let f = fixture(MockHttp::new(|_| token_ok("A")));
        assert_eq!(f.auth.exchange_code("C", "V").await.unwrap(), None);
        // 200 without access_token -> 502
        let f = fixture(MockHttp::new(|_| resp(200, serde_json::json!({ "foo": 1 }))));
        let e = f.auth.exchange_code("C", "V").await.unwrap_err();
        assert_eq!(e.status, Some(502));
        assert!(!e.expired && !e.network);
        let f = fixture(MockHttp::new(|_| resp_text(200, "<html>")));
        assert_eq!(f.auth.exchange_code("C", "V").await.unwrap_err().status, Some(502));
        // bad code -> permanent, expired
        let f = fixture(MockHttp::new(|_| resp(400, serde_json::json!({ "error": "invalid_grant" }))));
        let e = f.auth.exchange_code("C", "V").await.unwrap_err();
        assert!(e.expired);
        assert_eq!(e.status, Some(401));
    }

    #[tokio::test]
    async fn refresh_form_body_rotation_and_keeps_old_token_if_none_returned() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let http = MockHttp::new(move |_| {
            if n2.fetch_add(1, Ordering::SeqCst) == 0 {
                resp(200, serde_json::json!({ "access_token": "A1", "expires_in": 3600, "refresh_token": "RT-ROTATED" }))
            } else {
                resp(200, serde_json::json!({ "access_token": "A2", "expires_in": 3600 }))
            }
        });
        let f = fixture(http);
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer A1");
        let body = form(&f.http.calls()[0]);
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "RT0");
        assert_eq!(body["client_id"], "pkce_client");
        assert_eq!(f.store.lock().as_deref(), Some("RT-ROTATED"));
        f.clock.advance(3600 * 1000);
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer A2");
        assert_eq!(form(&f.http.calls()[1])["refresh_token"], "RT-ROTATED");
        assert_eq!(f.store.lock().as_deref(), Some("RT-ROTATED"));
    }

    #[tokio::test]
    async fn proactive_refresh_when_less_than_60_s_remain() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let http = MockHttp::new(move |_| resp(200, serde_json::json!({ "access_token": format!("A{}", n2.fetch_add(1, Ordering::SeqCst) + 1), "expires_in": 300 })));
        let f = fixture(http);
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer A1");
        f.clock.advance(200 * 1000);
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer A1");
        f.clock.advance(50 * 1000);
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer A2");
        assert_eq!(f.http.calls().len(), 2);
    }

    #[tokio::test]
    async fn missing_expires_in_defaults_to_300_seconds() {
        let f = fixture(MockHttp::new(|_| resp(200, serde_json::json!({ "access_token": "A" }))));
        f.auth.auth_header().await.unwrap();
        f.clock.advance(200 * 1000);
        f.auth.auth_header().await.unwrap();
        assert_eq!(f.http.calls().len(), 1);
        f.clock.advance(50 * 1000);
        f.auth.auth_header().await.unwrap();
        assert_eq!(f.http.calls().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_requests_trigger_a_single_refresh() {
        let http = MockHttp::delayed(Duration::from_millis(100), |_| {
            resp(200, serde_json::json!({ "access_token": "AT", "expires_in": 3600, "refresh_token": "RT9" }))
        });
        let f = fixture(http);
        let a = f.auth.clone();
        let (r1, r2, r3, r4, r5) = tokio::join!(a.auth_header(), a.auth_header(), a.auth_header(), a.auth_header(), a.refresh());
        assert_eq!(r1.unwrap(), "Bearer AT");
        assert_eq!(r2.unwrap(), "Bearer AT");
        assert_eq!(r3.unwrap(), "Bearer AT");
        assert_eq!(r4.unwrap(), "Bearer AT");
        r5.unwrap();
        assert_eq!(f.http.calls().len(), 1);
        assert_eq!(f.store.lock().as_deref(), Some("RT9"));
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_requests_share_a_failed_refresh_too() {
        let http = MockHttp::delayed(Duration::from_millis(100), |_| resp(400, serde_json::json!({ "error": "invalid_grant" })));
        let f = fixture(http);
        let a = f.auth.clone();
        let (r1, r2, r3) = tokio::join!(a.auth_header(), a.auth_header(), a.refresh());
        for e in [r1.unwrap_err(), r2.unwrap_err(), r3.unwrap_err()] {
            assert!(e.expired);
        }
        assert_eq!(f.http.calls().len(), 1);
        // a later call is a new flight
        assert!(f.auth.refresh().await.is_err());
        assert_eq!(f.http.calls().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn unauthorized_while_refresh_in_flight_waits_for_it() {
        let http = MockHttp::delayed(Duration::from_millis(100), |_| token_ok("NEW"));
        let f = fixture(http);
        let a = f.auth.clone();
        let (h, un) = tokio::join!(a.auth_header(), async {
            tokio::task::yield_now().await;
            a.handle_unauthorized("Bearer OLD").await
        });
        assert_eq!(h.unwrap(), "Bearer NEW");
        assert!(un.unwrap());
        assert_eq!(f.http.calls().len(), 1);
    }

    #[tokio::test]
    async fn api_401_refresh_retry_succeeds() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let http = MockHttp::new(move |c| {
            if c.url == TOKEN_URL {
                token_ok(&format!("T{}", n2.fetch_add(1, Ordering::SeqCst) + 1))
            } else if header(c, "Authorization") == Some("Bearer T1") {
                resp_text(401, "")
            } else {
                resp(200, serde_json::json!([{ "id": 1, "name": "P" }]))
            }
        });
        let f = fixture(http);
        assert_eq!(api_get(&f.auth, &f.http).await.unwrap(), 200);
        let api = f.http.calls_to(API_URL);
        assert_eq!(api.len(), 2);
        assert_eq!(header(&api[0], "Authorization"), Some("Bearer T1"));
        assert_eq!(header(&api[1], "Authorization"), Some("Bearer T2"));
        assert_eq!(f.http.calls_to(TOKEN_URL).len(), 2);
    }

    #[tokio::test]
    async fn api_second_401_after_refresh_is_reported_without_looping() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let http = MockHttp::new(move |c| if c.url == TOKEN_URL { token_ok(&format!("T{}", n2.fetch_add(1, Ordering::SeqCst))) } else { resp_text(401, "") });
        let f = fixture(http);
        assert_eq!(api_get(&f.auth, &f.http).await.unwrap(), 401);
        assert_eq!(f.http.calls_to(API_URL).len(), 2);
        assert_eq!(f.http.calls_to(TOKEN_URL).len(), 2);
    }

    #[tokio::test]
    async fn api_invalid_grant_on_refresh_is_expired_auth_error_and_no_api_call() {
        let http = MockHttp::new(|c| if c.url == TOKEN_URL { resp(400, serde_json::json!({ "error": "invalid_grant" })) } else { resp(200, serde_json::json!([])) });
        let f = fixture(http);
        let e = api_get(&f.auth, &f.http).await.unwrap_err();
        assert!(e.expired);
        assert_eq!(e.status, Some(401));
        assert_eq!(e.message, "Login expired (invalid_grant)");
        assert_eq!(classify_error(&e), ErrorKind::Auth);
        assert_eq!(f.http.calls_to(API_URL).len(), 0);
    }

    #[tokio::test]
    async fn token_endpoint_status_mapping() {
        for status in [401u16, 403] {
            let f = fixture(MockHttp::new(move |_| resp_text(status, "")));
            let e = f.auth.refresh().await.unwrap_err();
            assert!(e.expired);
            assert_eq!(e.status, Some(401));
            assert_eq!(e.message, format!("Login expired (HTTP {status})"));
        }
        let f = fixture(MockHttp::new(|_| resp_text(404, "")));
        let e = f.auth.refresh().await.unwrap_err();
        assert_eq!(e.status, Some(404));
        assert!(!e.expired);
    }

    #[tokio::test]
    async fn token_endpoint_network_error_and_5xx_are_temporary() {
        let f = fixture(MockHttp::new(|_| net_err()));
        let e = f.auth.auth_header().await.unwrap_err();
        assert!(e.network && !e.expired);
        assert_eq!(classify_error(&e), ErrorKind::Temporary);
        let f = fixture(MockHttp::new(|_| resp_text(503, "")));
        let e = f.auth.auth_header().await.unwrap_err();
        assert_eq!(e.status, Some(503));
        assert!(!e.expired);
        assert_eq!(classify_error(&e), ErrorKind::Temporary);
    }

    #[tokio::test]
    async fn not_signed_in_without_refresh_token_sends_nothing() {
        let f = fixture_with(MockHttp::new(|_| token_ok("A")), None);
        let e = f.auth.auth_header().await.unwrap_err();
        assert_eq!(e.status, Some(401));
        assert!(e.expired);
        assert_eq!(f.http.calls().len(), 0);
        f.auth.revoke().await; // nothing to revoke
        assert_eq!(f.http.calls().len(), 0);
    }

    #[tokio::test]
    async fn handle_unauthorized_semantics() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let f = fixture(MockHttp::new(move |_| token_ok(&format!("T{}", n2.fetch_add(1, Ordering::SeqCst) + 1))));
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer T1");
        // the header used was already replaced by someone else: retry without a new token request
        assert!(f.auth.handle_unauthorized("Bearer STALE").await.unwrap());
        assert_eq!(f.http.calls().len(), 1);
        // same header (or none given): token is dropped and refreshed
        assert!(f.auth.handle_unauthorized("Bearer T1").await.unwrap());
        assert_eq!(f.http.calls().len(), 2);
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer T2");
        assert!(f.auth.handle_unauthorized("").await.unwrap());
        assert_eq!(f.auth.auth_header().await.unwrap(), "Bearer T3");
        // after reset there is no token: refresh regardless of the header
        f.auth.reset();
        assert!(f.auth.handle_unauthorized("Bearer T3").await.unwrap());
        assert_eq!(f.http.calls().len(), 4);
    }

    #[tokio::test]
    async fn reset_forgets_access_token() {
        let f = fixture(MockHttp::new(|_| token_ok("A")));
        f.auth.auth_header().await.unwrap();
        f.auth.reset();
        f.auth.auth_header().await.unwrap();
        assert_eq!(f.http.calls().len(), 2);
    }

    #[tokio::test]
    async fn revoke_posts_the_refresh_token_best_effort_and_swallows_failures() {
        let f = fixture(MockHttp::new(|_| resp_text(200, "")));
        f.auth.revoke().await;
        let c = &f.http.calls()[0];
        assert_eq!(c.url, REVOCATION_URL);
        let b = form(c);
        assert_eq!(b["token"], "RT0");
        assert_eq!(b["token_type_hint"], "refresh_token");
        assert_eq!(b["client_id"], "pkce_client");
        let bad = fixture(MockHttp::new(|_| net_err()));
        bad.auth.revoke().await;
        assert_eq!(bad.http.calls().len(), 1);
        assert!(bad.logs.lock().iter().any(|l| l.contains("revocation failed")));
        // revoke also forgets the access token
        let f = fixture(MockHttp::new(|c| if c.url == TOKEN_URL { token_ok("A") } else { resp_text(200, "") }));
        f.auth.auth_header().await.unwrap();
        f.auth.revoke().await;
        f.auth.auth_header().await.unwrap();
        assert_eq!(f.http.calls_to(TOKEN_URL).len(), 2);
    }

    #[test]
    fn id_token_payload_gives_name_and_email() {
        let enc = |v: Value| URL_SAFE_NO_PAD.encode(v.to_string());
        let t = format!("h.{}.s", enc(serde_json::json!({ "name": "Jan Novák", "email": "jan@example.com" })));
        assert_eq!(parse_id_token(&t), IdentityInfo { name: "Jan Novák".into(), email: "jan@example.com".into() });
        let t = format!("h.{}.s", enc(serde_json::json!({ "given_name": "Jan", "family_name": "Novák" })));
        assert_eq!(parse_id_token(&t), IdentityInfo { name: "Jan Novák".into(), email: String::new() });
        let t = format!("h.{}.s", enc(serde_json::json!({ "preferred_username": "jn" })));
        assert_eq!(parse_id_token(&t).name, "jn");
        assert_eq!(parse_id_token("garbage"), IdentityInfo::default());
        assert_eq!(parse_id_token("a.!!!.c"), IdentityInfo::default());
        assert_eq!(parse_id_token(&format!("h.{}.s", enc(serde_json::json!([1, 2])))), IdentityInfo::default());
    }

    #[tokio::test]
    async fn tokens_codes_and_verifiers_never_appear_in_log_output_or_errors() {
        let http = MockHttp::new(|c| {
            if form(c).get("grant_type").map(String::as_str) == Some("authorization_code") {
                resp(200, serde_json::json!({ "access_token": "SECRET-AT", "expires_in": 3600, "refresh_token": "SECRET-RT", "id_token": "SECRET-ID" }))
            } else {
                resp(400, serde_json::json!({ "error": "invalid_grant", "error_description": "SECRET-RT expired" }))
            }
        });
        let f = fixture(http);
        f.auth.exchange_code("SECRET-CODE", "SECRET-VERIFIER").await.unwrap();
        f.clock.advance(7200 * 1000);
        let err = f.auth.auth_header().await.unwrap_err();
        f.auth.revoke().await;
        let all = f.logs.lock().join("\n");
        assert!(!all.is_empty());
        let secrets = ["SECRET-AT", "SECRET-RT", "SECRET-ID", "SECRET-CODE", "SECRET-VERIFIER"];
        for s in secrets {
            assert!(!all.contains(s), "{s} leaked into the log");
            assert!(!err.message.contains(s) && !err.detail.contains(s), "{s} leaked into the error");
        }
        let leaked = sanitize("url https://app.myhours.com/?code=SECRET-CODE&state=1 Authorization: Bearer SECRET-AT refresh_token=SECRET-RT");
        for s in ["SECRET-CODE", "SECRET-AT", "SECRET-RT"] {
            assert!(!leaked.contains(s), "{s}");
        }
    }
}
