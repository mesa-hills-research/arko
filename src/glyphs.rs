//! The symbols Arko prints, as the terminal can show them. The classic
//! Windows console (cmd.exe or PowerShell in conhost) draws only its fonts'
//! characters and shows the others as question marks, so there the symbols
//! are replaced with stand-ins from the set those fonts share (WGL4).
//! ARKO_SYMBOLS chooses: `full`, `basic` (those stand-ins) or `ascii`.
//!
//! Arko's code writes the full symbols, and [`render`] replaces them where
//! text is printed ([`Output`](crate::ui::Output) and the prompt), so a
//! symbol used in a new line needs only an entry in [`SYMBOLS_TABLE`].

use std::borrow::Cow;
use std::sync::OnceLock;

use crate::credentials;

/// The setting that chooses the symbols: `full`, `basic` or `ascii`.
pub const SYMBOLS: &str = "ARKO_SYMBOLS";

/// Which symbols the terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symbols {
    /// As written: every modern terminal font has them.
    Full,
    /// Stand-ins in WGL4, the characters of the classic Windows console's
    /// fonts (Consolas, Lucida Console).
    Basic,
    /// Plain ASCII, for any terminal (the console's raster fonts, say).
    Ascii,
}

/// The symbols Arko prints, with their `basic` and `ascii` stand-ins.
pub const SYMBOLS_TABLE: &[(char, &str, &str)] = &[
    ('●', "●", "*"),
    ('⎿', "└", "->"),
    ('↳', "→", ">"),
    ('│', "│", "|"),
    ('⟳', "»", "~"),
    ('✉', "►", ">>"),
    ('✗', "×", "x"),
    ('✓', "√", "v"),
    ('■', "■", "#"),
    ('✎', "+", "+"),
    ('⇠', "←", "<-"),
    ('⇢', "→", "->"),
    ('→', "→", "->"),
    ('…', "…", "..."),
    ('›', "›", ">"),
    ('─', "─", "-"),
    ('·', "·", "|"),
    ('—', "—", "--"),
    ('–', "–", "-"),
    ('≈', "≈", "~"),
    ('“', "\"", "\""),
    ('”', "\"", "\""),
    ('’', "'", "'"),
    // The full-screen view's.
    ('▎', "│", "|"),
    ('▸', "›", ">"),
    ('○', "○", "o"),
    ('•', "•", "*"),
    ('↓', "↓", "v"),
    ('↑', "↑", "^"),
    ('←', "←", "<-"),
    ('┌', "┌", "+"),
    ('┐', "┐", "+"),
    ('└', "└", "+"),
    ('┘', "┘", "+"),
    ('╭', "┌", "+"),
    ('╮', "┐", "+"),
    ('╰', "└", "+"),
    ('╯', "┘", "+"),
    ('├', "├", "+"),
    ('┤', "┤", "+"),
    ('█', "█", "#"),
    ('▀', "▀", "\""),
    ('▄', "▄", "_"),
];

/// A spinner's frames in `symbols`, and how long each shows (in
/// milliseconds): braille dots where the font has them, else a turning
/// line.
pub fn spinner_frames(symbols: Symbols) -> (&'static [&'static str], u64) {
    match symbols {
        Symbols::Full => (&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"], 100),
        Symbols::Basic | Symbols::Ascii => (&["|", "/", "-", "\\"], 200),
    }
}

/// The symbols this terminal shows: ARKO_SYMBOLS (in the environment or a
/// `.env` file), else `basic` in the classic Windows console and `full`
/// elsewhere.
pub fn symbols() -> Symbols {
    #[cfg(test)]
    if let Some(symbols) = tests::CHOSEN.get() {
        return symbols;
    }
    static CHOSEN: OnceLock<Symbols> = OnceLock::new();
    *CHOSEN.get_or_init(|| {
        let setting = credentials::var(SYMBOLS);
        choose(setting.as_deref(), cfg!(windows) && classic_console())
    })
}

/// `text` with its symbols as this terminal shows them.
pub fn render(text: &str) -> Cow<'_, str> {
    render_as(symbols(), text)
}

fn render_as(symbols: Symbols, text: &str) -> Cow<'_, str> {
    if symbols == Symbols::Full || text.is_ascii() {
        return Cow::Borrowed(text);
    }
    let mut rendered = String::with_capacity(text.len());
    for c in text.chars() {
        match SYMBOLS_TABLE.iter().find(|(symbol, _, _)| *symbol == c) {
            Some((_, basic, ascii)) => rendered.push_str(if symbols == Symbols::Basic {
                basic
            } else {
                ascii
            }),
            None => rendered.push(c),
        }
    }
    Cow::Owned(rendered)
}

/// The symbols for ARKO_SYMBOLS's `setting`, in the classic Windows console
/// or not. A setting Arko doesn't know is ignored, with a warning.
fn choose(setting: Option<&str>, classic: bool) -> Symbols {
    match setting
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("full") => Symbols::Full,
        Some("basic") => Symbols::Basic,
        Some("ascii") => Symbols::Ascii,
        other => {
            if let Some(other) = other.filter(|value| !value.is_empty()) {
                eprintln!("arko: {SYMBOLS} is `full`, `basic` or `ascii`, not `{other}`");
            }
            if classic {
                Symbols::Basic
            } else {
                Symbols::Full
            }
        }
    }
}

