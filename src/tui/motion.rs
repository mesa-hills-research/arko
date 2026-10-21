//! What moves on the screen while the agent is under way: a spinner in the
//! margin, and a highlight that sweeps across the word beside it. The
//! frames come from a clock, so each redraw shows the frame of its moment,
//! and the view redraws only while one shows.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;

use super::theme::Theme;
use crate::glyphs;

/// How often the view redraws while something moves: 10 frames a second.
pub const FRAME: Duration = Duration::from_millis(100);
/// One sweep of the highlight, from before the word to past it.
const SWEEP_MS: u64 = 2000;
/// How far the highlight reaches each side of its middle, in columns.
const REACH: f64 = 3.0;

/// The animations' clock: milliseconds since the first frame.
pub fn clock() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// The spinner's frame at `ms`, in the symbols the terminal shows.
pub fn spinner(ms: u64) -> &'static str {
    let (frames, each) = glyphs::spinner_frames(glyphs::symbols());
    frames[(ms / each) as usize % frames.len()]
}

/// How lit (0 to 1) the column `col` of a word `width` columns wide is at
/// `ms`: the highlight sweeps from left to right, fading at its edges.
pub fn shimmer(col: usize, width: usize, ms: u64) -> f64 {
    let travel = width as f64 + 2.0 * REACH;
    let middle = (ms % SWEEP_MS) as f64 / SWEEP_MS as f64 * travel - REACH;
    let distance = (col as f64 + 0.5 - middle).abs();
    if distance >= REACH {
        0.0
    } else {
        (1.0 + (std::f64::consts::PI * distance / REACH).cos()) / 2.0
    }
}

/// Paints the frame at `ms` over a line drawn at `x`, `y`: the spinner in
/// its margin's first column, and the highlight over the word of `width`
/// columns after the margin.
pub fn paint(buffer: &mut Buffer, (x, y): (u16, u16), width: usize, theme: &Theme, ms: u64) {
    let area = buffer.area;
    if !area.contains(ratatui::layout::Position { x, y }) {
        return;
    }
    buffer[(x, y)]
        .set_symbol(spinner(ms))
        .set_style(theme.dim());
    for col in 0..width {
        let at = x as usize + 2 + col;
        if at >= area.right() as usize {
            break;
        }
        buffer[(at as u16, y)].set_style(theme.lit(shimmer(col, width, ms)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The highlight crosses the word once a sweep: it is past the word's
    /// start early, and at its end later, and off it between sweeps.
    #[test]
    fn the_highlight_sweeps_across_the_word() {
        let lit = |ms: u64| -> Vec<bool> { (0..8).map(|col| shimmer(col, 8, ms) > 0.5).collect() };
        let first = |ms: u64| lit(ms).iter().position(|on| *on);
        assert_eq!(lit(0), [false; 8], "it starts before the word");
        let early = first(SWEEP_MS / 4).expect("lit early");
        let late = first(SWEEP_MS * 3 / 4).expect("lit later");
        assert!(early < late, "{early} {late}");
        assert_eq!(lit(SWEEP_MS - 1), [false; 8], "and ends past it");
        assert_eq!(
            lit(SWEEP_MS / 4),
            lit(SWEEP_MS + SWEEP_MS / 4),
            "and goes again"
        );
        for ms in (0..SWEEP_MS).step_by(100) {
            for col in 0..8 {
                assert!((0.0..=1.0).contains(&shimmer(col, 8, ms)));
            }
        }
    }

    /// A frame paints the spinner in the margin and lights the word where
    /// the highlight is, and leaves the rest of the line as it was.
    #[test]
    fn a_frame_paints_the_spinner_and_the_highlight() {
        use crate::tui::theme::{Catalog, DEFAULT, Depth};
        use ratatui::layout::Rect;
        use ratatui::style::Color;
        use ratatui::text::Line;
        let theme = Catalog::load(None).theme(DEFAULT, Depth::Rgb);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        let line = Line::styled("  Working… and more", theme.dim());
        buffer.set_line(0, 0, &line, 20);
        // The highlight's middle is between the word's fourth and fifth
        // columns.
        let ms = SWEEP_MS / 2;
        paint(&mut buffer, (0, 0), 8, &theme, ms);
        assert_eq!(buffer[(0, 0)].symbol(), spinner(ms));
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        let fg = |x: u16| buffer[(x, 0)].fg;
        let lit = |x: u16| fg(x) != theme.dim && matches!(fg(x), Color::Rgb(..));
        assert!(!lit(2) && lit(5) && lit(6) && !lit(9), "the word's middle");
        assert_eq!(fg(12), theme.dim, "past the word");
        assert_eq!(buffer[(2, 0)].symbol(), "W", "the word stays");
    }

    /// The spinner turns as the clock goes.
    #[test]
    fn the_spinner_turns() {
        let (frames, each) = glyphs::spinner_frames(glyphs::symbols());
        assert_eq!(spinner(0), frames[0]);
        assert_eq!(spinner(each), frames[1]);
        assert_eq!(spinner(each * frames.len() as u64), frames[0]);
    }
}
