//! OS clipboard writes via external commands: `pbcopy` (macOS), `xclip` then
//! `wl-copy` (Linux), `clip` (Windows).
//!
//! Chosen over the `arboard` crate on purpose: arboard drags X11/Wayland/
//! Cocoa bindings into an otherwise headless LSP binary, while every desktop
//! this server targets already ships a clipboard CLI. Spawn/exit failures are
//! surfaced as `Err` so the caller can fall back to e.g. writing the text
//! into the response file and telling the user where it is.
//!
//! Known limitation: `clip.exe` interprets piped bytes in the console code
//! page, so non-ASCII text may garble on Windows unless the system uses
//! UTF-8 (the same trade-off every CLI-based copy has there).

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context};

/// Clipboard commands to try in order for `os` (`std::env::consts::OS`
/// values). Split from the spawning so the per-OS table is unit-testable —
/// actually running a copy would clobber the developer's clipboard.
fn commands_for(os: &str) -> &'static [(&'static str, &'static [&'static str])] {
    match os {
        "macos" => &[("pbcopy", &[])],
        "linux" => &[("xclip", &["-selection", "clipboard"]), ("wl-copy", &[])],
        "windows" => &[("clip", &[])],
        _ => &[],
    }
}

/// Copies `text` to the OS clipboard. Blocking — it waits for the helper
/// process to exit; wrap in `spawn_blocking` when called from the async LSP
/// handlers if that ever shows up in traces (clipboard helpers exit in
/// milliseconds).
pub fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    copy_with(commands_for(std::env::consts::OS), text)
}

/// Tries each candidate command until one accepts the text on stdin and
/// exits successfully; returns the last failure when none does.
fn copy_with(commands: &[(&str, &[&str])], text: &str) -> anyhow::Result<()> {
    let mut last_error: Option<anyhow::Error> = None;
    for (program, args) in commands {
        match pipe_to(program, args, text) {
            Ok(()) => return Ok(()),
            Err(err) => last_error = Some(err),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no clipboard command known for this platform")))
}

fn pipe_to(program: &str, args: &[&str], text: &str) -> anyhow::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(text.as_bytes())
        .with_context(|| format!("failed to write to {program} stdin"))?;
    // The taken stdin handle dropped above, closing the pipe — the helper
    // sees EOF and commits the clipboard content.
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {program}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("{program} exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tables_per_os() {
        assert_eq!(commands_for("macos"), &[("pbcopy", &[] as &[&str])]);
        assert_eq!(
            commands_for("linux"),
            &[
                ("xclip", &["-selection", "clipboard"] as &[&str]),
                ("wl-copy", &[] as &[&str]),
            ]
        );
        assert_eq!(commands_for("windows"), &[("clip", &[] as &[&str])]);
        assert!(commands_for("freebsd").is_empty());
    }

    #[test]
    fn empty_command_list_errors() {
        let err = copy_with(&[], "text").unwrap_err();
        assert!(err.to_string().contains("no clipboard command"));
    }

    #[test]
    fn missing_program_errors() {
        let err = copy_with(&[("restcraft-no-such-clipboard-cmd", &[])], "text").unwrap_err();
        assert!(err.to_string().contains("failed to spawn"));
    }

    #[cfg(unix)]
    #[test]
    fn succeeding_command_returns_ok() {
        // `cat` consumes stdin and exits 0 — a stand-in for pbcopy/xclip that
        // does not touch the developer's real clipboard.
        copy_with(&[("cat", &[])], "hello clipboard\nline2").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failing_command_falls_through_to_next() {
        // First candidate cannot spawn; the second succeeds.
        copy_with(
            &[("restcraft-no-such-clipboard-cmd", &[]), ("cat", &[])],
            "fallback",
        )
        .unwrap();

        // `false` exits non-zero (or breaks the pipe) — either way an Err.
        assert!(copy_with(&[("false", &[])], "nope").is_err());
    }
}
