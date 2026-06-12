use std::fs;

use zed_extension_api::settings::LspSettings;
use zed_extension_api::{
    self as zed, Architecture, Command, DownloadedFileType, LanguageServerId,
    LanguageServerInstallationStatus, Os, Result, Worktree,
};

/// Name of the language server binary (also the prefix of release assets and
/// of the version cache directories inside the extension's working directory).
const LSP_BINARY_NAME: &str = "restcraft-lsp";

/// GitHub repository that publishes `restcraft-lsp` release binaries.
const GITHUB_REPO: &str = "jidohyun/restcraft";

/// Maps the current platform to the Rust target triple used in release asset
/// names. Returns an error for platforms without prebuilt binaries.
fn release_target_triple(os: Os, arch: Architecture) -> Result<&'static str> {
    match (os, arch) {
        (Os::Mac, Architecture::Aarch64) => Ok("aarch64-apple-darwin"),
        (Os::Mac, Architecture::X8664) => Ok("x86_64-apple-darwin"),
        (Os::Linux, Architecture::Aarch64) => Ok("aarch64-unknown-linux-musl"),
        (Os::Linux, Architecture::X8664) => Ok("x86_64-unknown-linux-musl"),
        (Os::Windows, Architecture::X8664) => Ok("x86_64-pc-windows-msvc"),
        (os, arch) => Err(format!(
            "no prebuilt {LSP_BINARY_NAME} binary is published for {}-{}",
            arch_str(arch),
            os_str(os),
        )),
    }
}

fn os_str(os: Os) -> &'static str {
    match os {
        Os::Mac => "macos",
        Os::Linux => "linux",
        Os::Windows => "windows",
    }
}

fn arch_str(arch: Architecture) -> &'static str {
    match arch {
        Architecture::Aarch64 => "aarch64",
        Architecture::X86 => "x86",
        Architecture::X8664 => "x86_64",
    }
}

/// Release asset file name for the given target triple, following the
/// restcraft release asset contract:
/// `restcraft-lsp-{target}.tar.gz` (unix) / `restcraft-lsp-{target}.zip` (windows).
fn release_asset_name(os: Os, target: &str) -> String {
    let ext = match os {
        Os::Windows => "zip",
        Os::Mac | Os::Linux => "tar.gz",
    };
    format!("{LSP_BINARY_NAME}-{target}.{ext}")
}

/// Directory (relative to the extension's working directory) where a given
/// release version of the server is cached, e.g. `restcraft-lsp-v0.1.0`.
fn version_dir_name(release_version: &str) -> String {
    format!("{LSP_BINARY_NAME}-{release_version}")
}

/// Path of the server binary inside a version directory, e.g.
/// `restcraft-lsp-v0.1.0/restcraft-lsp` (`.exe` on Windows).
fn binary_path_in_version_dir(os: Os, version_dir: &str) -> String {
    match os {
        Os::Windows => format!("{version_dir}/{LSP_BINARY_NAME}.exe"),
        Os::Mac | Os::Linux => format!("{version_dir}/{LSP_BINARY_NAME}"),
    }
}

struct RestcraftExtension {
    /// Path of the downloaded binary resolved earlier in this session, so we
    /// can skip the GitHub release lookup on subsequent worktrees.
    cached_binary_path: Option<String>,
}

impl RestcraftExtension {
    /// Downloads (or reuses a previously downloaded) `restcraft-lsp` release
    /// binary, returning its path relative to the extension's working
    /// directory.
    fn downloaded_binary_path(&mut self, language_server_id: &LanguageServerId) -> Result<String> {
        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path).is_ok_and(|stat| stat.is_file()) {
                return Ok(path.clone());
            }
        }

        let (os, arch) = zed::current_platform();
        let target = release_target_triple(os, arch)?;

        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release = match zed::latest_github_release(
            GITHUB_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        ) {
            Ok(release) => release,
            Err(error) => {
                // Offline / rate-limited: fall back to a previously
                // downloaded version, if any.
                if let Some(binary_path) = existing_version_binary(os) {
                    self.cached_binary_path = Some(binary_path.clone());
                    return Ok(binary_path);
                }
                return Err(format!("failed to look up the latest release: {error}"));
            }
        };

        let asset_name = release_asset_name(os, target);
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| {
                format!(
                    "release {} has no asset named {asset_name}",
                    release.version
                )
            })?;

        let version_dir = version_dir_name(&release.version);
        let binary_path = binary_path_in_version_dir(os, &version_dir);

        if !fs::metadata(&binary_path).is_ok_and(|stat| stat.is_file()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::Downloading,
            );

            let file_type = match os {
                Os::Windows => DownloadedFileType::Zip,
                Os::Mac | Os::Linux => DownloadedFileType::GzipTar,
            };
            // The archive is extracted into `version_dir`; it contains the
            // single binary `restcraft-lsp(.exe)`.
            zed::download_file(&asset.download_url, &version_dir, file_type)
                .map_err(|error| format!("failed to download {asset_name}: {error}"))?;

            if os != Os::Windows {
                zed::make_file_executable(&binary_path).map_err(|error| {
                    format!("failed to make {binary_path} executable: {error}")
                })?;
            }

            remove_stale_versions(&version_dir);
        }

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

