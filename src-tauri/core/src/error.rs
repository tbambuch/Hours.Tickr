//! One error type for everything that talks to My Hours (API calls and token requests).
//! Mirrors the JS `ApiError` / `AuthError`: `status` / `network` drive [`classify_error`].

use std::fmt;

#[derive(Debug, Clone, Default)]
pub struct ApiError {
    /// Never contains credentials.
    pub message: String,
    pub status: Option<u16>,
    /// The request never produced an HTTP response.
    pub network: bool,
    /// Response body excerpt (<= 300 chars), for the log only.
    pub detail: String,
    /// Token endpoint: the refresh token / grant is no longer valid, the user must sign in again.
    pub expired: bool,
}

impl ApiError {
    pub fn http(status: u16, message: impl Into<String>, detail: impl Into<String>) -> Self {
        ApiError { message: message.into(), status: Some(status), detail: detail.into(), ..Default::default() }
    }
    pub fn network(message: impl Into<String>) -> Self {
        ApiError { message: message.into(), network: true, ..Default::default() }
    }
    /// "Not signed in": behaves like a 401.
    pub fn not_signed_in() -> Self {
        ApiError { message: "Not signed in".into(), status: Some(401), expired: true, ..Default::default() }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 401 / 403: credentials are wrong or expired; retrying is pointless until they change.
    Auth,
    /// Network failure, 408, 429, 5xx, or anything unknown: retry later.
    Temporary,
    /// Other 4xx: this particular operation can never succeed.
    Permanent,
}

pub fn classify_error(err: &ApiError) -> ErrorKind {
    if err.network {
        return ErrorKind::Temporary;
    }
    match err.status {
        Some(401) | Some(403) => ErrorKind::Auth,
        Some(408) | Some(429) => ErrorKind::Temporary,
        Some(s) if (500..=599).contains(&s) => ErrorKind::Temporary,
        Some(s) if (400..=499).contains(&s) => ErrorKind::Permanent,
        _ => ErrorKind::Temporary, // unknown failure: be conservative and retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(classify_error(&ApiError::network("x")), ErrorKind::Temporary);
        assert_eq!(classify_error(&ApiError::http(401, "x", "")), ErrorKind::Auth);
        assert_eq!(classify_error(&ApiError::http(403, "x", "")), ErrorKind::Auth);
        assert_eq!(classify_error(&ApiError::http(429, "x", "")), ErrorKind::Temporary);
        assert_eq!(classify_error(&ApiError::http(408, "x", "")), ErrorKind::Temporary);
        assert_eq!(classify_error(&ApiError::http(503, "x", "")), ErrorKind::Temporary);
        assert_eq!(classify_error(&ApiError::http(400, "x", "")), ErrorKind::Permanent);
        assert_eq!(classify_error(&ApiError::http(404, "x", "")), ErrorKind::Permanent);
        assert_eq!(classify_error(&ApiError::default()), ErrorKind::Temporary);
        assert_eq!(classify_error(&ApiError::not_signed_in()), ErrorKind::Auth);
    }
}
