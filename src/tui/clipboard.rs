//! Copying a message: through the terminal (OSC 52, crossterm's
//! `CopyToClipboard`, which reaches the user's clipboard over SSH too
//! where the terminal honours it), and, on this machine, the system
//! clipboard as well, as not every terminal honours OSC 52.

/// Copies `text` to the system clipboard, on its own thread (a clipboard
/// program may take a moment); skipped over SSH, where this machine's
/// clipboard isn't the user's.
pub fn system(text: String) {
    let remote = ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    if remote {
        return;
    }
    std::thread::spawn(move || {
        copy(&text);
    });
}

#[cfg(windows)]
fn copy(text: &str) -> bool {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    // Windows' text has CRLF line breaks.
    let wide: Vec<u16> = text
        .replace("\r\n", "\n")
        .replace('\n', "\r\n")
        .encode_utf16()
        .chain([0])
        .collect();
    // SAFETY: the clipboard is opened, filled with memory it then owns
    // (or the memory is freed), and closed; the copy stays in bounds.
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let size = wide.len() * std::mem::size_of::<u16>();
        let memory = GlobalAlloc(GMEM_MOVEABLE, size);
        let mut done = false;
        if !memory.is_null() {
            let target = GlobalLock(memory) as *mut u16;
            if !target.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
                GlobalUnlock(memory);
                done = !SetClipboardData(CF_UNICODETEXT as u32, memory).is_null();
            }
            if !done {
                GlobalFree(memory);
            }
        }
        CloseClipboard();
        done
    }
}

#[cfg(not(windows))]
fn copy(text: &str) -> bool {
    use std::io::Write;
    let programs: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    } else if std::env::var_os("DISPLAY").is_some() {
        &[
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    } else {
        &[]
    };
    for (program, args) in programs {
        let child = std::process::Command::new(program)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // (xclip and wl-copy stay in the background to serve the
        // clipboard; the program started returns.)
        return child.wait().is_ok_and(|status| status.success());
    }
    false
}
