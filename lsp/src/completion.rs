//! textDocument/completion for .http documents.
//!
//! Semantics mirror vscode-restclient `providers/httpCompletionItemProvider.ts`
//! plus `utils/httpElementFactory.ts` (methods, headers, MIME values,
//! Authorization schemes, variables) and
//! `providers/requestVariableCompletionItemProvider.ts` (dot-by-dot request
//! variable path completion). Unlike the original — which offers every
//! element everywhere and filters by line prefix — the cursor context is
//! classified first ([`detect_context`]) so each spot gets only the relevant
//! items, with variables available in every non-path context like upstream.
//!
//! The request variable cache lives in `variables::request_vars`; everything
//! cache-shaped goes through the [`RequestVariableSource`] seam that
//! backend.rs wires up (`CacheSource`).

use std::collections::HashMap;

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, InsertTextFormat,
    MarkupContent, MarkupKind, Position, Range, TextEdit,
};

use crate::parser::{self, ParsedDocument};

// ---------------------------------------------------------------------------
// Request variable cache seam
// ---------------------------------------------------------------------------

/// Integration seam to the request variable cache (the `# @name` send store,
/// vscode-restclient `utils/requestVariableCache.ts` +
/// `utils/requestVariableCacheValueProcessor.ts`, developed in parallel).
/// completion.rs/hover.rs never touch the cache directly; backend.rs passes
/// an implementation in.
pub trait RequestVariableSource {
    /// Whether the named request has been sent (a cached response exists).
    fn has(&self, name: &str) -> bool;

    /// Header `(name, value)` pairs of the cached response (`Response`) or of
    /// the request that produced it (`Request`). `None` when nothing is
    /// cached for `name`.
    fn headers(&self, name: &str, entity: RequestEntity) -> Option<Vec<(String, String)>>;

