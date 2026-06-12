//! Request history: every successful send is recorded to
//! `~/.restcraft/history.json` (newest first, capped at 50). "View Request
//! History" renders the entries into a plain .http document so each item can
//! be resent with the regular code lens — our replacement for the original's
//! QuickPick UI.
//!
//! Semantics mirror vscode-restclient `models/httpRequest.ts`
//! (`HistoricalHttpRequest`), the history half of `utils/userDataManager.ts`,
//! and `controllers/historyController.ts`. Like the original, the *final*
//! request is recorded — after variable substitution, exactly as sent.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use serde::{Deserialize, Serialize};

/// `UserDataManager.historyItemsMaxCount`.
pub const HISTORY_MAX_ITEMS: usize = 50;

/// One sent request, as actually sent (post-substitution).
/// Field-for-field the original `HistoricalHttpRequest`, except headers keep
/// their order as a pair list instead of an object map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricalRequest {
    /// Uppercase (the original constructor upper-cases; render re-asserts it).
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Unix epoch milliseconds (original `startTime = Date.now()`).
    pub start_time: i64,
}

impl HistoricalRequest {
    /// Original `HistoricalHttpRequest.convertFromHttpRequest` — stamps the
    /// current wall-clock time as `start_time`.
    pub fn new(
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<String>,
    ) -> Self {
        Self {
            method,
            url,
            headers,
            body,
            start_time: chrono::Utc::now().timestamp_millis(),
        }
    }
}

fn history_file_path() -> PathBuf {
    crate::settings::restcraft_home().join("history.json")
}

/// Prepends `request` to `~/.restcraft/history.json`, dropping anything past
/// [`HISTORY_MAX_ITEMS`] (original `UserDataManager.addToRequestHistory`:
/// unshift + slice). Call once per *successful* send.
pub fn record(request: HistoricalRequest) -> anyhow::Result<()> {
    record_to(&history_file_path(), request)
}

fn record_to(path: &Path, request: HistoricalRequest) -> anyhow::Result<()> {
    let mut entries = load_from(path);
    entries.insert(0, request);
    entries.truncate(HISTORY_MAX_ITEMS);
    save_to(path, &entries)
}

/// All recorded requests, newest first. Missing or corrupt file -> empty
/// (a broken cache must never block sending; the next record rewrites it).
pub fn load() -> Vec<HistoricalRequest> {
    load_from(&history_file_path())
}

fn load_from(path: &Path) -> Vec<HistoricalRequest> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Empties the history (original `UserDataManager.clearRequestHistory`
/// serializes `[]`). The caller is responsible for the confirmation prompt.
pub fn clear() -> anyhow::Result<()> {
    clear_at(&history_file_path())
}

fn clear_at(path: &Path) -> anyhow::Result<()> {
    save_to(path, &[])
}

fn save_to(path: &Path, entries: &[HistoricalRequest]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::settings::create_private_dir(parent)?;
    }
    let json = serde_json::to_vec_pretty(entries)?;
    // History can carry live Authorization headers/bodies: owner-only (0600)
    // same-dir temp file + rename, the cookie-jar pattern from http.rs.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let written = write_private_file(&tmp, &json)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| anyhow!("failed to write {}: {e}", path.display()));
    if written.is_err() {
        let _ = fs::remove_file(&tmp); // best-effort cleanup of the temp file
    }
    written
}

/// Creates/truncates `path` with mode 0600 on unix, then writes `buf`.
/// Mirrors `http.rs::write_private_file` (kept private there on purpose —
/// each persistence site documents its own threat model).
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

/// Renders the whole history as a valid .http document: per entry a
/// `### METHOD url | <relative time>` separator (comment text is ignored by
/// the parser) followed by the request exactly as the original's temp-file
/// renderer wrote it (`historyController.createRequestInTempFile`):
/// request line, headers, blank line, body. Each block re-parses through
/// `parser::parse_document`, so the regular "Send Request" code lens works.
pub fn render_history_http() -> String {
    render_entries(&load(), chrono::Utc::now().timestamp_millis())
}

