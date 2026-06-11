//! tower-lsp `LanguageServer` implementation. Send triggers arrive as both
//! code lens (Zed: opt-in via `"code_lens": "on"`) and code action commands,
//! both dispatching through `executeCommand`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context as _};
use serde_json::json;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::parser::{self, RequestBody};
use crate::settings::{self, HttpSettings};
use crate::state::ServerState;
use crate::variables::{self, VariableContext};
use crate::{http, response};

pub const COMMAND_SEND_REQUEST: &str = "restcraft.sendRequest";
pub const COMMAND_SWITCH_ENVIRONMENT: &str = "restcraft.switchEnvironment";

/// Picker/lens label for the unselected (`$shared` only) environment, same
/// wording as vscode-restclient.
pub const NO_ENVIRONMENT_LABEL: &str = "No Environment";

#[derive(Clone)]
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

    fn current_environment(&self) -> Option<String> {
        self.state
            .current_environment
            .lock()
            .expect("environment lock poisoned")
            .clone()
    }

    /// `$shared` + selected environment for `document_dir`, empty when no
    /// `http-client.env.json` exists. Load failures surface as `Err` so the
    /// caller can decide between showMessage (send) and silence (diagnostics).
    fn environment_for(&self, document_dir: &Path) -> anyhow::Result<HashMap<String, String>> {
        let current = self.current_environment();
        Ok(settings::load_environment_file(document_dir)?
            .map(|file| settings::resolve_environment(&file, current.as_deref()))
            .unwrap_or_default())
    }

    /// Full send pipeline for the block at `line` of `uri`:
    /// parser::parse_document -> variables::substitute -> parser::parse_request
    /// -> http::execute -> response::show_response.
    async fn send_request(&self, uri: Url, line: u32) -> anyhow::Result<()> {
        let text = self
            .state
            .documents
            .get(&uri)
            .map(|doc| doc.value().clone())
            .ok_or_else(|| anyhow!("document not open: {uri}"))?;
        let document = parser::parse_document(&text);
        let block = parser::block_at_line(&document, line)
            .ok_or_else(|| anyhow!("no request block at line {}", line + 1))?
            .clone();

        let document_dir = document_dir(&uri);
        let environment = match self.environment_for(&document_dir) {
            Ok(environment) => environment,
            Err(e) => {
                self.client
                    .show_message(
                        MessageType::WARNING,
                        format!("{}: {e:#} — sending without it", settings::ENV_FILE_NAME),
                    )
                    .await;
                HashMap::new()
            }
        };
        let ctx = VariableContext {
            file_variables: &document.file_variables,
            environment: &environment,
            document_dir: &document_dir,
        };

        let substituted = variables::substitute(&block.text, &ctx);
        if !substituted.errors.is_empty() {
            let list: Vec<String> = substituted
                .errors
                .iter()
                .map(|e| format!("• {e}"))
                .collect();
            self.client
                .show_message(
                    MessageType::WARNING,
                    format!("Unresolved variables (sending anyway):\n{}", list.join("\n")),
                )
                .await;
        }

        let mut request = parser::parse_request(&substituted.text, &document_dir)?;

        // `<@ path` external bodies: the file content goes through variable
        // substitution too (detected on the block text, applied to the
        // already-resolved path parse_request produced).
        if let Some(RequestBody::File(path)) = &request.body {
            let process_variables = substituted
                .text
                .lines()
                .filter_map(parser::input_file_ref)
                .any(|file_ref| file_ref.process_variables);
            if process_variables {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read body file {}", path.display()))?;
                let body = variables::substitute(&content, &ctx);
                request.body = Some(RequestBody::Text(body.text));
            }
        }

        let request_name = block
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| default_request_name(&request.method, &request.url));

        let token = NumberOrString::String(format!("restcraft/send/{request_name}"));
        let progress = self
            .client
            .send_request::<request::WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
            .is_ok();
        if progress {
            self.client
                .send_notification::<notification::Progress>(ProgressParams {
                    token: token.clone(),
                    value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                        WorkDoneProgressBegin {
                            title: format!("{} {}", request.method, request.url),
                            cancellable: Some(false),
                            message: None,
                            percentage: None,
                        },
                    )),
                })
                .await;
        }

        let http_settings = self
            .state
            .http_settings
            .lock()
            .expect("settings lock poisoned")
            .clone();
        let jar = self.state.cookie_jar();
        let result = http::execute(&request, &block.metadata, &http_settings, Arc::clone(&jar)).await;

        if progress {
            let message = match &result {
                Ok(r) => format!("{} {} — {}ms", r.status, r.status_text, r.elapsed.as_millis()),
                Err(e) => format!("failed: {e}"),
            };
            self.client
                .send_notification::<notification::Progress>(ProgressParams {
                    token,
                    value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                        WorkDoneProgressEnd {
                            message: Some(message),
                        },
                    )),
                })
                .await;
        }

        let http_response = result?;

        if !block.metadata.no_cookie_jar {
            if let Err(e) = http::save_cookie_jar(&jar) {
                self.client
                    .show_message(
                        MessageType::WARNING,
                        format!("failed to persist cookie jar: {e:#}"),
                    )
                    .await;
            }
        }

        match response::show_response(&http_response, &request_name, response::DisplayMode::Full) {
            Ok(_path) => Ok(()),
            // The response file exists; only the tab-open failed. Tell the
            // user how to fix it instead of failing the whole send.
            Err(e @ response::ShowError::ZedCliMissing(_)) => {
                self.client.show_message(MessageType::WARNING, e.to_string()).await;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Persists the selection (settings::save_current_environment) and updates
    /// `state.current_environment`. `name == None` opens a picker
    /// (showMessageRequest buttons) over every environment reachable from the
    /// open documents, plus "No Environment".
    async fn switch_environment(&self, name: Option<String>) -> anyhow::Result<()> {
        let selected = match name {
            Some(name) => Some(name),
            None => {
                let mut names = self.known_environment_names();
                names.push(NO_ENVIRONMENT_LABEL.to_string());
                let actions = names
                    .into_iter()
                    .map(|title| MessageActionItem {
                        title,
                        properties: Default::default(),
                    })
                    .collect();
                let choice = self
                    .client
                    .show_message_request(MessageType::INFO, "Switch environment to:", Some(actions))
                    .await
                    .map_err(|e| anyhow!("environment picker failed: {e}"))?;
                match choice {
                    Some(item) => Some(item.title),
                    None => return Ok(()), // dismissed
                }
            }
        };
        let selected = selected.filter(|name| name != NO_ENVIRONMENT_LABEL);

        settings::save_current_environment(selected.as_deref())?;
        *self
            .state
            .current_environment
            .lock()
            .expect("environment lock poisoned") = selected.clone();

        let _ = self.client.code_lens_refresh().await; // lens shows the env name
        self.refresh_all_diagnostics().await; // env switch changes resolvability
        self.client
            .show_message(
                MessageType::INFO,
                format!(
                    "Environment: {}",
                    selected.as_deref().unwrap_or(NO_ENVIRONMENT_LABEL)
                ),
            )
            .await;
        Ok(())
    }

    /// Union of environment names across the directories of all open
    /// documents (each searched upward for `http-client.env.json`).
    fn known_environment_names(&self) -> Vec<String> {
        let mut dirs: Vec<PathBuf> = self
            .state
            .documents
            .iter()
            .map(|entry| document_dir(entry.key()))
            .collect();
        dirs.sort();
        dirs.dedup();

        let mut names = Vec::new();
        for dir in dirs {
            if let Ok(Some(file)) = settings::load_environment_file(&dir) {
                for name in settings::environment_names(&file) {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        names.sort();
        names
    }

    /// Warning diagnostics for every `{{ref}}` occurrence that does not
    /// resolve with the current file variables + environment.
    fn variable_diagnostics(&self, uri: &Url, text: &str) -> Vec<Diagnostic> {
        let document = parser::parse_document(text);
        let document_dir = document_dir(uri);
        let environment = self.environment_for(&document_dir).unwrap_or_default();
        let ctx = VariableContext {
            file_variables: &document.file_variables,
            environment: &environment,
            document_dir: &document_dir,
        };

        let mut diagnostics = Vec::new();
        for (line_idx, line) in text.lines().enumerate() {
            // Comment lines (incl. ### separators and # @metadata) never get
            // substituted, so they cannot produce unresolved references.
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') || trimmed.starts_with("//") {
                continue;
            }
            for (start, end) in reference_spans(line) {
                // Re-running the substitution on just this occurrence reuses
                // variables.rs's error reporting verbatim.
                let result = variables::substitute(&line[start..end], &ctx);
                for error in result.errors {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position {
                                line: line_idx as u32,
                                character: utf16_col(line, start),
                            },
                            end: Position {
                                line: line_idx as u32,
                                character: utf16_col(line, end),
                            },
                        },
                        severity: Some(DiagnosticSeverity::WARNING),
                        source: Some("restcraft".to_string()),
                        message: error.to_string(),
                        ..Default::default()
                    });
                }
            }
        }
        diagnostics
    }

    async fn publish_variable_diagnostics(&self, uri: Url) {
        let Some(text) = self
            .state
            .documents
            .get(&uri)
            .map(|doc| doc.value().clone())
        else {
            return;
        };
        let diagnostics = self.variable_diagnostics(&uri, &text);
        self.client.publish_diagnostics(uri, diagnostics, None).await;
    }

    async fn refresh_all_diagnostics(&self) {
        let uris: Vec<Url> = self
            .state
            .documents
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for uri in uris {
            self.publish_variable_diagnostics(uri).await;
        }
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
        match settings::load_current_environment() {
            Ok(environment) => {
                *self
                    .state
                    .current_environment
                    .lock()
                    .expect("environment lock poisoned") = environment;
            }
            Err(e) => {
                self.client
                    .show_message(
                        MessageType::WARNING,
                        format!("failed to load persisted environment: {e:#}"),
                    )
                    .await;
            }
        }
        // Warm the cookie jar so the first send doesn't pay the disk load.
        let _ = self.state.cookie_jar();
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // Zed delivers `lsp.restcraft-lsp.settings` as the params object.
        if let Ok(settings) = serde_json::from_value::<HttpSettings>(params.settings) {
            *self
                .state
                .http_settings
                .lock()
                .expect("settings lock poisoned") = settings;
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        self.state
            .documents
            .insert(uri.clone(), params.text_document.text);
        self.publish_variable_diagnostics(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last content change carries the whole document.
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().last() {
            self.state.documents.insert(uri.clone(), change.text);
        }
        self.publish_variable_diagnostics(uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.state.documents.remove(&params.text_document.uri);
        // Clear our diagnostics for the closed buffer.
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    /// One "Send Request" lens per request block (lens carries
    /// COMMAND_SEND_REQUEST with [uri, lens_line] arguments) plus an
    /// environment indicator lens on the first line.
    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        let Some(text) = self
            .state
            .documents
            .get(&uri)
            .map(|doc| doc.value().clone())
        else {
            return Ok(None);
        };
        let document = parser::parse_document(&text);

        let environment = self
            .current_environment()
            .unwrap_or_else(|| NO_ENVIRONMENT_LABEL.to_string());
        let mut lenses = vec![CodeLens {
            range: line_range(0),
            command: Some(Command {
                title: format!("Environment: {environment}"),
                command: COMMAND_SWITCH_ENVIRONMENT.to_string(),
                arguments: None, // no argument -> picker
            }),
            data: None,
        }];

        for block in &document.blocks {
            lenses.push(CodeLens {
                range: line_range(block.lens_line),
                command: Some(Command {
                    title: "Send Request".to_string(),
                    command: COMMAND_SEND_REQUEST.to_string(),
                    arguments: Some(vec![json!(uri), json!(block.lens_line)]),
                }),
                data: None,
            });
        }
        Ok(Some(lenses))
    }

    /// "Send Request" for the block under the cursor plus one
    /// "Switch Environment: <name>" action per environment — code actions are
    /// the always-available trigger since Zed code lens is opt-in.
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some(text) = self
            .state
            .documents
            .get(&uri)
            .map(|doc| doc.value().clone())
        else {
            return Ok(None);
        };
        let document = parser::parse_document(&text);
        let mut actions = CodeActionResponse::new();

        if let Some(block) = parser::block_at_line(&document, params.range.start.line) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Send Request".to_string(),
                kind: Some(CodeActionKind::EMPTY),
                command: Some(Command {
                    title: "Send Request".to_string(),
                    command: COMMAND_SEND_REQUEST.to_string(),
                    arguments: Some(vec![json!(uri), json!(block.lens_line)]),
                }),
                ..Default::default()
            }));
        }

        let document_dir = document_dir(&uri);
        if let Ok(Some(file)) = settings::load_environment_file(&document_dir) {
            let current = self.current_environment();
            let mut names = settings::environment_names(&file);
            names.push(NO_ENVIRONMENT_LABEL.to_string());
            for name in names {
                if current.as_deref() == Some(name.as_str())
                    || (current.is_none() && name == NO_ENVIRONMENT_LABEL)
                {
                    continue; // switching to the active env is a no-op
                }
                let title = format!("Switch Environment: {name}");
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: title.clone(),
                    kind: Some(CodeActionKind::EMPTY),
                    command: Some(Command {
                        title,
                        command: COMMAND_SWITCH_ENVIRONMENT.to_string(),
                        arguments: Some(vec![json!(name)]),
                    }),
                    ..Default::default()
                }));
            }
        }

        Ok(Some(actions))
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        match params.command.as_str() {
            COMMAND_SEND_REQUEST => {
                let mut args = params.arguments.into_iter();
                let uri = args
                    .next()
                    .and_then(|v| serde_json::from_value::<Url>(v).ok());
                let line = args.next().and_then(|v| v.as_u64()).map(|l| l as u32);
                let (Some(uri), Some(line)) = (uri, line) else {
                    self.client
                        .show_message(
                            MessageType::ERROR,
                            format!("{COMMAND_SEND_REQUEST}: expected [uri, line] arguments"),
                        )
                        .await;
                    return Ok(None);
                };
                // Sending is non-blocking: the executeCommand reply returns
                // immediately while the request runs in the background.
                let this = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = this.send_request(uri, line).await {
                        this.client
                            .show_message(MessageType::ERROR, format!("Send failed: {e:#}"))
                            .await;
                    }
                });
            }
            COMMAND_SWITCH_ENVIRONMENT => {
                let name = params
                    .arguments
                    .into_iter()
                    .next()
                    .and_then(|v| v.as_str().map(str::to_string));
                // Spawned too: the picker round-trips through the client.
                let this = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = this.switch_environment(name).await {
                        this.client
                            .show_message(
                                MessageType::ERROR,
                                format!("Switch environment failed: {e:#}"),
                            )
                            .await;
                    }
                });
            }
            other => {
                self.client
                    .show_message(MessageType::ERROR, format!("unknown command: {other}"))
                    .await;
            }
        }
        Ok(None)
    }
}

