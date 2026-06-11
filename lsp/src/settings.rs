//! Environment files (JetBrains-style `http-client.env.json`), the persisted
//! current-environment selection (`~/.restcraft/environment.json`), and HTTP
//! behavior settings passed via Zed's `lsp.restcraft-lsp.settings`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

pub const ENV_FILE_NAME: &str = "http-client.env.json";
pub const SHARED_ENV_KEY: &str = "$shared";

/// `~/.restcraft/` — environment selection and cookie jar live here.
pub fn restcraft_home() -> PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".restcraft");
    let _ = std::fs::create_dir_all(&dir); // create on demand, best effort
    dir
}

/// Raw `http-client.env.json`: env name -> (variable name -> value).
pub type EnvironmentFile = HashMap<String, HashMap<String, String>>;

/// Names of selectable environments (everything except `$shared`).
pub fn environment_names(file: &EnvironmentFile) -> Vec<String> {
    let mut names: Vec<String> = file
        .keys()
        .filter(|name| name.as_str() != SHARED_ENV_KEY)
        .cloned()
        .collect();
    names.sort();
    names
}

/// Finds and parses `http-client.env.json`, searching `document_dir` upward
/// to the worktree root. `Ok(None)` when no file exists.
pub fn load_environment_file(document_dir: &Path) -> anyhow::Result<Option<EnvironmentFile>> {
    let mut dir = Some(document_dir);
    while let Some(current) = dir {
        let candidate = current.join(ENV_FILE_NAME);
        if candidate.is_file() {
            let content = std::fs::read_to_string(&candidate)
                .with_context(|| format!("failed to read {}", candidate.display()))?;
            let file = serde_json::from_str(&content)
                .with_context(|| format!("invalid JSON in {}", candidate.display()))?;
            return Ok(Some(file));
        }
        dir = current.parent();
    }
    Ok(None)
}

/// Merges `$shared` with the selected environment (selected wins).
/// `name == None` yields `$shared` only.
pub fn resolve_environment(
    file: &EnvironmentFile,
    name: Option<&str>,
) -> HashMap<String, String> {
    let mut merged = file.get(SHARED_ENV_KEY).cloned().unwrap_or_default();
    if let Some(selected) = name.and_then(|name| file.get(name)) {
        merged.extend(selected.clone());
    }
    merged
}

/// On-disk shape of `~/.restcraft/environment.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentSelection {
    pub current: Option<String>,
}

fn environment_selection_path() -> PathBuf {
    restcraft_home().join("environment.json")
}

/// Reads the persisted selection. Missing/corrupt file -> `Ok(None)`.
pub fn load_current_environment() -> anyhow::Result<Option<String>> {
    Ok(load_selection_from(&environment_selection_path()))
}

fn load_selection_from(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<EnvironmentSelection>(&content).ok()?.current
}

/// Persists the selection to `~/.restcraft/environment.json`.
pub fn save_current_environment(name: Option<&str>) -> anyhow::Result<()> {
    save_selection_to(&environment_selection_path(), name)
}

fn save_selection_to(path: &Path, name: Option<&str>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let selection = EnvironmentSelection {
        current: name.map(str::to_string),
    };
    let json = serde_json::to_string_pretty(&selection)?;
    std::fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("restcraft-settings-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn env_file(json: &str) -> EnvironmentFile {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn environment_names_exclude_shared_and_sort() {
        let file = env_file(r#"{"$shared": {"a": "1"}, "prod": {}, "dev": {}}"#);
        assert_eq!(environment_names(&file), vec!["dev", "prod"]);
        assert!(environment_names(&EnvironmentFile::new()).is_empty());
    }

    #[test]
    fn resolve_environment_merges_with_selected_winning() {
        let file = env_file(
            r#"{
                "$shared": {"host": "shared.example.com", "version": "v1"},
                "dev": {"host": "dev.example.com"}
            }"#,
        );

        let merged = resolve_environment(&file, Some("dev"));
        assert_eq!(merged.get("host").map(String::as_str), Some("dev.example.com"));
        assert_eq!(merged.get("version").map(String::as_str), Some("v1"));

        // None and unknown names yield $shared only.
        let shared_only = resolve_environment(&file, None);
        assert_eq!(
            shared_only.get("host").map(String::as_str),
            Some("shared.example.com")
        );
        assert_eq!(resolve_environment(&file, Some("nope")), shared_only);
    }

    #[test]
    fn load_environment_file_searches_upward() {
        let tmp = TempDir::new();
        fs::write(
            tmp.0.join(ENV_FILE_NAME),
            r#"{"$shared": {}, "dev": {"host": "dev.example.com"}}"#,
        )
        .unwrap();
        let nested = tmp.0.join("a/b");
        fs::create_dir_all(&nested).unwrap();

        let file = load_environment_file(&nested).unwrap().unwrap();
        assert_eq!(environment_names(&file), vec!["dev"]);
    }

    #[test]
    fn load_environment_file_missing_is_none_and_corrupt_is_error() {
        let tmp = TempDir::new();
        let isolated = tmp.0.join("no-env-here");
        fs::create_dir_all(&isolated).unwrap();
        // NOTE: searching upward from a temp dir can only escape into real
        // ancestors ($TMPDIR etc.) that normally hold no env file.
        if load_environment_file(&isolated).unwrap().is_some() {
            return; // an ancestor outside the sandbox has one; nothing to assert
        }

        fs::write(tmp.0.join(ENV_FILE_NAME), "{ not json").unwrap();
        assert!(load_environment_file(&isolated).is_err());
    }

    #[test]
    fn selection_round_trips_and_degrades_on_corruption() {
        let tmp = TempDir::new();
        let path = tmp.0.join("nested/environment.json");

        assert_eq!(load_selection_from(&path), None); // missing

        save_selection_to(&path, Some("dev")).unwrap();
        assert_eq!(load_selection_from(&path), Some("dev".to_string()));

        save_selection_to(&path, None).unwrap();
        assert_eq!(load_selection_from(&path), None);

        fs::write(&path, "garbage").unwrap();
        assert_eq!(load_selection_from(&path), None); // corrupt
    }

    #[test]
    fn http_settings_deserialize_with_defaults() {
        let s: HttpSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.timeout_ms, 0);
        assert!(s.follow_redirects);
        assert_eq!(s.max_redirects, 10);

        let s: HttpSettings =
            serde_json::from_str(r#"{"timeoutMs": 5000, "followRedirects": false}"#).unwrap();
        assert_eq!(s.timeout_ms, 5000);
        assert!(!s.follow_redirects);
    }
}
