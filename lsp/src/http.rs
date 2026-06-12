//! HTTP execution via reqwest: timeout/redirect settings, persistent cookie
//! jar, Basic auth shorthand normalization, GraphQL body wrapping.
//! Mirrors vscode-restclient `utils/httpClient.ts`.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use reqwest_cookie_store::{CookieStore, CookieStoreMutex};
use thiserror::Error;

use crate::parser::{ParsedRequest, RequestBody, RequestMetadata};
use crate::settings::HttpSettings;

// Hosted as a child of `http` so main.rs stays untouched (same pattern as
// `variables::request_vars`); the path attribute keeps the file at
// src/digest.rs. Digest auth is part of the HTTP execution layer anyway.
#[path = "digest.rs"]
pub mod digest;

const DEFAULT_USER_AGENT: &str = "restcraft";

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
    /// In received order; duplicates preserved. Names are lowercase — reqwest
    /// does not expose raw wire casing (TODO if it ever does).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub content_type: Option<mime::Mime>,
    pub elapsed: Duration,
}

fn cookie_file_path() -> PathBuf {
    crate::settings::restcraft_home().join("cookies.json")
}

/// Loads the persistent cookie jar from `~/.restcraft/cookies.json`
/// (created empty when missing).
pub fn load_cookie_jar() -> anyhow::Result<Arc<CookieStoreMutex>> {
    load_cookie_jar_from(&cookie_file_path())
}

fn load_cookie_jar_from(path: &Path) -> anyhow::Result<Arc<CookieStoreMutex>> {
    let store = match fs::File::open(path) {
        // A corrupt jar is a cache problem; it must never block sending, so
        // fall back to an empty store (the next save rewrites the file).
        #[allow(deprecated)] // cookie_store::serde needs a direct dep; not worth it
        Ok(file) => CookieStore::load_json(BufReader::new(file)).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CookieStore::default(),
        Err(e) => return Err(anyhow!("failed to open {}: {e}", path.display())),
    };
    Ok(Arc::new(CookieStoreMutex::new(store)))
}

/// Persists the jar back to `~/.restcraft/cookies.json` after each send.
pub fn save_cookie_jar(jar: &CookieStoreMutex) -> anyhow::Result<()> {
    save_cookie_jar_to(jar, &cookie_file_path())
}

fn save_cookie_jar_to(jar: &CookieStoreMutex, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::settings::create_private_dir(parent)?;
    }
    let mut buf = Vec::new();
    {
        let store = jar
            .lock()
            .map_err(|_| anyhow!("cookie jar mutex poisoned"))?;
        // Session cookies must survive LSP server restarts (tough-cookie-file-store
        // also persists them); expired ones get dropped again on load.
        #[allow(deprecated)]
        store
            .save_incl_expired_and_nonpersistent_json(&mut buf)
            .map_err(|e| anyhow!("failed to serialize cookie jar: {e}"))?;
    }
    // The jar holds live session/auth tokens: write owner-only (0600) via a
    // same-dir temp file + rename so the jar is never observable as a
    // world-readable or torn file. The rename also re-tightens any legacy
    // 0644 cookies.json left by older builds.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let written = write_private_file(&tmp, &buf)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| anyhow!("failed to write {}: {e}", path.display()));
    if written.is_err() {
        let _ = fs::remove_file(&tmp); // best-effort cleanup of the temp file
    }
    written
}

/// Creates/truncates `path` with mode 0600 on unix, then writes `buf`.
/// The explicit `set_permissions` makes the mode exact even under a
/// permissive umask or when the temp file survived a previous crash.
#[cfg(unix)]
fn write_private_file(path: &Path, buf: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(buf)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, buf: &[u8]) -> std::io::Result<()> {
    fs::write(path, buf)
}

/// Normalizes the three `Authorization: Basic` shorthands accepted by
/// vscode-restclient into the RFC base64 form:
/// - `Basic user:pass` (raw credentials)
/// - `Basic dXNlcjpwYXNz` (already base64 — passed through)
/// - `Basic user pass` (space-separated; pass may itself contain spaces)
pub fn normalize_basic_auth(value: &str) -> String {
    let mut parts = value.split_whitespace();
    let Some(scheme) = parts.next() else {
        return value.to_string();
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return value.to_string();
    }
    let Some(first) = parts.next() else {
        return value.to_string();
    };
    let rest: Vec<&str> = parts.collect();
    if !rest.is_empty() {
        // `Basic user pass [with spaces]` — original joins args with spaces.
        let pass = rest.join(" ");
        return format!("Basic {}", BASE64.encode(format!("{first}:{pass}")));
    }
    // Original splits on the first colon only for the user part; unlike the
    // original (which drops everything past a second colon) we keep the full
    // remainder as the password, per RFC 7617.
    if let Some((user, pass)) = first.split_once(':') {
        return format!("Basic {}", BASE64.encode(format!("{user}:{pass}")));
    }
    // Single token without colon: assume it is already base64.
    value.to_string()
}

