//! Request variable chaining (Phase 2): after a `# @name foo` request is
//! sent, `{{foo.(request|response).(body|headers).<path>}}` references in
//! later requests resolve against the cached exchange.
//!
//! Faithful port of vscode-restclient's
//! `utils/requestVariableCacheValueProcessor.ts` and
//! `utils/httpVariableProviders/requestVariableProvider.ts`.
//!
//! Reference grammar (original regex
//! `^(\w+)(?:\.(request|response)(?:\.(body|headers)(?:\.(.*))?)?)?$`):
//! - body paths: `*` = whole body regardless of Content-Type; otherwise the
//!   query engine is chosen by Content-Type exactly like the original —
//!   JSONPath when it is JSON (or `asJson.`-forced, or a JavaScript
//!   Content-Type whose body parses as JSON), XPath when it is XML (or
//!   `asXml.`-forced). Anything else warns `UnsupportedBodyContentType`.
//! - header paths: case-insensitive header name lookup.
//! - partial references degrade to the original's warnings
//!   (`MissingRequestEntityName` → … → `MissingBodyPath`/`MissingHeaderName`).
//!
//! # Dialect differences vs the original
//!
//! JSONPath uses the RFC 9535 `serde_json_path` crate; the original uses
//! `jsonpath-plus`, which has non-standard extensions:
//! - the leading `$` is mandatory here (jsonpath-plus tolerates some
//!   unrooted forms);
//! - jsonpath-plus extras are unsupported: parent (`^`), property name
//!   (`~`), type selectors (`@string()` …) and arbitrary JS in filters —
//!   filters work, but only with RFC 9535 syntax (`?@.price < 10`);
//! - on multiple matches BOTH return the first match (original `result[0]`);
//! - non-string matches are serialized compactly like `JSON.stringify`, but
//!   serde_json orders object keys alphabetically, not by insertion order;
//! - an empty-string match warns `IncorrectJsonPath` — the original's JS
//!   falsy check does the same, so the quirk is kept on purpose;
//! - a JSON Content-Type with an unparsable body warns `InvalidJsonBody`
//!   here; the original throws an uncaught exception.
//!
//! XPath (1.0) is best-effort via `sxd-document`/`sxd-xpath`:
//! - no namespace bindings are registered, so prefixed queries won't match;
//! - malformed XML warns `InvalidXPath` (xmldom is more lenient);
//! - element results serialize their children with a minimal writer using
//!   local names (original `childNodes.toString()`), attributes resolve to
//!   their value, document results to the serialized root element;
//! - boolean/number XPath results stringify (the original crashes on them).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value as JsonValue;
use serde_json_path::JsonPath;
use sxd_document::dom::{ChildOfElement, ChildOfRoot, Element};
use sxd_xpath::nodeset::Node;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Cached exchange (stored per document/name in state.rs)
// ---------------------------------------------------------------------------

/// One sent `# @name` exchange. `state::ServerState` keys these by document
/// URI + request name, mirroring the original document-scoped
/// `RequestVariableCache`.
#[derive(Debug, Clone)]
pub struct CachedExchange {
    pub request: CachedRequest,
    pub response: CachedResponse,
}

/// The request as actually sent (after variable substitution).
#[derive(Debug, Clone)]
pub struct CachedRequest {
    // Not read by path resolution; captured for parity with the original
    // cache value (and for a richer hover later).
    #[allow(dead_code)]
    pub method: String,
    #[allow(dead_code)]
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Empty = no body (the original's JS falsy check treats `""` as absent).
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct CachedResponse {
    // Same as CachedRequest.method/url: original-cache parity, not rendered.
    #[allow(dead_code)]
    pub status: u16,
    #[allow(dead_code)]
    pub status_text: String,
    /// In received order, duplicates preserved (http.rs lowercases names;
    /// lookups here are case-insensitive anyway).
    pub headers: Vec<(String, String)>,
    /// Empty = no body. Lossy UTF-8 of the wire bytes.
    pub body: String,
}

// ---------------------------------------------------------------------------
// Resolve results (port of models/httpVariableResolveResult.ts)
// ---------------------------------------------------------------------------

/// `ResolveErrorMessage` — hard errors: the reference can never resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ResolveError {
    #[error("Request variable path is not provided")]
    NoPath,
    #[error(
        "Incorrect request variable reference syntax, it should be \
         \"variableName.(response|request).(headers|body).(headerName|JSONPath|XPath|*)\""
    )]
    InvalidReference,
    #[error("Request variable does not exist")]
    NotExist,
}