fn render_entries(entries: &[HistoricalRequest], now_ms: i64) -> String {
    if entries.is_empty() {
        // The caller normally short-circuits with a showMessage instead
        // (original: "No request history items are found!").
        return "# No request history items are found!\n".to_string();
    }
    let mut out = String::new();
    for entry in entries {
        let method = entry.method.to_uppercase();
        out.push_str(&format!(
            "### {method} {} | {}\n",
            entry.url,
            relative_time(entry.start_time, now_ms)
        ));
        out.push_str(&format!("{method} {}\n", entry.url));
        for (name, value) in &entry.headers {
            out.push_str(&format!("{name}: {value}\n"));
        }
        // NOTE: body lines starting with `#`/`//` would re-parse as comments —
        // an inherent .http format limitation, same as hand-written files.
        if let Some(body) = entry.body.as_deref().filter(|b| !b.is_empty()) {
            out.push('\n');
            out.push_str(body);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// English relative time ("5 minutes ago"), bucketed like dayjs
/// `relativeTime` — the original shows `dayjs().to(startTime)` in its
/// QuickPick rows; we show it in the `###` separator comment.
fn relative_time(start_ms: i64, now_ms: i64) -> String {
    let secs = (now_ms - start_ms).max(0) / 1000;
    let mins = (secs + 30) / 60; // round to nearest, dayjs-style
    let hours = (mins + 30) / 60;
    let days = (hours + 12) / 24;
    let months = (days as f64 / 30.4).round() as i64;
    let years = (months + 6) / 12;
    if secs < 45 {
        "a few seconds ago".to_string()
    } else if secs < 90 {
        "a minute ago".to_string()
    } else if mins < 45 {
        format!("{mins} minutes ago")
    } else if mins < 90 {
        "an hour ago".to_string()
    } else if hours < 22 {
        format!("{hours} hours ago")
    } else if hours < 36 {
        "a day ago".to_string()
    } else if days < 26 {
        format!("{days} days ago")
    } else if days < 46 {
        "a month ago".to_string()
    } else if days < 320 {
        format!("{months} months ago")
    } else if days < 548 {
        "a year ago".to_string()
    } else {
        format!("{years} years ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{self, RequestBody};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("restcraft-history-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn history_path(&self) -> PathBuf {
            self.0.join("history.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(method: &str, url: &str) -> HistoricalRequest {
        HistoricalRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
            start_time: 1_000_000,
        }
    }

    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;

    #[test]
    fn load_missing_file_is_empty() {
        let tmp = TempDir::new();
        assert!(load_from(&tmp.history_path()).is_empty());
    }

    #[test]
    fn load_corrupt_file_is_empty() {
        let tmp = TempDir::new();
        let path = tmp.history_path();
        fs::write(&path, "{{{ not history").unwrap();
        assert!(load_from(&path).is_empty());

        // Wrong-but-valid JSON shape degrades the same way.
        fs::write(&path, r#"{"unexpected": "object"}"#).unwrap();
        assert!(load_from(&path).is_empty());

        // A record afterwards heals the file.
        record_to(&path, entry("GET", "https://example.com")).unwrap();
        assert_eq!(load_from(&path).len(), 1);
    }

    #[test]
    fn record_round_trips_every_field() {
        let tmp = TempDir::new();
        let path = tmp.history_path();
        let request = HistoricalRequest {
            method: "POST".to_string(),
            url: "https://example.com/users?q=1".to_string(),
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), "Bearer abc".to_string()),
            ],
            body: Some("{\"name\": \"도현\"}".to_string()),
            start_time: 1_700_000_000_000,
        };
        record_to(&path, request.clone()).unwrap();
        assert_eq!(load_from(&path), vec![request]);
    }

    #[test]
    fn record_orders_newest_first() {
        let tmp = TempDir::new();
        let path = tmp.history_path();
        record_to(&path, entry("GET", "https://example.com/first")).unwrap();
        record_to(&path, entry("GET", "https://example.com/second")).unwrap();

        let entries = load_from(&path);
        let urls: Vec<&str> = entries.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(
            urls,
            vec!["https://example.com/second", "https://example.com/first"]
        );
    }

    #[test]
    fn record_caps_at_max_items() {
        let tmp = TempDir::new();
        let path = tmp.history_path();
        for i in 0..HISTORY_MAX_ITEMS + 5 {
            record_to(&path, entry("GET", &format!("https://example.com/{i}"))).unwrap();
        }
        let entries = load_from(&path);
        assert_eq!(entries.len(), HISTORY_MAX_ITEMS);
        // Newest survives, oldest five were dropped.
        assert_eq!(entries[0].url, "https://example.com/54");
        assert_eq!(entries.last().unwrap().url, "https://example.com/5");
    }

    #[test]
    fn clear_empties_history() {
        let tmp = TempDir::new();
        let path = tmp.history_path();
        record_to(&path, entry("GET", "https://example.com")).unwrap();
        clear_at(&path).unwrap();
        assert!(load_from(&path).is_empty());
        // The file holds a valid empty list, not garbage.
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "[]");
    }

    #[cfg(unix)]
    #[test]
    fn history_file_saved_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempDir::new();
        let path = tmp.history_path();
        record_to(&path, entry("GET", "https://example.com")).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "history holds auth headers; must be 0600");
        assert!(
            !path.with_extension(format!("tmp.{}", std::process::id())).exists(),
            "temp file must be renamed away"
        );
    }

    #[test]
    fn render_round_trips_through_parser() {
        let entries = vec![
            HistoricalRequest {
                method: "POST".to_string(),
                url: "https://example.com/users".to_string(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("X-Api-Key".to_string(), "k1".to_string()),
                ],
                body: Some("{\n  \"name\": \"foo\"\n}".to_string()),
                start_time: 0,
            },
            entry("GET", "https://example.com/health"),
        ];
        let doc = render_entries(&entries, 5 * MIN);

        let parsed = parser::parse_document(&doc);
        assert_eq!(parsed.blocks.len(), entries.len());

        let base = std::env::temp_dir();
        let first = parser::parse_request(&parsed.blocks[0].text, &base).unwrap();
        assert_eq!(first.method, "POST");
        assert_eq!(first.url, "https://example.com/users");
        assert_eq!(
            first.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Api-Key".to_string(), "k1".to_string()),
            ]
        );
        assert_eq!(
            first.body,
            Some(RequestBody::Text("{\n  \"name\": \"foo\"\n}".to_string()))
        );

        let second = parser::parse_request(&parsed.blocks[1].text, &base).unwrap();
        assert_eq!(second.method, "GET");
        assert_eq!(second.url, "https://example.com/health");
        assert!(second.headers.is_empty());
        assert_eq!(second.body, None);
    }

    #[test]
    fn render_block_without_headers_keeps_body_after_blank_line() {
        let mut e = entry("POST", "https://example.com/echo");
        e.body = Some("hello".to_string());
        let doc = render_entries(&[e], 0);

        let parsed = parser::parse_document(&doc);
        let request =
            parser::parse_request(&parsed.blocks[0].text, &std::env::temp_dir()).unwrap();
        assert!(request.headers.is_empty());
        assert_eq!(request.body, Some(RequestBody::Text("hello".to_string())));
    }

    #[test]
    fn render_uppercases_method_and_shows_relative_time() {
        let mut e = entry("get", "https://example.com");
        e.start_time = 0;
        let doc = render_entries(&[e], 5 * MIN);
        assert!(doc.starts_with("### GET https://example.com | 5 minutes ago\n"));
        assert!(doc.contains("\nGET https://example.com\n"));
    }

    #[test]
    fn render_empty_history_is_harmless_comment() {
        let doc = render_entries(&[], 0);
        assert!(doc.starts_with('#'));
        assert!(parser::parse_document(&doc).blocks.is_empty());
    }

    #[test]
    fn relative_time_buckets_match_dayjs() {
        let cases: &[(i64, &str)] = &[
            (0, "a few seconds ago"),
            (44_000, "a few seconds ago"),
            (60_000, "a minute ago"),
            (5 * MIN, "5 minutes ago"),
            (44 * MIN, "44 minutes ago"),
            (60 * MIN, "an hour ago"),
            (5 * HOUR, "5 hours ago"),
            (24 * HOUR, "a day ago"),
            (5 * DAY, "5 days ago"),
            (30 * DAY, "a month ago"),
            (90 * DAY, "3 months ago"),
            (370 * DAY, "a year ago"),
            (800 * DAY, "2 years ago"),
        ];
        for (delta, expected) in cases {
            assert_eq!(
                relative_time(0, *delta),
                *expected,
                "delta = {delta}ms"
            );
        }
        // Clock skew (start in the future) clamps to "just now"-ish.
        assert_eq!(relative_time(10_000, 0), "a few seconds ago");
    }
}