/// Whether Arko runs in the classic Windows console: no terminal says what
/// it is (Windows Terminal, VS Code, WezTerm, ConEmu, Alacritty).
pub fn classic_console() -> bool {
    [
        "WT_SESSION",
        "TERM_PROGRAM",
        "ConEmuPID",
        "ALACRITTY_WINDOW_ID",
    ]
    .iter()
    .all(|name| std::env::var_os(name).is_none_or(|value| value.is_empty()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        /// The symbols a test chose for its thread, in place of the
        /// terminal's.
        pub(super) static CHOSEN: std::cell::Cell<Option<Symbols>> =
            const { std::cell::Cell::new(None) };
    }

    /// Whether `c` is in WGL4, the set of the classic Windows console's
    /// fonts (Latin-1, the arrows, box drawing, a few shapes, punctuation
    /// and `≈`, which code page 437 has too).
    pub(crate) fn wgl4(c: char) -> bool {
        matches!(c as u32,
            0x20..=0x7E | 0xA0..=0xFF | 0x2013 | 0x2014 | 0x2022 | 0x2026 | 0x203A | 0x221A
            | 0x2248 | 0x2190..=0x2195 | 0x2500..=0x257F | 0x2580 | 0x2584 | 0x2588
            | 0x25A0 | 0x25BA | 0x25CB | 0x25CF)
    }

    /// Shows `symbols` on this thread until the guard goes.
    pub(crate) fn choose_symbols(symbols: Symbols) -> impl Drop {
        struct Chosen;
        impl Drop for Chosen {
            fn drop(&mut self) {
                CHOSEN.set(None);
            }
        }
        CHOSEN.set(Some(symbols));
        Chosen
    }

    #[test]
    fn the_setting_chooses_and_the_console_decides_otherwise() {
        assert_eq!(choose(None, false), Symbols::Full);
        assert_eq!(choose(None, true), Symbols::Basic);
        assert_eq!(choose(Some(" ASCII "), false), Symbols::Ascii);
        assert_eq!(choose(Some("full"), true), Symbols::Full);
        assert_eq!(choose(Some("basic"), false), Symbols::Basic);
        assert_eq!(choose(Some("fancy"), true), Symbols::Basic);
    }

    #[test]
    fn symbols_are_replaced_and_other_text_kept() {
        let line = "  ⎿ exit code 0 · 3 lines… ✗ café";
        assert_eq!(render_as(Symbols::Full, line), line);
        assert_eq!(
            render_as(Symbols::Basic, line),
            "  └ exit code 0 · 3 lines… × café"
        );
        assert_eq!(
            render_as(Symbols::Ascii, line),
            "  -> exit code 0 | 3 lines... x café"
        );
        assert!(matches!(
            render_as(Symbols::Ascii, "plain"),
            Cow::Borrowed("plain")
        ));
    }

    /// The `basic` stand-ins are characters the classic console's fonts
    /// draw (WGL4: Latin-1, the arrows, box drawing, a few shapes,
    /// punctuation and `≈`, which code page 437 has too), and the `ascii`
    /// ones are ASCII.
    #[test]
    fn the_stand_ins_stay_in_their_sets() {
        for (symbol, basic, ascii) in SYMBOLS_TABLE {
            assert!(basic.chars().all(wgl4), "{symbol}: {basic:?}");
            assert!(ascii.is_ascii(), "{symbol}: {ascii:?}");
        }
    }

    /// The spinner's frames are one column wide, and the stand-ins' are
    /// ASCII (which the classic console's fonts have too).
    #[test]
    fn the_spinner_has_frames_for_every_terminal() {
        for symbols in [Symbols::Full, Symbols::Basic, Symbols::Ascii] {
            let (frames, ms) = spinner_frames(symbols);
            assert!(frames.len() >= 4 && ms >= 80, "{symbols:?}");
            for frame in frames {
                assert_eq!(frame.chars().count(), 1, "{frame:?}");
                assert!(symbols == Symbols::Full || frame.is_ascii(), "{frame:?}");
            }
        }
    }

    /// Every symbol in the lines Arko prints has stand-ins.
    #[test]
    fn the_symbols_in_the_terminal_lines_have_stand_ins() {
        for (file, source) in [
            ("ui.rs", include_str!("ui.rs")),
            ("main.rs", include_str!("main.rs")),
            ("crew.rs", include_str!("crew.rs")),
            ("workers.rs", include_str!("workers.rs")),
            ("worker_commands.rs", include_str!("worker_commands.rs")),
            ("providers.rs", include_str!("providers.rs")),
            ("tui/app.rs", include_str!("tui/app.rs")),
            ("tui/transcript.rs", include_str!("tui/transcript.rs")),
            ("tui/markdown.rs", include_str!("tui/markdown.rs")),
            ("tui/input.rs", include_str!("tui/input.rs")),
            ("tui/overlay.rs", include_str!("tui/overlay.rs")),
            ("tui/screen.rs", include_str!("tui/screen.rs")),
            ("tui/mod.rs", include_str!("tui/mod.rs")),
        ] {
            for c in source.chars().filter(|c| !c.is_ascii()) {
                // Letters (in test strings and names) are shown as they are.
                if c.is_alphabetic() {
                    continue;
                }
                assert!(
                    SYMBOLS_TABLE.iter().any(|(symbol, _, _)| *symbol == c),
                    "{file}: {c:?} (U+{:04X}) has no stand-ins",
                    c as u32
                );
            }
        }
    }
}
