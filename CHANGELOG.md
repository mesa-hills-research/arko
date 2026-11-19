# Changelog

## Unreleased

### Editing files

- The agents change files with `apply_diff` in place of `edit_file`. One call makes several
  changes to a file, a SEARCH/REPLACE block for each, so an edit of many places takes one call
  instead of one a place. Every block applies or none does, and when one doesn't, the error says
  which block and why, and the file is left as it was.
- A block's search lines must match whole lines of the file exactly and be found once, or with
  `replace_all`, everywhere. The file is replaced in one step and keeps its permissions, owner,
  group, ACL and other properties. A file with one that can't be kept, such as a second hard
  link, is refused unchanged.
- An edit's row says how many edits it made ("2 edits") and opens to the changes. The line prompt
  says how many edits a call of several makes, and in Quiet each block counts as an edit ("Made 3
  edits to app.rs").
- `run_code` calls `apply_diff` in place of `edit_file`.
- A session saved with `edit_file` resumes with it.

### Code only

- `--tools code` (or `ARKO_TOOLS=code`) offers the agents `run_code` in place of `read_file`,
  `list_files`, `write_file`, `apply_diff`, `shell`, `index_search` and `index_status`, which they
  then reach only from code. `run_code`'s description declares each of them with its whole
  description and arguments, and a direct call of one is answered as unavailable. `index_create`,
  jobs, terminals, monitors, workers, questions, messages, notes, skills and MCP servers stay
  direct.
- A conversation or worker saved with one `--tools` keeps its tools and `run_code`'s description
  when the session resumes with another.
- The notice of what saved conversations lack names `run_code` and the direct calls a saved `code`
  run lacks, and says that a conversation Claude Code compacts keeps the tools it started with,
  where it promised them at the next handover.

## 0.23.1 (2026-11-18)

### The transcript

- "N more lines" opens below the lines it showed. Opening a long output or a file's lines used to
  move the preview and everything above it up by the lines that came. Closing keeps the preview's
  lines on their rows, from the mouse and the keyboard alike, and the line numbers keep their
  column as a content opens.
- A worker's report shows once in its lead's transcript. A turn the report started opened with
  "r1 reported" just under the report's own row. That line now says only what the workers' rows
  don't, and a turn whose rows say it all has none. A reopened session keeps the lines.
- A wait on a background job reads as a wait ("Waiting for job 4d2c1a9b"), as a wait for workers
  does, where it read as a check of the job.

### The web page

- On a phone, a running row such as "Ran ls · running tail -3" fits the screen, with its Move to
  background button whole. Its words give way first, then its outcome.
- On a phone, the room the keyboard took no longer stays blank below the end after a message is
  sent from a little above it.

### Background jobs

- An agent's `terminal_control wait` lasts as long as its `wait_ms` asks, up to the longest wait
  that ends before the agent's prompt cache lapses (4 min 30 s for a 5-minute cache, 54 min for an
  hour). It used to end after 30 seconds, so agents asked again and again or ran `sleep` in the
  shell. The agents are told to wait for a job this way.
- Every wait of a request, `wait_for_agents` and `wait_for_message` included, ends before the
  prompt cache lapses, counted from a moment no later than the request's use of the cache. For a
  Claude Code agent it counted from the reply, so a slow reply could leave a wait running past the
  cache.

### Workers

- Workers linked with each other (started in one group, with `peers` or with `link_agents`) hear
  when one of them finishes, fails or is stopped. A finish gives the first lines of the report, and
  a failure or a stop gives the reason. A worker waiting for a peer that had ended used to wait on,
  since nothing told it. The note reaches a worker at work at its next step and ends its
  `wait_for_agents`, and one that has ended reads it when it runs again, without being started.
- When a session resumes, a worker whose monitors, moved calls or commands stopped with Arko is
  told which, as the main agent is. A worker's monitor, such as a reviewer's watch of the commits
  it reads, used to stay gone after a restart without a word.
- `/peer-messages N` sets how many messages two linked workers may send each other before their
  lead writes to either, 200 by default. It was 20, which stopped a reviewer in the middle of a
  review. A change applies at once before the conversation starts, and otherwise when the session
  opens again.

### Costs

- Token counts read in the largest unit that is at least 1, with one decimal below 100 (`820`,
  `8.2k`, `940k`, `32.8M`, `11.2B`), and dollars have their thousands marked and their cents
  (`$4,007.05`). `/cost`, the status line in the terminal and on the web page, the turn's footer, a
  worker's cost, and the handover, compaction and cache notices all say them this way. `/cost` used
  to print `11151855k input and 32800k output tokens` and `$4007.05`.
- `/cost` puts each line one step in under the line it belongs to, and says a plan's usage as a
  sentence, such as "95% of the Claude plan's week used" instead of `Claude plan 7 d 95%`.
- An amount of dollars that isn't a real number, or is a trillion dollars or more, shows as `—`.
- The turn's footer and the embeddings line of `/cost` say dollars as the rest do, with a turn
  under half a cent as `< $0.01`.

### Skills

- When `load_skill` finds no skill of the name asked for, its answer ends with a note that a skill
  under Active Skills in the agent's instructions, such as `prose`, is loaded already. Agents tried
  to load `prose` and took the answer that it wasn't found for a broken setup.
