use zed_extension_api::settings::LspSettings;
use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

struct RestcraftExtension;

impl zed::Extension for RestcraftExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        // (a) Explicit override from Zed settings:
        //     { "lsp": { "restcraft-lsp": { "binary": { "path": "...", "arguments": [...] } } } }
        let binary_settings = LspSettings::for_worktree("restcraft-lsp", worktree)
            .ok()
            .and_then(|settings| settings.binary);
        if let Some(path) = binary_settings.as_ref().and_then(|binary| binary.path.clone()) {
            return Ok(Command {
                command: path,
                args: binary_settings
                    .and_then(|binary| binary.arguments)
                    .unwrap_or_default(),
                env: Vec::new(),
            });
        }

        // (b) restcraft-lsp found on $PATH.
        if let Some(path) = worktree.which("restcraft-lsp") {
            return Ok(Command {
                command: path,
                args: Vec::new(),
                env: Vec::new(),
            });
        }

        // TODO: download a prebuilt binary from GitHub Releases
        // (zed::latest_github_release + zed::download_file) once releases are published.
        Err("restcraft-lsp binary not found. Install it with `cargo install --path lsp` \
             from the restcraft repository, or add `restcraft-lsp` to your PATH."
            .to_string())
    }
}

zed::register_extension!(RestcraftExtension);