/// Mirrors httpRequestParser.ts `createGraphQlBody`: the text after the first
/// blank line is a JSON `variables` object; `operationName` comes from a
/// `query <Name>` prefix when present.
fn build_graphql_body(body_text: &str) -> Result<String, HttpError> {
    let (query, variables_text) = split_graphql_body(body_text);

    let variables: serde_json::Value = match variables_text {
        Some(ref v) if !v.trim().is_empty() => serde_json::from_str(v)
            .map_err(|e| anyhow!("invalid GraphQL variables JSON: {e}"))?,
        _ => serde_json::json!({}),
    };

    let mut payload = serde_json::Map::new();
    payload.insert("query".into(), serde_json::Value::String(query.clone()));
    if let Some(name) = graphql_operation_name(&query) {
        payload.insert("operationName".into(), serde_json::Value::String(name));
    }
    payload.insert("variables".into(), variables);

    serde_json::to_string(&serde_json::Value::Object(payload))
        .map_err(|e| HttpError::Other(anyhow!("failed to serialize GraphQL payload: {e}")))
}

/// Splits on the first blank line: (query, Some(variables)) or (query, None).
fn split_graphql_body(text: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = text.lines().collect();
    match lines.iter().position(|l| l.trim().is_empty()) {
        Some(i) => (lines[..i].join("\n"), Some(lines[i + 1..].join("\n"))),
        None => (text.to_string(), None),
    }
}

/// Port of `/^\s*query\s+([^@\{\(\s]+)/i`.
fn graphql_operation_name(query: &str) -> Option<String> {
    let s = query.trim_start();
    let keyword = s.get(..5)?;
    if !keyword.eq_ignore_ascii_case("query") {
        return None;
    }
    let rest = s.get(5..)?;
    let trimmed = rest.trim_start();
    if trimmed.len() == rest.len() {
        // no whitespace after "query" — regex `\s+` requires one
        return None;
    }
    let name: String = trimmed
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '@' | '{' | '('))
        .collect();
    (!name.is_empty()).then_some(name)
}

fn resolve_body_bytes(request: &ParsedRequest) -> Result<Option<Vec<u8>>, HttpError> {
    match &request.body {
        None => Ok(None),
        Some(RequestBody::Text(text)) => Ok(Some(text.clone().into_bytes())),
        Some(RequestBody::File(path)) => fs::read(path).map(Some).map_err(|source| {
            HttpError::BodyFile {
                path: path.display().to_string(),
                source,
            }
        }),
    }
}

