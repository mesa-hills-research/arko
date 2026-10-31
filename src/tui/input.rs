//! The input box's text and its editing: readline's keys, the history
//! (kept in Arko's `history.txt`, in the line editor's format, so both
//! front ends share it), and long pastes folded into chips.

use ratatui::style::Style;
use ratatui::text::Span;

use super::wrap::char_width;
pub use crate::history::History;

/// A paste of more lines than this (or more characters) is a chip: the box
/// grows to about this many lines.
const CHIP_LINES: usize = 8;
const CHIP_CHARS: usize = 2000;
/// Chips are characters of the supplementary private use area, from here.
const CHIP_BASE: u32 = 0xF0000;

/// What was typed, read back from the history.
struct Recall {
    /// The entry shown.
    index: usize,
    /// What was in the box before.
    draft: Vec<char>,
}

/// The input box's text.
pub struct Editor {
    text: Vec<char>,
    /// Where the cursor is: before `text[cursor]`.
    cursor: usize,
    /// The pasted blocks, by chip.
    chips: Vec<String>,
    /// What the last kill took, for Ctrl+Y.
    kill: String,
    history: History,
    recall: Option<Recall>,
    /// The column ↑ and ↓ keep to.
    goal: Option<usize>,
    /// Dictation's words that may still change, shown dimmed after the
    /// cursor. They aren't part of the text.
    tentative: Vec<char>,
}

impl Editor {
    pub fn new(history: History) -> Self {
        Self {
            text: Vec::new(),
            cursor: 0,
            chips: Vec::new(),
            kill: String::new(),
            history,
            recall: None,
            goal: None,
            tentative: Vec::new(),
        }
    }

    /// Shows dictation's tentative words after the cursor, in place of
    /// those shown before.
    pub fn set_tentative(&mut self, text: &str) {
        self.tentative = text.chars().filter(|c| !c.is_control()).collect();
    }

    pub fn has_tentative(&self) -> bool {
        !self.tentative.is_empty()
    }

    /// Inserts `text` at the cursor as typed (dictated text: never a chip).
    pub fn insert_text(&mut self, text: &str) {
        for c in text.chars().filter(|c| *c == '\n' || !c.is_control()) {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
        self.edited();
    }

    /// The character before the cursor.
    pub fn before_cursor(&self) -> Option<char> {
        self.cursor.checked_sub(1).map(|at| self.text[at])
    }

    /// The characters from `from` to `to`, if the text has them.
    pub fn slice(&self, from: usize, to: usize) -> Option<String> {
        self.text.get(from..to).map(|chars| chars.iter().collect())
    }

    /// Removes the characters from `from` to `to`; the cursor keeps its
    /// place in what's left.
    pub fn remove(&mut self, from: usize, to: usize) {
        if from >= to || to > self.text.len() {
            return;
        }
        self.text.drain(from..to);
        if self.cursor >= to {
            self.cursor -= to - from;
        } else if self.cursor > from {
            self.cursor = from;
        }
        self.edited();
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The text as typed, with each chip's label: what the command menu
    /// looks at.
    pub fn shown(&self) -> String {
        self.text
            .iter()
            .map(|c| self.chip(*c).unwrap_or_else(|| c.to_string()))
            .collect()
    }

    /// Whether the text starts with `c`.
    pub fn starts_with(&self, c: char) -> bool {
        self.text.first() == Some(&c)
    }

    /// The text when it is a command's name being typed (`/se`): a slash,
    /// then no space; cheap on a long text.
    pub fn command_typed(&self) -> Option<String> {
        if !self.starts_with('/') {
            return None;
        }
        let mut name = String::new();
        for c in &self.text {
            if c.is_whitespace() || name.len() > 40 || self.chip(*c).is_some() {
                return None;
            }
            name.push(*c);
        }
        Some(name)
    }

    /// The text when it is a command line, cursor at its end (`/view q`):
    /// one short line that starts with a slash, without chips. What the
    /// argument completion looks at.
    pub fn command_line(&self) -> Option<String> {
        let plain = self.starts_with('/')
            && self.cursor == self.text.len()
            && self.text.len() <= 200
            && self
                .text
                .iter()
                .all(|c| *c != '\n' && self.chip(*c).is_none());
        plain.then(|| self.text.iter().collect())
    }

    /// The chip's label, for a chip's character.
    fn chip(&self, c: char) -> Option<String> {
        let index = (c as u32).checked_sub(CHIP_BASE)? as usize;
        let text = self.chips.get(index)?;
        let lines = text.lines().count();
        Some(if lines > 1 {
            format!("[pasted {lines} lines]")
        } else {
            format!("[pasted {} characters]", text.chars().count())
        })
    }

    /// The text with the chips' pastes in place: what is sent.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for c in &self.text {
            match (*c as u32)
                .checked_sub(CHIP_BASE)
                .and_then(|index| self.chips.get(index as usize))
            {
                Some(paste) => text.push_str(paste),
                None => text.push(*c),
            }
        }
        text
    }