/// `ResolveWarningMessage` — soft failures: the reference is understandable
/// but does not (yet) resolve to a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ResolveWarning {
    #[error("Request variable has not been sent")]
    NotSent,
    #[error(
        "Http entity name \"response\" or \"request\" should be provided \
         right after the request variable name"
    )]
    MissingRequestEntityName,
    #[error(
        "Http entity part \"headers\" or \"body\" should be provided \
         right after the http entity name"
    )]
    MissingRequestEntityPart,
    #[error("Header name should be provided right after \"headers\"")]
    MissingHeaderName,
    #[error("Body path should be provided right after \"body\"")]
    MissingBodyPath,
    #[error("Request body of given request doesn't exist")]
    RequestBodyNotExist,
    #[error("Response body of given request doesn't exist")]
    ResponseBodyNotExist,
    #[error("No value is resolved for given header name")]
    IncorrectHeaderName,
    #[error("No value is resolved for given JSONPath")]
    IncorrectJsonPath,
    #[error("Invalid JSONPath query")]
    InvalidJsonPath,
    #[error("No value is resolved for given XPath")]
    IncorrectXPath,
    #[error("Invalid XPath query")]
    InvalidXPath,
    #[error("Only JSON and XML response/request body is supported to query the result")]
    UnsupportedBodyContentType,
    /// Rust-specific: the original `JSON.parse` throws uncaught when a JSON
    /// Content-Type carries a non-JSON body; we degrade to a warning.
    #[error("Body is not valid JSON although the Content-Type indicates JSON")]
    InvalidJsonBody,
}

/// Port of `ResolveResult` (`ResolveState` folded into the variants).
/// `Warning::value` carries the partial value the original attaches where it
/// is cheap and meaningful (whole body, formatted headers) — hover can show
/// it even when substitution must leave the reference verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveResult {
    Success(String),
    Warning {
        warning: ResolveWarning,
        value: Option<String>,
    },
    Error(ResolveError),
}

fn warn(warning: ResolveWarning, value: Option<String>) -> ResolveResult {
    ResolveResult::Warning { warning, value }
}

// ---------------------------------------------------------------------------
// Per-document view (port of RequestVariableProvider)
// ---------------------------------------------------------------------------

/// Everything one substitution/hover pass needs: the `# @name`s declared in
/// the document (sent or not) plus the cached exchanges for those already
/// sent. Build it from `parser::ParsedDocument` metadata names and
/// `ServerState::request_variables_snapshot`.
#[derive(Debug, Default)]
pub struct RequestVariables {
    declared: HashSet<String>,
    cache: HashMap<String, Arc<CachedExchange>>,
}

impl RequestVariables {
    pub fn new(
        declared: impl IntoIterator<Item = String>,
        cache: HashMap<String, Arc<CachedExchange>>,
    ) -> Self {
        Self {
            declared: declared.into_iter().collect(),
            cache,
        }
    }

    /// Declared `# @name`s. Unused for now: completion scans the document
    /// text itself (original definition regex); kept as the API for unifying
    /// the two name sources later.
    #[allow(dead_code)]
    pub fn declared_names(&self) -> impl Iterator<Item = &str> {
        self.declared.iter().map(String::as_str)
    }

    /// The cached exchange for `name`, when that request has been sent.
    pub fn exchange(&self, name: &str) -> Option<&CachedExchange> {
        self.cache.get(name).map(Arc::as_ref)
    }

    /// Whether `reference` (full `{{...}}` inner text) names a request
    /// variable: original `RequestVariableProvider.has` — the first
    /// `.`-segment must be a declared `# @name`. A `true` here claims the
    /// reference for this provider even when resolution then fails (the
    /// original `break`s out of the provider chain on warning/error).
    pub fn is_request_variable(&self, reference: &str) -> bool {
        self.declared.contains(first_segment(reference))
    }

    /// Original `RequestVariableProvider.get`: undeclared name → `NotExist`,
    /// declared but never sent → `NotSent`, otherwise path resolution against
    /// the cached exchange.
    pub fn resolve(&self, reference: &str) -> ResolveResult {
        let name = first_segment(reference);
        if !self.declared.contains(name) {
            return ResolveResult::Error(ResolveError::NotExist);
        }
        match self.cache.get(name) {
            None => warn(ResolveWarning::NotSent, None),
            Some(exchange) => resolve_path(exchange, reference),
        }
    }
}