fn document_dir(uri: &Url) -> PathBuf {
    uri.to_file_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn line_range(line: u32) -> Range {
    Range {
        start: Position { line, character: 0 },
        end: Position { line, character: 0 },
    }
}

/// Stable per-request fallback name when `# @name` is absent; stability keeps
/// the response file path (and thus the Zed tab) reused across resends.
fn default_request_name(method: &str, url: &str) -> String {
    // FNV-1a, so the name survives process restarts (std's DefaultHasher
    // makes no cross-version guarantee).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in method.bytes().chain(url.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{}-{:08x}", method.to_ascii_lowercase(), hash as u32)
}

/// Byte spans of `{{...}}` references in one line — same shape the resolver
/// in variables.rs matches (shortest match, non-empty name, single line).
fn reference_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while let Some(rel) = line[i..].find("{{") {
        let open = i + rel;
        let inner_start = open + 2;
        let Some(rel_close) = line[inner_start..].find("}}") else {
            break;
        };
        let close = inner_start + rel_close;
        if line[inner_start..close].trim().is_empty() {
            i = open + 2;
            continue;
        }
        spans.push((open, close + 2));
        i = close + 2;
    }
    spans
}

/// LSP positions are UTF-16 code units by default.
fn utf16_col(line: &str, byte_offset: usize) -> u32 {
    line[..byte_offset].encode_utf16().count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_spans_finds_each_occurrence() {
        let line = "GET {{host}}/api?q={{%query}}&id={{$guid}}";
        let spans = reference_spans(line);
        let texts: Vec<&str> = spans.iter().map(|&(s, e)| &line[s..e]).collect();
        assert_eq!(texts, vec!["{{host}}", "{{%query}}", "{{$guid}}"]);
    }

    #[test]
    fn reference_spans_skips_empty_and_unclosed() {
        assert!(reference_spans("{{}} {{ }}").is_empty());
        assert!(reference_spans("{{open but never closed").is_empty());
        assert_eq!(reference_spans("{{ }} {{x}}").len(), 1);
    }

    #[test]
    fn utf16_col_counts_code_units() {
        let line = "한글 {{x}}";
        let byte = line.find("{{").unwrap();
        // "한글 " = 2 chars (1 UTF-16 unit each) + 1 space
        assert_eq!(utf16_col(line, byte), 3);
        assert_eq!(utf16_col("abc", 3), 3);
    }

    #[test]
    fn default_request_name_is_stable_and_method_prefixed() {
        let a = default_request_name("GET", "https://example.com/api");
        let b = default_request_name("GET", "https://example.com/api");
        let c = default_request_name("POST", "https://example.com/api");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("get-"), "{a}");
    }
}
