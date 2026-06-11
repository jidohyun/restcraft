//! Stage 1 (`parse_document`) and stage 3 (`parse_request`) of the send pipeline.
//! Stage 2 (variable substitution) lives in `variables.rs` and runs on
//! `RequestBlock::text` between the two stages.
//!
//! Semantics mirror vscode-restclient `utils/selector.ts`,
//! `utils/httpRequestParser.ts`, `utils/requestParserUtil.ts` and
//! `utils/httpVariableProviders/fileVariableProvider.ts`.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// 0-based, end-exclusive line range within a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

/// `# @...` metadata comments attached to a request block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestMetadata {
    /// `# @name foo` — also used as the response file name.
    pub name: Option<String>,
    /// `# @no-redirect`
    pub no_redirect: bool,
    /// `# @no-cookie-jar`
    pub no_cookie_jar: bool,
}

/// One `###`-delimited request block, before variable substitution.
#[derive(Debug, Clone)]
pub struct RequestBlock {
    /// Full extent of the block, including its leading `###` separator and
    /// comment lines. Unlike vscode-restclient (cursor on a separator selects
    /// nothing), the separator belongs to the block below it so code actions
    /// work anywhere in the section.
    pub range: LineRange,
    /// Line the request line sits on — where the "Send Request" code lens goes.
    pub lens_line: u32,
    pub metadata: RequestMetadata,
    /// Request text (request line + headers + body) with comments and
    /// metadata lines stripped. Variables are NOT substituted yet.
    pub text: String,
}

/// `@name = value` file-level variable, raw (value may reference `{{other}}`).
/// All definitions are kept in document order; the original resolves with
/// last-definition-wins (`Map.set` per line), so lookups should scan in reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVariable {
    pub name: String,
    pub value: String,
    pub line: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedDocument {
    pub blocks: Vec<RequestBlock>,
    pub file_variables: Vec<FileVariable>,
}

/// Stage 1: split a .http/.rest document into `###` request blocks and
/// collect `@var =` file variables. `#`/`//` comment lines are recognized,
/// `# @name`/`# @no-redirect`/`# @no-cookie-jar` become metadata.
pub fn parse_document(text: &str) -> ParsedDocument {
    let lines = split_lines(text);

    // The original scans every line of the document, even inside blocks.
    let mut file_variables = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some((name, raw_value)) = parse_file_variable_line(line) {
            file_variables.push(FileVariable {
                name: name.to_string(),
                value: unescape_file_variable_value(raw_value),
                line: idx as u32,
            });
        }
    }

    let mut boundaries: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_separator_line(line))
        .map(|(idx, _)| idx)
        .collect();
    boundaries.push(lines.len());

    let mut blocks = Vec::new();
    let mut range_start = 0usize; // separator line (or 0 for the first section)
    let mut content_start = 0usize; // first line after the separator
    for boundary in boundaries {
        if let Some(block) = build_block(&lines, range_start, content_start, boundary) {
            blocks.push(block);
        }
        range_start = boundary;
        content_start = boundary + 1;
    }

    ParsedDocument {
        blocks,
        file_variables,
    }
}

/// Returns the block containing `line` (code lens/action and executeCommand
/// both address blocks by line).
pub fn block_at_line(document: &ParsedDocument, line: u32) -> Option<&RequestBlock> {
    document
        .blocks
        .iter()
        .find(|block| block.range.start <= line && line < block.range.end)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    Text(String),
    /// `< ./payload.json` external body; path resolved against the .http file dir.
    /// `<@ path` / `<@encoding path` also land here — use [`input_file_ref`]
    /// on the pre-substitution block text to detect the `@` (process
    /// variables) flavor. TODO(Phase 2): substitute variables in `<@` file
    /// content and honor the encoding hint (MVP reads files as-is).
    File(PathBuf),
}

/// A fully substituted, sendable request.
#[derive(Debug, Clone)]
pub struct ParsedRequest {
    /// Uppercase. `GET` when the request line omits the method.
    pub method: String,
    pub url: String,
    /// `HTTP/1.1` etc. when present on the request line. Parsed and kept for
    /// fidelity; reqwest picks the protocol itself, so nothing reads it yet.
    #[allow(dead_code)]
    pub http_version: Option<String>,
    /// First-occurrence order. Duplicate names (case-insensitive) are merged
    /// into the first entry — `;`-joined for Cookie, `,`-joined otherwise —
    /// mirroring vscode-restclient `parseRequestHeaders`.
    pub headers: Vec<(String, String)>,
    pub body: Option<RequestBody>,
}

