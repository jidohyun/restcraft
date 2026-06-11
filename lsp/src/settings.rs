//! Environment files (JetBrains-style `http-client.env.json`), the persisted
//! current-environment selection (`~/.restcraft/environment.json`), and HTTP
//! behavior settings passed via Zed's `lsp.restcraft-lsp.settings`.

#![allow(dead_code)] // scaffold: wired up incrementally

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const ENV_FILE_NAME: &str = "http-client.env.json";
pub const SHARED_ENV_KEY: &str = "$shared";

/// `~/.restcraft/` — environment selection and cookie jar live here.
pub fn restcraft_home() -> PathBuf {
    todo!("dirs::home_dir().join(\".restcraft\"), create on demand")
}

/// Raw `http-client.env.json`: env name -> (variable name -> value).
pub type EnvironmentFile = HashMap<String, HashMap<String, String>>;

/// Names of selectable environments (everything except `$shared`).
pub fn environment_names(file: &EnvironmentFile) -> Vec<String> {
    let _ = file;
    todo!()
}

/// Finds and parses `http-client.env.json`, searching `document_dir` upward
/// to the worktree root. `Ok(None)` when no file exists.
pub fn load_environment_file(document_dir: &Path) -> anyhow::Result<Option<EnvironmentFile>> {
    let _ = document_dir;
    todo!()
}

/// Merges `$shared` with the selected environment (selected wins).
/// `name == None` yields `$shared` only.
pub fn resolve_environment(
    file: &EnvironmentFile,
    name: Option<&str>,
) -> HashMap<String, String> {
    let _ = (file, name);
    todo!()
}

/// On-disk shape of `~/.restcraft/environment.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentSelection {
    pub current: Option<String>,
}

/// Reads the persisted selection. Missing/corrupt file -> `Ok(None)`.
pub fn load_current_environment() -> anyhow::Result<Option<String>> {
    todo!()
}

/// Persists the selection to `~/.restcraft/environment.json`.
pub fn save_current_environment(name: Option<&str>) -> anyhow::Result<()> {
    let _ = name;
    todo!()
}

/// HTTP behavior knobs, deserialized from Zed's
/// `lsp.restcraft-lsp.settings` (workspace/didChangeConfiguration), with
/// vscode-restclient-compatible defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HttpSettings {
    /// Milliseconds; 0 = no timeout (vscode-restclient default).
    pub timeout_ms: u64,
    /// Per-request `# @no-redirect` overrides this.
    pub follow_redirects: bool,
    pub max_redirects: usize,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            timeout_ms: 0,
            follow_redirects: true,
            max_redirects: 10,
        }
    }
}
