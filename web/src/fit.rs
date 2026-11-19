//! A row's line fitted to its width before the browser paints it, by the
//! rule every client follows ([`quiet::fit`]). The page measures the room a
//! call line leaves for its words and what follows them, and the width of
//! what it draws there, in pixels. The row then draws its words cut where a
//! word ends and the parts after them whole or not at all, as the terminal
//! does in its cells.
//!
//! A line is its grapheme clusters, so a cut never splits one. Printable
//! ASCII takes the call lines' monospaced font's advance (`ch`). Anything
//! else (an accent, a wide character, an emoji, another script, a glyph the
//! font draws from a fallback) is measured as the browser draws it, and a
//! text that holds any is measured whole, since shaping across clusters can
//! change their advances.
//!
//! The room is measured in the task that drew or changed the line, after
//! the browser lays it out and before it paints, so no frame shows a line
//! that doesn't fit and then the line fitted.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use leptos::prelude::*;
use unicode_segmentation::UnicodeSegmentation;

use crate::quiet;

/// The gap between the parts of a call line (`.call-line` and `.said` in
/// app.css), in pixels.
pub const GAP: f64 = 6.0;

/// Hundredths of a pixel, the unit the page fits lines in.
const UNIT: f64 = 100.0;

/// The call lines' font (`.row.call` in app.css).
pub const CALL_FONT: &str = "13px/20px var(--mono)";

/// The font of what a code run's calls did, in its open row (`.run-calls`
/// in app.css).
pub const RUN_FONT: &str = "12px/18px var(--mono)";

/// What the page measures of a monospaced font, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// The font, as CSS writes it.
    pub font: &'static str,
    /// The advance of a printable ASCII character.
    pub ch: f64,
    /// ✓ and ✗, which the font may not have.
    pub mark: f64,
    /// "…".
    pub ellipsis: f64,
}

impl Metrics {
    /// The width of a dot and a space before a part ("· ").
    pub fn dot(self) -> f64 {
        self.text("· ")
    }

    /// The width of `text` as the browser draws it in this font.
    pub fn text(self, text: &str) -> f64 {
        match ascii(text) {
            true => text.len() as f64 * self.ch,
            false => measure(self.font, text),
        }
    }
}

/// Whether `text` is printable ASCII alone, whose every character takes the
/// monospaced font's advance.
fn ascii(text: &str) -> bool {
    text.bytes().all(|byte| (0x20..0x7F).contains(&byte))
}

fn units(px: f64) -> usize {
    (px * UNIT).round().max(0.0) as usize
}

/// `text`'s grapheme clusters, each with the units it takes.
fn clusters(text: &str, m: Metrics) -> Vec<(&str, usize)> {
    text.graphemes(true)
        .map(|cluster| (cluster, units(m.text(cluster))))
        .collect()
}

/// A line's words and the parts after them as they fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fitted {
    /// Its words, cut where a word ends, with an ellipsis.
    pub words: String,
    /// How many of the parts after its words show, from the first.
    pub parts: usize,
    /// It cut its words.
    pub words_cut: bool,
    /// It cut its words or left out a part.
    pub cut: bool,
}

/// `words` and the parts after them (`parts`, the room each takes with the
/// gap and dot before it, and the columns the words keep before it gives
/// way, [`quiet::WORDS_KEEP`] or [`quiet::WORDS_KEEP_RUNNING`]) fitted to
/// `room` pixels, where `fixed` is the width of what never gives way among
/// them (a call's ✓ or ✗).
pub fn line(words: &str, fixed: f64, parts: &[(f64, usize)], room: f64, m: Metrics) -> Fitted {
    let parts: Vec<quiet::Part> = parts
        .iter()
        .map(|(width, keep)| quiet::Part {
            width: units(*width),
            keep: units(*keep as f64 * m.ch),
        })
        .collect();
    let fit = quiet::fit(units(m.text(words)), units(fixed), &parts, units(room));
    let shown = cut(words, fit.words, m);
    Fitted {
        cut: shown != words || fit.parts < parts.len(),
        words_cut: shown != words,
        words: shown,
        parts: fit.parts,
    }
}