/// `name.trim().split('.')[0]`, as in the original provider.
fn first_segment(reference: &str) -> &str {
    reference.trim().split('.').next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Path resolution (port of RequestVariableCacheValueProcessor)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Entity {
    Request,
    Response,
}

#[derive(Debug, Clone, Copy)]
enum Part {
    Body,
    Headers,
}

struct Reference<'a> {
    entity: Option<Entity>,
    part: Option<Part>,
    /// Header name / JSONPath / XPath / `*`. `Some("")` (trailing dot) is
    /// normalized to missing later, matching the JS falsy check.
    name_or_path: Option<&'a str>,
}

/// Resolves `reference` (e.g. `login.response.body.$.token`) against a
/// cached exchange — `RequestVariableCacheValueProcessor.resolveRequestVariable`.
pub fn resolve_path(exchange: &CachedExchange, reference: &str) -> ResolveResult {
    if reference.is_empty() {
        return ResolveResult::Error(ResolveError::NoPath);
    }
    let Some(parsed) = parse_reference(reference) else {
        return ResolveResult::Error(ResolveError::InvalidReference);
    };
    let Some(entity) = parsed.entity else {
        return warn(ResolveWarning::MissingRequestEntityName, None);
    };
    let (headers, body, is_request) = match entity {
        Entity::Request => (&exchange.request.headers, &exchange.request.body, true),
        Entity::Response => (&exchange.response.headers, &exchange.response.body, false),
    };
    let Some(part) = parsed.part else {
        return warn(ResolveWarning::MissingRequestEntityPart, None);
    };
    let name_or_path = parsed.name_or_path.filter(|p| !p.is_empty());
    match part {
        Part::Body => resolve_body(body, headers, name_or_path, is_request),
        Part::Headers => resolve_headers(headers, name_or_path),
    }
}

/// Hand-rolled port of
/// `^(\w+)(?:\.(request|response)(?:\.(body|headers)(?:\.(.*))?)?)?$`.
/// `None` = no match (`InvalidReference`).
fn parse_reference(reference: &str) -> Option<Reference<'_>> {
    let name_len = reference
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(reference.len());
    if name_len == 0 {
        return None;
    }
    let rest = &reference[name_len..];
    if rest.is_empty() {
        return Some(Reference {
            entity: None,
            part: None,
            name_or_path: None,
        });
    }

    let rest = rest.strip_prefix('.')?;
    let (entity, rest) = if let Some(rest) = rest.strip_prefix("response") {
        (Entity::Response, rest)
    } else if let Some(rest) = rest.strip_prefix("request") {
        (Entity::Request, rest)
    } else {
        return None;
    };
    if rest.is_empty() {
        return Some(Reference {
            entity: Some(entity),
            part: None,
            name_or_path: None,
        });
    }

    let rest = rest.strip_prefix('.')?;
    let (part, rest) = if let Some(rest) = rest.strip_prefix("headers") {
        (Part::Headers, rest)
    } else if let Some(rest) = rest.strip_prefix("body") {
        (Part::Body, rest)
    } else {
        return None;
    };
    if rest.is_empty() {
        return Some(Reference {
            entity: Some(entity),
            part: Some(part),
            name_or_path: None,
        });
    }

    let name_or_path = rest.strip_prefix('.')?;
    Some(Reference {
        entity: Some(entity),
        part: Some(part),
        name_or_path: Some(name_or_path),
    })
}

fn resolve_body(
    body: &str,
    headers: &[(String, String)],
    name_or_path: Option<&str>,
    is_request: bool,
) -> ResolveResult {
    if body.is_empty() {
        let warning = if is_request {
            ResolveWarning::RequestBodyNotExist
        } else {
            ResolveWarning::ResponseBodyNotExist
        };
        return warn(warning, None);
    }
    let Some(mut path) = name_or_path else {
        return warn(ResolveWarning::MissingBodyPath, Some(body.to_string()));
    };

    // '*' fetches the whole body regardless of the Content-Type.
    if path == "*" {
        return ResolveResult::Success(body.to_string());
    }

    let mut force_json = false;
    let mut force_xml = false;
    if let Some(stripped) = path.strip_prefix("asJson.") {
        path = stripped;
        force_json = true;
    } else if let Some(stripped) = path.strip_prefix("asXml.") {
        path = stripped;
        force_xml = true;
    }

    let content_type = get_header(headers, "content-type");
    let content_type = content_type.as_deref();
    if is_json_mime(content_type)
        || ((force_json || is_javascript_mime(content_type)) && is_json_string(body))
    {
        match serde_json::from_str::<JsonValue>(body) {
            Ok(json) => resolve_json_body(&json, path),
            Err(_) => warn(ResolveWarning::InvalidJsonBody, None),
        }
    } else if force_xml || is_xml_mime(content_type) {
        resolve_xml_body(body, path)
    } else {
        warn(
            ResolveWarning::UnsupportedBodyContentType,
            Some(body.to_string()),
        )
    }
}

