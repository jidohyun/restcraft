//! Response display: write to a stable file path, then open a tab via the
//! `zed` CLI. Zed has no `window/showDocument`, so the CLI spawn is the only
//! automatic open path; overwriting the same path makes Zed reload an
//! already-open clean buffer in place (cursor kept, focus not stolen).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::http::HttpResponse;

const MAX_FILE_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// Status line + headers + formatted body, written as `.txt`.
    Full,
    /// Body only, extension derived from the response MIME type
    /// (`.json`, `.xml`, `.html`, ...; fallback `.txt`).
    /// TODO(Phase 2): expose as a second send command; only `Full` is wired.
    #[allow(dead_code)]
    BodyOnly,
}

#[derive(Debug, Error)]
pub enum ShowError {
    #[error("failed to write response file: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "zed CLI not found — run 'cli: install cli binary' inside Zed, then resend. \
         Response saved at {0}"
    )]
    ZedCliMissing(PathBuf),
}

/// Stable path: `$TMPDIR/restcraft/<sanitized-name>.<ext>`. Stability is what
/// enables in-place tab refresh on resend.
pub fn response_file_path(
    request_name: &str,
    mode: DisplayMode,
    content_type: Option<&mime::Mime>,
) -> PathBuf {
    let ext = match mode {
        DisplayMode::Full => "txt",
        DisplayMode::BodyOnly => extension_for_mime(content_type),
    };
    std::env::temp_dir()
        .join("restcraft")
        .join(format!("{}.{ext}", sanitize_file_name(request_name)))
}

/// Strips path-unsafe characters; empty results fall back to `"response"`
/// (callers derive the incoming name from `@name` or a request-line hash, so
/// this is a last resort).
pub fn sanitize_file_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for c in name.chars() {
        let unsafe_char = c.is_control()
            || c.is_whitespace()
            || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
        if unsafe_char {
            if !last_was_dash && !out.is_empty() {
                out.push('-');
                last_was_dash = true;
            }
        } else {
            out.push(c);
            last_was_dash = false;
        }
    }
    let trimmed: String = out
        .trim_matches('-')
        .trim_start_matches('.') // no hidden files
        .chars()
        .take(MAX_FILE_NAME_CHARS)
        .collect();
    let trimmed = trimmed.trim_end_matches(['-', '.']).to_string();
    if trimmed.is_empty() {
        "response".to_string()
    } else {
        trimmed
    }
}

fn is_json_mime(m: &mime::Mime) -> bool {
    // mirrors MimeUtility.isJSON: application/json, text/json, *+json, x-amz-json*
    m.subtype() == mime::JSON
        || m.suffix() == Some(mime::JSON)
        || m.subtype().as_str().starts_with("x-amz-json")
}

fn is_xml_mime(m: &mime::Mime) -> bool {
    m.subtype() == mime::XML || m.suffix() == Some(mime::XML)
}

/// Body bytes that must pass through untouched (no text formatting).
fn is_binary_mime(m: &mime::Mime) -> bool {
    matches!(m.type_(), mime::IMAGE | mime::AUDIO | mime::VIDEO)
        || m.essence_str() == "application/octet-stream"
        || m.essence_str() == "application/pdf"
}

/// File extension for `BodyOnly` mode. Checked before XML so `image/svg+xml`
/// lands on `.svg`.
fn extension_for_mime(content_type: Option<&mime::Mime>) -> &'static str {
    let Some(m) = content_type else { return "txt" };
    if m.type_() == mime::IMAGE {
        return match m.subtype().as_str() {
            "png" => "png",
            "jpeg" => "jpg",
            "gif" => "gif",
            "webp" => "webp",
            "bmp" => "bmp",
            "svg+xml" | "svg" => "svg",
            _ => "bin",
        };
    }
    if is_json_mime(m) {
        return "json";
    }
    if is_xml_mime(m) {
        return "xml";
    }
    match m.essence_str() {
        "text/html" => "html",
        "application/javascript" | "text/javascript" => "js",
        "text/css" => "css",
        "application/pdf" => "pdf",
        "application/octet-stream" => "bin",
        _ => "txt",
    }
}