fn build_header_map(request: &ParsedRequest) -> Result<HeaderMap, HttpError> {
    let mut map = HeaderMap::new();
    for (name, value) in &request.headers {
        // Stripped before sending, mirroring httpRequestParser.ts: reqwest
        // computes content-length; X-Request-Type is our GraphQL marker.
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("x-request-type")
        {
            continue;
        }
        let value = if name.eq_ignore_ascii_case("authorization") {
            normalize_basic_auth(value)
        } else {
            value.clone()
        };
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| anyhow!("invalid header name {name:?}: {e}"))?;
        let header_value = HeaderValue::from_str(&value)
            .map_err(|e| anyhow!("invalid value for header {name}: {e}"))?;
        map.append(header_name, header_value);
    }
    Ok(map)
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
    let url = reqwest::Url::parse(&request.url)
        .map_err(|e| HttpError::InvalidUrl(format!("{}: {e}", request.url)))?;
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .map_err(|e| HttpError::Other(anyhow!("invalid method {:?}: {e}", request.method)))?;

    let follow = settings.follow_redirects && !metadata.no_redirect;
    let policy = if follow {
        Policy::limited(settings.max_redirects)
    } else {
        Policy::none()
    };

    let mut builder = reqwest::Client::builder()
        .redirect(policy)
        .user_agent(DEFAULT_USER_AGENT)
        // vscode-restclient sends with `rejectUnauthorized: false`
        .danger_accept_invalid_certs(true);
    if settings.timeout_ms > 0 {
        builder = builder.timeout(Duration::from_millis(settings.timeout_ms));
    }
    if !metadata.no_cookie_jar {
        builder = builder.cookie_provider(cookie_jar);
    }
    let client = builder.build()?;

    let mut headers = build_header_map(request)?;

    // `Authorization: Digest <user> <pass>` carries credentials, not a wire
    // header (httpClient.ts removes it and registers the digest hook): strip
    // it here and answer the 401 challenge after the first send.
    let digest_creds = headers
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(digest::parse_credentials);
    if digest_creds.is_some() {
        headers.remove(reqwest::header::AUTHORIZATION);
    }

    let mut body_bytes = resolve_body_bytes(request)?;
    if request.is_graphql() {
        let text = body_bytes
            .as_deref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        body_bytes = Some(build_graphql_body(&text)?.into_bytes());
    }

    // The digest retry resends the original request, so keep a copy of the
    // body only when digest credentials are armed.
    let retry_body = digest_creds.as_ref().and_then(|_| body_bytes.clone());

    let mut req = client
        .request(method.clone(), url.clone())
        .headers(headers.clone());
    if let Some(bytes) = body_bytes {
        req = req.body(bytes);
    }

    let start = Instant::now();
    let mut response = req.send().await?;

    // 401 + `WWW-Authenticate: Digest ...` → compute the digest Authorization
    // header and retry exactly once (the got afterResponse hook in digest.ts).
    if let Some(creds) = digest_creds {
        let auth = digest::answer_challenge(
            &creds,
            &request.method,
            response.status().as_u16(),
            response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            response.url(),
        );
        if let Some(auth) = auth {
            let mut retry = client
                .request(method, url)
                .headers(headers)
                .header(reqwest::header::AUTHORIZATION, auth);
            if let Some(bytes) = retry_body {
                retry = retry.body(bytes);
            }
            response = retry.send().await?;
        }
    }

    let status = response.status();
    let version = format!("{:?}", response.version()); // Debug prints "HTTP/1.1"
    let response_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<mime::Mime>().ok());

    let body = response.bytes().await?.to_vec();
    // TODO: phase breakdown (dns/connect/ttfb) like got's timings
    let elapsed = start.elapsed();

    Ok(HttpResponse {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        version,
        headers: response_headers,
        body,
        content_type,
        elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_basic_auth ---

    #[test]
    fn basic_raw_credentials_encoded() {
        assert_eq!(normalize_basic_auth("Basic user:pass"), "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn basic_base64_passed_through() {
        assert_eq!(
            normalize_basic_auth("Basic dXNlcjpwYXNz"),
            "Basic dXNlcjpwYXNz"
        );
    }

    #[test]
    fn basic_space_separated_encoded() {
        assert_eq!(normalize_basic_auth("Basic user pass"), "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn basic_password_with_spaces_joined() {
        let expected = format!("Basic {}", BASE64.encode("user:my secret pass"));
        assert_eq!(normalize_basic_auth("Basic user my secret pass"), expected);
    }

    #[test]
    fn basic_scheme_case_insensitive() {
        assert_eq!(normalize_basic_auth("basic user:pass"), "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn basic_password_containing_colon_kept_whole() {
        let expected = format!("Basic {}", BASE64.encode("user:pa:ss"));
        assert_eq!(normalize_basic_auth("Basic user:pa:ss"), expected);
    }

    #[test]
    fn non_basic_scheme_untouched() {
        assert_eq!(normalize_basic_auth("Bearer abc:def"), "Bearer abc:def");
        assert_eq!(normalize_basic_auth("Digest user pass"), "Digest user pass");
    }

    // --- GraphQL body wrapping ---

    #[test]
    fn graphql_query_only_gets_empty_variables() {
        let body = build_graphql_body("query { hero { name } }").unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["query"], "query { hero { name } }");
        assert_eq!(v["variables"], serde_json::json!({}));
        assert!(v.get("operationName").is_none());
    }

    #[test]
    fn graphql_variables_split_on_first_blank_line() {
        let body =
            build_graphql_body("query Hero($id: ID!) {\n  hero(id: $id) { name }\n}\n\n{\"id\": \"42\"}")
                .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["variables"], serde_json::json!({"id": "42"}));
        assert_eq!(v["operationName"], "Hero");
        assert!(v["query"].as_str().unwrap().contains("hero(id: $id)"));
    }

    #[test]
    fn graphql_operation_name_extraction() {
        assert_eq!(
            graphql_operation_name("query GetUser { user }"),
            Some("GetUser".to_string())
        );
        assert_eq!(graphql_operation_name("query { user }"), None);
        assert_eq!(graphql_operation_name("mutation AddUser { x }"), None);
        assert_eq!(graphql_operation_name("queryGetUser { x }"), None);
        assert_eq!(
            graphql_operation_name("  QUERY foo($a: Int) { x }"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn graphql_invalid_variables_is_error() {
        let err = build_graphql_body("query { x }\n\nnot-json").unwrap_err();
        assert!(err.to_string().contains("variables"));
    }

    #[test]
    fn graphql_detected_case_insensitively() {
        let request = ParsedRequest {
            method: "POST".into(),
            url: "https://example.com/graphql".into(),
            http_version: None,
            headers: vec![("X-Request-Type".into(), "graphql".into())],
            body: None,
        };
        assert!(request.is_graphql());
    }

    // --- cookie jar persistence ---

    #[test]
    fn cookie_jar_missing_file_starts_empty() {
        let dir = std::env::temp_dir().join("restcraft-test-jar-missing");
        let _ = fs::remove_dir_all(&dir);
        let jar = load_cookie_jar_from(&dir.join("cookies.json")).unwrap();
        assert_eq!(jar.lock().unwrap().iter_unexpired().count(), 0);
    }

    #[test]
    fn cookie_jar_round_trips_session_cookies() {
        let dir = std::env::temp_dir().join("restcraft-test-jar-roundtrip");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("cookies.json");

        let jar = load_cookie_jar_from(&path).unwrap();
        let url: reqwest::Url = "https://example.com/".parse().unwrap();
        jar.lock().unwrap().parse("sessionid=abc123", &url).unwrap();
        save_cookie_jar_to(&jar, &path).unwrap();

        let reloaded = load_cookie_jar_from(&path).unwrap();
        let store = reloaded.lock().unwrap();
        let cookie = store.get("example.com", "/", "sessionid").unwrap();
        assert_eq!(cookie.value(), "abc123");
    }

    /// Security: the jar holds session tokens, so the saved file must be
    /// owner-only (0600), a freshly created parent dir 0700, an existing
    /// world-readable jar re-tightened on save, and no temp file left behind.
    #[cfg(unix)]
    #[test]
    fn cookie_jar_saved_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join("restcraft-test-jar-perms");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("cookies.json");

        let jar = load_cookie_jar_from(&path).unwrap();
        let url: reqwest::Url = "https://example.com/".parse().unwrap();
        jar.lock().unwrap().parse("sessionid=abc123", &url).unwrap();
        save_cookie_jar_to(&jar, &path).unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "cookie jar must not be world-readable");
        assert_eq!(mode(&dir), 0o700, "created jar dir must be owner-only");
        assert!(
            !path.with_extension(format!("tmp.{}", std::process::id())).exists(),
            "temp file must be renamed away"
        );

        // A legacy world-readable jar gets re-tightened by the next save.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save_cookie_jar_to(&jar, &path).unwrap();
        assert_eq!(mode(&path), 0o600, "legacy 0644 jar must be re-tightened");
    }

    #[test]
    fn cookie_jar_corrupt_file_degrades_to_empty() {
        let dir = std::env::temp_dir().join("restcraft-test-jar-corrupt");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.json");
        fs::write(&path, "{{{ not cookies").unwrap();
        let jar = load_cookie_jar_from(&path).unwrap();
        assert_eq!(jar.lock().unwrap().iter_unexpired().count(), 0);
    }

    // --- live network (opt-in) ---

    #[tokio::test]
    #[ignore = "hits httpbin.org; run with cargo test -- --ignored"]
    async fn httpbin_get_round_trip() {
        let request = ParsedRequest {
            method: "GET".into(),
            url: "https://httpbin.org/get".into(),
            http_version: None,
            headers: vec![("Accept".into(), "application/json".into())],
            body: None,
        };
        let jar = Arc::new(CookieStoreMutex::default());
        let response = execute(
            &request,
            &RequestMetadata::default(),
            &HttpSettings::default(),
            jar,
        )
        .await
        .unwrap();

        assert_eq!(response.status, 200);
        assert!(response.version.starts_with("HTTP/"));
        assert!(response
            .content_type
            .as_ref()
            .is_some_and(|m| m.subtype() == mime::JSON));
        assert!(!response.body.is_empty());
    }
}