    /// Empties the box.
    pub fn clear(&mut self) {
        self.text.clear();
        self.chips.clear();
        self.cursor = 0;
        self.recall = None;
        self.goal = None;
    }

    /// Puts `text` in the box, the cursor at its end.
    pub fn set(&mut self, text: &str) {
        self.text.clear();
        self.chips.clear();
        self.cursor = 0;
        self.paste(text);
        self.goal = None;
    }

    /// Takes the text to send: it goes in the history, and the box empties.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.history.add(&text);
        self.clear();
        text
    }

    fn edited(&mut self) {
        self.recall = None;
        self.goal = None;
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += 1;
        self.edited();
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    /// Inserts pasted `text`: a long one as a chip.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let long = text.lines().count() > CHIP_LINES || text.chars().count() > CHIP_CHARS;
        if long {
            let index = self.chips.len() as u32;
            self.chips.push(text);
            if let Some(c) = char::from_u32(CHIP_BASE + index) {
                self.insert(c);
            }
        } else {
            for c in text.chars() {
                if c == '\n' || !c.is_control() {
                    self.text.insert(self.cursor, c);
                    self.cursor += 1;
                }
            }
            self.edited();
        }
    }

    /// Folds the text typed since `start` into a chip when it is long: a
    /// paste that came as keys (where the terminal doesn't mark pastes).
    pub fn fold_paste(&mut self, start: usize) {
        if start > self.cursor || start > self.text.len() {
            return;
        }
        let pasted: String = self.text[start..self.cursor].iter().collect();
        if pasted.lines().count() > CHIP_LINES || pasted.chars().count() > CHIP_CHARS {
            self.text.drain(start..self.cursor);
            self.cursor = start;
            self.paste(&pasted);
        }
    }

    /// Puts the cursor before character `at`, or at the end past it.
    pub fn set_cursor(&mut self, at: usize) {
        self.cursor = at.min(self.text.len());
        self.goal = None;
    }

    /// Where the cursor is, in characters: for [`fold_paste`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
            self.edited();
        }
    }

    /// Deletes the character under the cursor; false when there is none.
    pub fn delete(&mut self) -> bool {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
            self.edited();
            true
        } else {
            false
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.goal = None;
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.len());
        self.goal = None;
    }

    fn word_start(&self) -> usize {
        let mut at = self.cursor;
        while at > 0 && !self.text[at - 1].is_alphanumeric() {
            at -= 1;
        }
        while at > 0 && self.text[at - 1].is_alphanumeric() {
            at -= 1;
        }
        at
    }

    fn word_end(&self) -> usize {
        let mut at = self.cursor;
        while at < self.text.len() && !self.text[at].is_alphanumeric() {
            at += 1;
        }
        while at < self.text.len() && self.text[at].is_alphanumeric() {
            at += 1;
        }
        at
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
        self.goal = None;
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_end();
        self.goal = None;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |at| at + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.text.len(), |at| self.cursor + at)
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
        self.goal = None;
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
        self.goal = None;
    }

    fn cut(&mut self, from: usize, to: usize) {
        if from < to {
            self.kill = self.text.drain(from..to).collect();
            self.cursor = from;
            self.edited();
        }
    }

    /// Ctrl+K: to the end of the line (the line break, at its end).
    pub fn kill_end(&mut self) {
        let end = self.line_end();
        let end = if end == self.cursor && end < self.text.len() {
            end + 1
        } else {
            end
        };
        self.cut(self.cursor, end);
    }

    /// Ctrl+U: to the start of the line.
    pub fn kill_start(&mut self) {
        self.cut(self.line_start(), self.cursor);
    }

    /// Ctrl+W, Alt+Backspace: the word before the cursor.
    pub fn kill_word(&mut self) {
        self.cut(self.word_start(), self.cursor);
    }

    /// Alt+D: the word after the cursor.
    pub fn kill_word_forward(&mut self) {
        self.cut(self.cursor, self.word_end());
    }

    /// Ctrl+Y: what the last kill took.
    pub fn yank(&mut self) {
        let kill: Vec<char> = self.kill.chars().collect();
        for c in kill {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
        self.edited();
    }

    /// The box's rows at `width`: each row's characters (by index), wrapped
    /// at line breaks and, at the width, at a space: a word that doesn't
    /// fit moves whole to the next row, and only a word longer than a row
    /// breaks inside. Spaces stay on the row they end (they show nothing
    /// past its edge), so the next row starts with the word. A chip is a
    /// word of its own.
    fn rows(&self, width: usize) -> Vec<Vec<usize>> {
        self.rows_of(&self.text, width)
    }

    /// The text as the box shows it: with the tentative words at the
    /// cursor.
    fn laid_out(&self) -> std::borrow::Cow<'_, [char]> {
        if self.tentative.is_empty() {
            return std::borrow::Cow::Borrowed(&self.text);
        }
        let mut shown = self.text[..self.cursor].to_vec();
        shown.extend_from_slice(&self.tentative);
        shown.extend_from_slice(&self.text[self.cursor..]);
        std::borrow::Cow::Owned(shown)
    }

    /// [`rows`](Self::rows) of `text`.
    fn rows_of(&self, text: &[char], width: usize) -> Vec<Vec<usize>> {
        let width = width.max(2);
        let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
        let mut used = 0;
        // The word being laid out: where it starts in the row, and the
        // width of the row before it.
        let mut word: Option<(usize, usize)> = None;
        for (index, c) in text.iter().enumerate() {
            if *c == '\n' {
                rows.last_mut().unwrap().push(index);
                rows.push(Vec::new());
                used = 0;
                word = None;
                continue;
            }
            let w = self.width_of(*c);
            if *c == ' ' || *c == '\t' {
                rows.last_mut().unwrap().push(index);
                used += w;
                word = None;
                continue;
            }
            let chip = self.chip(*c).is_some();
            if chip {
                word = None;
            }
            if used + w > width && used > 0 {
                match word {
                    // The word goes to the next row, unless it began this
                    // one (it is longer than a row).
                    Some((at, before)) if at > 0 => {
                        let moved = rows.last_mut().unwrap().split_off(at);
                        rows.push(moved);
                        used -= before;
                        word = Some((0, 0));
                    }
                    _ => {
                        rows.push(Vec::new());
                        used = 0;
                        word = None;
                    }
                }
            }
            if word.is_none() {
                word = Some((rows.last().unwrap().len(), used));
            }
            rows.last_mut().unwrap().push(index);
            used += w;
            if chip {
                word = None;
            }
        }
        rows
    }

    fn width_of(&self, c: char) -> usize {
        match self.chip(c) {
            Some(label) => label.chars().count(),
            None => char_width(c),
        }
    }

    /// The cursor's row and column at `width`, as the box shows the text
    /// (tentative words after the cursor).
    pub fn position(&self, width: usize) -> (usize, usize) {
        let text = self.laid_out();
        let rows = self.rows_of(&text, width);
        let last = rows.len() - 1;
        for (row, indices) in rows.into_iter().enumerate() {
            let mut column = 0;
            for index in indices {
                if index == self.cursor {
                    return (row, column);
                }
                if text[index] != '\n' {
                    column += self.width_of(text[index]);
                }
            }
            // After the last character. A full row leaves the cursor no
            // cell, so it starts the next row, where the next character goes.
            if row == last {
                return if column >= width {
                    (row + 1, 0)
                } else {
                    (row, column)
                };
            }
        }
        (0, 0)
    }

    /// ↑ (`up`) or ↓ within the text; false at its first or last row (the
    /// history's turn).
    pub fn vertical(&mut self, up: bool, width: usize) -> bool {
        let rows = self.rows(width);
        let (row, column) = self.position(width);
        let target = if up {
            match row.checked_sub(1) {
                Some(row) => row,
                None => return false,
            }
        } else if row + 1 < rows.len() {
            row + 1
        } else {
            return false;
        };
        let goal = *self.goal.get_or_insert(column);
        let row = &rows[target];
        let mut at = row.first().copied().unwrap_or(self.text.len());
        let mut used = 0;
        for &index in row {
            at = index;
            if self.text[index] == '\n' {
                break;
            }
            let w = self.width_of(self.text[index]);
            if used + w > goal {
                break;
            }
            used += w;
            at = index + 1;
        }
        // Past the end of a row the width wrapped: at its last character,
        // not the next row's first.
        let wrapped = row.last().is_some_and(|&i| self.text[i] != '\n')
            && rows
                .get(target + 1)
                .is_some_and(|next| next.first() == Some(&at));
        if wrapped {
            at -= 1;
        }
        self.cursor = at.min(self.text.len());
        true
    }

    /// The previous message from the history; false when there is none.
    pub fn history_back(&mut self) -> bool {
        let index = match &self.recall {
            Some(recall) if recall.index == 0 => return false,
            Some(recall) => recall.index - 1,
            None if self.history.entries.is_empty() => return false,
            None => self.history.entries.len() - 1,
        };
        let draft = match self.recall.take() {
            Some(recall) => recall.draft,
            None => self.text.clone(),
        };
        let entry = self.history.entries[index].clone();
        self.set(&entry);
        self.recall = Some(Recall { index, draft });
        true
    }

    /// The next message from the history, and then what was typed before;
    /// false when not reading the history.
    pub fn history_forward(&mut self) -> bool {
        let Some(recall) = self.recall.take() else {
            return false;
        };
        if recall.index + 1 < self.history.entries.len() {
            let entry = self.history.entries[recall.index + 1].clone();
            self.set(&entry);
            self.recall = Some(Recall {
                index: recall.index + 1,
                draft: recall.draft,
            });
        } else {
            let draft: String = recall.draft.iter().collect();
            self.set(&draft);
        }
        true
    }

    /// The box's lines at `width` as spans (a chip in `chip` style, the
    /// tentative words in `tentative` style).
    pub fn lines(
        &self,
        width: usize,
        text: Style,
        chip: Style,
        tentative: Style,
    ) -> Vec<Vec<Span<'static>>> {
        let shown = self.laid_out();
        let dictated = self.cursor..self.cursor + self.tentative.len();
        self.rows_of(&shown, width)
            .into_iter()
            .map(|indices| {
                let mut spans = Vec::new();
                // The run of characters in one style, and that style.
                let mut run = String::new();
                let mut style = text;
                for index in indices {
                    let c = shown[index];
                    if c == '\n' {
                        continue;
                    }
                    let (piece, piece_style) = match self.chip(c) {
                        _ if dictated.contains(&index) => (c.to_string(), tentative),
                        Some(label) => (label, chip),
                        None => (c.to_string(), text),
                    };
                    if piece_style != style && !run.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut run), style));
                    }
                    style = piece_style;
                    run.push_str(&piece);
                }
                if !run.is_empty() {
                    spans.push(Span::styled(run, style));
                }
                spans
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        Editor::new(History::load(None))
    }

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            editor.insert(c);
        }
    }

    #[test]
    fn readline_keys_edit_the_line() {
        let mut e = editor();
        typed(&mut e, "hello brave world");
        e.kill_word();
        assert_eq!(e.text(), "hello brave ");
        e.yank();
        assert_eq!(e.text(), "hello brave world");
        e.home();
        e.word_right();
        e.kill_end();
        assert_eq!(e.text(), "hello");
        e.left();
        e.kill_start();
        assert_eq!(e.text(), "o");
        e.end();
        assert!(!e.delete());
        e.home();
        assert!(e.delete());
        assert!(e.is_empty());
    }

    #[test]
    fn up_and_down_move_through_lines_then_the_history() {
        let mut e = editor();
        e.history.entries = vec!["first".into(), "second".into()];
        typed(&mut e, "draft");
        assert!(!e.vertical(true, 20));
        assert!(e.history_back());
        assert_eq!(e.text(), "second");
        assert!(e.history_back());
        assert_eq!(e.text(), "first");
        assert!(!e.history_back());
        assert!(e.history_forward());
        assert!(e.history_forward());
        assert_eq!(e.text(), "draft");
        assert!(!e.history_forward());
        e.set("one\ntwo");
        assert_eq!(e.position(20), (1, 3));
        assert!(e.vertical(true, 20));
        assert_eq!(e.position(20), (0, 3));
        assert!(!e.vertical(true, 20));
        assert!(e.vertical(false, 20));
        assert_eq!(e.position(20), (1, 3));
    }

    /// The box's rows at `width`, as text.
    fn rows(e: &Editor, width: usize) -> Vec<String> {
        e.rows(width)
            .iter()
            .map(|row| row.iter().map(|&i| e.text[i]).collect())
            .collect()
    }

    /// A row filled exactly, by narrow or wide characters, leaves the cursor
    /// at its end no cell, so it is at the start of the next row.
    #[test]
    fn the_cursor_after_a_full_row_is_on_the_next() {
        let mut e = editor();
        typed(&mut e, "See the wrappe");
        assert_eq!(rows(&e, 14), ["See the wrappe"]);
        assert_eq!(e.position(14), (1, 0));
        typed(&mut e, "d");
        assert_eq!(e.position(14), (1, 7));
        let mut e = editor();
        typed(&mut e, "日本語の文章で");
        assert_eq!(e.position(14), (1, 0));
        e.left();
        assert_eq!(e.position(14), (0, 12));
    }

    /// The box wraps at spaces: a word that crosses the edge moves whole
    /// to the next row as it is typed, comes back as it is deleted, and
    /// the cursor stays where it was in the text. Only a word longer than
    /// a row breaks inside.
    #[test]
    fn the_box_wraps_whole_words_and_the_cursor_follows() {
        let mut e = editor();
        typed(&mut e, "See the wrapp");
        assert_eq!(rows(&e, 14), ["See the wrapp"]);
        assert_eq!(e.position(14), (0, 13));
        typed(&mut e, "ed");
        assert_eq!(rows(&e, 14), ["See the ", "wrapped"]);
        assert_eq!(e.position(14), (1, 7));
        e.backspace();
        e.backspace();
        assert_eq!(rows(&e, 14), ["See the wrapp"]);
        assert_eq!(e.position(14), (0, 13));
        // Typing inside the word that moved: the cursor stays in it.
        typed(&mut e, "ed");
        for _ in 0..3 {
            e.left();
        }
        assert_eq!(e.position(14), (1, 4));
        e.insert('X');
        assert_eq!(rows(&e, 14), ["See the ", "wrapXped"]);
        assert_eq!(e.position(14), (1, 5));
        e.backspace();
        // Before the word, after the space the row ends with: the start of
        // the next row.
        e.word_left();
        assert_eq!(e.position(14), (1, 0));
        // ↑ and ↓ keep the column across the rows.
        e.end();
        assert!(e.vertical(true, 14));
        assert_eq!(e.position(14), (0, 7));
        assert!(e.vertical(false, 14));
        assert_eq!(e.position(14), (1, 7));
        // A word longer than a row breaks inside. Spaces at the edge stay
        // on their row, and the next row starts with the word.
        let mut e = editor();
        typed(&mut e, "a abcdefghijklmn");
        assert_eq!(rows(&e, 6), ["a ", "abcdef", "ghijkl", "mn"]);
        let mut e = editor();
        typed(&mut e, "abcde  fg");
        assert_eq!(rows(&e, 5), ["abcde  ", "fg"]);
        e.left();
        e.left();
        assert_eq!(e.position(5), (1, 0));
        // A line break starts a row, and a word is wrapped within its line.
        let mut e = editor();
        typed(&mut e, "one\nMethodology here");
        assert_eq!(rows(&e, 12), ["one\n", "Methodology ", "here"]);
        // A chip is a word of its own.
        let mut e = editor();
        typed(&mut e, "see ");
        e.paste(&"x\n".repeat(20));
        typed(&mut e, "now");
        assert_eq!(e.shown(), "see [pasted 20 lines]now");
        assert_eq!(e.rows(20).len(), 2);
        // The second row is full, so the cursor after it starts a third.
        assert_eq!(e.position(20), (2, 0));
    }

    #[test]
    fn a_long_paste_is_a_chip_that_is_sent_whole() {
        let mut e = editor();
        typed(&mut e, "see ");
        let paste = (1..=240)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        e.paste(&paste);
        assert_eq!(e.shown(), "see [pasted 240 lines]");
        e.backspace();
        assert_eq!(e.shown(), "see ");
        e.paste(&paste);
        e.paste("\nshort");
        assert_eq!(e.shown(), "see [pasted 240 lines]\nshort");
        assert_eq!(e.take(), format!("see {paste}\nshort"));
        assert!(e.is_empty());
        // A paste typed as keys is folded once it ends.
        let start = e.cursor();
        for c in paste.chars() {
            e.insert(c);
        }
        e.fold_paste(start);
        assert_eq!(e.shown(), "[pasted 240 lines]");
    }

    #[test]
    fn the_history_file_is_the_line_editors() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.txt");
        std::fs::write(&path, "#V2\n/help\nline one\\nline two\nback\\\\slash\n").unwrap();
        let mut history = History::load(Some(path.clone()));
        assert_eq!(
            history.entries,
            ["/help", "line one\nline two", "back\\slash"]
        );
        history.add("new\nentry");
        history.add("new\nentry");
        let again = History::load(Some(path.clone()));
        assert_eq!(again.entries.len(), 4);
        assert_eq!(again.entries[3], "new\nentry");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("new\\nentry\n")
        );
    }
}