impl ParsedRequest {
    /// Case-insensitive lookup of the first header with `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `X-Request-Type: GraphQL` — http.rs must wrap the body into
    /// `{"query": ..., "variables": ...}` (see [`build_graphql_body`]) and
    /// drop the `X-Request-Type` header before sending.
    pub fn is_graphql(&self) -> bool {
        self.header("x-request-type")
            .is_some_and(|v| v.eq_ignore_ascii_case("graphql"))
    }
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("request block contains no request line")]
    EmptyBlock,
    #[error("malformed request line: {0}")]
    MalformedRequestLine(String),
}

/// Stage 3: parse substituted block text into a sendable request.
/// Handles: method omission (defaults to GET), multi-line query continuation
/// lines starting with `?`/`&`, headers, blank-line-separated body, and
/// `< filepath` external bodies (resolved against `base_dir`).
pub fn parse_request(text: &str, base_dir: &Path) -> Result<ParsedRequest, ParseError> {
    let lines = split_lines(text);

    enum State {
        Url,
        Header,
        Body,
    }

    let mut request_parts: Vec<&str> = Vec::new();
    let mut header_lines: Vec<&str> = Vec::new();
    let mut body_lines: Vec<&str> = Vec::new();

    let mut state = State::Url;
    let mut i = 0;
    while i < lines.len() {
        let current = lines[i];
        let next = lines.get(i + 1).copied();
        match state {
            State::Url => {
                request_parts.push(current.trim());
                match next {
                    None => {}
                    Some(n) if is_query_continuation_line(n) => {}
                    Some(n) if !n.trim().is_empty() => state = State::Header,
                    Some(_) => {
                        i += 1; // swallow the blank line between request line and body
                        state = State::Body;
                    }
                }
            }
            State::Header => {
                header_lines.push(current.trim());
                if let Some(n) = next {
                    if n.trim().is_empty() {
                        i += 1; // swallow the blank line between headers and body
                        state = State::Body;
                    }
                }
            }
            State::Body => body_lines.push(current),
        }
        i += 1;
    }

    // The original joins the (trimmed) request + query continuation lines
    // with an empty string.
    let request_line: String = request_parts.concat();
    if request_line.trim().is_empty() {
        return Err(ParseError::EmptyBlock);
    }

    let (method, rest) = split_method(&request_line);
    let (url, http_version) = strip_http_version(rest.trim());
    let mut url = url.trim().to_string();
    if url.is_empty() {
        return Err(ParseError::MalformedRequestLine(request_line.clone()));
    }

    let mut headers = parse_headers(&header_lines);
    // Let the HTTP client recalculate the content length.
    headers.retain(|(n, _)| !n.eq_ignore_ascii_case("content-length"));

    // If a Host header is provided and the url is a relative path, combine them.
    if url.starts_with('/') {
        let host = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.clone());
        if let Some(host) = host {
            let port = host.split(':').nth(1);
            let scheme = if port == Some("443") || port == Some("8443") {
                "https"
            } else {
                "http"
            };
            url = format!("{scheme}://{host}{url}");
        }
    }

    let content_type = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone());
    let body = parse_body(&body_lines, content_type.as_deref(), base_dir);

    Ok(ParsedRequest {
        method,
        url,
        http_version,
        headers,
        body,
    })
}

/// A `< path` / `<@ path` / `<@encoding path` external body reference.
/// Public so the send pipeline can detect `<@` lines (whose file content must
/// be variable-substituted) in the pre-substitution block text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputFileRef<'a> {
    /// `<@` flavor: the file content goes through variable substitution.
    pub process_variables: bool,
    /// Raw path as written; resolve relative paths against the document dir.
    pub path: &'a str,
}

