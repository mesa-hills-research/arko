//! Copying: to the system clipboard, and through the terminal (OSC 52,
//! crossterm's `CopyToClipboard`, which reaches the user's clipboard over
//! SSH where the terminal honours it: GNOME Terminal doesn't).
//!
//! The system clipboard is arboard's: the X11 clipboard, Wayland's where
//! the compositor has the data-control protocol (arboard tries it first,
//! then X11), macOS's and Windows'. On X11 and Wayland the copy lives in
//! the program that made it, so one clipboard is kept for the whole run,
//! and when it goes arboard hands the text to a clipboard manager where
//! there is one.
//!
//! On a Wayland compositor without data-control, such as GNOME's, arboard
//! falls back to XWayland's clipboard, which some programs that share the
//! clipboard never see (a virtual machine's tools, for one). There wl-copy
//! takes the text first when it is installed, so a Wayland program owns
//! the copy. Where arboard can't reach a clipboard either, wl-copy, xclip
//! or xsel take the text if one is installed.

use std::sync::Mutex;

/// What became of a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copied {
    /// The system clipboard has it.
    System,
    /// Only the terminal was asked (OSC 52): over SSH, or without a
    /// display, this machine's clipboard isn't the user's.
    Terminal,
    /// Nothing could take it: what would fix that.
    Failed(String),
}

/// The clipboard, kept from the first copy to the end of the run.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// Copies `text` to the system clipboard (the caller sends OSC 52 too).
pub fn copy(text: &str) -> Copied {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return Copied::Terminal;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if wl_copy_first(set("WAYLAND_DISPLAY"), data_control) && run(text, "wl-copy", &[]) {
        return Copied::System;
    }
    let error = match system(text) {
        Ok(()) => return Copied::System,
        Err(error) => error,
    };
    log::info!("the system clipboard didn't take a copy: {error}");
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if tools(text) {
            return Copied::System;
        }
        if !set("DISPLAY") && !set("WAYLAND_DISPLAY") {
            return Copied::Terminal;
        }
        if set("WAYLAND_DISPLAY") {
            return Copied::Failed(
                "Couldn't copy, because this desktop's clipboard needs wl-clipboard (install it)"
                    .into(),
            );
        }
        Copied::Failed(
            "Couldn't copy, because this desktop's clipboard needs xclip (install it)".into(),
        )
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    Copied::Failed(format!("Couldn't copy to the clipboard ({error})"))
}

/// Copies through arboard, with the clipboard kept for the run.
fn system(text: &str) -> Result<(), String> {
    let mut clipboard = CLIPBOARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if clipboard.is_none() {
        *clipboard = Some(arboard::Clipboard::new().map_err(|error| error.to_string())?);
    }
    let result = clipboard
        .as_mut()
        .map(|clipboard| clipboard.set_text(text.to_string()));
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => {
            // Opened again next time (the display may have come back).
            *clipboard = None;
            Err(error.to_string())
        }
        None => Err("no clipboard".into()),
    }
}

/// Whether wl-copy takes a copy before arboard: on a Wayland desktop
/// (`wayland`) whose compositor has no data-control protocol, as
/// `data_control` finds out (asked only on Wayland).
#[cfg(all(unix, not(target_os = "macos")))]
fn wl_copy_first(wayland: bool, data_control: impl FnOnce() -> bool) -> bool {
    wayland && !data_control()
}

/// Whether the Wayland compositor has the data-control protocol, as arboard
/// checks it, asked once a run. A compositor Arko can't reach counts as
/// having it, so arboard copies as it would.
#[cfg(all(unix, not(target_os = "macos")))]
fn data_control() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        let check = wl_clipboard_rs::utils::is_primary_selection_supported();
        !matches!(
            check,
            Err(wl_clipboard_rs::utils::PrimarySelectionCheckError::MissingProtocol)
        )
    })
}

/// Copies through an installed clipboard program.
#[cfg(all(unix, not(target_os = "macos")))]
fn tools(text: &str) -> bool {
    let programs: &[(&str, &[&str])] = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    } else {
        &[
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    programs
        .iter()
        .any(|(program, args)| run(text, program, args))
}

/// Copies through the clipboard program `program`, when it is installed.
#[cfg(all(unix, not(target_os = "macos")))]
fn run(text: &str, program: &str, args: &[&str]) -> bool {
    use std::io::Write;
    let child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    // (wl-copy and xclip go on in the background to serve the clipboard,
    // and the program started returns.)
    child.wait().is_ok_and(|status| status.success())
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;

    /// wl-copy goes first only on a Wayland desktop whose compositor has
    /// no data-control protocol, and the compositor is asked only on
    /// Wayland. Elsewhere arboard copies first, as on X11.
    #[test]
    fn wl_copy_goes_first_on_wayland_without_data_control() {
        assert!(wl_copy_first(true, || false));
        assert!(!wl_copy_first(true, || true));
        assert!(!wl_copy_first(false, || {
            panic!("the compositor is asked only on Wayland")
        }));
    }

    /// A clipboard program that isn't installed takes nothing, so the next
    /// way is tried.
    #[test]
    fn a_program_that_isnt_installed_takes_nothing() {
        assert!(!run("text", "arko-no-such-clipboard-program", &[]));
    }
}
