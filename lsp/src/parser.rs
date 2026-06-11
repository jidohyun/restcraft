//! Stage 1 (`parse_document`) and stage 3 (`parse_request`) of the send pipeline.
//! Stage 2 (variable substitution) lives in `variables.rs` and runs on
//! `RequestBlock::text` between the two stages.
//!
//! Semantics mirror vscode-restclient `utils/selector.ts` and
//! `utils/httpRequestParser.ts`.

#![allow(dead_code)] // scaffold: wired up incrementally

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
    /// Full extent of the block, including separator/comment lines.
    pub range: LineRange,
    /// Line the request line sits on — where the "Send Request" code lens goes.
    pub lens_line: u32,
    pub metadata: RequestMetadata,
    /// Request text (request line + headers + body) with comments and
    /// metadata lines stripped. Variables are NOT substituted yet.
    pub text: String,
}

/// `@name = value` file-level variable, raw (value may reference `{{other}}`).
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
    let _ = text;
    todo!("port vscode-restclient utils/selector.ts")
}

/// Returns the block containing `line` (code lens/action and executeCommand
/// both address blocks by line).
pub fn block_at_line(document: &ParsedDocument, line: u32) -> Option<&RequestBlock> {
    let _ = (document, line);
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    Text(String),
    /// `< ./payload.json` external body; path resolved against the .http file dir.
    File(PathBuf),
}

/// A fully substituted, sendable request.
#[derive(Debug, Clone)]
pub struct ParsedRequest {
    /// Uppercase. `GET` when the request line omits the method.
    pub method: String,
    pub url: String,
    /// `HTTP/1.1` etc. when present on the request line.
    pub http_version: Option<String>,
    /// In order of appearance; duplicates preserved.
    pub headers: Vec<(String, String)>,
    pub body: Option<RequestBody>,
}

impl ParsedRequest {
    /// Case-insensitive lookup of the first header with `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        let _ = name;
        todo!()
    }

    /// `X-Request-Type: GraphQL` — http.rs must wrap the body into
    /// `{"query": ..., "variables": ...}` before sending.
    pub fn is_graphql(&self) -> bool {
        todo!()
    }
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("request block contains no request line")]
    EmptyBlock,
    #[error("malformed request line: {0}")]
    MalformedRequestLine(String),
    #[error("malformed header: {0}")]
    MalformedHeader(String),
}

/// Stage 3: parse substituted block text into a sendable request.
/// Handles: method omission (defaults to GET), multi-line query continuation
/// lines starting with `?`/`&`, headers, blank-line-separated body, and
/// `< filepath` external bodies (resolved against `base_dir`).
pub fn parse_request(text: &str, base_dir: &Path) -> Result<ParsedRequest, ParseError> {
    let _ = (text, base_dir);
    todo!("port vscode-restclient utils/httpRequestParser.ts")
}
