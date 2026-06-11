//! Response display: write to a stable file path, then open a tab via the
//! `zed` CLI. Zed has no `window/showDocument`, so the CLI spawn is the only
//! automatic open path; overwriting the same path makes Zed reload an
//! already-open clean buffer in place (cursor kept, focus not stolen).

#![allow(dead_code)] // scaffold: wired up incrementally

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::http::HttpResponse;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// Status line + headers + formatted body, written as `.txt`.
    Full,
    /// Body only, extension derived from the response MIME type
    /// (`.json`, `.xml`, `.html`, ...; fallback `.txt`).
    BodyOnly,
}

#[derive(Debug, Error)]
pub enum ShowError {
    #[error("failed to write response file: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "zed CLI not found — run 'zed: install cli' inside Zed, then resend. \
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
    let _ = (request_name, mode, content_type);
    todo!()
}

/// Strips path-unsafe characters; empty results fall back to a
/// method+host-derived name.
pub fn sanitize_file_name(name: &str) -> String {
    let _ = name;
    todo!()
}

/// Renders the response document: status line, headers, body pretty-printed
/// by MIME type (JSON indented, etc.). Returns bytes so binary bodies pass
/// through untouched in `BodyOnly` mode.
pub fn format_response(response: &HttpResponse, mode: DisplayMode) -> Vec<u8> {
    let _ = (response, mode);
    todo!()
}

/// Writes (overwrites) the response file, then opens it with `zed <path>`.
/// The write happens first so a missing CLI still leaves the file on disk —
/// `ShowError::ZedCliMissing` carries the path for the user-facing message.
pub fn show_response(
    response: &HttpResponse,
    request_name: &str,
    mode: DisplayMode,
) -> Result<PathBuf, ShowError> {
    let _ = (response, request_name, mode);
    todo!()
}

/// Spawns `zed <path>` detached. Single seam for the tab-open mechanism so it
/// can be swapped if Zed ever supports `window/showDocument`.
pub fn open_in_zed(path: &Path) -> Result<(), ShowError> {
    let _ = path;
    todo!()
}