- A new built-in skill, `worker-choice`, loads on demand like `ponytail` and `ui-taste`. It covers
  billing, the model for a kind of work, effort and profiles, and has the agent ask you before it
  starts a worker billed per token, even when the session's worker billing would start it without
  asking. `--no-builtin-skill worker-choice` leaves it out.

### Dependencies

- arko_ai 0.3.1, arko_agents 0.9.1 and arko_workflows 0.10.1, for the longer waits.

### For apps

- `remote::Item` has `number_width`, the widest line number of a content's whole, and
  `beside_rows`, what a turn's opening line says beside the workers' rows. `Feed::turn_opening`
  takes the second.

## 0.23.0 (2026-11-18)

### Models

- GPT-6.2 Sol and GPT-6.2 Astra, on the ChatGPT login or an OpenAI API key. `sol` and `astra` now
  name them, and a ChatGPT login or an OpenAI key starts a new session on GPT-6.2 Sol. GPT-6.1 Sol
  and GPT-6 Astra stay available by their IDs.
- Gemini 4 Carbon, Argon and Hydrogen, as `carbon`, `argon` and `hydrogen`. `gemini` now names
  Carbon, Google's most capable, and a Gemini key starts a new session on it. `flash` names
  Hydrogen, which Google gives as the upgrade from Gemini 3.8 Flash. Hydrogen takes the xhigh
  effort.
- On the ChatGPT login, GPT-6.2 Sol, GPT-6.1 Sol and GPT-6.1 Terra hand over at 475k, below the
  500k the login now takes for them. The other OpenAI models still hand over at 250k.
- A skill for `sol` or `astra` also reaches OpenRouter's `openai/gpt-6.2-sol-pro` and
  `openai/gpt-6.2-astra-pro`, the same models in pro reasoning mode.

### Code mode

- `--tools both` (or `ARKO_TOOLS=both`) offers the agents `run_code` beside their tools. It runs
  JavaScript that calls read_file, list_files, index_search, index_status, shell, write_file and
  edit_file, and returns only what the code returns, so loops and filtering over many files stay
  out of the conversation. `classic`, without it, stays the default for now, and a resumed session
  takes the setting for the conversations and workers that start after.
  [Code mode](docs/code-mode.md) says more.
- Each call from code passes the same checks as a call of the model's, meaning outside access, the
  command guard, the path and approval policies and every hook. A call that would wait for
  approval is refused at once, and the third refused call ends the run.
- The code runs in a process of its own, Arko started again as a code runner, with none of Arko's
  environment or keys. The operating system holds it to its memory and processor time, so a
  runaway script or a fault in the engine ends that process and never Arko. A run may compute for
  30 seconds, hold 64 MB, make 500 tool calls and take timeout_seconds (5 minutes by default, at
  most an hour). A failed run says its line and what it had changed.

### Search indexes

- A lexical search is now an exact search of names and words. It lists every chunk that holds the
  query as written (case-sensitive, or with `ignore_case=true` in any case of the ASCII letters),
  grouped by file, with each chunk's lines and scope and the lines that hold the query. It used to
  rank the chunks that held any word of the query and show the top 10 whole, so `search_blocking`
  also found every chunk with `search`, and a list of callers lost all but the first few.
- Lexical search returns 100 results by default and any number asked for, and `exhaustive=true`
  lists every match. A list cut short says how many there are in all ("100 of 340 results").
  Semantic and hybrid search keep 10 by default and at most 50. `ARKO_INDEX_LIMITS` (such as
  `lexical=50,semantic=5`) changes each mode's default.
- The agents use lexical search for every use of a name or an exact string, such as a function's
  callers or an error message, and hybrid for a name's definition and for words in docs. rg stays
  for regexes, and for punctuation alone and text inside a longer name, which lexical search
  doesn't find.
- `arko index search` takes `--exhaustive` and `--ignore-case`, and lists lexical matches the same
  way.
- A lexical list that is cut ends its note with the cursor of its next page, such as
  `after="src/a.rs:804:5"` (the path, line and column where its last result starts, with any `%`,
  `"`, single or curly single quote, control character or leading space of the path written as `%`
  and hex digits). Calling with `after` lists the matches that follow it, `limit` of them or all of
  them with `exhaustive=true`, and says where the page lies in the list ("101-200 of 340
  results"). A page ends after every result at its last place, so a search of indexes that hold
  the same file can run over `limit` by up to one less than the number of indexes, and the note
  counts the next page as it will be. Nothing is kept between calls, so a page starts after that
  place in the files as they are now. `arko index search` takes `--after`, and the command it
  suggests for the next page quotes the cursor for POSIX shells and PowerShell.
- A lexical search stays within its `language`. When nothing in it matches, the search says so,
  where it used to list the matches of every language. Semantic and hybrid search still do.

### Dependencies

- arko_ai 0.3.0, arko_agents 0.9.0 and arko_workflows 0.10.0, for the new models and the calls
  code mode makes.
- rquickjs 0.14.0, behind the `code-mode` feature, on by default. It needs a C compiler.

### For apps

- An app that also names arko_ai's types takes arko_ai 0.3, where a `match` on
  `GeminiThinkingLevel` needs a `_` arm.
- `Options::tools` chooses `code_mode::Tools::Classic` or `Both`. A program that sets `Both` calls
  `arko::code_mode::run_if_asked()` first in `main`, and an app built with
  `default-features = false` adds the `code-mode` feature for it.

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