/// `/^<(?:(?<processVariables>@)(?<encoding>\w+)?)?\s+(?<filepath>.+?)\s*$/`
/// No leading whitespace is allowed before `<`.
pub fn input_file_ref(line: &str) -> Option<InputFileRef<'_>> {
    let rest = line.strip_prefix('<')?;
    let (process_variables, rest) = match rest.strip_prefix('@') {
        Some(after_at) => {
            let encoding_end = after_at
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after_at.len());
            // TODO(Phase 2): honor `after_at[..encoding_end]` as the file encoding.
            (true, &after_at[encoding_end..])
        }
        None => (false, rest),
    };
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let path = rest.trim();
    if path.is_empty() {
        return None;
    }
    Some(InputFileRef {
        process_variables,
        path,
    })
}

// ---------------------------------------------------------------------------
// Document splitting (selector.ts)
// ---------------------------------------------------------------------------

fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// `/^#{3,}/` — no leading whitespace allowed.
fn is_separator_line(line: &str) -> bool {
    line.starts_with("###")
}

/// `/^\s*(#|\/{2})/`
fn is_comment_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('#') || trimmed.starts_with("//")
}

fn is_empty_line(line: &str) -> bool {
    line.trim().is_empty()
}

/// `/^\s*HTTP\/[\d.]+/` — a saved response section, not a request.
fn is_response_status_line(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("HTTP/")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit() || c == '.')
}

fn is_file_variable_line(line: &str) -> bool {
    parse_file_variable_line(line).is_some()
}

/// `/^\s*@([^\s=]+)\s*=\s*(.*?)\s*$/` → `(name, raw value)`.
fn parse_file_variable_line(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix('@')?;
    let name_end = rest.find(|c: char| c.is_whitespace() || c == '=')?;
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    let value = rest[name_end..].trim_start().strip_prefix('=')?;
    Some((name, value.trim()))
}