/// `text` in `room` units, cut where a word ends ([`quiet::cut_at_word`])
/// with an ellipsis after it. A text whose clusters the browser may shape
/// together is measured whole once cut, and cut further while that doesn't
/// fit.
fn cut(text: &str, room: usize, m: Metrics) -> String {
    if units(m.text(text)) <= room {
        return text.to_string();
    }
    let clusters = clusters(text, m);
    let ellipsis = units(m.ellipsis);
    let mut left = room.saturating_sub(ellipsis);
    loop {
        let kept = quiet::cut_at_word(&clusters, left, units(quiet::WORDS_KEEP as f64 * m.ch));
        let shown: String = clusters[..kept].iter().map(|(c, _)| *c).collect::<String>() + "…";
        let over = units(m.text(&shown)).saturating_sub(room);
        if over == 0 || kept == 0 {
            return shown;
        }
        left = left.saturating_sub(over.max(1));
    }
}

/// A file tool's line as it fits ([`file`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FittedFile {
    pub folders: String,
    pub name: String,
    /// Its outcome's words show.
    pub outcome: bool,
    /// It cut its folders or its name.
    pub words_cut: bool,
    /// It cut its folders or its name, or left out its outcome's words.
    pub cut: bool,
}

/// A file tool's line fitted to `room` pixels by [`quiet::file_fit`], from
/// its `lead`, `folders` and `name`, the width of what never gives way
/// after them (`fixed`, its ✓ or ✗ with its dot) and of its outcome's words
/// (`outcome`, none without them). Folders that don't fit show from where
/// a folder starts, after "…/", and a name that doesn't fit is cut at its
/// end.
pub fn file(
    lead: &str,
    folders: &str,
    name: &str,
    fixed: f64,
    outcome: Option<f64>,
    room: f64,
    m: Metrics,
) -> FittedFile {
    let elided = "…/";
    let fit = quiet::file_fit(
        units(m.text(lead) + fixed),
        outcome.map_or(0, units),
        units(m.text(folders)),
        units(m.text(name)),
        units(m.text(elided)),
        units(room),
    );
    let shown_folders = if fit.folders >= units(m.text(folders)) {
        folders.to_string()
    } else {
        let clusters = clusters(folders, m);
        let from =
            quiet::folders_from(&clusters, fit.folders.saturating_sub(units(m.text(elided))));
        let kept: String = clusters[from..].iter().map(|(c, _)| *c).collect();
        format!("{elided}{kept}")
    };
    // The name with the rest of the line after it ("app.rs (all)"), cut as
    // words are, so a group in parentheses goes whole. Drawn whole, the
    // browser may shape it narrower or wider than its clusters, so it's
    // measured again and cut further while it doesn't fit.
    let room_left = units(room).saturating_sub(
        units(fixed)
            + if fit.outcome {
                outcome.map_or(0, units)
            } else {
                0
            },
    );
    let mut name_room = fit.name;
    let mut shown_name = cut(name, name_room, m);
    while name_room > 0 && units(m.text(&format!("{lead}{shown_folders}{shown_name}"))) > room_left
    {
        name_room = name_room.saturating_sub(units(m.ch).max(1));
        shown_name = cut(name, name_room, m);
    }
    let dropped = outcome.is_some() && !fit.outcome;
    FittedFile {
        words_cut: shown_folders != folders || shown_name != name,
        cut: shown_folders != folders || shown_name != name || dropped,
        folders: shown_folders,
        name: shown_name,
        outcome: !dropped,
    }
}

