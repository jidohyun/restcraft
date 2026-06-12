//! curl command import/export for .http documents.
//!
//! Import mirrors vscode-restclient `utils/curlRequestParser.ts` (yargs-parser
//! based) plus the `/^\s*curl/i` recognition regex in
//! `models/requestParserFactory.ts`. Export mirrors the `shell:curl`
//! httpsnippet output used by `controllers/codeSnippetController.ts`
//! ("Copy Request As cURL").
//!
//! Deliberate import differences from the original (each one closer to what a
//! real shell does, enabled by tokenizing with `shell-words` instead of
//! yargs-parser):
//! - The original collapses every whitespace run to a single space before
//!   parsing (a yargs-tokenizer workaround), which corrupts quoted bodies.
//!   Proper tokenization makes that pre-pass unnecessary, so quoted arguments
//!   keep their exact bytes here.
//! - Line continuations: the original strips only `\<LF>`/`\<CR>`; we also
//!   strip cmd.exe-style `^<newline>` so Windows-copied commands import, and
//!   we swallow the whole `\r\n` pair (the original leaves a stray `\n`
//!   behind on CRLF input).
//! - Common no-value curl switches (`-s`, `-v`, `-k`, `--silent`, short
//!   clusters like `-sS`, ...) never swallow the next token. The original
//!   feeds them to yargs, which makes `curl -s URL` lose its URL. Other
//!   unknown options still consume a following non-flag token, matching the
//!   yargs behavior the original relies on (`curl -o out.txt URL` works).
//! - `-I`/`--head` sets the method to HEAD only when no explicit
//!   `-X`/`--request` is present (the original's textual `-I` → `-X HEAD`
//!   substitution produces a broken array method when both appear).
//! - The method is uppercased per the `ParsedRequest::method` contract; the
//!   original passes `-X post` through as typed.

use std::iter::Peekable;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use thiserror::Error;

use crate::http::normalize_basic_auth;
use crate::parser::{ParsedRequest, RequestBody};

/// Default Content-Type curl applies to `-d` bodies (and the original to any
/// imported body without an explicit Content-Type header).
const DEFAULT_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

/// Methods recognized when splitting an attached `-XPOST` token. Mirrors the
/// original's `(-X)(GET|POST|…)` pre-pass regex, which is case-sensitive.
const METHODS: [&str; 19] = [
    "GET",
    "POST",
    "PUT",
    "DELETE",
    "PATCH",
    "HEAD",
    "OPTIONS",
    "CONNECT",
    "TRACE",
    "LOCK",
    "UNLOCK",
    "PROPFIND",
    "PROPPATCH",
    "COPY",
    "MOVE",
    "MKCOL",
    "MKCALENDAR",
    "ACL",
    "SEARCH",
];

/// `RequestParserFactory.curlRegex`: `/^\s*curl/i`. Like the original, this
/// matches any leading word starting with "curl" ("curling ..." included) —
/// no word boundary is applied.
pub fn is_curl(text: &str) -> bool {
    text.trim_start()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("curl"))
}

