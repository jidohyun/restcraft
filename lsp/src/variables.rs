//! Stage 2: `{{...}}` substitution on raw block text.
//!
//! Resolution order mirrors vscode-restclient `utils/variableProcessor.ts`:
//! system -> request (TODO, Phase 2) -> file -> environment.

#![allow(dead_code)] // scaffold: wired up incrementally

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;

use crate::parser::FileVariable;

#[derive(Debug, Error)]
pub enum VariableError {
    #[error("undefined variable: {0}")]
    Undefined(String),
    #[error("invalid system variable: {0}")]
    InvalidSystemVariable(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Everything needed to resolve `{{...}}` references for one send.
pub struct VariableContext<'a> {
    /// File variables in declaration order; values may themselves contain refs.
    pub file_variables: &'a [FileVariable],
    /// Already merged environment (`$shared` + selected env, selected wins) —
    /// see `settings::resolve_environment`.
    pub environment: &'a HashMap<String, String>,
    /// Directory of the .http file; `{{$dotenv ...}}` reads `.env` from here.
    pub document_dir: &'a Path,
}

/// Replaces every `{{...}}` occurrence in `text`. Unresolvable references
/// produce `VariableError::Undefined` (vscode-restclient leaves them in place
/// for hover, but fails the send — we fail the send).
pub fn substitute(text: &str, ctx: &VariableContext) -> Result<String, VariableError> {
    let _ = (text, ctx);
    todo!("port vscode-restclient utils/variableProcessor.ts + httpVariableProviders/")
}

/// Resolves one `$...` system variable expression (without braces):
/// - `$guid`
/// - `$randomInt min max`
/// - `$timestamp [offset unit]`
/// - `$datetime rfc1123|iso8601|"custom format" [offset unit]`
/// - `$localDatetime rfc1123|iso8601|"custom format" [offset unit]`
/// - `$processEnv [%]name`  (`%` = indirect via file/env variable)
/// - `$dotenv [%]name`
///
/// Returns `None` when `expr` is not a known system variable, so callers can
/// fall through to file/environment lookup.
pub fn resolve_system_variable(
    expr: &str,
    ctx: &VariableContext,
) -> Option<Result<String, VariableError>> {
    let _ = (expr, ctx);
    todo!("port vscode-restclient httpVariableProviders/systemVariableProvider.ts")
}

// TODO(Phase 2): request variable chaining — {{name.response.body.$.json.path}}
// (vscode-restclient utils/requestVariableCache.ts + requestVariableProvider.ts)