thread_local! {
    /// The lines waiting to be measured, with where their room goes.
    static WAITING: RefCell<Vec<(web_sys::Element, RwSignal<Option<Room>>)>> = const { RefCell::new(Vec::new()) };
    /// A measure of them is on its way.
    static ASKED: Cell<bool> = const { Cell::new(false) };
    /// What was measured of each font, until the fonts load.
    static METRICS: RefCell<HashMap<&'static str, Metrics>> = RefCell::new(HashMap::new());
    /// The widths of the texts measured in each font, until the fonts load.
    static WIDTHS: RefCell<HashMap<(&'static str, String), f64>> = RefCell::new(HashMap::new());
}

/// What the page measures of the call lines' font ([`CALL_FONT`]).
pub fn metrics() -> Metrics {
    metrics_of(CALL_FONT)
}

/// What the page measures of `font`, measured once and again when the
/// fonts load. Whatever fits a line with it fits the line again then, even
/// where the line's room stays the same.
pub fn metrics_of(font: &'static str) -> Metrics {
    loaded().track();
    if let Some(metrics) = METRICS.with(|metrics| metrics.borrow().get(font).copied()) {
        return metrics;
    }
    let measured = Metrics {
        font,
        ch: measure(font, "0000000000") / 10.0,
        mark: measure(font, "✓").max(measure(font, "✗")),
        ellipsis: measure(font, "…"),
    };
    METRICS.with(|metrics| metrics.borrow_mut().insert(font, measured));
    measured
}

/// The width of `text` as the browser draws it in `font`, measured once.
fn measure(font: &'static str, text: &str) -> f64 {
    let key = (font, text.to_string());
    if let Some(width) = WIDTHS.with(|widths| widths.borrow().get(&key).copied()) {
        return width;
    }
    let width = draw_width(font, text);
    WIDTHS.with(|widths| widths.borrow_mut().insert(key, width));
    width
}

/// The width of `text` drawn in a hidden probe in `font`.
#[cfg(not(test))]
fn draw_width(font: &str, text: &str) -> f64 {
    use wasm_bindgen::JsCast;
    thread_local! {
        static PROBE: RefCell<Option<web_sys::HtmlElement>> = const { RefCell::new(None) };
    }
    PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        if probe.as_ref().is_none_or(|probe| !probe.is_connected()) {
            let document = document();
            let made = document
                .create_element("span")
                .ok()
                .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok());
            if let (Some(made), Some(body)) = (made, document.body()) {
                let _ = made.set_attribute("aria-hidden", "true");
                // (Set through the style object, since the page's content
                // security policy refuses a style attribute.)
                let style = made.style();
                for (name, value) in [
                    ("position", "absolute"),
                    ("left", "0"),
                    ("top", "0"),
                    ("visibility", "hidden"),
                    ("white-space", "pre"),
                    ("pointer-events", "none"),
                ] {
                    let _ = style.set_property(name, value);
                }
                let _ = body.append_child(&made);
                *probe = Some(made);
            }
        }
        probe.as_ref().map_or(0.0, |probe| {
            let _ = probe.style().set_property("font", font);
            probe.set_text_content(Some(text));
            probe.get_bounding_client_rect().width()
        })
    })
}

/// The width of `text` in tests, where no browser draws it: 8 px a cluster,
/// 16 for one past U+2E80 (CJK, emoji).
#[cfg(test)]
fn draw_width(_font: &str, text: &str) -> f64 {
    text.graphemes(true)
        .map(|cluster| match cluster.chars().next() {
            Some(c) if (c as u32) >= 0x2E80 => 16.0,
            _ => 8.0,
        })
        .sum()
}

/// The room a line has for its words and what follows them, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Room {
    /// As it stands now, past the spinner and the buttons it shows.
    pub now: f64,
    /// As it stands through its run, without the spinner and the buttons,
    /// which come and go. Whether a row opens to what its line left out is
    /// decided by this room, so that never comes or goes with them.
    pub steady: f64,
}