#[derive(Debug, Error)]
pub enum CurlError {
    #[error("curl command has no URL")]
    MissingUrl,
    #[error("malformed curl command: {0}")]
    Tokenize(#[from] shell_words::ParseError),
}

/// Parses a curl command block into a sendable request. `base_dir` is the
/// .http document's directory, used to resolve `-d @relative/path` bodies
/// (the original tries the workspace root first, then the document dir; the
/// LSP has no workspace-root notion, so only the document dir is tried).
pub fn parse_curl(text: &str, base_dir: &Path) -> Result<ParsedRequest, CurlError> {
    let merged = merge_continuation_lines(text.trim());
    let tokens = shell_words::split(&merged)?;

    let mut positionals: Vec<String> = Vec::new();
    let mut method: Option<String> = None;
    let mut head = false;
    let mut header_lines: Vec<String> = Vec::new();
    // One bucket per body switch, resolved with the original's
    // `d || data || data-ascii || data-binary || data-raw` priority chain.
    let mut data_buckets: [Vec<String>; 5] = Default::default();
    let mut cookie: Option<String> = None;
    let mut user: Option<String> = None;
    // URL fallbacks when there is no positional URL, in the original's
    // priority order: `L || location || compressed || url`.
    let mut url_fallbacks: [Option<String>; 4] = Default::default();

    let mut iter = tokens.into_iter().peekable();
    while let Some(token) = iter.next() {
        if !token.starts_with('-') || token == "-" {
            positionals.push(token);
            continue;
        }

        // `-XPOST` (attached, known uppercase method only) — mirrors the
        // original's regex that re-inserts the missing space.
        if let Some(rest) = token.strip_prefix("-X") {
            if METHODS.contains(&rest) {
                method = Some(rest.to_string());
                continue;
            }
        }

        let (name, attached) = split_attached(&token);
        match name {
            "-X" | "--request" => {
                if let Some(value) = take_value(attached, &mut iter) {
                    method = Some(value);
                }
            }
            "-H" | "--header" => {
                if let Some(value) = take_value(attached, &mut iter) {
                    header_lines.push(value);
                }
            }
            "-d" | "--data" | "--data-ascii" | "--data-binary" | "--data-raw" => {
                let bucket = match name {
                    "-d" => 0,
                    "--data" => 1,
                    "--data-ascii" => 2,
                    "--data-binary" => 3,
                    _ => 4,
                };
                if let Some(value) = take_value(attached, &mut iter) {
                    data_buckets[bucket].push(value);
                }
            }
            "-b" | "--cookie" => {
                if let Some(value) = take_value(attached, &mut iter) {
                    cookie = Some(value);
                }
            }
            "-u" | "--user" => {
                if let Some(value) = take_value(attached, &mut iter) {
                    user = Some(value);
                }
            }
            "-I" | "--head" => head = true,
            "-L" => url_fallbacks[0] = take_value(attached, &mut iter),
            "--location" => url_fallbacks[1] = take_value(attached, &mut iter),
            "--compressed" => url_fallbacks[2] = take_value(attached, &mut iter),
            "--url" => url_fallbacks[3] = take_value(attached, &mut iter),
            _ if is_no_value_flag(name) => {}
            _ => {
                // Unknown option: mirror yargs — a long option or a single
                // short option consumes the next non-flag token as its value;
                // a short cluster (`-sSL`) is a bag of boolean flags.
                let is_short_cluster = !token.starts_with("--") && token.len() > 2;
                if !is_short_cluster {
                    let _ = take_value(attached, &mut iter);
                }
            }
        }
    }

    // The original reads `parsedArguments._[1]` — the second positional,
    // `_[0]` being the "curl" word itself.
    let url = positionals
        .get(1)
        .cloned()
        .or_else(|| url_fallbacks.into_iter().flatten().next())
        .ok_or(CurlError::MissingUrl)?;

    let mut headers = parse_header_lines(&header_lines);

    // `-b a=1` — only cookie strings (not cookie-jar file paths) become a
    // header; the original keys on the presence of `=`.
    if let Some(cookie) = cookie {
        if cookie.contains('=') {
            set_header(&mut headers, "Cookie", cookie);
        }
    }

    if let Some(user) = user {
        set_header(
            &mut headers,
            "Authorization",
            format!("Basic {}", BASE64.encode(&user)),
        );
    }

    let mut body_text = data_buckets
        .iter()
        .find(|bucket| !bucket.is_empty())
        .map(|bucket| bucket.join("&"));
    if body_text.as_deref() == Some("") {
        body_text = None; // the original's `||` chain treats "" as no body
    }
    let body = body_text.map(|text| resolve_body(text, base_dir));

    if body.is_some()
        && !headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("Content-Type".to_string(), DEFAULT_CONTENT_TYPE.to_string()));
    }

    let method = match method {
        Some(method) => method.to_ascii_uppercase(),
        None if head => "HEAD".to_string(),
        None if body.is_some() => "POST".to_string(),
        None => "GET".to_string(),
    };

    Ok(ParsedRequest {
        method,
        url,
        http_version: None,
        headers,
        body,
    })
}

