//! tower-lsp `LanguageServer` implementation. Send triggers arrive as both
//! code lens (Zed: opt-in via `"code_lens": "on"`) and code action commands,
//! both dispatching through `executeCommand`.

use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::state::ServerState;

pub const COMMAND_SEND_REQUEST: &str = "restcraft.sendRequest";
pub const COMMAND_SWITCH_ENVIRONMENT: &str = "restcraft.switchEnvironment";

pub struct Backend {
    pub client: Client,
    pub state: Arc<ServerState>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(ServerState::new()),
        }
    }

    /// Full send pipeline for the block at `line` of `uri`:
    /// parser::parse_document -> variables::substitute -> parser::parse_request
    /// -> http::execute -> response::show_response.
    #[allow(dead_code)]
    async fn send_request(&self, uri: Url, line: u32) -> anyhow::Result<()> {
        let _ = (uri, line);
        todo!("wire parser -> variables -> http -> response, report errors via showMessage")
    }

    /// Persists the selection (settings::save_current_environment) and updates
    /// `state.current_environment`.
    #[allow(dead_code)]
    async fn switch_environment(&self, name: Option<String>) -> anyhow::Result<()> {
        let _ = name;
        todo!()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        COMMAND_SEND_REQUEST.to_string(),
                        COMMAND_SWITCH_ENVIRONMENT.to_string(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "restcraft-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        // TODO: load persisted environment selection (settings::load_current_environment)
        // and the cookie jar (http::load_cookie_jar).
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.state
            .documents
            .insert(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last content change carries the whole document.
        if let Some(change) = params.content_changes.into_iter().last() {
            self.state
                .documents
                .insert(params.text_document.uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.state.documents.remove(&params.text_document.uri);
    }

    /// One "Send Request" lens per request block (lens carries
    /// COMMAND_SEND_REQUEST with [uri, lens_line] arguments).
    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let _ = params;
        todo!("parser::parse_document over state.documents[uri]")
    }

    /// "Send Request" for the block under the cursor plus one
    /// "Switch Environment: <name>" action per environment — code actions are
    /// the always-available trigger since Zed code lens is opt-in.
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let _ = params;
        todo!("block via parser::block_at_line, env names via settings::environment_names")
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        let _ = params;
        todo!("dispatch COMMAND_SEND_REQUEST -> send_request, COMMAND_SWITCH_ENVIRONMENT -> switch_environment")
    }
}
