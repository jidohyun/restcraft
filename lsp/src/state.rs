//! Shared mutable server state — one instance per LSP process.

#![allow(dead_code)] // scaffold: wired up incrementally

use std::sync::Mutex;

use dashmap::DashMap;
use tower_lsp::lsp_types::Url;

pub struct ServerState {
    /// Open document contents, kept in sync via didOpen/didChange/didClose
    /// (FULL text sync).
    pub documents: DashMap<Url, String>,
    /// Currently selected environment name; `None` = `$shared` only.
    /// Loaded from `settings::load_current_environment` during `initialize`,
    /// written through `settings::save_current_environment` on switch.
    pub current_environment: Mutex<Option<String>>,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            documents: DashMap::new(),
            current_environment: Mutex::new(None),
        }
    }
}
