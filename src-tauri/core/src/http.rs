//! Minimal HTTP abstraction so the API client and the token manager can be tested with a mock.

use std::time::Duration;

use async_trait::async_trait;

#[derive(Clone, Debug)]
pub struct HttpRequest {
    /// "GET" | "POST" | "DELETE"
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The request never produced an HTTP response (DNS, connection, TLS, timeout...).
#[derive(Clone, Debug)]
pub struct HttpError {
    pub message: String,
}

#[async_trait]
pub trait Http: Send + Sync {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// Production implementation on top of reqwest + rustls.
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    pub fn new() -> Self {
        let client = reqwest::Client::builder().user_agent("Hours.Tickr").build().unwrap_or_default();
        ReqwestHttp { client }
    }
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Http for ReqwestHttp {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| HttpError { message: e.to_string() })?;
        let mut builder = self.client.request(method, &req.url).timeout(req.timeout);
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        }
        let res = builder.send().await.map_err(|e| HttpError {
            message: if e.is_timeout() { "timeout".to_string() } else { e.without_url().to_string() },
        })?;
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }
}
