//! The terminal's modes as they were at startup, restored at exit. The line
//! editor reads in raw mode (no echo) on its own thread, and the process
//! can end while it waits there, which would leave the terminal raw.

#[cfg(unix)]
mod imp {
    use std::sync::OnceLock;

    static SAVED: OnceLock<libc::termios> = OnceLock::new();

    pub fn save() {
        // SAFETY: `termios` is plain data that `tcgetattr` fills in.
        unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            if libc::isatty(libc::STDIN_FILENO) == 1
                && libc::tcgetattr(libc::STDIN_FILENO, &mut mode) == 0
            {
                let _ = SAVED.set(mode);
            }
        }
    }

    pub fn restore() {
        if let Some(mode) = SAVED.get() {
            // SAFETY: restores the mode `tcgetattr` read from the same descriptor.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, mode);
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    /// The input and output console modes.
    static SAVED: OnceLock<(Option<u32>, Option<u32>)> = OnceLock::new();

    fn mode(which: u32) -> Option<u32> {
        let mut mode = 0;
        // SAFETY: a standard handle and a mode it writes.
        (unsafe { GetConsoleMode(GetStdHandle(which), &mut mode) } != 0).then_some(mode)
    }

    pub fn save() {
        let _ = SAVED.set((mode(STD_INPUT_HANDLE), mode(STD_OUTPUT_HANDLE)));
    }

    pub fn restore() {
        if let Some((input, output)) = SAVED.get() {
            for (which, mode) in [(STD_INPUT_HANDLE, input), (STD_OUTPUT_HANDLE, output)] {
                if let Some(mode) = mode {
                    // SAFETY: restores the mode read from the same standard handle.
                    unsafe { SetConsoleMode(GetStdHandle(which), *mode) };
                }
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn save() {}
    pub fn restore() {}
}

/// Saves the terminal's modes, and restores them if the process panics.
pub fn save() {
    imp::save();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        imp::restore();
        default(info);
    }));
}

/// Restores the modes [`save`] saved.
pub fn restore() {
    imp::restore();
}