/// Returns the binary path of an already-downloaded version, if one exists in
/// the extension's working directory. Used as a fallback when the GitHub
/// release lookup fails (e.g. offline). If multiple versions are present, the
/// lexicographically greatest directory name is picked.
fn existing_version_binary(os: Os) -> Option<String> {
    let entries = fs::read_dir(".").ok()?;
    let mut version_dirs: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&format!("{LSP_BINARY_NAME}-")))
        .collect();
    version_dirs.sort();
    let version_dir = version_dirs.pop()?;
    let binary_path = binary_path_in_version_dir(os, &version_dir);
    fs::metadata(&binary_path)
        .is_ok_and(|stat| stat.is_file())
        .then_some(binary_path)
}

/// Removes version directories of releases other than `current_version_dir`
/// from the extension's working directory.
fn remove_stale_versions(current_version_dir: &str) {
    let Ok(entries) = fs::read_dir(".") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with(&format!("{LSP_BINARY_NAME}-")) && name != current_version_dir {
            fs::remove_dir_all(entry.path()).ok();
        }
    }
}

impl zed::Extension for RestcraftExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        // (a) Explicit override from Zed settings:
        //     { "lsp": { "restcraft-lsp": { "binary": { "path": "...", "arguments": [...] } } } }
        let binary_settings = LspSettings::for_worktree(LSP_BINARY_NAME, worktree)
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

        // (b) restcraft-lsp found on $PATH (developer setups).
        if let Some(path) = worktree.which(LSP_BINARY_NAME) {
            return Ok(Command {
                command: path,
                args: Vec::new(),
                env: Vec::new(),
            });
        }

        // (c) Prebuilt binary from GitHub Releases, cached per version.
        match self.downloaded_binary_path(language_server_id) {
            Ok(path) => Ok(Command {
                command: path,
                args: Vec::new(),
                env: Vec::new(),
            }),
            Err(download_error) => {
                zed::set_language_server_installation_status(
                    language_server_id,
                    &LanguageServerInstallationStatus::Failed(download_error.clone()),
                );
                Err(format!(
                    "restcraft-lsp binary not found. Downloading a prebuilt binary from \
                     GitHub Releases ({GITHUB_REPO}) failed: {download_error}. \
                     Alternatively, install it with `cargo install --path lsp` from the \
                     restcraft repository, add `restcraft-lsp` to your PATH, or set \
                     `lsp.restcraft-lsp.binary.path` in your Zed settings."
                ))
            }
        }
    }
}

zed::register_extension!(RestcraftExtension);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_target_triple_supports_the_five_published_targets() {
        assert_eq!(
            release_target_triple(Os::Mac, Architecture::Aarch64),
            Ok("aarch64-apple-darwin")
        );
        assert_eq!(
            release_target_triple(Os::Mac, Architecture::X8664),
            Ok("x86_64-apple-darwin")
        );
        assert_eq!(
            release_target_triple(Os::Linux, Architecture::Aarch64),
            Ok("aarch64-unknown-linux-musl")
        );
        assert_eq!(
            release_target_triple(Os::Linux, Architecture::X8664),
            Ok("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            release_target_triple(Os::Windows, Architecture::X8664),
            Ok("x86_64-pc-windows-msvc")
        );
    }

    #[test]
    fn release_target_triple_rejects_unsupported_platforms() {
        let error = release_target_triple(Os::Windows, Architecture::Aarch64).unwrap_err();
        assert!(error.contains("aarch64-windows"), "got: {error}");

        for os in [Os::Mac, Os::Linux, Os::Windows] {
            assert!(release_target_triple(os, Architecture::X86).is_err());
        }
    }

    #[test]
    fn release_asset_name_follows_the_asset_contract() {
        assert_eq!(
            release_asset_name(Os::Mac, "aarch64-apple-darwin"),
            "restcraft-lsp-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release_asset_name(Os::Linux, "x86_64-unknown-linux-musl"),
            "restcraft-lsp-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            release_asset_name(Os::Windows, "x86_64-pc-windows-msvc"),
            "restcraft-lsp-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn cached_binary_lives_in_a_version_directory() {
        let version_dir = version_dir_name("v0.1.0");
        assert_eq!(version_dir, "restcraft-lsp-v0.1.0");
        assert_eq!(
            binary_path_in_version_dir(Os::Mac, &version_dir),
            "restcraft-lsp-v0.1.0/restcraft-lsp"
        );
        assert_eq!(
            binary_path_in_version_dir(Os::Linux, &version_dir),
            "restcraft-lsp-v0.1.0/restcraft-lsp"
        );
        assert_eq!(
            binary_path_in_version_dir(Os::Windows, &version_dir),
            "restcraft-lsp-v0.1.0/restcraft-lsp.exe"
        );
    }
}
