# Changelog

## 0.21.0 (2026-11-14)

### The transcript

- A thought shows where it happened, as its first line in dim italic, with no label or timer. A
  click opens it whole, and one that fits on its line has nothing to open. While it streams it is
  the live line, its spinner in the markers' column, where "Working…" turns too.
- In Quiet, a thought between two calls of one row goes inside the row. The reads and edits of one
  file are one row ("Read app.rs and made 3 edits").
- Every closed row is one line at any width. Its words are cut to fit and its outcome stays whole,
  and a cut row opens to the call as written. Commands are no longer cut at 48 characters.
- Anything open has a blank line on each side. A command in Full, and a thought shown whole,
  stand apart from the moment they show, so nothing below them moves when they open.
- A job started in a terminal says "running" on its row, then how it ended. A call moved to the
  background no longer shows its job's ID.
- In Messages, a turn's count of calls opens to the turn's work from its first thought.
- A content's preview ends at a line too long to show whole, and its count says what follows.
- Numbered lists keep their numbers in one column, with room for two digits, so items 10 and up
  line up and a list doesn't move as its tenth item comes. On the web page, two-digit numbers no
  longer sit on the message's rail.
- The web page follows the same rules, with one type scale and one set of gaps. Its reconnecting
  line shows only its status.

### Scrolling in the terminal

- Scrolled up, the text at the top stays put while replies stream, folds open and close, and the
  window changes width. A selection keeps its words at another width.
- While you follow the end, no line moves down. When the end gets shorter, as when "Working…"
  gives way to a thought or a fold closes, blank rows stand below it until new output fills them.
- A menu, the approval tray or the agent list that opens and closes puts every line back where
  it was.
- A scroll up stops following as soon as it moves a line, and a scroll down moves the text up
  until the end reaches the bottom, then follows. End, Esc and "↓ N new lines" follow the end
  again. End and the marker close the blank rows below it, and Esc keeps them where the end is in
  sight.
- "↓ N new lines" counts the lines below the screen you haven't seen, across views.
- A click on a fold keeps the clicked line under the pointer, and a fold opened with the keyboard
  stays in sight. A click while following catches up with the output that came during it.
- Switching views, or looking at a worker and coming back, keeps your place.
- Where the terminal supports scroll regions, a scroll writes only the rows that came in.
- A frame wraps only the blocks it shows, so another width no longer wraps the whole session.

### The terminal view

- The view runs on termelon in place of ratatui and crossterm. A frame of the spinner alone draws
  only what moves, and a copy says it went to the terminal's clipboard only when it could have.

### Skills

- The built-in prose skill counts "word, its description" appositions ("the post, its PDF") among
  the tells of a model.

### Dependencies

- Mesquite 0.26.1, whose syntax trees for the web page's highlighting take less memory.
- rustix 1.1.6, which fixes unsound xattr calls (RUSTSEC-2026-0486).

### For apps

[Moving to 0.21](docs/migrating-to-0.21.md) lists what an app such as Arko Studio changes,
including `content::preview`'s new kind argument, `quiet::Words.news` and `quiet::counts`.
Quiet rows of many calls are now made in time growing with the calls, where it grew with their
square.