/// Renders `req` as a single-line `curl` command, equivalent to the
/// httpsnippet `shell:curl` output vscode-restclient copies. Pasting it into
/// a terminal sends the same request; the text differs cosmetically:
/// - single line — httpsnippet emits `\`-continuations with the `--request`,
///   `--url` and `--header` long options.
/// - every argument is single-quoted (`'` → `'\''`); httpsnippet quotes
///   conditionally. (Like httpsnippet's output, this targets POSIX shells
///   and PowerShell; cmd.exe has no single quotes.)
/// - the body is preserved byte-exact; vscode trims and joins body lines
///   before handing them to httpsnippet.
/// - the URL is emitted as written; vscode percent-encodes it first. Since
///   restcraft's own send path also receives the raw URL, the pasted command
///   matches what "Send Request" sends.
/// - a `Cookie` header stays `-H 'Cookie: …'` instead of `--cookie` (same
///   wire bytes).
/// - `-X` is always emitted so a re-import (`parse_curl`) keeps the method
///   even for GET-with-body or bodyless-POST requests.
/// - file bodies become `--data-binary '@path'`.
///
/// `Authorization: Basic user pass` shorthands are normalized to the base64
/// form like the original's HAR conversion; other schemes (e.g. the Digest
/// shorthand) pass through as-is, also matching the original.
pub fn to_curl(req: &ParsedRequest) -> String {
    let mut parts = vec![
        "curl".to_string(),
        "-X".to_string(),
        req.method.clone(),
        shell_quote(&req.url),
    ];
    for (name, value) in &req.headers {
        let value = if name.eq_ignore_ascii_case("authorization") {
            normalize_basic_auth(value)
        } else {
            value.clone()
        };
        parts.push("-H".to_string());
        parts.push(shell_quote(&format!("{name}: {value}")));
    }
    match &req.body {
        Some(RequestBody::Text(text)) => {
            parts.push("--data".to_string());
            parts.push(shell_quote(text));
        }
        Some(RequestBody::File(path)) => {
            parts.push("--data-binary".to_string());
            parts.push(shell_quote(&format!("@{}", path.display())));
        }
        None => {}
    }
    parts.join(" ")
}

/// Removes `\<newline>` (POSIX) and `^<newline>` (cmd.exe) line
/// continuations, swallowing the whole `\r`, `\n` or `\r\n` break so the
/// halves join exactly like a real shell joins them.
fn merge_continuation_lines(text: &str) -> String {
    let mut merged = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if (c == '\\' || c == '^') && matches!(chars.peek(), Some('\r' | '\n')) {
            if chars.next() == Some('\r') && chars.peek() == Some(&'\n') {
                chars.next();
            }
            continue;
        }
        merged.push(c);
    }
    merged
}

/// `--name=value` → (`--name`, `value`). Short options never carry attached
/// values here (yargs treats `-dfoo` as a flag cluster; the lone `-X<METHOD>`
/// exception is handled before this is called).
fn split_attached(token: &str) -> (&str, Option<String>) {
    if token.starts_with("--") {
        if let Some(eq) = token.find('=') {
            return (&token[..eq], Some(token[eq + 1..].to_string()));
        }
    }
    (token, None)
}

/// Option value: the `=`-attached part if present, else the next token when
/// it does not look like another option (yargs' rule).
fn take_value<I: Iterator<Item = String>>(
    attached: Option<String>,
    iter: &mut Peekable<I>,
) -> Option<String> {
    if attached.is_some() {
        return attached;
    }
    match iter.peek() {
        Some(next) if !next.starts_with('-') => iter.next(),
        _ => None,
    }
}

/// curl switches that never take a value. Not in the original (see module
/// doc); curated so `curl -s URL` and friends keep their URL.
fn is_no_value_flag(token: &str) -> bool {
    if let Some(cluster) = token.strip_prefix('-') {
        if !cluster.is_empty() && !cluster.starts_with('-') {
            return cluster.chars().all(|c| "sSvkifgG46".contains(c));
        }
    }
    matches!(
        token,
        "--silent"
            | "--show-error"
            | "--verbose"
            | "--insecure"
            | "--include"
            | "--fail"
            | "--globoff"
            | "--get"
    )
}

/// `parseRequestHeaders`: split each line on the first `:` (no `:` → empty
/// value); duplicate names (case-insensitive) merge into the first entry,
/// `;`-joined for Cookie and `,`-joined otherwise.
fn parse_header_lines(lines: &[String]) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        let (name, value) = match line.find(':') {
            Some(idx) => (line[..idx].trim(), line[idx + 1..].trim()),
            None => (line.trim(), ""),
        };
        if let Some(existing) = headers
            .iter_mut()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            let splitter = if existing.0.eq_ignore_ascii_case("cookie") {
                ';'
            } else {
                ','
            };
            existing.1.push(splitter);
            existing.1.push_str(value);
        } else {
            headers.push((name.to_string(), value.to_string()));
        }
    }
    headers
}

/// Sets `name` to `value`, replacing an existing entry case-insensitively
/// (keeping its original spelling) — `-b`/`-u` win over `-H` duplicates,
/// like the original's object-key assignment.
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some(existing) = headers
        .iter_mut()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        existing.1 = value;
    } else {
        headers.push((name.to_string(), value));
    }
}

