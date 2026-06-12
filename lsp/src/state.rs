//! Shared mutable server state — one instance per LSP process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use reqwest_cookie_store::CookieStoreMutex;
use tower_lsp::lsp_types::Url;

use crate::settings::HttpSettings;
use crate::variables::request_vars::CachedExchange;

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
    /// Request variable cache: document URI -> `# @name` -> last sent
    /// exchange. Document-scoped like the original `RequestVariableCache`;
    /// resending under the same name overwrites. Kept across didChange (the
    /// original keeps responses across edits too) but evicted on didClose —
    /// references are document-scoped, so a closed document's cache can only
    /// ever serve that document again, and holding full bodies for the
    /// process lifetime would leak. (Divergence from upstream, which never
    /// evicts: reopening a file requires resending its named requests.)
    request_variables: DashMap<Url, HashMap<String, Arc<CachedExchange>>>,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            documents: DashMap::new(),
            current_environment: Mutex::new(None),
            http_settings: Mutex::new(HttpSettings::default()),
            cookie_jar: Mutex::new(None),
            request_variables: DashMap::new(),
        }
    }

    /// Stores the exchange of a just-sent `# @name name` request — call after
    /// every successful send (mirrors `RequestVariableCache.add`).
    pub fn insert_request_variable(&self, document: &Url, name: &str, exchange: CachedExchange) {
        self.request_variables
            .entry(document.clone())
            .or_default()
            .insert(name.to_string(), Arc::new(exchange));
    }

    /// The cached exchange for one request variable, if it has been sent
    /// (mirrors `RequestVariableCache.get`). Production paths go through
    /// `request_variables_snapshot`; kept for API symmetry and tests.
    #[allow(dead_code)]
    pub fn get_request_variable(
        &self,
        document: &Url,
        name: &str,
    ) -> Option<Arc<CachedExchange>> {
        self.request_variables
            .get(document)
            .and_then(|names| names.get(name).cloned())
    }

    /// Snapshot of all cached exchanges for `document` — cheap (`Arc` values),
    /// feed it to `request_vars::RequestVariables::new` for one
    /// substitution/hover/completion pass.
    pub fn request_variables_snapshot(
        &self,
        document: &Url,
    ) -> HashMap<String, Arc<CachedExchange>> {
        self.request_variables
            .get(document)
            .map(|names| names.clone())
            .unwrap_or_default()
    }

    /// Drops every cached exchange for `document` — call on didClose.
    pub fn evict_request_variables(&self, document: &Url) {
        self.request_variables.remove(document);
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

    fn exchange(token: &str) -> CachedExchange {
        use crate::variables::request_vars::{CachedRequest, CachedResponse};
        CachedExchange {
            request: CachedRequest {
                method: "POST".into(),
                url: "https://example.com/login".into(),
                headers: vec![],
                body: String::new(),
            },
            response: CachedResponse {
                status: 200,
                status_text: "OK".into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: format!(r#"{{"token":"{token}"}}"#),
            },
        }
    }

    #[test]
    fn request_variable_cache_is_document_scoped_and_overwrites() {
        let state = ServerState::new();
        let doc_a: Url = "file:///a.http".parse().unwrap();
        let doc_b: Url = "file:///b.http".parse().unwrap();

        assert!(state.get_request_variable(&doc_a, "login").is_none());
        state.insert_request_variable(&doc_a, "login", exchange("v1"));

        // Same document key, like the original DocumentCache.
        let cached = state.get_request_variable(&doc_a, "login").unwrap();
        assert!(cached.response.body.contains("v1"));
        // Other documents do not see it.
        assert!(state.get_request_variable(&doc_b, "login").is_none());

        // Resending under the same name overwrites.
        state.insert_request_variable(&doc_a, "login", exchange("v2"));
        let cached = state.get_request_variable(&doc_a, "login").unwrap();
        assert!(cached.response.body.contains("v2"));

        // Snapshot shares the cached exchanges (Arc identity).
        let snapshot = state.request_variables_snapshot(&doc_a);
        assert_eq!(snapshot.len(), 1);
        assert!(Arc::ptr_eq(snapshot.get("login").unwrap(), &cached));
        assert!(state.request_variables_snapshot(&doc_b).is_empty());
    }

    #[test]
    fn evict_drops_only_the_closed_document() {
        let state = ServerState::new();
        let doc_a: Url = "file:///a.http".parse().unwrap();
        let doc_b: Url = "file:///b.http".parse().unwrap();
        state.insert_request_variable(&doc_a, "login", exchange("a"));
        state.insert_request_variable(&doc_b, "login", exchange("b"));

        state.evict_request_variables(&doc_a);

        assert!(state.get_request_variable(&doc_a, "login").is_none());
        assert!(state.get_request_variable(&doc_b, "login").is_some());
    }
}