/// The room the call line `line` (a `.call-line`) has for what its `.said`
/// shows. That is from where `.said` starts to the line's end, past what
/// follows it (the spinner, the buttons, a row's ✗) and the gaps before
/// those. A text alone in its box (`.fitted`) has the room of the box it's
/// in. `None` while the line isn't laid out.
fn room(line: &web_sys::Element) -> Option<Room> {
    if line
        .class_name()
        .split_whitespace()
        .any(|class| class == "fitted")
    {
        let width = f64::from(line.parent_element()?.client_width());
        return (width > 0.0).then_some(Room {
            now: width,
            steady: width,
        });
    }
    let said = line.query_selector(":scope > .said").ok()??;
    let end = line.get_bounding_client_rect();
    if end.width() <= 0.0 {
        return None;
    }
    let start = said.get_bounding_client_rect().left();
    let mut after = 0.0;
    let mut passing = 0.0;
    let mut next = said.next_element_sibling();
    while let Some(element) = next {
        let width = element.get_bounding_client_rect().width();
        // (A button's margin, 4 px in app.css. The ✗ that stands at a row's
        // right has an automatic margin, which takes whatever room is left,
        // so it isn't counted.)
        let button = element.class_name().contains("call-action");
        let margin = |side: &str| {
            button
                .then(|| window().get_computed_style(&element).ok().flatten())
                .flatten()
                .and_then(|style| style.get_property_value(side).ok())
                .and_then(|value| value.trim_end_matches("px").parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        let taken = width + GAP + margin("margin-left") + margin("margin-right");
        after += taken;
        // (The spinner and the buttons, which come and go.)
        let has = |selector: &str| element.query_selector(selector).ok().flatten().is_some();
        if button || has(".spinner") || has(".call-action") {
            passing += taken;
        }
        next = element.next_element_sibling();
    }
    let now = (end.right() - start - after).max(0.0);
    Some(Room {
        now,
        steady: now + passing,
    })
}

/// The room of the call line `line` holds ([`Room`]), kept up to date before
/// the browser paints, whenever `track` reads something new, the
/// transcript's width changes or the fonts load. `None` until it's
/// measured.
pub fn watch(
    line: NodeRef<leptos::html::Span>,
    track: impl Fn() + Send + Sync + 'static,
) -> RwSignal<Option<Room>> {
    let measured = RwSignal::new(None);
    let page = crate::app::page();
    let fonts = loaded();
    Effect::new(move |_| {
        track();
        page.width.track();
        fonts.track();
        if let Some(line) = line.get() {
            ask(line.into(), measured);
        }
    });
    measured
}

/// Measures `line` with every other line asked for, together, so the
/// browser lays the page out once for all of them, and only then gives
/// each its room. (A microtask, which runs before the browser paints.)
fn ask(line: web_sys::Element, measured: RwSignal<Option<Room>>) {
    WAITING.with(|waiting| waiting.borrow_mut().push((line, measured)));
    if ASKED.with(|asked| asked.replace(true)) {
        return;
    }
    leptos::task::spawn_local(async {
        ASKED.with(|asked| asked.set(false));
        let waiting = WAITING.with(|waiting| std::mem::take(&mut *waiting.borrow_mut()));
        let rooms: Vec<(RwSignal<Option<Room>>, Option<Room>)> = waiting
            .into_iter()
            .map(|(line, signal)| (signal, line.is_connected().then(|| room(&line)).flatten()))
            .collect();
        // (A row that went meanwhile has let go of its signal.)
        for (signal, room) in rooms {
            if room.is_some() && signal.try_get_untracked().is_some_and(|was| was != room) {
                signal.try_set(room);
            }
        }
        // (The lines just laid out may have asked for a font that loads.)
        fonts_loading(false);
    });
}

thread_local! {
    /// The font-load signal ([`loaded`]), once asked for.
    static LOADED: RefCell<Option<ArcRwSignal<u32>>> = const { RefCell::new(None) };
    /// A wait for the fonts loading now is on its way ([`fonts_loading`]).
    static AWAITING: Cell<bool> = const { Cell::new(false) };
}

/// A signal that changes whenever the page's fonts have loaded, which
/// changes what [`metrics`] and [`measure`] measure. That is when the fonts
/// the page asked for first are ready, and each time a font that a line
/// asked for later has loaded (the call lines' own, once one shows), which
/// [`fonts_loading`] waits for. It belongs to no row, so it outlives the
/// row that first asked for it.
fn loaded() -> ArcRwSignal<u32> {
    if let Some(loaded) = LOADED.with(|cell| cell.borrow().clone()) {
        return loaded;
    }
    let loaded = ArcRwSignal::new(0);
    LOADED.with(|cell| *cell.borrow_mut() = Some(loaded.clone()));
    fonts_loading(true);
    loaded
}

/// What was measured goes, and every line fits again.
fn fonts_loaded() {
    METRICS.with(|metrics| metrics.borrow_mut().clear());
    WIDTHS.with(|widths| widths.borrow_mut().clear());
    if let Some(loaded) = LOADED.with(|cell| cell.borrow().clone()) {
        loaded.update(|count| *count += 1);
    }
}

/// Waits for the fonts the page is loading, where it is (or, the first
/// time, `first`, for those it asked for first), and then has every line
/// fit again ([`fonts_loaded`]). A font loads when text first asks for it,
/// after the line that asked was laid out, and not every browser says so
/// with an event, so each measure asks again.
fn fonts_loading(first: bool) {
    use wasm_bindgen::JsCast;
    if AWAITING.with(Cell::get) {
        return;
    }
    let Some(fonts) = js_sys::Reflect::get(&document(), &"fonts".into()).ok() else {
        return;
    };
    let status = js_sys::Reflect::get(&fonts, &"status".into())
        .ok()
        .and_then(|status| status.as_string());
    if !first && status.as_deref() != Some("loading") {
        return;
    }
    let Some(ready) = js_sys::Reflect::get(&fonts, &"ready".into())
        .ok()
        .and_then(|ready| ready.dyn_into::<js_sys::Promise>().ok())
    else {
        return;
    };
    AWAITING.with(|awaiting| awaiting.set(true));
    leptos::task::spawn_local(async move {
        let _ = wasm_bindgen_futures::JsFuture::from(ready).await;
        AWAITING.with(|awaiting| awaiting.set(false));
        fonts_loaded();
    });
}

/// The width of `text` as the browser draws it in `font`, any font,
/// measured again once the fonts load.
pub fn text_width(font: &'static str, text: &str) -> f64 {
    loaded().track();
    measure(font, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics {
        font: CALL_FONT,
        ch: 8.0,
        mark: 8.0,
        ellipsis: 8.0,
    };

    /// The page cuts a line's words where a word ends, and leaves out the
    /// parts after them whole, as the terminal does in its cells.
    #[test]
    fn a_line_gives_way_as_the_terminals_does() {
        let note = (GAP + M.dot() + M.text("running"), quiet::WORDS_KEEP);
        let live = (GAP + M.dot() + M.text("2 calls"), quiet::WORDS_KEEP_RUNNING);
        // Room for all of it.
        let all = line("Started cargo build", 0.0, &[note], 400.0, M);
        assert_eq!(
            (all.words.as_str(), all.parts, all.cut),
            ("Started cargo build", 1, false)
        );
        // How it runs goes while the words would keep less than 24 columns,
        // and what happened only below 12. The words take what's left.
        let words = "Started cargo build --release in a terminal";
        let narrow = line(words, 0.0, &[note, live], 30.0 * 8.0, M);
        assert_eq!(narrow.parts, 1);
        assert_eq!(narrow.words, "Started cargo build…");
        assert!(narrow.cut);
        let narrower = line(words, 0.0, &[note], 22.0 * 8.0, M);
        assert_eq!((narrower.words.as_str(), narrower.parts), ("Started…", 1));
        let narrowest = line(words, 0.0, &[note], 21.0 * 8.0, M);
        assert_eq!(
            (narrowest.words.as_str(), narrowest.parts),
            ("Started cargo build…", 0)
        );
        // A word too long for half the room is cut inside.
        let path = line("$ ./target/release/arko-something", 0.0, &[], 20.0 * 8.0, M);
        assert_eq!(path.words, "$ ./target/release/…");
    }

    /// A cut never splits a grapheme cluster (an accent with its letter, an
    /// emoji its joiner holds together, a flag), and a cluster takes the
    /// room the browser draws it in.
    #[test]
    fn a_cut_keeps_each_cluster_whole() {
        let joined = ["e\u{301}", "👩\u{200D}💻", "🇫🇷", "1\u{FE0F}\u{20E3}"];
        for cluster in joined {
            let text = format!("ab{cluster}cd");
            for room in 1..8 {
                let shown = cut(&text, units(room as f64 * 8.0), M);
                let kept = shown.trim_end_matches('…');
                assert!(text.starts_with(kept), "{text} in {room}: {shown}");
                assert!(
                    kept.len() <= 2 || kept.contains(cluster),
                    "{text} in {room}: {shown}"
                );
            }
        }
        // (Room for "caf" and the ellipsis cuts before the accented e.)
        assert_eq!(cut("cafe\u{301}s", units(32.0), M), "caf…");
        // A wide character takes its own width.
        assert_eq!(cut("ab界cd", units(32.0), M), "ab…");
    }

    /// A file's name with the rest of its line after it gives the group in
    /// parentheses way whole, as words do.
    #[test]
    fn a_files_name_gives_its_group_way_whole() {
        let fixed = GAP + M.dot() + M.mark;
        for columns in 14..40 {
            let fitted = file(
                "write ",
                "src/",
                "runtime_config.rs (123 lines)",
                fixed,
                None,
                columns as f64 * 8.0,
                M,
            );
            let name = fitted.name.trim_end_matches('…');
            assert!(
                !name.contains('(') || name.ends_with(')'),
                "{columns}: {fitted:?}"
            );
        }
    }

    /// A file's line keeps its name, and its folders show from where a
    /// folder starts.
    #[test]
    fn a_file_line_keeps_its_name_and_whole_folders() {
        let fixed = GAP + M.dot() + M.mark;
        let outcome = M.text(" 1 edit");
        let fitted = file(
            "edit ",
            "path/results/shortening/",
            "limits.rs",
            fixed,
            Some(outcome),
            30.0 * 8.0,
            M,
        );
        assert_eq!(fitted.name, "limits.rs");
        assert!(fitted.folders.starts_with("…/"), "{fitted:?}");
        assert!(!fitted.folders.contains("sults"), "{fitted:?}");
        assert!(fitted.cut);
    }
}