    /// Resolves a full `name.(request|response).(body|headers).<path>`
    /// reference (the text between `{{` and `}}`, trimmed) to a hover
    /// display value — `requestVariableCacheValueProcessor.resolveRequestVariable`
    /// semantics.
    fn resolve(&self, path: &str) -> RequestVariableResolution;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestEntity {
    Request,
    Response,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestVariableResolution {
    /// The request is defined in the document but has not been sent yet.
    NotSent,
    Resolved(ResolvedValue),
    /// Cached, but the path does not resolve cleanly (incorrect header name,
    /// JSONPath miss, ...) — carries the human-readable reason.
    Warning(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedValue {
    /// Plain text (header value, body fragment).
    Text(String),
    /// Structured value pre-serialized as JSON; hover renders it fenced.
    Json(String),
}

/// The "nothing has been sent" stand-in for completion/hover tests.
#[cfg(test)]
pub struct NoRequestVariables;

#[cfg(test)]
impl RequestVariableSource for NoRequestVariables {
    fn has(&self, _name: &str) -> bool {
        false
    }

    fn headers(&self, _name: &str, _entity: RequestEntity) -> Option<Vec<(String, String)>> {
        None
    }

    fn resolve(&self, _path: &str) -> RequestVariableResolution {
        RequestVariableResolution::NotSent
    }
}

// ---------------------------------------------------------------------------
// Context detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    /// Cursor after `{{name.` — request variable path stages.
    RequestVariablePath {
        /// Text between `{{` and the cursor, e.g. `login.response.`.
        path: String,
        /// Byte offset of the `{{` within the line.
        open: usize,
    },
    /// Cursor inside `{{` (no `name.` path yet) — variable references.
    VariableReference {
        /// Text between `{{` and the cursor.
        partial: String,
        /// Byte offset of the `{{` within the line.
        open: usize,
    },
    /// On an `Authorization:` line — scheme snippets.
    AuthorizationScheme,
    /// On a `Content-Type:` / `Accept:` line — MIME values.
    MimeType,
    /// Request line position (or anywhere no more specific context applies).
    RequestLine,
    /// Header area below a block's request line.
    Header,
}

/// Classifies the cursor position. `line`/`character` are LSP (UTF-16)
/// coordinates.
pub fn detect_context(text: &str, line: u32, character: u32) -> CompletionContext {
    let line_text = line_at(text, line);
    let cursor = utf16_to_byte(line_text, character);

    // Inside `{{`: an opener before the cursor with no `}}` in between.
    if let Some(open) = open_brace_before(line_text, cursor) {
        let partial = line_text[open + 2..cursor].to_string();
        if is_partial_request_variable_path(&partial) {
            return CompletionContext::RequestVariablePath { path: partial, open };
        }
        return CompletionContext::VariableReference { partial, open };
    }

    // Line-prefix contexts, matched against the whole line like the original
    // `^\s*(Content-Type|Accept)\s*:` / `^\s*Authorization\s*:` (case-insensitive).
    if matches_header_value_prefix(line_text, &["content-type", "accept"]) {
        return CompletionContext::MimeType;
    }
    if matches_header_value_prefix(line_text, &["authorization"]) {
        return CompletionContext::AuthorizationScheme;
    }

    let document = parser::parse_document(text);
    match parser::block_at_line(&document, line) {
        Some(block) if line > block.lens_line => CompletionContext::Header,
        _ => CompletionContext::RequestLine,
    }
}

/// All completion items for the cursor position. `environment` is the merged
/// `$shared` + selected environment (`settings::resolve_environment` output).
pub fn completion_items(
    text: &str,
    position: Position,
    environment: &HashMap<String, String>,
    request_variables: &dyn RequestVariableSource,
) -> Vec<CompletionItem> {
    let context = detect_context(text, position.line, position.character);

    if let CompletionContext::RequestVariablePath { path, open } = &context {
        return request_variable_path_items(text, position, *open, path, request_variables);
    }

    let document = parser::parse_document(text);
    let request_names = request_variable_names(text);

    let brace_edit = match &context {
        CompletionContext::VariableReference { open, .. } => {
            Some(brace_edit_range(text, position, *open))
        }
        _ => None,
    };
    let variables = variable_items(
        &document,
        environment,
        &request_names,
        request_variables,
        brace_edit,
    );

    let mut items = match context {
        CompletionContext::VariableReference { .. } => Vec::new(),
        CompletionContext::AuthorizationScheme => auth_items(),
        CompletionContext::MimeType => mime_items(),
        CompletionContext::RequestLine => method_items(),
        CompletionContext::Header => header_items(),
        CompletionContext::RequestVariablePath { .. } => unreachable!("handled above"),
    };
    items.extend(variables);
    items
}

// ---------------------------------------------------------------------------
// Item tables (httpElementFactory.ts)
// ---------------------------------------------------------------------------

const METHODS: [&str; 9] = [
    "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

const HEADERS: [(&str, &str); 34] = [
    ("Accept", "Specify certain media types which are acceptable for the response"),
    ("Accept-Charset", "Indicate what character sets are acceptable for the response"),
    ("Accept-Encoding", "Indicate the content-codings that are acceptable in the response"),
    ("Accept-Language", "Indicate the set of natural languages that are preferred as a response to the request"),
    ("Accept-Datetime", "Indicate it wants to access a past state of an original resource"),
    ("Authorization", "Consists of credentials containing the authentication information of the user agent for the realm of the resource being requested"),
    ("Cache-Control", "Specify directives that MUST be obeyed by all caching mechanisms along the request/response chain"),
    ("Connection", "Specify options that are desired for that particular connection and MUST NOT be communicated by proxies over further connections"),
    ("Content-Length", "Indicate the size of the entity-body"),
    ("Content-MD5", "Provide an end-to-end message integrity check of the entity-body"),
    ("Content-Type", "Indicate the media type of the entity-body sent to the recipient or, in the case of the HEAD method, the media type that would have been sent had the request been a GET"),
    ("Cookie", "An HTTP cookie previously sent by the server with Set-Cookie"),
    ("Date", "Represent the date and time at which the message was originated"),
    ("Expect", "Indicate that particular server behaviors are required by the client"),
    ("Forwarded", "Disclose original information of a client connecting to a web server through an HTTP proxy"),
    ("From", "The email address of the user making the request"),
    ("Host", "Specify the Internet host and port number of the resource being requested"),
    ("If-Match", "Only perform the action if the client supplied entity matches the same entity on the server. This is mainly for methods like PUT to only update a resource if it has not been modified since the user last updated it"),
    ("If-Modified-Since", "Allows a 304 Not Modified to be returned if content is unchanged since the time specified in this field"),
    ("If-None-Match", "Allows a 304 Not Modified to be returned if content is unchanged for ETag"),
    ("If-Range", "If the entity is unchanged, send me the part(s) that I am missing; otherwise, send me the entire new entity."),
    ("If-Unmodified-Since", "Only send the response if the entity has not been modified since a specific time"),
    ("Max-Forwards", "Provide a mechanism with the TRACE and OPTIONS methods to limit the number of proxies or gateways that can forward the request to the next inbound server"),
    ("Origin", "Initiate a request for cross-origin resource sharing"),
    ("Pragma", "Include implementation-specific directives that might apply to any recipient along the request/response chain"),
    ("Proxy-Authorization", "Allows the client to identify itself (or its user) to a proxy which requires authentication"),
    ("Range", "Request only part of an entity. Bytes are numbered from 0"),
    ("Referer", "Allow the client to specify, for the server's benefit, the address (URI) of the resource from which the Request-URI was obtained"),
    ("TE", "Indicate what extension transfer-codings it is willing to accept in the response and whether or not it is willing to accept trailer fields in a chunked transfer-coding"),
    ("Upgrade", "Allow the client to specify what additional communication protocols it supports and would like to use if the server finds it appropriate to switch protocols"),
    ("User-Agent", "Contain information about the user agent originating the request"),
    ("Via", "Indicate the intermediate protocols and recipients between the user agent and the server on requests, and between the origin server and the client on responses"),
    ("Warning", "Carry additional information about the status or transformation of a message which might not be reflected in the message"),
    ("X-Http-Method-Override", "Requests a web application override the method specified in the request (typically POST) with the method given in the header field (typically PUT or DELETE). Can be used when a user agent or firewall prevents PUT or DELETE methods from being sent directly"),
];

const MIME_TYPES: [&str; 19] = [
    "application/json",
    "application/xml",
    "application/javascript",
    "application/xhtml+xml",
    "application/octet-stream",
    "application/soap+xml",
    "application/zip",
    "application/gzip",
    "application/x-www-form-urlencoded",
    "image/gif",
    "image/jpeg",
    "image/png",
    "message/http",
    "multipart/form-data",
    "text/css",
    "text/csv",
    "text/html",
    "text/plain",
    "text/xml",
];

/// `(label, description, snippet)` — Authorization scheme snippets.
const AUTH_SCHEMES: [(&str, &str, &str); 4] = [
    (
        "Basic Base64",
        "Base64 encoded username and password",
        "Basic ${1:base64-user-password}",
    ),
    (
        "Basic Raw Credential (Colon Separated)",
        "Raw username and password",
        "Basic ${1:username}:${2:password}",
    ),
    (
        "Basic Raw Credential (Space Separated)",
        "Raw username and password",
        "Basic ${1:username} ${2:password}",
    ),
    ("Digest", "Raw username and password", "Digest ${1:username} ${2:password}"),
];

pub(crate) struct SystemVariable {
    pub(crate) name: &'static str,
    /// Upstream constants.ts description, also shown on hover.
    pub(crate) description: &'static str,
    /// `Some` = snippet body without the outer braces (`\$` escapes the
    /// literal dollar for snippet syntax); `None` = plain-text insert.
    snippet: Option<&'static str>,
}

/// In-MVP-scope system variables ($aadToken/$aadV2Token/$oidcAccessToken are
/// Phase 3+, mirroring variables.rs).
pub(crate) const SYSTEM_VARIABLES: [SystemVariable; 7] = [
    SystemVariable {
        name: "$guid",
        description: "Add a RFC 4122 v4 UUID",
        snippet: None,
    },
    SystemVariable {
        name: "$randomInt",
        description: "Returns a random integer between min (included) and max (excluded)",
        snippet: Some("\\$randomInt ${1:min} ${2:max}"),
    },
    SystemVariable {
        name: "$timestamp",
        description: "Add a number of milliseconds between 1970/1/1 UTC Time and now. \
                      You can also provide the offset with current time in the format \
                      {{$timestamp number string}}",
        snippet: None,
    },
    SystemVariable {
        name: "$datetime",
        description: "Add a datetime string in either ISO8601 or RFC1123 format",
        snippet: Some("\\$datetime ${1|rfc1123,iso8601|}"),
    },
    SystemVariable {
        name: "$localDatetime",
        description: "Add a local datetime string in either ISO8601 or RFC1123 format",
        snippet: Some("\\$localDatetime ${1|rfc1123,iso8601|}"),
    },
    SystemVariable {
        name: "$processEnv",
        description: "Returns the value of process environment variable or '' if not found",
        snippet: Some("\\$processEnv ${1:variable name}"),
    },
    SystemVariable {
        name: "$dotenv",
        description: "Returns the environment value stored in a .env file",
        snippet: Some("\\$dotenv ${1:variable name}"),
    },
];

// ---------------------------------------------------------------------------
// Item builders
// ---------------------------------------------------------------------------

fn base_item(label: &str, kind: CompletionItemKind, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail: Some(detail.to_string()),
        ..Default::default()
    }
}

fn markdown_doc(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

fn method_items() -> Vec<CompletionItem> {
    METHODS
        .iter()
        .map(|method| {
            let mut item = base_item(method, CompletionItemKind::METHOD, "HTTP Method");
            item.insert_text = Some(format!("{method} ${{1:url}}"));
            item.insert_text_format = Some(InsertTextFormat::SNIPPET);
            item
        })
        .collect()
}

fn header_items() -> Vec<CompletionItem> {
    HEADERS
        .iter()
        .map(|(name, description)| {
            let mut item = base_item(name, CompletionItemKind::PROPERTY, "HTTP Header");
            item.documentation = Some(Documentation::String((*description).to_string()));
            item
        })
        .collect()
}

fn mime_items() -> Vec<CompletionItem> {
    MIME_TYPES
        .iter()
        .map(|mime| base_item(mime, CompletionItemKind::FIELD, "HTTP MIME"))
        .collect()
}

fn auth_items() -> Vec<CompletionItem> {
    AUTH_SCHEMES
        .iter()
        .map(|(label, description, snippet)| {
            let mut item = base_item(label, CompletionItemKind::FIELD, "HTTP Authentication");
            item.documentation = Some(Documentation::String((*description).to_string()));
            item.insert_text = Some((*snippet).to_string());
            item.insert_text_format = Some(InsertTextFormat::SNIPPET);
            item
        })
        .collect()
}

/// System + file + environment + request-name variable items.
/// `brace_edit = Some(range)` means the cursor is inside `{{`: the bare
/// expression replaces `range` via TextEdit. Outside, the insert text is
/// wrapped in `{{ }}`.
fn variable_items(
    document: &ParsedDocument,
    environment: &HashMap<String, String>,
    request_names: &[String],
    source: &dyn RequestVariableSource,
    brace_edit: Option<Range>,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    for sys in &SYSTEM_VARIABLES {
        let mut item = base_item(sys.name, CompletionItemKind::VARIABLE, "HTTP SystemVariable");
        item.documentation = Some(Documentation::String(sys.description.to_string()));
        match sys.snippet {
            Some(snippet) => set_variable_insert(&mut item, brace_edit, snippet.to_string(), true),
            None => set_variable_insert(&mut item, brace_edit, sys.name.to_string(), false),
        }
        items.push(item);
    }

    // File variables: first-occurrence order, last definition wins the value
    // (the variables.rs document-scan semantics).
    let mut seen: Vec<&str> = Vec::new();
    for variable in &document.file_variables {
        if seen.contains(&variable.name.as_str()) {
            continue;
        }
        seen.push(&variable.name);
        let value = document
            .file_variables
            .iter()
            .rev()
            .find(|v| v.name == variable.name)
            .map(|v| v.value.as_str())
            .unwrap_or_default();
        let mut item = base_item(
            &variable.name,
            CompletionItemKind::VARIABLE,
            "HTTP FileCustomVariable",
        );
        item.documentation = Some(markdown_doc(format!("Value: `{value}`")));
        set_variable_insert(&mut item, brace_edit, variable.name.clone(), false);
        items.push(item);
    }

    // Environment variables, sorted for determinism (HashMap order is random).
    let mut env_names: Vec<&String> = environment.keys().collect();
    env_names.sort();
    for name in env_names {
        let mut item = base_item(
            name,
            CompletionItemKind::VARIABLE,
            "HTTP EnvironmentCustomVariable",
        );
        item.documentation = Some(markdown_doc(format!("Value: `{}`", environment[name])));
        set_variable_insert(&mut item, brace_edit, name.clone(), false);
        items.push(item);
    }

    // Request variables (`# @name` blocks); inactive until sent.
    for name in request_names {
        let inactive = if source.has(name) { "" } else { " *(Inactive)*" };
        let mut item = base_item(
            name,
            CompletionItemKind::VARIABLE,
            "HTTP RequestCustomVariable",
        );
        item.documentation = Some(markdown_doc(format!(
            "Value: Request Variable {name}{inactive}"
        )));
        let snippet = format!(
            "{name}.${{1|request,response|}}.${{2|headers,body|}}.\
             ${{3:Header Name, *(Full Body), JSONPath or XPath}}"
        );
        set_variable_insert(&mut item, brace_edit, snippet, true);
        items.push(item);
    }

    items
}

fn set_variable_insert(
    item: &mut CompletionItem,
    brace_edit: Option<Range>,
    expression: String,
    snippet: bool,
) {
    match brace_edit {
        Some(range) => {
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: expression,
            }));
        }
        None => item.insert_text = Some(format!("{{{{{expression}}}}}")),
    }
    if snippet {
        item.insert_text_format = Some(InsertTextFormat::SNIPPET);
    }
}

// ---------------------------------------------------------------------------
// Request variable path completion (requestVariableCompletionItemProvider.ts)
// ---------------------------------------------------------------------------

/// Dot-by-dot stages for `{{name.<...>` paths. The partially typed segment
/// after the last `.` is replaced via TextEdit (the client filters by it);
/// the stage is decided by the path up to that dot:
/// `name.` -> request|response, `name.<entity>.` -> body|headers,
/// `....headers.` -> cached header names, `....body.` -> `*` / `$` guidance.
fn request_variable_path_items(
    text: &str,
    position: Position,
    open: usize,
    path: &str,
    source: &dyn RequestVariableSource,
) -> Vec<CompletionItem> {
    let Some(last_dot) = path.rfind('.') else {
        return Vec::new();
    };
    let base = &path[..=last_dot];
    let segments: Vec<&str> = base.split('.').collect(); // trailing "" element

    let name = segments[0];
    if !request_variable_names(text).iter().any(|n| n == name) {
        return Vec::new();
    }

    // Replace from right after the last dot up to the cursor.
    let line_text = line_at(text, position.line);
    let typed_start = open + 2 + last_dot + 1;
    let edit_range = Range {
        start: Position {
            line: position.line,
            character: byte_to_utf16(line_text, typed_start),
        },
        end: position,
    };
    let plain = |label: &str| {
        let mut item = base_item(label, CompletionItemKind::FIELD, "HTTP RequestCustomVariable");
        item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
            range: edit_range,
            new_text: label.to_string(),
        }));
        item
    };

    match segments.as_slice() {
        [_, ""] => vec![plain("request"), plain("response")],
        [_, entity, ""] if is_entity(entity) => vec![plain("body"), plain("headers")],
        [_, entity, "headers", ""] if is_entity(entity) => {
            let entity = if *entity == "request" {
                RequestEntity::Request
            } else {
                RequestEntity::Response
            };
            let Some(headers) = source.headers(name, entity) else {
                return Vec::new(); // not sent yet — nothing to complete
            };
            headers
                .iter()
                .map(|(header, value)| {
                    let mut item = plain(header);
                    item.documentation = Some(markdown_doc(format!("Value: `{value}`")));
                    item
                })
                .collect()
        }
        [_, entity, "body", ""] if is_entity(entity) => {
            let mut star = plain("*");
            star.documentation = Some(Documentation::String(
                "The full response/request body regardless of the content type".to_string(),
            ));
            let mut json_path = plain("$");
            json_path.documentation = Some(markdown_doc(
                "JSONPath root for JSON bodies (e.g. `$.token`); \
                 use an XPath (e.g. `/note/to`) for XML bodies"
                    .to_string(),
            ));
            vec![star, json_path]
        }
        _ => Vec::new(),
    }
}

