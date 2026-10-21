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
//! On X11 and Wayland the text goes to the primary selection too, as when
//! text is selected in other programs, so a middle click pastes it. Some
//! programs that share the clipboard read the primary selection. A
//! virtual machine's tools (open-vm-tools) send the host whichever of the
//! two says it changed last, and the primary selection when the clipboard
//! doesn't say, which XWayland's copy of a Wayland clipboard and arboard's
//! never do. With the text in both, the host gets it either way.
//!
//! On a Wayland compositor without data-control, such as GNOME's, arboard
//! falls back to XWayland's clipboard. There wl-copy takes the text first
//! when it is installed, so a Wayland program owns the copy. Where arboard
//! can't reach a clipboard either, wl-copy, xclip or xsel take the text if
//! one is installed.

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
    if wl_copy_first(set("WAYLAND_DISPLAY"), data_control) && copy_with(text, &WL_COPY) {
        return Copied::System;
    }
    let error = match system(text) {
        Ok(()) => return Copied::System,
        Err(error) => error,
    };
    log::info!("the system clipboard didn't take a copy: {error}");
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let wayland = set("WAYLAND_DISPLAY");
        if programs(wayland)
            .iter()
            .any(|program| copy_with(text, program))
        {
            return Copied::System;
        }
        if !set("DISPLAY") && !wayland {
            return Copied::Terminal;
        }
        if wayland {
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
    let result = clipboard.as_mut().map(|clipboard| {
        clipboard.set_text(text.to_string())?;
        #[cfg(all(unix, not(target_os = "macos")))]
        primary(clipboard, text);
        Ok::<(), arboard::Error>(())
    });
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

/// Puts `text` in the primary selection too, where the desktop has one.
/// The copy counts as made without it.
#[cfg(all(unix, not(target_os = "macos")))]
fn primary(clipboard: &mut arboard::Clipboard, text: &str) {
    use arboard::{LinuxClipboardKind, SetExtLinux};
    let set = clipboard
        .set()
        .clipboard(LinuxClipboardKind::Primary)
        .text(text.to_string());
    if let Err(error) = set {
        log::info!("the primary selection didn't take a copy: {error}");
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

/// A clipboard program, with its arguments for the clipboard and for the
/// primary selection.
#[cfg(all(unix, not(target_os = "macos")))]
struct Program<'a> {
    name: &'a str,
    clipboard: &'a [&'a str],
    primary: &'a [&'a str],
}

#[cfg(all(unix, not(target_os = "macos")))]
const WL_COPY: Program<'static> = Program {
    name: "wl-copy",
    clipboard: &[],
    primary: &["--primary"],
};

#[cfg(all(unix, not(target_os = "macos")))]
const XCLIP: Program<'static> = Program {
    name: "xclip",
    clipboard: &["-selection", "clipboard"],
    primary: &["-selection", "primary"],
};

#[cfg(all(unix, not(target_os = "macos")))]
const XSEL: Program<'static> = Program {
    name: "xsel",
    clipboard: &["--clipboard", "--input"],
    primary: &["--primary", "--input"],
};

/// The clipboard programs tried in turn when arboard can't copy, on a
/// Wayland desktop (`wayland`) or an X11 one.
#[cfg(all(unix, not(target_os = "macos")))]
fn programs(wayland: bool) -> &'static [Program<'static>] {
    if wayland {
        &[WL_COPY, XCLIP]
    } else {
        &[XCLIP, XSEL]
    }
}

/// Copies through `program` to the clipboard, then to the primary
/// selection. Whether the clipboard took it, which is what counts.
#[cfg(all(unix, not(target_os = "macos")))]
fn copy_with(text: &str, program: &Program) -> bool {
    if !run(text, program.name, program.clipboard) {
        return false;
    }
    if !run(text, program.name, program.primary) {
        log::info!(
            "{} didn't take a copy for the primary selection",
            program.name
        );
    }
    true
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

    /// When arboard can't copy, wl-copy is tried first on Wayland and
    /// xclip on X11, and each of them sets the primary selection too.
    #[test]
    fn programs_are_tried_by_desktop_and_set_the_primary_selection() {
        let names = |wayland| {
            programs(wayland)
                .iter()
                .map(|program| program.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(true), ["wl-copy", "xclip"]);
        assert_eq!(names(false), ["xclip", "xsel"]);
        for program in programs(true).iter().chain(programs(false)) {
            assert!(
                program.primary.iter().any(|arg| arg.contains("primary")),
                "{} sets the primary selection",
                program.name
            );
        }
    }

    /// Runs a stand-in clipboard program that logs the selection it is
    /// asked for and the text it gets, and fails for the selection
    /// `fails`. Returns what `copy_with` returned and the log.
    fn copy_with_stand_in(fails: &str) -> (bool, String) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("clipboard.sh");
        let log = dir.path().join("log");
        std::fs::write(
            &script,
            format!("printf '%s=%s\\n' \"$1\" \"$(cat)\" >> \"$2\"\n[ \"$1\" != {fails} ]\n"),
        )
        .unwrap();
        let script = script.to_str().unwrap();
        let log = log.to_str().unwrap();
        // (Run by sh, so the script needn't be executable.)
        let program = Program {
            name: "sh",
            clipboard: &[script, "clipboard", log],
            primary: &[script, "primary", log],
        };
        let copied = copy_with("arko text", &program);
        let log = std::fs::read_to_string(log).unwrap_or_default();
        (copied, log)
    }

    /// A clipboard program gets the text for the clipboard, then for the
    /// primary selection.
    #[test]
    fn a_copy_goes_to_the_clipboard_and_the_primary_selection() {
        assert_eq!(
            copy_with_stand_in("none"),
            (true, "clipboard=arko text\nprimary=arko text\n".into())
        );
    }

    /// The copy counts as made when the clipboard took it, even if the
    /// primary selection didn't.
    #[test]
    fn the_clipboard_alone_counts_as_a_copy() {
        assert_eq!(
            copy_with_stand_in("primary"),
            (true, "clipboard=arko text\nprimary=arko text\n".into())
        );
    }

    /// A program that can't set the clipboard isn't asked for the primary
    /// selection, so the next program sets both.
    #[test]
    fn a_program_that_cant_set_the_clipboard_sets_nothing_else() {
        assert_eq!(
            copy_with_stand_in("clipboard"),
            (false, "clipboard=arko text\n".into())
        );
    }
}