/// Port of ResponseFormatUtility.formatBody: JSON is prettified when the
/// declared type is JSON-ish, or as a fallback when the body parses as JSON
/// despite another declared type (#239). XML/CSS pretty-printing is TODO.
fn format_body_text(body: &str, content_type: Option<&mime::Mime>) -> String {
    let Some(m) = content_type else {
        return body.to_string();
    };
    if is_xml_mime(m) || m.essence_str() == "text/css" {
        return body.to_string();
    }
    // JSON-declared or fallback sniff — same prettify either way.
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| body.to_string()),
        Err(_) => body.to_string(),
    }
}

fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes}B")
    } else if b < MB {
        format!("{:.1}KB", b / KB)
    } else {
        format!("{:.2}MB", b / MB)
    }
}

/// Renders the response document. `Full` is the exchange view:
/// `# name | elapsed | size` meta comment, status line, headers, blank line,
/// formatted body. `BodyOnly` is just the (formatted) body — binary MIME
/// types pass through as raw bytes.
pub fn format_response(
    response: &HttpResponse,
    request_name: &str,
    mode: DisplayMode,
) -> Vec<u8> {
    let binary = response
        .content_type
        .as_ref()
        .is_some_and(is_binary_mime);

    match mode {
        DisplayMode::BodyOnly if binary => response.body.clone(),
        DisplayMode::BodyOnly => {
            let text = String::from_utf8_lossy(&response.body);
            format_body_text(&text, response.content_type.as_ref()).into_bytes()
        }
        DisplayMode::Full => {
            let mut out = String::new();
            out.push_str(&format!(
                "# {request_name} | {}ms | {}\n\n",
                response.elapsed.as_millis(),
                format_size(response.body.len()),
            ));
            out.push_str(&format!(
                "{} {} {}\n",
                response.version, response.status, response.status_text
            ));
            for (name, value) in &response.headers {
                out.push_str(&format!("{name}: {value}\n"));
            }
            out.push('\n');
            if binary {
                out.push_str(&format!(
                    "<binary body: {} — resend with body view for raw bytes>",
                    format_size(response.body.len())
                ));
            } else {
                let text = String::from_utf8_lossy(&response.body);
                out.push_str(&format_body_text(&text, response.content_type.as_ref()));
            }
            out.into_bytes()
        }
    }
}

/// Writes (overwrites) the response file, then opens it with `zed <path>`.
/// The write happens first so a missing CLI still leaves the file on disk —
/// `ShowError::ZedCliMissing` carries the path for the user-facing message.
pub fn show_response(
    response: &HttpResponse,
    request_name: &str,
    mode: DisplayMode,
) -> Result<PathBuf, ShowError> {
    let path = response_file_path(request_name, mode, response.content_type.as_ref());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, format_response(response, request_name, mode))?;
    open_in_zed(&path)?;
    Ok(path)
}