fn is_entity(segment: &str) -> bool {
    segment == "request" || segment == "response"
}

// ---------------------------------------------------------------------------
// Request variable definitions
// ---------------------------------------------------------------------------

/// `# @name foo` request names in document order, deduplicated. Mirrors
/// `RequestVariableDefinitionRegex` `/^\s*(?:#{1,}|\/{2,})\s+@name\s+(\w+)\s*$/`
/// — note this accepts `## @name x` (any number of `#`) and requires
/// whitespace after the comment marker, both unlike parser.rs metadata.
pub(crate) fn request_variable_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(name) = request_variable_definition(line) {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn request_variable_definition(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = if trimmed.starts_with('#') {
        trimmed.trim_start_matches('#')
    } else if trimmed.starts_with("//") {
        trimmed.trim_start_matches('/')
    } else {
        return None;
    };
    if !rest.starts_with(char::is_whitespace) {
        return None; // `#@name x` is not a request variable definition
    }
    let rest = rest.trim_start().strip_prefix("@name")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let name = rest.trim();
    (!name.is_empty() && name.bytes().all(is_word_byte)).then_some(name)
}

/// JS `\w`: `[A-Za-z0-9_]`.
pub(crate) fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ---------------------------------------------------------------------------
// Line / position helpers (shared with hover.rs)
// ---------------------------------------------------------------------------

pub(crate) fn line_at(text: &str, line: u32) -> &str {
    text.split('\n')
        .nth(line as usize)
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .unwrap_or("")
}

/// LSP UTF-16 column -> byte offset within `line` (clamped to the line end).
pub(crate) fn utf16_to_byte(line: &str, column: u32) -> usize {
    let mut units: u32 = 0;
    for (idx, c) in line.char_indices() {
        if units >= column {
            return idx;
        }
        units += c.len_utf16() as u32;
    }
    line.len()
}

pub(crate) fn byte_to_utf16(line: &str, byte: usize) -> u32 {
    line[..byte].encode_utf16().count() as u32
}

/// Byte spans (open..close+2 exclusive) of complete `{{...}}` references on
/// one line, skipping empty names — the variables.rs reference shape.
pub(crate) fn reference_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while let Some(rel) = line[i..].find("{{") {
        let open = i + rel;
        let inner_start = open + 2;
        let Some(rel_close) = line[inner_start..].find("}}") else {
            break;
        };
        let close = inner_start + rel_close;
        if line[inner_start..close].trim().is_empty() {
            i = open + 2;
            continue;
        }
        spans.push((open, close + 2));
        i = close + 2;
    }
    spans
}