/// `@file` bodies: an existing file becomes a file body; otherwise the text
/// minus the `@` is used literally (original fallback). Relative paths
/// resolve against the document directory.
fn resolve_body(text: String, base_dir: &Path) -> RequestBody {
    let Some(path) = text.strip_prefix('@') else {
        return RequestBody::Text(text);
    };
    let candidate = PathBuf::from(path);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        base_dir.join(candidate)
    };
    if resolved.exists() {
        RequestBody::File(resolved)
    } else {
        RequestBody::Text(path.to_string())
    }
}

/// POSIX single-quote escaping: wrap in `'…'` with embedded `'` as `'\''`.
fn shell_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for c in text.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_dir() -> &'static Path {
        Path::new("/tmp/restcraft-curl-doc-dir")
    }

    fn parse(text: &str) -> ParsedRequest {
        parse_curl(text, base_dir()).unwrap()
    }

    /// Unique-per-test temp file (no tempfile dev-dependency needed).
    fn temp_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("restcraft-curl-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn assert_req_eq(a: &ParsedRequest, b: &ParsedRequest) {
        assert_eq!(a.method, b.method);
        assert_eq!(a.url, b.url);
        assert_eq!(a.headers, b.headers);
        assert_eq!(a.body, b.body);
    }

    // -- is_curl ------------------------------------------------------------

    #[test]
    fn is_curl_mirrors_the_original_regex() {
        assert!(is_curl("curl http://example.com"));
        assert!(is_curl("  CURL http://example.com"));
        assert!(is_curl("\n\tCurl -X POST http://example.com"));
        // No word boundary in the original regex — quirk preserved.
        assert!(is_curl("curling"));
        assert!(!is_curl("# curl http://example.com"));
        assert!(!is_curl("wget http://example.com"));
        assert!(!is_curl("cur"));
        assert!(!is_curl(""));
    }

    // -- parse_curl ----------------------------------------------------------

    #[test]
    fn simple_get() {
        let req = parse("curl http://example.com/api?x=1&y=2");
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "http://example.com/api?x=1&y=2");
        assert_eq!(req.http_version, None);
        assert!(req.headers.is_empty());
        assert_eq!(req.body, None);
    }

    #[test]
    fn method_and_headers_in_order() {
        let req = parse(
            "curl -X POST http://x.com -H 'Content-Type: application/json' -H 'X-A: one'",
        );
        assert_eq!(req.method, "POST");
        assert_eq!(
            req.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-A".to_string(), "one".to_string()),
            ]
        );
    }

    #[test]
    fn attached_method_and_equals_syntax() {
        assert_eq!(parse("curl -XPUT http://x.com").method, "PUT");
        assert_eq!(parse("curl --request=PATCH http://x.com").method, "PATCH");
        let req = parse("curl --header='X-B: two' http://x.com");
        assert_eq!(req.header("x-b"), Some("two"));
        // Lowercase method is uppercased (ParsedRequest contract).
        assert_eq!(parse("curl -X post http://x.com").method, "POST");
    }

    #[test]
    fn data_implies_post_and_default_content_type() {
        let req = parse("curl http://x.com -d 'a=1'");
        assert_eq!(req.method, "POST");
        assert_eq!(req.header("content-type"), Some(DEFAULT_CONTENT_TYPE));
        assert_eq!(req.body, Some(RequestBody::Text("a=1".to_string())));

        // An explicit Content-Type suppresses the default.
        let req = parse("curl http://x.com -H 'content-type: application/json' -d '{}'");
        assert_eq!(req.header("Content-Type"), Some("application/json"));
    }

    #[test]
    fn repeated_data_joins_with_ampersand_and_buckets_keep_priority() {
        let req = parse("curl http://x.com -d a=1 -d b=2");
        assert_eq!(req.body, Some(RequestBody::Text("a=1&b=2".to_string())));

        // `d || data || data-ascii || data-binary || data-raw` chain: the
        // `-d` bucket wins even when listed later.
        let req = parse("curl http://x.com --data-raw raw --data dee");
        assert_eq!(req.body, Some(RequestBody::Text("dee".to_string())));
        let req = parse("curl http://x.com --data-binary bin -d short");
        assert_eq!(req.body, Some(RequestBody::Text("short".to_string())));
    }

    #[test]
    fn user_becomes_basic_auth_header() {
        let req = parse("curl -u user:pass http://x.com");
        assert_eq!(req.header("authorization"), Some("Basic dXNlcjpwYXNz"));
        // `-u` overrides an `-H` Authorization, keeping its spelling.
        let req = parse("curl -H 'authorization: Bearer t' -u user:pass http://x.com");
        assert_eq!(req.headers.len(), 1);
        assert_eq!(req.headers[0].0, "authorization");
        assert_eq!(req.header("Authorization"), Some("Basic dXNlcjpwYXNz"));
    }

    #[test]
    fn cookie_flag_requires_pair_syntax() {
        let req = parse("curl -b 'a=1; b=2' http://x.com");
        assert_eq!(req.header("cookie"), Some("a=1; b=2"));
        // A cookie-jar file path (no `=`) is ignored, like the original.
        let req = parse("curl -b jarfile.txt http://x.com");
        assert_eq!(req.header("cookie"), None);
    }

    #[test]
    fn head_flag_sets_method_unless_explicit() {
        assert_eq!(parse("curl -I http://x.com").method, "HEAD");
        assert_eq!(parse("curl --head http://x.com").method, "HEAD");
        assert_eq!(parse("curl -I -X OPTIONS http://x.com").method, "OPTIONS");
    }

    #[test]
    fn url_fallbacks_for_flag_swallowed_urls() {
        // yargs quirk the original depends on: `-L URL` parses as L=URL.
        assert_eq!(parse("curl -L http://x.com").url, "http://x.com");
        assert_eq!(parse("curl --location http://x.com").url, "http://x.com");
        assert_eq!(parse("curl --compressed http://x.com").url, "http://x.com");
        assert_eq!(parse("curl --url http://x.com -X POST").url, "http://x.com");
        // A positional URL still wins.
        let req = parse("curl http://a.com -L");
        assert_eq!(req.url, "http://a.com");
    }

    #[test]
    fn multiline_continuations_backslash_and_caret() {
        let req = parse("curl http://x.com \\\n  -H 'X-A: one' \\\r\n  -d hello");
        assert_eq!(req.method, "POST");
        assert_eq!(req.header("x-a"), Some("one"));
        assert_eq!(req.body, Some(RequestBody::Text("hello".to_string())));

        // cmd.exe-style `^` continuation (extension over the original).
        let req = parse("curl http://x.com ^\r\n -H \"X-B: two\"");
        assert_eq!(req.header("x-b"), Some("two"));
    }

    #[test]
    fn quoted_body_bytes_are_preserved() {
        // The original's whitespace-collapsing would mangle this body; proper
        // tokenization keeps newlines and double spaces inside quotes.
        let req = parse(
            "curl http://x.com -H 'Content-Type: application/json' --data '{\n  \"a\": \"b  c\"\n}'",
        );
        assert_eq!(
            req.body,
            Some(RequestBody::Text("{\n  \"a\": \"b  c\"\n}".to_string()))
        );
    }

    #[test]
    fn at_file_body_resolves_when_present() {
        let abs = temp_file("at-body.json", "{\"k\":1}");
        let req = parse(&format!("curl http://x.com -d @{}", abs.display()));
        assert_eq!(req.method, "POST");
        assert_eq!(req.body, Some(RequestBody::File(abs.clone())));

        // Relative paths resolve against the document directory.
        let dir = abs.parent().unwrap();
        let req = parse_curl("curl http://x.com -d @at-body.json", dir).unwrap();
        assert_eq!(req.body, Some(RequestBody::File(dir.join("at-body.json"))));
    }

    #[test]
    fn at_file_missing_falls_back_to_literal_text() {
        let req = parse("curl http://x.com -d @/definitely/not/here.json");
        assert_eq!(
            req.body,
            Some(RequestBody::Text("/definitely/not/here.json".to_string()))
        );
        assert_eq!(req.method, "POST");
    }

    #[test]
    fn no_value_flags_do_not_eat_the_url() {
        assert_eq!(parse("curl -s http://x.com").url, "http://x.com");
        assert_eq!(parse("curl -sS -v -k http://x.com").url, "http://x.com");
        assert_eq!(parse("curl --silent --insecure http://x.com").url, "http://x.com");
    }

    #[test]
    fn unknown_value_options_consume_their_value() {
        // yargs-equivalent: `out.txt` must not be mistaken for the URL.
        assert_eq!(parse("curl -o out.txt http://x.com").url, "http://x.com");
        assert_eq!(
            parse("curl --output out.txt http://x.com").url,
            "http://x.com"
        );
        // Unknown short clusters are boolean bags (yargs) — no value taken.
        assert_eq!(parse("curl -vL4 http://x.com").url, "http://x.com");
    }

    #[test]
    fn duplicate_headers_merge_like_the_original() {
        let req = parse("curl http://x.com -H 'X-D: a' -H 'x-d: b' -H 'Cookie: a=1' -H 'cookie: b=2'");
        assert_eq!(req.header("x-d"), Some("a,b"));
        assert_eq!(req.header("cookie"), Some("a=1;b=2"));
    }

    #[test]
    fn missing_url_is_an_error() {
        assert!(matches!(parse_curl("curl -X POST", base_dir()), Err(CurlError::MissingUrl)));
        assert!(matches!(parse_curl("curl", base_dir()), Err(CurlError::MissingUrl)));
        // Unbalanced quotes surface the tokenizer error.
        assert!(matches!(
            parse_curl("curl 'http://x.com", base_dir()),
            Err(CurlError::Tokenize(_))
        ));
    }

    // -- to_curl -------------------------------------------------------------

    #[test]
    fn to_curl_renders_single_line_with_quotes() {
        let req = ParsedRequest {
            method: "POST".to_string(),
            url: "http://x.com/a?b=1&c=2".to_string(),
            http_version: None,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: Some(RequestBody::Text("{\"k\":\"v\"}".to_string())),
        };
        assert_eq!(
            to_curl(&req),
            "curl -X POST 'http://x.com/a?b=1&c=2' -H 'Content-Type: application/json' --data '{\"k\":\"v\"}'"
        );
    }

    #[test]
    fn to_curl_escapes_single_quotes() {
        let req = ParsedRequest {
            method: "POST".to_string(),
            url: "http://x.com".to_string(),
            http_version: None,
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body: Some(RequestBody::Text("it's".to_string())),
        };
        assert!(to_curl(&req).ends_with("--data 'it'\\''s'"));
    }

    #[test]
    fn to_curl_normalizes_basic_auth_shorthand() {
        let req = ParsedRequest {
            method: "GET".to_string(),
            url: "http://x.com".to_string(),
            http_version: None,
            headers: vec![("Authorization".to_string(), "Basic user pass".to_string())],
            body: None,
        };
        assert_eq!(
            to_curl(&req),
            "curl -X GET 'http://x.com' -H 'Authorization: Basic dXNlcjpwYXNz'"
        );

        // Non-Basic schemes pass through untouched (matches the original).
        let req = ParsedRequest {
            headers: vec![("Authorization".to_string(), "Digest user pass".to_string())],
            ..req
        };
        assert!(to_curl(&req).contains("'Authorization: Digest user pass'"));
    }

    // -- round trips ----------------------------------------------------------

    #[test]
    fn round_trip_text_body() {
        let req = ParsedRequest {
            method: "PUT".to_string(),
            url: "https://example.com/items/1?flag=a&x='y'".to_string(),
            http_version: None,
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Custom".to_string(), "a, b".to_string()),
            ],
            body: Some(RequestBody::Text("{\n  \"name\": \"caf\u{e9} 'quoted'\"\n}".to_string())),
        };
        let reparsed = parse_curl(&to_curl(&req), base_dir()).unwrap();
        assert_req_eq(&req, &reparsed);
    }

    #[test]
    fn round_trip_file_body() {
        let path = temp_file("round-trip.bin", "payload");
        let req = ParsedRequest {
            method: "POST".to_string(),
            url: "https://example.com/upload".to_string(),
            http_version: None,
            headers: vec![("Content-Type".to_string(), "application/octet-stream".to_string())],
            body: Some(RequestBody::File(path)),
        };
        let reparsed = parse_curl(&to_curl(&req), base_dir()).unwrap();
        assert_req_eq(&req, &reparsed);
    }

    #[test]
    fn round_trip_bare_get_and_get_with_body() {
        let req = ParsedRequest {
            method: "GET".to_string(),
            url: "https://example.com".to_string(),
            http_version: None,
            headers: vec![],
            body: None,
        };
        let reparsed = parse_curl(&to_curl(&req), base_dir()).unwrap();
        assert_req_eq(&req, &reparsed);

        // The always-emitted `-X` keeps a GET-with-body a GET on re-import.
        let req = ParsedRequest {
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body: Some(RequestBody::Text("q".to_string())),
            ..req
        };
        let reparsed = parse_curl(&to_curl(&req), base_dir()).unwrap();
        assert_req_eq(&req, &reparsed);
    }
}
