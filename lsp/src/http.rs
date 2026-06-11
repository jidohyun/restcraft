//! HTTP execution via reqwest: timeout/redirect settings, persistent cookie
//! jar, Basic auth shorthand normalization, GraphQL body wrapping.
//! Mirrors vscode-restclient `utils/httpClient.ts`.

#![allow(dead_code)] // scaffold: wired up incrementally

use std::sync::Arc;
use std::time::Duration;

use reqwest_cookie_store::CookieStoreMutex;
use thiserror::Error;

use crate::parser::{ParsedRequest, RequestMetadata};
use crate::settings::HttpSettings;

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("failed to read body file {path}: {source}")]
    BodyFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Everything `response.rs` needs to render and persist a response.
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    /// Canonical reason phrase ("OK", "Not Found", ...).
    pub status_text: String,
    /// "HTTP/1.1" etc.
    pub version: String,
    /// In received order; duplicates preserved.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub content_type: Option<mime::Mime>,
    pub elapsed: Duration,
}

/// Loads the persistent cookie jar from `~/.restcraft/cookies.json`
/// (created empty when missing).
pub fn load_cookie_jar() -> anyhow::Result<Arc<CookieStoreMutex>> {
    todo!()
}

/// Persists the jar back to `~/.restcraft/cookies.json` after each send.
pub fn save_cookie_jar(jar: &CookieStoreMutex) -> anyhow::Result<()> {
    let _ = jar;
    todo!()
}

/// Normalizes the three `Authorization: Basic` shorthands accepted by
/// vscode-restclient into the RFC base64 form:
/// - `Basic user:pass` (raw credentials)
/// - `Basic dXNlcjpwYXNz` (already base64 — passed through)
/// - `Basic user pass` (space-separated)
pub fn normalize_basic_auth(value: &str) -> String {
    let _ = value;
    todo!("port vscode-restclient utils/httpClient.ts Authorization handling")
}

/// Sends the request:
/// - timeout from `settings.timeout_ms` (0 = none)
/// - redirects: `settings.follow_redirects` unless `metadata.no_redirect`
/// - cookie jar attached unless `metadata.no_cookie_jar`
/// - `normalize_basic_auth` applied to the Authorization header
/// - GraphQL (`X-Request-Type: GraphQL`): body split on first blank line into
///   query/variables and wrapped as `{"query": ..., "variables": ...}`
pub async fn execute(
    request: &ParsedRequest,
    metadata: &RequestMetadata,
    settings: &HttpSettings,
    cookie_jar: Arc<CookieStoreMutex>,
) -> Result<HttpResponse, HttpError> {
    let _ = (request, metadata, settings, cookie_jar);
    todo!()
}