fn resolve_headers(headers: &[(String, String)], name: Option<&str>) -> ResolveResult {
    let Some(name) = name else {
        let formatted = headers
            .iter()
            .map(|(n, v)| format!("{n}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        return warn(ResolveWarning::MissingHeaderName, Some(formatted));
    };
    match get_header(headers, name) {
        // JS falsy check: an empty header value also warns.
        Some(value) if !value.is_empty() => ResolveResult::Success(value),
        _ => warn(ResolveWarning::IncorrectHeaderName, None),
    }
}

fn resolve_json_body(json: &JsonValue, path: &str) -> ResolveResult {
    let Ok(query) = JsonPath::parse(path) else {
        return warn(ResolveWarning::InvalidJsonPath, None);
    };
    // Original takes `result[0]` of the match array.
    let Some(first) = query.query(json).into_iter().next() else {
        return warn(ResolveWarning::IncorrectJsonPath, None);
    };
    let value = match first {
        // Strings are taken raw; everything else like `JSON.stringify`.
        JsonValue::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    if value.is_empty() {
        // JS falsy quirk: an empty-string match reports like a miss.
        return warn(ResolveWarning::IncorrectJsonPath, None);
    }
    ResolveResult::Success(value)
}

fn resolve_xml_body(body: &str, path: &str) -> ResolveResult {
    use sxd_xpath::{Context, Factory, Value};

    let Ok(package) = sxd_document::parser::parse(body) else {
        // xmldom is lenient; sxd is not — degrade to InvalidXPath.
        return warn(ResolveWarning::InvalidXPath, None);
    };
    let document = package.as_document();
    let xpath = match Factory::new().build(path) {
        Ok(Some(xpath)) => xpath,
        _ => return warn(ResolveWarning::InvalidXPath, None),
    };
    let value = match xpath.evaluate(&Context::new(), document.root()) {
        Ok(value) => value,
        Err(_) => return warn(ResolveWarning::InvalidXPath, None),
    };
    match value {
        Value::String(s) => ResolveResult::Success(s),
        // The original crashes on non-string non-nodeset results; stringify.
        Value::Boolean(b) => ResolveResult::Success(b.to_string()),
        Value::Number(n) => ResolveResult::Success(format_xpath_number(n)),
        Value::Nodeset(nodes) => match nodes.document_order_first() {
            None => warn(ResolveWarning::IncorrectXPath, None),
            Some(node) => resolve_xml_node(node),
        },
    }
}

/// XPath numbers print like JS (`2`, not `2.0`).
fn format_xpath_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

fn resolve_xml_node(node: Node) -> ResolveResult {
    match node {
        // Original: `documentElement.toString()`.
        Node::Root(root) => {
            let element = root.children().into_iter().find_map(|c| match c {
                ChildOfRoot::Element(e) => Some(e),
                _ => None,
            });
            match element {
                Some(element) => {
                    let mut out = String::new();
                    serialize_element(element, &mut out);
                    ResolveResult::Success(out)
                }
                None => warn(ResolveWarning::IncorrectXPath, None),
            }
        }
        // Original: `childNodes.toString()` — concatenated child markup;
        // for a text-only element this is its text.
        Node::Element(element) => {
            let mut out = String::new();
            for child in element.children() {
                serialize_child(child, &mut out);
            }
            ResolveResult::Success(out)
        }
        // Original: `nodeValue` for everything else (attributes, text, ...).
        Node::Attribute(attribute) => ResolveResult::Success(attribute.value().to_string()),
        Node::Text(text) => ResolveResult::Success(text.text().to_string()),
        Node::Comment(comment) => ResolveResult::Success(comment.text().to_string()),
        Node::ProcessingInstruction(pi) => {
            ResolveResult::Success(pi.value().unwrap_or("").to_string())
        }
        Node::Namespace(ns) => ResolveResult::Success(ns.uri().to_string()),
    }
}

/// Minimal element writer (local names only) for XPath element/document
/// results — fidelity target is xmldom's `toString()`, best-effort.
fn serialize_element(element: Element, out: &mut String) {
    let tag = match element.preferred_prefix() {
        Some(prefix) => format!("{prefix}:{}", element.name().local_part()),
        None => element.name().local_part().to_string(),
    };
    out.push('<');
    out.push_str(&tag);
    for attribute in element.attributes() {
        out.push(' ');
        out.push_str(attribute.name().local_part());
        out.push_str("=\"");
        out.push_str(&escape_xml_attr(attribute.value()));
        out.push('"');
    }
    let children = element.children();
    if children.is_empty() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for child in children {
        serialize_child(child, out);
    }
    out.push_str("</");
    out.push_str(&tag);
    out.push('>');
}

fn serialize_child(child: ChildOfElement, out: &mut String) {
    match child {
        ChildOfElement::Element(element) => serialize_element(element, out),
        ChildOfElement::Text(text) => out.push_str(&escape_xml_text(text.text())),
        ChildOfElement::Comment(comment) => {
            out.push_str("<!--");
            out.push_str(comment.text());
            out.push_str("-->");
        }
        ChildOfElement::ProcessingInstruction(pi) => {
            out.push_str("<?");
            out.push_str(pi.target());
            if let Some(value) = pi.value() {
                out.push(' ');
                out.push_str(value);
            }
            out.push_str("?>");
        }
    }
}

fn escape_xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_xml_attr(text: &str) -> String {
    escape_xml_text(text).replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Header / MIME helpers (ports of utils/misc.ts + utils/mimeUtility.ts)
// ---------------------------------------------------------------------------

/// Case-insensitive header lookup. Multiple matches join with `,` like a JS
/// array's `toString()` (node folds repeated headers into arrays).
fn get_header(headers: &[(String, String)], name: &str) -> Option<String> {
    let matches: Vec<&str> = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect();
    if matches.is_empty() {
        None
    } else {
        Some(matches.join(","))
    }
}

/// `type/subtype` lowercased, parameters stripped.
fn mime_essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn mime_subtype(essence: &str) -> &str {
    essence.split('/').nth(1).unwrap_or("")
}

fn is_json_mime(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let essence = mime_essence(content_type);
    let subtype = mime_subtype(&essence);
    essence == "application/json"
        || essence == "text/json"
        || subtype.ends_with("+json")
        || subtype.starts_with("x-amz-json")
}

fn is_xml_mime(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let essence = mime_essence(content_type);
    let subtype = mime_subtype(&essence);
    essence == "application/xml" || essence == "text/xml" || subtype.ends_with("+xml")
}

fn is_javascript_mime(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let essence = mime_essence(content_type);
    essence == "application/javascript" || essence == "text/javascript"
}

fn is_json_string(text: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(content_type: &str, body: &str) -> CachedExchange {
        CachedExchange {
            request: CachedRequest {
                method: "POST".into(),
                url: "https://example.com/login".into(),
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("X-Client".into(), "restcraft".into()),
                ],
                body: r#"{"user":"jido"}"#.into(),
            },
            response: CachedResponse {
                status: 200,
                status_text: "OK".into(),
                headers: vec![
                    ("content-type".into(), content_type.into()),
                    ("x-token".into(), "secret-token".into()),
                    ("x-empty".into(), String::new()),
                ],
                body: body.into(),
            },
        }
    }

    fn json_exchange() -> CachedExchange {
        exchange(
            "application/json; charset=utf-8",
            r#"{"id": 7, "user": {"profile": {"name": "jido"}}, "items": [{"id": "a"}, {"id": "b"}], "ok": false, "none": null, "empty": ""}"#,
        )
    }

    fn vars(declared: &[&str], sent: &[(&str, CachedExchange)]) -> RequestVariables {
        RequestVariables::new(
            declared.iter().map(|s| s.to_string()),
            sent.iter()
                .map(|(n, e)| (n.to_string(), Arc::new(e.clone())))
                .collect(),
        )
    }

    fn success(result: ResolveResult) -> String {
        match result {
            ResolveResult::Success(value) => value,
            other => panic!("expected success, got {other:?}"),
        }
    }

    fn warning(result: ResolveResult) -> ResolveWarning {
        match result {
            ResolveResult::Warning { warning, .. } => warning,
            other => panic!("expected warning, got {other:?}"),
        }
    }

    // --- declaration / sent-state gates (RequestVariableProvider) ---

    #[test]
    fn undeclared_name_is_not_exist_and_not_claimed() {
        let v = vars(&["login"], &[]);
        assert!(!v.is_request_variable("other.response.body.*"));
        assert_eq!(
            v.resolve("other.response.body.*"),
            ResolveResult::Error(ResolveError::NotExist)
        );
    }

    #[test]
    fn declared_but_unsent_warns_not_sent() {
        let v = vars(&["login"], &[]);
        assert!(v.is_request_variable("login.response.body.*"));
        assert_eq!(warning(v.resolve("login.response.body.*")), ResolveWarning::NotSent);
    }

    #[test]
    fn invalid_reference_syntax_is_error() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        for reference in ["login.foo", "login.response.foo", "login.responsex.body", "login.."] {
            assert_eq!(
                v.resolve(reference),
                ResolveResult::Error(ResolveError::InvalidReference),
                "reference: {reference}"
            );
        }
    }

    #[test]
    fn partial_references_degrade_to_original_warnings() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(
            warning(v.resolve("login")),
            ResolveWarning::MissingRequestEntityName
        );
        assert_eq!(
            warning(v.resolve("login.response")),
            ResolveWarning::MissingRequestEntityPart
        );
        // `Some(body)` partial value travels with the MissingBodyPath warning.
        match v.resolve("login.response.body") {
            ResolveResult::Warning {
                warning: ResolveWarning::MissingBodyPath,
                value: Some(body),
            } => assert!(body.contains("jido")),
            other => panic!("expected MissingBodyPath with body, got {other:?}"),
        }
        assert_eq!(
            warning(v.resolve("login.response.headers")),
            ResolveWarning::MissingHeaderName
        );
        // Trailing dot (empty path) is falsy in the original too.
        assert_eq!(
            warning(v.resolve("login.response.headers.")),
            ResolveWarning::MissingHeaderName
        );
    }

    // --- body: '*' and content-type gating ---

    #[test]
    fn star_returns_whole_body_regardless_of_content_type() {
        let v = vars(&["r"], &[("r", exchange("application/octet-stream", "raw-bytes"))]);
        assert_eq!(success(v.resolve("r.response.body.*")), "raw-bytes");
    }

    #[test]
    fn jsonpath_requires_json_content_type_unless_forced() {
        let v = vars(&["r"], &[("r", exchange("text/plain", r#"{"id": 7}"#))]);
        assert_eq!(
            warning(v.resolve("r.response.body.$.id")),
            ResolveWarning::UnsupportedBodyContentType
        );
        // `asJson.` forces the JSON engine for a parsable body.
        assert_eq!(success(v.resolve("r.response.body.asJson.$.id")), "7");
    }

    #[test]
    fn javascript_content_type_with_json_body_uses_jsonpath() {
        let v = vars(&["r"], &[("r", exchange("text/javascript", r#"{"id": 42}"#))]);
        assert_eq!(success(v.resolve("r.response.body.$.id")), "42");
    }

    #[test]
    fn json_content_type_with_invalid_body_degrades_to_warning() {
        let v = vars(&["r"], &[("r", exchange("application/json", "not json"))]);
        assert_eq!(
            warning(v.resolve("r.response.body.$.id")),
            ResolveWarning::InvalidJsonBody
        );
    }

    #[test]
    fn empty_body_warns_not_exist_per_entity() {
        let mut e = exchange("application/json", "");
        e.request.body = String::new();
        let v = vars(&["r"], &[("r", e)]);
        assert_eq!(
            warning(v.resolve("r.response.body.$.id")),
            ResolveWarning::ResponseBodyNotExist
        );
        assert_eq!(
            warning(v.resolve("r.request.body.$.id")),
            ResolveWarning::RequestBodyNotExist
        );
    }

    // --- body: JSONPath ---

    #[test]
    fn jsonpath_nested_object() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(
            success(v.resolve("login.response.body.$.user.profile.name")),
            "jido"
        );
    }

    #[test]
    fn jsonpath_array_index() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(success(v.resolve("login.response.body.$.items[1].id")), "b");
    }

    #[test]
    fn jsonpath_multiple_matches_return_first() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(
            success(v.resolve("login.response.body.$.items[*].id")),
            "a"
        );
    }

    #[test]
    fn jsonpath_non_string_values_are_stringified() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(success(v.resolve("login.response.body.$.id")), "7");
        assert_eq!(success(v.resolve("login.response.body.$.ok")), "false");
        assert_eq!(success(v.resolve("login.response.body.$.none")), "null");
        assert_eq!(
            success(v.resolve("login.response.body.$.user.profile")),
            r#"{"name":"jido"}"#
        );
    }

    #[test]
    fn jsonpath_no_match_and_empty_string_match_warn() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(
            warning(v.resolve("login.response.body.$.missing")),
            ResolveWarning::IncorrectJsonPath
        );
        // JS falsy quirk kept on purpose: "" resolves like a miss.
        assert_eq!(
            warning(v.resolve("login.response.body.$.empty")),
            ResolveWarning::IncorrectJsonPath
        );
    }

    #[test]
    fn jsonpath_invalid_query_warns() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(
            warning(v.resolve("login.response.body.$.[")),
            ResolveWarning::InvalidJsonPath
        );
    }

    // --- headers ---

    #[test]
    fn header_lookup_is_case_insensitive() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(
            success(v.resolve("login.response.headers.X-TOKEN")),
            "secret-token"
        );
        assert_eq!(
            success(v.resolve("login.response.headers.Content-Type")),
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn header_missing_or_empty_warns_incorrect_name() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(
            warning(v.resolve("login.response.headers.X-Nope")),
            ResolveWarning::IncorrectHeaderName
        );
        // Empty header value is falsy in the original.
        assert_eq!(
            warning(v.resolve("login.response.headers.x-empty")),
            ResolveWarning::IncorrectHeaderName
        );
    }

    // --- request-side references ---

    #[test]
    fn request_entity_body_and_headers_resolve() {
        let v = vars(&["login"], &[("login", json_exchange())]);
        assert_eq!(success(v.resolve("login.request.body.$.user")), "jido");
        assert_eq!(success(v.resolve("login.request.body.*")), r#"{"user":"jido"}"#);
        assert_eq!(
            success(v.resolve("login.request.headers.x-client")),
            "restcraft"
        );
    }

    // --- body: XPath ---

    #[test]
    fn xpath_element_resolves_to_child_content() {
        let body = "<root><token>abc</token><nested><a>1</a><b>2</b></nested></root>";
        let v = vars(&["r"], &[("r", exchange("application/xml", body))]);
        assert_eq!(success(v.resolve("r.response.body./root/token")), "abc");
        // Element with element children serializes their markup.
        assert_eq!(
            success(v.resolve("r.response.body./root/nested")),
            "<a>1</a><b>2</b>"
        );
    }

    #[test]
    fn xpath_attribute_and_text_nodes() {
        let body = r#"<root><token id="t-1">abc</token></root>"#;
        let v = vars(&["r"], &[("r", exchange("text/xml", body))]);
        assert_eq!(success(v.resolve("r.response.body./root/token/@id")), "t-1");
        assert_eq!(
            success(v.resolve("r.response.body./root/token/text()")),
            "abc"
        );
        // string(...) XPath functions return a string result directly.
        assert_eq!(
            success(v.resolve("r.response.body.string(/root/token)")),
            "abc"
        );
    }

    #[test]
    fn xpath_no_match_and_invalid_query_warn() {
        let body = "<root><token>abc</token></root>";
        let v = vars(&["r"], &[("r", exchange("application/xml", body))]);
        assert_eq!(
            warning(v.resolve("r.response.body./root/missing")),
            ResolveWarning::IncorrectXPath
        );
        assert_eq!(
            warning(v.resolve("r.response.body./root/token[")),
            ResolveWarning::InvalidXPath
        );
        // Malformed XML degrades to InvalidXPath (xmldom is more lenient).
        let bad = vars(&["r"], &[("r", exchange("application/xml", "<root><unclosed>"))]);
        assert_eq!(
            warning(bad.resolve("r.response.body./root")),
            ResolveWarning::InvalidXPath
        );
    }

    #[test]
    fn xml_with_plus_xml_subtype_is_accepted() {
        let body = "<feed><id>42</id></feed>";
        let v = vars(&["r"], &[("r", exchange("application/atom+xml", body))]);
        assert_eq!(success(v.resolve("r.response.body./feed/id")), "42");
    }
}