/// Spawns `zed <path>` detached. Single seam for the tab-open mechanism so it
/// can be swapped if Zed ever supports `window/showDocument`.
pub fn open_in_zed(path: &Path) -> Result<(), ShowError> {
    match Command::new("zed")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap in the background; the CLI exits as soon as it hands the
            // path to the running Zed instance.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ShowError::ZedCliMissing(path.to_path_buf()))
        }
        Err(e) => Err(ShowError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn mime(s: &str) -> mime::Mime {
        s.parse().unwrap()
    }

    fn response(content_type: Option<&str>, body: &[u8]) -> HttpResponse {
        HttpResponse {
            status: 200,
            status_text: "OK".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: vec![
                (
                    "content-type".to_string(),
                    content_type.unwrap_or("application/json").to_string(),
                ),
                ("x-custom".to_string(), "a".to_string()),
                ("x-custom".to_string(), "b".to_string()),
            ],
            body: body.to_vec(),
            content_type: content_type.map(mime),
            elapsed: Duration::from_millis(123),
        }
    }

    // --- sanitize_file_name ---

    #[test]
    fn sanitize_replaces_unsafe_chars_and_collapses() {
        assert_eq!(
            sanitize_file_name("my request/name: <test>"),
            "my-request-name-test"
        );
    }

    #[test]
    fn sanitize_empty_falls_back() {
        assert_eq!(sanitize_file_name(""), "response");
        assert_eq!(sanitize_file_name("///"), "response");
        assert_eq!(sanitize_file_name("..."), "response");
    }

    #[test]
    fn sanitize_keeps_unicode_and_truncates() {
        assert_eq!(sanitize_file_name("요청 1"), "요청-1");
        let long = "a".repeat(200);
        assert_eq!(sanitize_file_name(&long).chars().count(), 64);
    }

    // --- MIME mapping ---

    #[test]
    fn extension_mapping() {
        assert_eq!(extension_for_mime(Some(&mime("application/json"))), "json");
        assert_eq!(
            extension_for_mime(Some(&mime("application/vnd.api+json"))),
            "json"
        );
        assert_eq!(extension_for_mime(Some(&mime("text/html"))), "html");
        assert_eq!(extension_for_mime(Some(&mime("application/xml"))), "xml");
        assert_eq!(extension_for_mime(Some(&mime("image/svg+xml"))), "svg");
        assert_eq!(extension_for_mime(Some(&mime("image/png"))), "png");
        assert_eq!(extension_for_mime(Some(&mime("image/jpeg"))), "jpg");
        assert_eq!(extension_for_mime(Some(&mime("text/plain"))), "txt");
        assert_eq!(
            extension_for_mime(Some(&mime("application/octet-stream"))),
            "bin"
        );
        assert_eq!(extension_for_mime(None), "txt");
    }

    #[test]
    fn file_path_is_stable_and_extension_follows_mode() {
        let full = response_file_path("demo", DisplayMode::Full, Some(&mime("application/json")));
        assert!(full.ends_with(Path::new("restcraft/demo.txt")));

        let body = response_file_path("demo", DisplayMode::BodyOnly, Some(&mime("application/json")));
        assert!(body.ends_with(Path::new("restcraft/demo.json")));

        // same inputs → same path (in-place tab refresh relies on this)
        assert_eq!(
            full,
            response_file_path("demo", DisplayMode::Full, Some(&mime("application/json")))
        );
    }

    // --- formatting ---

    #[test]
    fn full_mode_renders_meta_status_headers_and_pretty_body() {
        let r = response(Some("application/json"), br#"{"a":1,"b":[2,3]}"#);
        let text = String::from_utf8(format_response(&r, "demo", DisplayMode::Full)).unwrap();

        assert!(text.starts_with("# demo | 123ms | 17B\n"));
        assert!(text.contains("HTTP/1.1 200 OK\n"));
        assert!(text.contains("content-type: application/json\n"));
        // duplicate headers preserved as separate lines
        assert!(text.contains("x-custom: a\n"));
        assert!(text.contains("x-custom: b\n"));
        assert!(text.contains("{\n  \"a\": 1,"));
    }

    #[test]
    fn body_only_json_is_prettified() {
        let r = response(Some("application/json"), br#"{"a":1}"#);
        let text = String::from_utf8(format_response(&r, "demo", DisplayMode::BodyOnly)).unwrap();
        assert_eq!(text, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn body_only_binary_passes_through_untouched() {
        let png = [0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00];
        let r = response(Some("image/png"), &png);
        assert_eq!(format_response(&r, "demo", DisplayMode::BodyOnly), png);
    }

    #[test]
    fn json_body_with_mislabeled_content_type_still_prettified() {
        // vscode-restclient #239: sniff JSON under inaccurate content types
        let r = response(Some("text/plain"), br#"{"a":1}"#);
        let text = String::from_utf8(format_response(&r, "demo", DisplayMode::BodyOnly)).unwrap();
        assert_eq!(text, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn invalid_json_left_as_is() {
        let r = response(Some("application/json"), b"not json");
        let text = String::from_utf8(format_response(&r, "demo", DisplayMode::BodyOnly)).unwrap();
        assert_eq!(text, "not json");
    }

    #[test]
    fn missing_content_type_leaves_body_untouched() {
        let r = response(None, br#"{"a":1}"#);
        let text = String::from_utf8(format_response(&r, "demo", DisplayMode::BodyOnly)).unwrap();
        assert_eq!(text, r#"{"a":1}"#);
    }

    #[test]
    fn size_humanized() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(4300), "4.2KB");
        assert_eq!(format_size(5_500_000), "5.25MB");
    }

    // --- write path (show_response itself spawns zed, so it is exercised
    // manually; here only the write half is covered to keep tests headless) ---

    #[test]
    fn formatted_response_round_trips_through_stable_path() {
        let r = response(Some("application/json"), br#"{"ok":true}"#);
        let name = "restcraft-write-test";
        let path = response_file_path(name, DisplayMode::Full, r.content_type.as_ref());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format_response(&r, name, DisplayMode::Full)).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(&format!("# {name} |")));
        assert!(written.contains("\"ok\": true"));
    }
}