/// Byte offset of the `{{` the cursor is inside of, if any: the last opener
/// before the cursor with no closing `}}` between it and the cursor.
fn open_brace_before(line: &str, cursor: usize) -> Option<usize> {
    let prefix = line.get(..cursor)?;
    let open = prefix.rfind("{{")?;
    (!prefix[open + 2..].contains("}}")).then_some(open)
}

/// `\w+.` head — the partial request variable path shape
/// (`/\{{2}(\w+)\.(.*?)?\}{2}/` semantics on the text before the cursor).
fn is_partial_request_variable_path(partial: &str) -> bool {
    match partial.find('.') {
        Some(dot) if dot > 0 => partial.as_bytes()[..dot].iter().copied().all(is_word_byte),
        _ => false,
    }
}

/// `^\s*<name>\s*:` whole-line check, case-insensitive, like the original
/// element prefix regexes.
fn matches_header_value_prefix(line: &str, names: &[&str]) -> bool {
    let trimmed = line.trim_start();
    names.iter().any(|name| {
        trimmed
            .get(..name.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(name))
            && trimmed[name.len()..].trim_start().starts_with(':')
    })
}

/// TextEdit range covering the partial text between `{{` and the cursor.
fn brace_edit_range(text: &str, position: Position, open: usize) -> Range {
    let line_text = line_at(text, position.line);
    Range {
        start: Position {
            line: position.line,
            character: byte_to_utf16(line_text, open + 2),
        },
        end: position,
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    fn find<'a>(items: &'a [CompletionItem], label: &str) -> &'a CompletionItem {
        items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("no item labeled `{label}`"))
    }

    fn edit_of(item: &CompletionItem) -> &TextEdit {
        match item.text_edit.as_ref().expect("text_edit missing") {
            CompletionTextEdit::Edit(edit) => edit,
            other => panic!("unexpected edit shape: {other:?}"),
        }
    }

    /// `login` has been sent; everything else hasn't.
    struct FakeCache;

    impl RequestVariableSource for FakeCache {
        fn has(&self, name: &str) -> bool {
            name == "login"
        }

        fn headers(&self, name: &str, entity: RequestEntity) -> Option<Vec<(String, String)>> {
            (name == "login" && entity == RequestEntity::Response).then(|| {
                vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("X-Request-Id".to_string(), "abc123".to_string()),
                ]
            })
        }

        fn resolve(&self, _path: &str) -> RequestVariableResolution {
            RequestVariableResolution::Resolved(ResolvedValue::Text("tok".to_string()))
        }
    }

    const DOC: &str = "@host = https://example.com\n\
                       # @name login\n\
                       POST {{host}}/login\n\
                       X-Api-Key: 1\n\
                       Content-Type: application/json\n\
                       \n\
                       {\"u\":1}\n\
                       \n\
                       ###\n\
                       GET {{host}}/me\n\
                       Authorization: ";

    // -- detect_context ----------------------------------------------------

    #[test]
    fn context_request_line_on_and_above_lens_line() {
        assert_eq!(detect_context(DOC, 2, 0), CompletionContext::RequestLine);
        // file variable line above the request line is still RequestLine
        assert_eq!(detect_context(DOC, 0, 0), CompletionContext::RequestLine);
    }

    #[test]
    fn context_header_below_request_line() {
        assert_eq!(detect_context(DOC, 3, 0), CompletionContext::Header);
    }

    #[test]
    fn context_mime_and_authorization_lines() {
        assert_eq!(detect_context(DOC, 4, 14), CompletionContext::MimeType);
        assert_eq!(
            detect_context(DOC, 10, 15),
            CompletionContext::AuthorizationScheme
        );
        // `Accept:` matches MIME too, `Accept-Encoding:` must not
        assert_eq!(detect_context("accept: ", 0, 8), CompletionContext::MimeType);
        assert_eq!(
            detect_context("Accept-Encoding: ", 0, 17),
            CompletionContext::RequestLine
        );
    }

    #[test]
    fn context_inside_braces() {
        // line 2 = `POST {{host}}/login`, cursor inside `{{ho|st}}`
        assert_eq!(
            detect_context(DOC, 2, 9),
            CompletionContext::VariableReference {
                partial: "ho".to_string(),
                open: 5,
            }
        );
        // right after `{{`
        assert_eq!(
            detect_context(DOC, 2, 7),
            CompletionContext::VariableReference {
                partial: String::new(),
                open: 5,
            }
        );
        // after the closing `}}` the brace context ends
        assert_eq!(detect_context(DOC, 2, 14), CompletionContext::RequestLine);
    }

    #[test]
    fn context_request_variable_path_after_dot() {
        let doc = "# @name login\nPOST https://x.com\n\n###\nGET {{login.";
        let line = "GET {{login.";
        assert_eq!(
            detect_context(doc, 4, line.len() as u32),
            CompletionContext::RequestVariablePath {
                path: "login.".to_string(),
                open: 4,
            }
        );
    }

    // -- items per context -------------------------------------------------

    #[test]
    fn request_line_items_have_method_snippets_and_variables() {
        let environment = env(&[("envToken", "abc")]);
        let items = completion_items(DOC, pos(2, 0), &environment, &NoRequestVariables);

        let get = find(&items, "GET");
        assert_eq!(get.kind, Some(CompletionItemKind::METHOD));
        assert_eq!(get.insert_text.as_deref(), Some("GET ${1:url}"));
        assert_eq!(get.insert_text_format, Some(InsertTextFormat::SNIPPET));

        // variables ride along, wrapped in braces outside `{{`
        let host = find(&items, "host");
        assert_eq!(host.detail.as_deref(), Some("HTTP FileCustomVariable"));
        assert_eq!(host.insert_text.as_deref(), Some("{{host}}"));
        let guid = find(&items, "$guid");
        assert_eq!(guid.insert_text.as_deref(), Some("{{$guid}}"));
        assert_eq!(guid.insert_text_format, None, "$guid is a plain insert");
        let token = find(&items, "envToken");
        assert_eq!(token.detail.as_deref(), Some("HTTP EnvironmentCustomVariable"));
    }

    #[test]
    fn header_items_carry_descriptions() {
        let environment = env(&[]);
        let items = completion_items(DOC, pos(3, 0), &environment, &NoRequestVariables);
        let auth = find(&items, "Authorization");
        assert_eq!(auth.kind, Some(CompletionItemKind::PROPERTY));
        assert_eq!(auth.detail.as_deref(), Some("HTTP Header"));
        assert!(matches!(&auth.documentation, Some(Documentation::String(s)) if s.contains("credentials")));
        assert!(labels(&items).contains(&"X-Http-Method-Override"));
        // no methods in header context
        assert!(!labels(&items).contains(&"POST"));
    }

    #[test]
    fn mime_items_on_content_type_line() {
        let environment = env(&[]);
        let items = completion_items(DOC, pos(4, 14), &environment, &NoRequestVariables);
        for mime in MIME_TYPES {
            assert!(labels(&items).contains(&mime), "missing {mime}");
        }
        assert!(!labels(&items).contains(&"Accept"), "no header names here");
        assert!(labels(&items).contains(&"$guid"), "variables still offered");
    }

    #[test]
    fn auth_items_are_snippets() {
        let environment = env(&[]);
        let items = completion_items(DOC, pos(10, 15), &environment, &NoRequestVariables);
        let basic = find(&items, "Basic Base64");
        assert_eq!(
            basic.insert_text.as_deref(),
            Some("Basic ${1:base64-user-password}")
        );
        assert_eq!(basic.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert!(labels(&items).contains(&"Digest"));
    }

    #[test]
    fn variables_inside_braces_replace_partial_via_text_edit() {
        let environment = env(&[]);
        // cursor inside `{{ho|` on line 2 (open brace at byte 5)
        let items = completion_items(DOC, pos(2, 9), &environment, &NoRequestVariables);
        assert!(!labels(&items).contains(&"GET"), "no methods inside braces");

        let host = find(&items, "host");
        let edit = edit_of(host);
        assert_eq!(edit.new_text, "host");
        assert_eq!(edit.range.start, pos(2, 7));
        assert_eq!(edit.range.end, pos(2, 9));

        let random = find(&items, "$randomInt");
        let edit = edit_of(random);
        assert_eq!(edit.new_text, "\\$randomInt ${1:min} ${2:max}");
        assert_eq!(random.insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn request_variable_item_shows_inactive_until_sent() {
        let environment = env(&[]);
        let items = completion_items(DOC, pos(2, 0), &environment, &NoRequestVariables);
        let login = find(&items, "login");
        assert_eq!(login.detail.as_deref(), Some("HTTP RequestCustomVariable"));
        assert!(matches!(
            &login.documentation,
            Some(Documentation::MarkupContent(m)) if m.value.contains("*(Inactive)*")
        ));
        assert!(login
            .insert_text
            .as_deref()
            .unwrap()
            .starts_with("{{login.${1|request,response|}"));

        let items = completion_items(DOC, pos(2, 0), &environment, &FakeCache);
        let login = find(&items, "login");
        assert!(matches!(
            &login.documentation,
            Some(Documentation::MarkupContent(m)) if !m.value.contains("Inactive")
        ));
    }

    // -- request variable path stages ---------------------------------------

    const PATH_DOC_HEAD: &str = "# @name login\nPOST https://x.com/login\n\n###\n";

    fn path_items(line5: &str, source: &dyn RequestVariableSource) -> Vec<CompletionItem> {
        let doc = format!("{PATH_DOC_HEAD}{line5}");
        let environment = env(&[]);
        completion_items(&doc, pos(4, line5.len() as u32), &environment, source)
    }

    #[test]
    fn path_stage_one_offers_request_and_response() {
        let items = path_items("GET {{login.", &NoRequestVariables);
        assert_eq!(labels(&items), vec!["request", "response"]);
        // undefined request variable name -> nothing
        assert!(path_items("GET {{nope.", &NoRequestVariables).is_empty());
    }

    #[test]
    fn path_stage_two_offers_body_and_headers() {
        let items = path_items("GET {{login.response.", &NoRequestVariables);
        assert_eq!(labels(&items), vec!["body", "headers"]);
        let items = path_items("GET {{login.request.", &NoRequestVariables);
        assert_eq!(labels(&items), vec!["body", "headers"]);
        // junk entity -> nothing
        assert!(path_items("GET {{login.bogus.", &NoRequestVariables).is_empty());
    }

    #[test]
    fn path_headers_stage_lists_cached_header_names() {
        let items = path_items("GET {{login.response.headers.", &FakeCache);
        assert_eq!(labels(&items), vec!["Content-Type", "X-Request-Id"]);
        let ct = find(&items, "Content-Type");
        assert!(matches!(
            &ct.documentation,
            Some(Documentation::MarkupContent(m)) if m.value.contains("application/json")
        ));
        // not sent -> no headers to offer
        assert!(path_items("GET {{login.response.headers.", &NoRequestVariables).is_empty());
        // FakeCache only caches the response entity
        assert!(path_items("GET {{login.request.headers.", &FakeCache).is_empty());
    }

    #[test]
    fn path_body_stage_offers_wildcard_and_jsonpath_hint() {
        let items = path_items("GET {{login.response.body.", &NoRequestVariables);
        assert_eq!(labels(&items), vec!["*", "$"]);
    }

    #[test]
    fn path_partial_segment_is_replaced_via_text_edit() {
        // typed `res` after `login.` — stage 1 items replacing the partial
        let line = "GET {{login.res";
        let items = path_items(line, &NoRequestVariables);
        assert_eq!(labels(&items), vec!["request", "response"]);
        let edit = edit_of(find(&items, "response"));
        assert_eq!(edit.new_text, "response");
        // `GET {{` = 6 cols, `login.` = 6 more -> partial starts at col 12
        assert_eq!(edit.range.start, pos(4, 12));
        assert_eq!(edit.range.end, pos(4, line.len() as u32));
    }

    // -- request variable definitions ---------------------------------------

    #[test]
    fn request_variable_names_follow_original_definition_regex() {
        let doc = "## @name a\n\
                   //  @name b\n\
                   #@name c\n\
                   # @namex d\n\
                   # @name e f\n\
                   # @name dup\n\
                   # @name dup\n\
                   @name = notarequest";
        assert_eq!(request_variable_names(doc), vec!["a", "b", "dup"]);
    }

    // -- helpers -------------------------------------------------------------

    #[test]
    fn utf16_helpers_round_trip_wide_chars() {
        let line = "한글 {{x}}";
        let byte = line.find("{{").unwrap();
        assert_eq!(byte_to_utf16(line, byte), 3);
        assert_eq!(utf16_to_byte(line, 3), byte);
        assert_eq!(utf16_to_byte(line, 999), line.len());
    }
}