/// Backslash escapes in file variable values: `\n`/`\r`/`\t` expand, any other
/// escaped char is kept literally, and a trailing lone `\` is dropped.
fn unescape_file_variable_value(raw: &str) -> String {
    let mut value = String::with_capacity(raw.len());
    let mut escaping = false;
    for c in raw.chars() {
        if escaping {
            escaping = false;
            value.push(match c {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
        } else if c == '\\' {
            escaping = true;
        } else {
            value.push(c);
        }
    }
    value
}

/// `Selector.getRequestRanges` trimming for one delimited section: skip
/// comment/empty/file-variable lines from the start and comment/empty lines
/// from the end; a response status line at the start disqualifies the section.
/// Returns inclusive `(start, end)` indices into `lines`.
fn request_range(lines: &[&str]) -> Option<(usize, usize)> {
    let mut start = 0usize;
    let mut end = lines.len().checked_sub(1)?;
    loop {
        if start > end {
            return None;
        }
        let start_line = lines[start];
        if is_response_status_line(start_line) {
            return None;
        }
        if is_comment_line(start_line)
            || is_empty_line(start_line)
            || is_file_variable_line(start_line)
        {
            start += 1;
            continue;
        }
        let end_line = lines[end];
        if is_comment_line(end_line) || is_empty_line(end_line) {
            // `end > start` here: lines[start] just failed both checks.
            end -= 1;
            continue;
        }
        return Some((start, end));
    }
}

fn build_block(
    lines: &[&str],
    range_start: usize,
    content_start: usize,
    end: usize,
) -> Option<RequestBlock> {
    let section = &lines[content_start..end];

    // Lens position comes from the unfiltered lines (httpCodeLensProvider).
    let (request_start, _) = request_range(section)?;
    let lens_line = (content_start + request_start) as u32;

    let metadata = parse_block_metadata(section);

    // Request text comes from the comment-filtered lines (Selector.getRequest).
    let raw: Vec<&str> = section
        .iter()
        .copied()
        .filter(|line| !is_comment_line(line))
        .collect();
    let (text_start, text_end) = request_range(&raw)?;
    let text = raw[text_start..=text_end].join("\n");

    Some(RequestBlock {
        range: LineRange {
            start: range_start as u32,
            end: end as u32,
        },
        lens_line,
        metadata,
        text,
    })
}

/// `Selector.parseReqMetadatas`: scan comment lines above the request line.
fn parse_block_metadata(section: &[&str]) -> RequestMetadata {
    let mut metadata = RequestMetadata::default();
    for line in section {
        if is_empty_line(line) || is_file_variable_line(line) {
            continue;
        }
        if !is_comment_line(line) {
            break; // first request line — metadata below it does not count
        }
        let Some((key, value)) = parse_metadata_line(line) else {
            continue;
        };
        match key.as_str() {
            "name" => metadata.name = value,
            "no-redirect" => metadata.no_redirect = true,
            "no-cookie-jar" => metadata.no_cookie_jar = true,
            // TODO(Phase 2): `note` (confirmation prompt) and `prompt`
            // (interactive variables) are parsed but ignored for now.
            "note" | "prompt" => {}
            _ => {}
        }
    }
    metadata
}

/// `/^\s*(?:#|\/{2})\s*@([\w-]+)(?:\s+(.*?))?\s*$/` → lowercased key + value.
/// Note: exactly one `#` or `//` — `## @name` is NOT metadata, per the original.
fn parse_metadata_line(line: &str) -> Option<(String, Option<String>)> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("//")
        .or_else(|| trimmed.strip_prefix('#'))?;
    let rest = rest.trim_start().strip_prefix('@')?;
    let key_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    if key_end == 0 {
        return None;
    }
    let key = rest[..key_end].to_lowercase();
    let after = &rest[key_end..];
    if after.is_empty() {
        return Some((key, None));
    }
    if !after.starts_with(char::is_whitespace) {
        return None; // e.g. `@name=x` — the original regex rejects this line
    }
    let value = after.trim();
    Some((
        key,
        (!value.is_empty()).then(|| value.to_string()),
    ))
}

// ---------------------------------------------------------------------------
// Request parsing (httpRequestParser.ts)
// ---------------------------------------------------------------------------

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

/// `/^\s*[&\?]/`
fn is_query_continuation_line(line: &str) -> bool {
    line.trim_start().starts_with(['&', '?'])
}

/// `Method SP Request-URI` — when the first token is not a known method the
/// whole line is the URL and the method defaults to GET.
fn split_method(line: &str) -> (String, &str) {
    if let Some(ws) = line.find(char::is_whitespace) {
        let token = line[..ws].to_ascii_uppercase();
        if METHODS.contains(&token.as_str()) {
            return (token, &line[ws..]);
        }
    }
    ("GET".to_string(), line)
}

/// Strip a `\s+HTTP\/...` tail (case-insensitive) off the URL.
fn strip_http_version(url: &str) -> (&str, Option<String>) {
    for (idx, c) in url.char_indices() {
        if !c.is_whitespace() {
            continue;
        }
        let tail = url[idx..].trim_start();
        if tail
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("HTTP/"))
        {
            return (&url[..idx], Some(tail.to_string()));
        }
    }
    (url, None)
}

