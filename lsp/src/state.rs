//! Shared mutable server state — one instance per LSP process.

use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use reqwest_cookie_store::CookieStoreMutex;
use tower_lsp::lsp_types::Url;

use crate::settings::HttpSettings;

pub struct ServerState {
    /// Open document contents, kept in sync via didOpen/didChange/didClose
    /// (FULL text sync).
    pub documents: DashMap<Url, String>,
    /// Currently selected environment name; `None` = `$shared` only.
    /// Loaded from `settings::load_current_environment` during `initialized`,
    /// written through `settings::save_current_environment` on switch.
    pub current_environment: Mutex<Option<String>>,
    /// HTTP behavior knobs from Zed's `lsp.restcraft-lsp.settings`
    /// (workspace/didChangeConfiguration).
    pub http_settings: Mutex<HttpSettings>,
    /// Lazily loaded persistent cookie jar (`~/.restcraft/cookies.json`).
    cookie_jar: Mutex<Option<Arc<CookieStoreMutex>>>,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            documents: DashMap::new(),
            current_environment: Mutex::new(None),
            http_settings: Mutex::new(HttpSettings::default()),
            cookie_jar: Mutex::new(None),
        }
    }

    /// The shared cookie jar, loaded from disk on first use. A jar that fails
    /// to load degrades to an empty in-memory one — a cache problem must
    /// never block sending.
    pub fn cookie_jar(&self) -> Arc<CookieStoreMutex> {
        let mut slot = self.cookie_jar.lock().expect("cookie jar lock poisoned");
        if let Some(jar) = slot.as_ref() {
            return Arc::clone(jar);
        }
        let jar = crate::http::load_cookie_jar().unwrap_or_default();
        *slot = Some(Arc::clone(&jar));
        jar
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_jar_is_loaded_once_and_shared() {
        let state = ServerState::new();
        let a = state.cookie_jar();
        let b = state.cookie_jar();
        assert!(Arc::ptr_eq(&a, &b), "jar must be cached after first load");
    }
}
