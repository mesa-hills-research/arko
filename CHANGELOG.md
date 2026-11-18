# Changelog

## Unreleased

### Code mode

- `--tools both` (or `ARKO_TOOLS=both`) offers the agents `run_code` beside their tools. It runs
  JavaScript that calls read_file, list_files, index_search, index_status, shell, write_file and
  edit_file, and returns only what the code returns, so loops and filtering over many files stay
  out of the conversation. `classic`, without it, stays the default for now, and a resumed session
  takes the setting for the conversations and workers that start after.
- Each call from code passes the same checks as a call of the model's, meaning outside access, the
  command guard, the path and approval policies and every hook. A call that would wait for
  approval is refused at once, and the third refused call ends the run.
- The code runs in a process of its own, Arko started again as a code runner, with none of Arko's
  environment or keys. The operating system holds it to its memory and processor time, so a
  runaway script or a fault in the engine ends that process and never Arko. A run may compute for
  30 seconds, hold 64 MB, make 500 tool calls and take timeout_seconds (5 minutes by default, at
  most an hour). A failed run says its line and what it had changed.
- A program built on the library that sets `Options::tools` to `both` calls
  `arko::code_mode::run_if_asked()` first in `main`.

## 0.22.2 (2026-11-14)

### The web page

- Opening a fold no longer moves the lines above it, as in the terminal since 0.22.1. An open
  item has its 12 px gap below it only, and a running command in the Full view stacks with the
  calls around it until its output shows.

## 0.22.1 (2026-11-14)

### The transcript

- Opening a fold no longer moves the lines above it. An open item has its blank line below it
  only, so the item and what follows grow downward, as a file tree does. A command's output in the
  Full view and a thought shown whole grow the same way as they come.

### Skills

- The built-in arko-taste skill is now ui-taste, for any interface, graphical or terminal. It puts
  the fewest layout shifts first and has sixteen checks. Agents load it when a task calls for it,
  as before. `--no-builtin-skill arko-taste` and `ARKO_BUILTIN_SKILLS_OFF=arko-taste` now get the
  answer for a name that isn't a built-in skill, so name `ui-taste` there.

## 0.22.0 (2026-11-14)

### The transcript

- A worker's start is one row, in Full, Quiet and Messages, where it drew the start call and then
  the worker's line under it. The row names the task until the worker has an ID, then says how the
  worker stands (queued, running, failed or stopped), its tool calls and its time, and opens to the
  task. The words stay in place as the ID comes. On a narrow screen the title gives way first, so
  the state, the tool calls and the time always show.
- A message that sets a finished worker going again is that worker's row from then on, and only a
  worker's latest row says how it stands. A worker's line still shows where no call of its turn
  stands for it, with the · marker of a row, and it stays while another start or message of its
  turn runs.
- A start that failed with nothing to open says how it ended, such as "stopped", and a start with
  an empty task says "Start a worker".
- Counts of one take the singular. A command of two lines ends "(1 more line)" where it said
  "(+1 lines)", and the same goes for "1 chunk in 1 file", "1 day ago" and the other counts, in the
  terminal, on the web page and in the apps.

### The web page

- The page follows its end by the terminal's rules. It keeps your choice to follow or to read,
  and following never moves a line down. When the end gets shorter, blank space stays below it
  until new output fills it. A scroll up that moves a line stops following, and a scroll down that
  brings the end into view, End, Esc, the marker and sending follow again.
- "Latest" is now the marker "↓ N new lines", counting the lines below the screen, or "↓ end".
- A fold keeps the clicked line where it was, and a panel, a menu or the message box that opens
  and closes puts every line back. The page keeps its place when anything above the screen
  changes.
- The wheel, a touchpad and the keys scroll the transcript with no glide and no bounce.
- A question card keeps its line as its record opens under it, inline code no longer makes its
  line taller, and a line of "-" streaming under a paragraph no longer shows as a heading for a
  moment.

### Dictation

- In English, dictation writes spoken symbols as code, so "dash dash force" comes out as `--force`.
  It also writes a dictated path as the file of the session's workspace it names, so "source slash
  engine dot rs" comes out as `src/engine.rs`. Prose stays as said. `workspace_paths` in
  dictation.json turns both off, and [Dictation](docs/dictation.md) says more.

### Dependencies

- Telmo 0.5.0, for spoken symbols and paths.

### For apps

- `remote::Item` has two new public fields, `sets` and `task_title`, so code that builds one field
  by field needs them. `Item::new` gives them.
- `remote::plural` and `remote::noun` give a count's singular or plural, as the terminal and the
  page use them.
- `dictation::settings::Settings` has the new `workspace_paths` field.

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