/// `parseRequestHeaders`: split on the first `:`; a line without one becomes a
/// header with an empty value. Duplicate names (case-insensitive) merge into
/// the first entry, `;`-joined for Cookie and `,`-joined otherwise.
fn parse_headers(header_lines: &[&str]) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in header_lines {
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

fn parse_body(lines: &[&str], content_type: Option<&str>, base_dir: &Path) -> Option<RequestBody> {
    if lines.is_empty() {
        return None;
    }

    if lines.iter().any(|line| input_file_ref(line).is_some()) {
        // TODO(Phase 2): the original builds a combined stream mixing text
        // lines with any number of `< file` references (multipart bodies,
        // GraphQL `< query.graphql` + inline variables). MVP supports a
        // single external file body; `<@` content substitution is wired at
        // integration via `input_file_ref` on the pre-substitution text.
        let file_ref = lines.iter().find_map(|line| input_file_ref(line))?;
        let path = PathBuf::from(file_ref.path);
        let resolved = if path.is_absolute() {
            path
        } else {
            base_dir.join(path)
        };
        return Some(RequestBody::File(resolved));
    }

    let essence = content_type.map(content_type_essence);
    let text = if essence.as_deref() == Some("application/x-www-form-urlencoded") {
        // Continuation lines starting with `&` are joined without a newline.
        let mut joined = String::new();
        for (idx, line) in lines.iter().enumerate() {
            if idx > 0 && !line.starts_with('&') {
                joined.push('\n');
            }
            joined.push_str(line);
        }
        joined
    } else {
        let mut joined = lines.join("\n");
        if essence.as_deref() == Some("application/x-ndjson") {
            joined.push('\n');
        }
        joined
    };
    Some(RequestBody::Text(text))
}

fn content_type_essence(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_dir() -> &'static Path {
        Path::new("/tmp/doc-dir")
    }

    // -- parse_document --------------------------------------------------

    #[test]
    fn splits_blocks_on_separators() {
        let doc = "GET https://a.com\n\n###\n# @name second\nPOST https://b.com\n\n### trailing comment\nDELETE https://c.com";
        let parsed = parse_document(doc);
        assert_eq!(parsed.blocks.len(), 3);

        let b = &parsed.blocks[0];
        assert_eq!(b.range, LineRange { start: 0, end: 2 });
        assert_eq!(b.lens_line, 0);
        assert_eq!(b.text, "GET https://a.com");

        let b = &parsed.blocks[1];
        assert_eq!(b.range, LineRange { start: 2, end: 6 });
        assert_eq!(b.lens_line, 4);
        assert_eq!(b.metadata.name.as_deref(), Some("second"));
        assert_eq!(b.text, "POST https://b.com");

        let b = &parsed.blocks[2];
        assert_eq!(b.range, LineRange { start: 6, end: 8 });
        assert_eq!(b.lens_line, 7);
        assert_eq!(b.text, "DELETE https://c.com");
    }

    #[test]
    fn document_without_separators_is_one_block() {
        let doc = "POST https://a.com\nContent-Type: application/json\n\n{\"a\": 1}\n";
        let parsed = parse_document(doc);
        assert_eq!(parsed.blocks.len(), 1);
        let b = &parsed.blocks[0];
        assert_eq!(b.range.start, 0);
        assert_eq!(b.lens_line, 0);
        assert_eq!(
            b.text,
            "POST https://a.com\nContent-Type: application/json\n\n{\"a\": 1}"
        );
    }

    #[test]
    fn collects_file_variables_with_escapes_and_lines() {
        let doc = "@host = https://example.com\n@token = abc\\ndef\\\\g\n\nGET {{host}}/api\n\n###\n@inner = 1\nGET {{host}}/2";
        let parsed = parse_document(doc);
        assert_eq!(
            parsed.file_variables,
            vec![
                FileVariable {
                    name: "host".into(),
                    value: "https://example.com".into(),
                    line: 0
                },
                FileVariable {
                    name: "token".into(),
                    value: "abc\ndef\\g".into(),
                    line: 1
                },
                FileVariable {
                    name: "inner".into(),
                    value: "1".into(),
                    line: 6
                },
            ]
        );
        // Leading file variable lines are trimmed out of the request text.
        assert_eq!(parsed.blocks[0].text, "GET {{host}}/api");
        assert_eq!(parsed.blocks[0].lens_line, 3);
        assert_eq!(parsed.blocks[1].text, "GET {{host}}/2");
        assert_eq!(parsed.blocks[1].lens_line, 7);
    }

    #[test]
    fn parses_metadata_flags_and_name() {
        let doc = "# @name createUser\n// @no-redirect\n# @no-cookie-jar\nPOST https://x.com\n# @name ignored-after";
        let parsed = parse_document(doc);
        let b = &parsed.blocks[0];
        assert_eq!(b.metadata.name.as_deref(), Some("createUser"));
        assert!(b.metadata.no_redirect);
        assert!(b.metadata.no_cookie_jar);
        // Comments (incl. the post-request metadata line) are stripped from text.
        assert_eq!(b.text, "POST https://x.com");
    }

    #[test]
    fn double_hash_is_comment_but_not_metadata() {
        let doc = "## @name double\nGET https://x.com";
        let parsed = parse_document(doc);
        let b = &parsed.blocks[0];
        assert_eq!(b.metadata.name, None);
        assert_eq!(b.text, "GET https://x.com");
        assert_eq!(b.lens_line, 1);
    }

    #[test]
    fn comment_only_and_response_sections_have_no_block() {
        let doc = "# just a comment\n\n###\nHTTP/1.1 200 OK\nContent-Type: application/json\n\n###\nGET https://real.com";
        let parsed = parse_document(doc);
        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0].text, "GET https://real.com");
        assert_eq!(parsed.blocks[0].lens_line, 7);
    }

    #[test]
    fn crlf_document_parses_cleanly() {
        let doc = "@v = 1\r\nGET https://a.com\r\n###\r\n# @name b\r\nPOST https://b.com\r\n";
        let parsed = parse_document(doc);
        assert_eq!(parsed.blocks.len(), 2);
        assert_eq!(parsed.file_variables[0].value, "1");
        assert_eq!(parsed.blocks[0].text, "GET https://a.com");
        assert_eq!(parsed.blocks[1].metadata.name.as_deref(), Some("b"));
        assert_eq!(parsed.blocks[1].text, "POST https://b.com");
    }

    #[test]
    fn block_at_line_maps_lines_to_blocks() {
        let doc = "GET https://a.com\n\n###\nPOST https://b.com";
        let parsed = parse_document(doc);
        assert_eq!(block_at_line(&parsed, 0).unwrap().lens_line, 0);
        assert_eq!(block_at_line(&parsed, 1).unwrap().lens_line, 0);
        // The separator line belongs to the block below it.
        assert_eq!(block_at_line(&parsed, 2).unwrap().lens_line, 3);
        assert_eq!(block_at_line(&parsed, 3).unwrap().lens_line, 3);
        assert!(block_at_line(&parsed, 4).is_none());
    }

    // -- parse_request ----------------------------------------------------

    #[test]
    fn method_omitted_defaults_to_get() {
        let req = parse_request("https://example.com/api", base_dir()).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "https://example.com/api");
        assert_eq!(req.http_version, None);
        assert!(req.headers.is_empty());
        assert_eq!(req.body, None);
    }

    #[test]
    fn method_is_uppercased_and_version_tail_is_split() {
        let req = parse_request("post https://example.com/api HTTP/1.1", base_dir()).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://example.com/api");
        assert_eq!(req.http_version.as_deref(), Some("HTTP/1.1"));
    }

    #[test]
    fn unknown_first_token_is_part_of_url() {
        // Original quirk: `FETCH url` is not a known method, so the whole
        // line becomes the URL of a GET.
        let req = parse_request("FETCH https://example.com", base_dir()).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "FETCH https://example.com");
    }

    #[test]
    fn multiline_query_is_joined_without_whitespace() {
        let text = "GET https://example.com/comments\n    ?page=2\n    &pageSize=10";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(req.url, "https://example.com/comments?page=2&pageSize=10");
    }

    #[test]
    fn multiline_query_followed_by_headers() {
        let text = "GET https://example.com/comments\n  ?page=2\nAccept: application/json";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(req.url, "https://example.com/comments?page=2");
        assert_eq!(req.header("accept"), Some("application/json"));
    }

    #[test]
    fn headers_merge_duplicates_and_drop_content_length() {
        let text = "GET https://x.com\nContent-Type: application/json\nCookie: a=1\nCookie: b=2\nX-Dup: one\nx-dup: two\nContent-Length: 99";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(req.header("CONTENT-TYPE"), Some("application/json"));
        assert_eq!(req.header("cookie"), Some("a=1;b=2"));
        assert_eq!(req.header("X-DUP"), Some("one,two"));
        assert_eq!(req.header("content-length"), None);
        // Name case of the first occurrence is preserved.
        assert!(req.headers.iter().any(|(n, _)| n == "X-Dup"));
    }

    #[test]
    fn body_after_blank_line() {
        let text = "POST https://x.com\nContent-Type: application/json\n\n{\n  \"a\": 1\n}";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(
            req.body,
            Some(RequestBody::Text("{\n  \"a\": 1\n}".to_string()))
        );
    }

    #[test]
    fn body_without_headers() {
        let text = "POST https://x.com\n\nhello\nworld";
        let req = parse_request(text, base_dir()).unwrap();
        assert!(req.headers.is_empty());
        assert_eq!(req.body, Some(RequestBody::Text("hello\nworld".to_string())));
    }

    #[test]
    fn file_body_resolves_against_base_dir() {
        let text = "POST https://x.com\nContent-Type: application/json\n\n< ./payload.json";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(
            req.body,
            Some(RequestBody::File(base_dir().join("./payload.json")))
        );

        let text = "POST https://x.com\n\n< /abs/payload.json";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(
            req.body,
            Some(RequestBody::File(PathBuf::from("/abs/payload.json")))
        );
    }

    #[test]
    fn file_body_with_variables_variants() {
        // `<@` / `<@encoding` parse as file bodies too; the process-variables
        // flavor is detectable through `input_file_ref`.
        let text = "POST https://x.com\n\n<@ ./payload.json";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(
            req.body,
            Some(RequestBody::File(base_dir().join("./payload.json")))
        );

        assert_eq!(
            input_file_ref("< ./payload.json"),
            Some(InputFileRef {
                process_variables: false,
                path: "./payload.json"
            })
        );
        assert_eq!(
            input_file_ref("<@ ./payload.json"),
            Some(InputFileRef {
                process_variables: true,
                path: "./payload.json"
            })
        );
        assert_eq!(
            input_file_ref("<@latin1 ./payload.json"),
            Some(InputFileRef {
                process_variables: true,
                path: "./payload.json"
            })
        );
        // No whitespace before the path means plain body text, not a file ref.
        assert_eq!(input_file_ref("<notafile"), None);
        assert_eq!(input_file_ref("<@latin1"), None);

        let text = "POST https://x.com\n\n<notafile";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(req.body, Some(RequestBody::Text("<notafile".to_string())));
    }

    #[test]
    fn form_urlencoded_joins_ampersand_continuations() {
        let text = "POST https://x.com\nContent-Type: application/x-www-form-urlencoded; charset=utf-8\n\nname=foo\n&age=30\nbar=baz";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(
            req.body,
            Some(RequestBody::Text("name=foo&age=30\nbar=baz".to_string()))
        );
    }

    #[test]
    fn ndjson_body_gets_trailing_newline() {
        let text = "POST https://x.com\nContent-Type: application/x-ndjson\n\n{\"a\":1}\n{\"b\":2}";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(
            req.body,
            Some(RequestBody::Text("{\"a\":1}\n{\"b\":2}\n".to_string()))
        );
    }

    #[test]
    fn graphql_request_keeps_query_and_variables_in_body() {
        // http.rs wraps the body into {query, operationName?, variables};
        // the parser must keep both blank-line-separated sections intact.
        let text = "POST https://api.github.com/graphql\nX-Request-Type: GraphQL\n\nquery getUser($login: String!) {\n  user(login: $login) { name }\n}\n\n{\n  \"login\": \"octocat\"\n}";
        let req = parse_request(text, base_dir()).unwrap();
        assert!(req.is_graphql());
        assert_eq!(req.header("X-Request-Type"), Some("GraphQL"));
        assert_eq!(
            req.body,
            Some(RequestBody::Text(
                "query getUser($login: String!) {\n  user(login: $login) { name }\n}\n\n{\n  \"login\": \"octocat\"\n}"
                    .to_string()
            ))
        );
    }

    #[test]
    fn is_graphql_requires_header_value() {
        let req = parse_request("POST https://x.com\nX-Request-Type: graphql", base_dir()).unwrap();
        assert!(req.is_graphql());
        let req = parse_request("POST https://x.com\nAccept: text/plain", base_dir()).unwrap();
        assert!(!req.is_graphql());
    }

    #[test]
    fn host_header_combines_with_relative_url() {
        let req = parse_request("GET /api/users\nHost: example.com", base_dir()).unwrap();
        assert_eq!(req.url, "http://example.com/api/users");

        let req = parse_request("GET /api/users\nHost: example.com:8443", base_dir()).unwrap();
        assert_eq!(req.url, "https://example.com:8443/api/users");

        let req = parse_request("GET /api/users\nHost: example.com:443", base_dir()).unwrap();
        assert_eq!(req.url, "https://example.com:443/api/users");
    }

    #[test]
    fn empty_text_is_empty_block_error() {
        assert!(matches!(
            parse_request("", base_dir()),
            Err(ParseError::EmptyBlock)
        ));
        assert!(matches!(
            parse_request("   ", base_dir()),
            Err(ParseError::EmptyBlock)
        ));
    }

    #[test]
    fn crlf_request_text_parses() {
        let text = "POST https://x.com\r\nContent-Type: application/json\r\n\r\n{\"a\":1}";
        let req = parse_request(text, base_dir()).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.body, Some(RequestBody::Text("{\"a\":1}".to_string())));
    }
}
