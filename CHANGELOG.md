# Changelog

## 0.24.0 (2026-11-18)

### Editing files

- `edit_file` makes several changes to a file in one call, an edit for each, so an edit of many
  places takes one call instead of one a place. The edits apply in order, each to the text the
  ones before it left, and every edit applies or none does. When one doesn't, the error says which
  and why, and the file is left as it was.
- An edit's row says how many edits it made ("2 edits") and opens to the changes. The line prompt
  says how many edits a call of several makes, and in Quiet each counts ("Made 3 edits to
  app.rs").
- An edit with `replace_all` counts each place it changed, "5 edits" on its row and "Made 5 edits
  to app.rs" in Quiet. It said "5 replacements" and "Made 1 edit". A Quiet row of several edits
  counts only those that went in ("Made 2 edits to a.rs ✗"), and one whose edits all failed reads
  "Couldn't edit a.rs ✗".
- A failed edit's row reads "edit src/app.rs · ✗ Failed", and opening it shows why the edit
  didn't apply. A failed call's error no longer says "Execution failed:" twice.
- On a narrow screen a file's row keeps the file's name whole. Its outcome's words give way first,
  then its folders ("edit …/app.rs").
- A path that leaves the working directory through a symbolic link is refused however it's
  written. This covers Arko's file tools, Claude Code's Read, Write, Edit, NotebookEdit, Glob and
  Grep, and a worker's workspace while outside access is off. It's checked when the call is made,
  so shell commands and links changed afterwards aren't covered.

### Code only

- `--tools code` (or `ARKO_TOOLS=code`) offers the agents `run_code` in place of `read_file`,
  `list_files`, `write_file`, `edit_file`, `shell`, `index_search` and `index_status`, which they
  then reach only from code. `run_code`'s description declares each of them with its whole
  description and arguments, and a direct call of one is answered as unavailable. `index_create`,
  jobs, terminals, monitors, workers, questions, messages, notes, skills and MCP servers stay
  direct.
- A conversation or worker saved with one `--tools` keeps its tools and `run_code`'s description
  when the session resumes with another.
- The notice of what saved conversations lack names `run_code` and the direct calls a saved `code`
  run lacks, and says that a conversation Claude Code compacts keeps the tools it started with,
  where it promised them at the next handover.
- A `run_code` call has a row of its own, worded from what its code is for ("Running code to
  count TODO comments per crate", then "Ran code to …"). It opens to its code, highlighted, then
  one line of what its calls did ("Listed 1 folder and read 31 files") and a line with ✗ for each
  call that failed, in the order they came, and the result. While it runs, the row counts its
  calls and names the one that runs. Full has each call's line after the code. It read "Used
  run_code" and opened to its result alone. A reopened session shows the row, the code and the
  result.

### The transcript

- A message sent while the agent works no longer jumps as "Working…" comes and goes, or when the
  agent reads it. It waits under the agent's output with "Working…" below it, in the terminal and
  on the web page, and only output coming above it moves it. It used to wait below "Working…", so
  it moved down and up with it and swapped rows with it when the agent read it.
- An answer, or words sent with an approval, now shows after a message you typed before it. It
  used to show above that message.
- A long call (a command, a wait, an MCP tool's call) has a Quiet row of its own while it runs,
  which says what it does. When it ends it joins the row before it, unless something came below it
  meanwhile, so nothing above what you're reading moves. A row of several calls says what runs
  while one runs, and what they did once all ended.
- A long line of code, of a diff or of a job's output that wraps goes on under its own indent, on
  the page and in the terminal, where it went on at the edge. The page draws a tab 4 columns wide,
  as the terminal does.
- An index search's Quiet row names the index alone, and opening it shows the query once, then the
  results.
- In the terminal a failed row paints only its ✗ in the error's colour, as the page does, and a
  failed Quiet row's ✗ stands a column in from the edge.
- A row's line gives way to its width by one rule, in the terminal and on the web page. The
  marker, a worker's name, ✓ or ✗, how long a call has run, the spinner, Move to background and
  Stop stay, each in the room it keeps from the start of the call's run or its job's, so the words
  don't change as a button comes or the time grows. How a row runs, such as a code run's count of
  calls, shows only in the room the words leave. Then the words are cut where a word ends, down
  to their first 12 columns, before what happened goes, so a narrow wait reads "Waited for job… ·
  failed (exit 1)". Each part goes whole. A file's line keeps its name, and its folders go a whole
  folder at a time. Lines used to be cut inside words ("runni…", "run_cod…"). A row opens to what
  its line as drawn left out, each thing once, and without the ✓ or ✗ its line shows.
- In the terminal, a call that runs over an hour says so in whole hours ("2h").
- A wait's line names a run only where its own words don't ("Waited for r4 · finished"). A wait on
  a job that failed shows ✗, and one that asks for input or was stopped shows no mark, where both
  showed ✓.
- A failed call's line says "Failed", and its content says why, once. A failed code run says it
  couldn't, where it said "Ran code to …". An edit's error on screen leaves out what it tells the
  agent to do next.
- A note that waits below a long call keeps its place as the call ends and as the agent reads it,
  and the call's row no longer regroups. A read note keeps the room its "pending" tag took.
- An opened call shows what it says once. The call as written is left out where what it opens to
  holds the rest of its line, for messages, workers' tasks, queries and commands. A long command
  whose words open with a group in parentheses keeps what fits, where it read "Ran…".
- Closing a fold no longer leaves blank under the end, in the terminal and on the web page. After
  you open a call near the end, scroll down to read it and close it, the lines come down so the
  end is at the bottom again, as before the call opened. The view follows again unless you
  scrolled up. The blank used to stay until the next scroll.

### Spacing

- The space between the transcript's rows follows one table, in the terminal and on the web page,
  so the same neighbours always stand the same distance apart. Lines of work stack, what is said
  and cards stand apart from their neighbours, and so does what follows anything open. An exchange
  starts with your message and ends with the line after a turn, with more space before the one and
  after the other. The message box stands where the next row would, so the last row has as much
  space under it as two rows of its kind have between them.
- On the page, the line after a turn sits under its turn, with the next message an exchange below
  it. It sat closer to the next message than to its own turn. The first message stands 24 px under
  the header, where it stood 40.
- On a computer, a message's Copy shows in its corner when the pointer is on it, and beside your
  message's bubble, and takes no room of its own. A reply and your message stood 26 px further
  from what followed them. On a phone, Copy stays under the message.
- A worker's card and its report, and a question's record, hold what they say 12 px from their
  frame, top and bottom, as a message's bubble does. A report's Copy stands in its corner.
- In the terminal, the end of what is said, a note waiting or anything open has a blank row under
  it before the status line, as rows have between them, and "Working…" has none, as a line of work
  doesn't. "Working…" now follows a notice or a worker's line with no blank row, as it follows a
  call. On a screen of one row, the row shows text rather than that blank row.

### The web page

- Every row and line has its marker, a triangle on one that opens and a dot on one with nothing to
  open. A row with nothing to open had an empty marker column.
- A running call's Move to background and Stop no longer make its line taller, so the rows under it
  stay where they are when it ends.
- Opening or closing a content's door keeps the lines above it in place, also when the page has
  to fetch the whole first. Closing a long content from its end keeps the door under the pointer.
- After a fold near the end, a trip to another view and back brings you back to the block you
  were reading, where it went to the end.
- A call that runs leaves no blank under it. The hidden "Working…" line under a running call
  carried the call's whole command and wrapped on a phone, up to half the screen, and that blank
  stayed once the call ended. The live line is one line now, a call that runs on the last row is
  the live line itself, and text that streams has nothing under it until it pauses.
- Rows that come below a call that still runs, such as the lead's message to a worker, put the
  line "Running cargo test…" at the end, in the terminal too, so the end says the agent works.
- With an iPhone's keyboard up, the page takes the height of what shows above the keyboard, so a
  scroll over the message box no longer shows an empty band under it.
- With an iPhone's keyboard up, the answer panel fits above it, with its field and Send in sight
  and its questions scrolling, and it's back as it was once the keyboard goes. The keyboard covered
  the field and Send. Safari also offered a contact's name for the field when a question asked for
  a name.
- A running command's details in Full show the command once.
- Move to background and Stop are their words alone until the pointer is on them, in the link
  colour, so the space under a running row is the same with them as without.
- Where there's room, a note's "pending" stands beside its bubble, so the note needs no line under
  it, waiting or read.
- A scroll to the end by the browser, with the scrollbar or a wheel, and a button's touch target on
  the last row no longer leave blank between the end and the message box.
- A selection of a command's or a job's output copies its blank lines.
- A worker's card is two lines in every state, its title and a line of its calls, time and state,
  so the cards below it stay where they are. It gained a third line with the worker's latest call
  and then its report's first line, which its "reported" row shows. Where its second line is too
  long, the parts that don't change go first (its profile and model), and it no longer wraps.
- On the web page, a worker's long title is cut where a word ends, with "…" right after it, on its
  card and in the workers' sheet. The browser cut it inside a word or left a space before the "…".
- A worker's view that has only its report, as when its work came before the page's history, opens
  at its end as other worker views do, and stays where you scroll it. On a phone it went back to its
  top when the finger lifted, and the wheel didn't scroll it.

### Questions

- Only the answer panel answers the agent's questions, or a message sent with them attached (Esc
  in the panel, "Replying to" on the page). A message from the message box used to answer the
  question the agent waited for, and the question left the panel. Now it is an ordinary message
  in the full-screen view, on the web page and in an app, and the question stays open.
- Such a message reaches an agent that waits for an answer at once, in Arko's own runtime and in
  Claude Code. A message typed while the agent asked, before it began to wait, used to sit pending
  until the answer came, and the agent then got it together with the answer.
- So does a worker's report, a monitor's alert or another update that comes while the agent waits
  for your answers. The questions stay open in the answer panel, and your answers reach the agent
  when you give them.
- Messages typed while the agent asks show in the order they were sent. The one taken as the
  answer used to come before the one typed first.
- In a worker's view, a message goes to that worker also while the main agent waits for an answer.
- At the line prompt (`--repl`), which has no answer panel, the line typed after a question still
  answers it.
- A question's card shows its questions from the start, in the terminal and on the web page, each
  with a line that says "Needs your answer". The answer, or what else became of the question, takes
  that line, so the card keeps its height. It used to be one line until a question was answered,
  then grew into a box and pushed everything below it down. A long answer is cut to its line, and a
  click on it shows it whole. A click on an open question opens the answer panel on it.
- On the web page, the answer panel, the chip of open questions and "Replying to" follow the
  questions as their cards change. A question the agent didn't wait for, and one settled elsewhere,
  reached them up to 2 s later, so the panel could open or close, and the transcript move, under
  whatever came next.
- On the web page, a new question's card no longer has "Working…" under it for a moment as it
  comes. Its line could leave blank under the card when it went.
- When a question you're answering in the panel is settled elsewhere, "That question is no longer
  open." takes the panel's place at the panel's height until you type, send or close it, and a new
  question opens in that room. It stood above the message box for 5 s, and the transcript moved
  when it went.

### The full-screen view

- No frame of the terminal profile's background shows around a theme any more. Many terminals keep
  a margin of a few pixels around the cells (VTE, 8 px in Windows Terminal) in their own
  background. While Arko runs, the terminal's background is now the theme's (OSC 11), where the
  terminal reports its own. It follows a theme the picker previews or keeps, and the `terminal`
  theme puts the terminal's own back as it was at start, as quitting, a stop, a panic, a signal and
  on Windows Ctrl+Break or closing the console do. A colour a shell's theme set over the profile's
  stays set.
- Ptyxis draws its own background behind the cells, so its margin keeps the profile's colour.
  `gsettings set org.gnome.Ptyxis disable-padding true` takes the margin away.
- Esc acts at once in terminals without the kitty keyboard protocol, such as Ptyxis, GNOME
  Terminal and tmux. It waited for the next key or click, so the web page's QR code and the other
  popups closed only then, and Esc followed by a letter came out as Alt with that letter. The
  thread that reads the terminal also kept a CPU core busy while the view was open.

### Models

- Gemini 4 Neon, as `neon`, in the Gemini group of the model menu. Google places it between
  Hydrogen and Carbon, for complex software engineering, knowledge work and science. Like
  Hydrogen, it takes the xhigh effort, and max runs it at xhigh. It needs a Gemini API key.

### Antigravity

- `agy:` models run in the Antigravity CLI on your Google AI plan, for the main agent and for
  workers. Install the CLI (1.5.14 or later), run `agy` once to sign in, and start Arko with one,
  such as `arko -m agy:neon`. The short names are `agy:neon`, `agy:argon`, `agy:hydrogen`,
  `agy:flash`, `agy:opus`, `agy:sonnet` and `agy:gpt-oss`, and worker profiles take them too. The
  CLI runs the model, and Arko gives it the instructions, the tools and the approvals.
  Antigravity's own project customizations don't run, so project instructions go in `AGENTS.md`.
  [Antigravity](docs/antigravity.md) has the details.
- The model menu has a group "Antigravity, on your Google AI plan", whose heading says when the
  CLI isn't installed or signed in. The effort step offers the levels your account has for the
  model. Arko checks the CLI only when a model needs it.
- `/cost` says how much of each of the plan's windows the session used ("3% of the Google AI
  plan's week used and 9% of its 5 hours used"), for each model group when it used more than one,
  and estimates the calls at API prices, which the plan covers. The status line and the worker
  options show the windows too.
- Workers on `agy:` models run on a plan, as `cc:` workers do, for `/worker-billing`.
- An Antigravity conversation keeps its place across follow-ups and restarts. After a turn that
  was cut off, it goes on from Arko's record of the conversation, with the messages you sent
  during it.
- The context meter shows the size of the conversation, not the sum of the turn's input.
- An agent on Antigravity writes its report before a switch of model as a reply.
- Keep-warm leaves Antigravity sessions alone, as it does Gemini and OpenRouter ones.

### Handovers

- On the ChatGPT login, GPT-6.2 Sol, GPT-6.1 Sol and GPT-6.1 Terra hand over at 428k, and the
  other OpenAI models at 222,800. Codex compacts a conversation at 90% of what the login takes
  (450k and 244,800), and the handover with its report now ends before that. They handed over at
  475k and 250k.
- A worker on an OpenAI model in a session whose own models aren't OpenAI's goes by the way the
  session reaches them. A Sol worker on the ChatGPT login handed over at 200k, the API key's size,
  and `worker_options` said so. A workers' size set with `/handover-at` could also take it past
  the 500k the login accepts, and now stops at 475k. Where the session reaches OpenAI by API key,
  such a worker is billed per token as `/worker-billing` says, where it counted as on a plan.
- `worker_options` gives the size a worker started now hands over at, with the workers' size set
  with `/handover-at` up to what each model leaves room for. It gave each model's own size.
- An OpenAI API key added with `/keys` while Arko runs, in a session that didn't reach OpenAI, is
  how its workers reach OpenAI models from then on, for their sizes, caches and billing. A worker
  on one counted as on the ChatGPT plan, so `/worker-billing plans` let it start billed per token.
- A session saved before the way to OpenAI could be chosen goes on over the ChatGPT login it used,
  and its workers go by that login. A Sol worker in one handed over at 200k.

### Workers

- While outside access is off, a worker's workspace can't be reached through a link that leads out
  of the working directory, such as `up -> ..`. The link's target became the worker's working
  directory, so its file tools and commands could change anything in it.
- A worker may have any number of workers of its own running or queued. It was held to 8.

### Approvals

- A message you send while a request waits in the approval tray reaches the agent at once, and so
  does a worker's report or another update. The request leaves the tray, nothing it asked for is
  done, and the agent asks again if it still needs it. This holds for an MCP server to add, a
  skill or worker profile to save, a kept index to remove and a worker billed per token. The agent
  used to wait for your answer first.

### Search indexes

- Each index has a lifetime, kept or a number of days. An agent that creates one gives it, and
  `arko index create --lifetime` takes the same, kept by default. A temporary index goes after
  that many days on which Arko ran without a search of it. `arko index lifetime INDEX LIFETIME`
  changes it, and so can the agents, and `arko index list` shows it.
- At its start and once a day, Arko removes the indexes whose folder was deleted, those of git
  worktrees whose branch is merged with nothing left uncommitted, and those whose days ran out.
  It leaves an index that is in use for later.
- When 0.24.0 first starts, an index with embeddings is kept, an index of a git worktree whose
  main checkout has one becomes that worktree's index, and an index searched by words alone goes
  after 30 days of use without a search.
- A search in a git worktree uses an index of its own, `<main>@<branch>`, made from the main
  checkout's with its rules and its embeddings, so only changed files are embedded again. It
  catches up on each search, and it goes with a worker's worktree, once its branch is merged, or
  after 30 days of use without a search.
- A build or an embedding run copies the embeddings other indexes of the same kind and model have
  for the same text, and says how many it borrowed.
- An agent asks you before it removes a kept index, in the approval tray, with its size and what
  its embeddings cost.
- `index_status` lists the indexes of the working directory, the session and its worktrees, with
  worktree indexes under their main index, and warns of indexes that go within 3 days of use.

### Skills

- The built-in `ponytail` skill is gone, which leaves `prose`, `ui-taste` and `worker-choice`.
  Settings that leave it out still load, with a warning.
- `prose` loads when a task calls for it, as the other built-in skills do. Its whole text was in
  every agent's instructions.
- `worker-choice` follows `/worker-billing`. It had the agent ask before every worker billed per
  token, also where the setting starts one without asking.

### Extra rules

- The extra rules for agents on small models are gone, with `--extra-rules`, `--no-extra-rules`,
  `/extra-rules` and their row in `/settings` and on the web page. A command line with either flag
  fails. A session saved with the setting still opens.

### Background jobs

- A note you type while an agent waits for a background job ends the wait, and the agent reads it
  at once. Since 0.23.1 the wait ran on to its end, up to 4 min 30 s for a 5-minute cache. A
  worker's wait ends the same way when its lead writes to it, and a lead's when a worker reports.
- A wait on a job says what it found (new output, finished, failed with its exit code, asks for
  input, stopped, timed out or nothing new), where it showed the tool's JSON. It opens to the
  job's output as text, and "new output" means output past what the agent already had. Waits on a
  job merge only with waits on the same job.
- Window titles, links and the other controls in a job's output no longer take text with them.
- Each command the agents run may take half of the computer's memory, and at least 512 MiB, so
  one that runs away ends on its own instead of taking Arko or the terminal down. On Linux the
  cap needs systemd user scopes, and the out-of-memory killer ends the agents' commands before
  Arko. On Windows the command's job object holds the cap, and macOS has none.
  `ARKO_COMMAND_MEMORY_LIMIT`, in the environment or a `.env` file, sets another cap (`8G`) or
  none (`off`). [Command memory limits](docs/command-limits.md) has the details.
- When Arko is ended by SIGTERM or SIGHUP, as when its terminal closes, or ends while stuck, the
  commands its agents started end with it. They and what they started ran on.
- A monitor can last the whole session (`lasts: session`), or stand by for workers (`lasts:
  standing`), paused while none runs and going on when one starts. Neither expires or asks to be
  kept. `/jobs` and the web page show them without a countdown, and a standing monitor that waits
  as paused.

### Version

- `arko --version` adds a short hash of the sources the build was made from, as in `arko
  0.24.0+a4f8c91d2e0b`, with `-dev` after the version for a build with uncommitted changes or a
  dependency from a local path. Each session's folder records every time it opened, with the
  build, in `launches.jsonl`, and its checkpoints name the build.

### Dependencies

- arko_ai 0.4.0, arko_agents 0.10.0, arko_workflows 0.11.0 and arko_index 0.3.3, for Gemini 4
  Neon, Antigravity, `edit_file`'s edits, the waits that notes end, the tools reached only from
  code, the workspace link check, the memory cap, the monitors and the borrowed embeddings.
- termelon 0.2.2, for the terminal's background and the Esc key.

### For apps

- An app that also names the framework's types takes arko_ai 0.4, arko_agents 0.10,
  arko_workflows 0.11 and arko_index 0.3.3.
- `remote::Act` has `edits`, how many edits an `edit_file` call of several edits, or an
  `apply_diff` call of several blocks, makes (zero for one), so code that builds an `Act` field by
  field needs it.
- An app that offers `agy:` models sets `Options::antigravity_helper` to a command that runs
  Antigravity's relay and hooks, as `arko agy-helper` does.
- `ExtraRules` is gone, with `Options::extra_rules`.
- `approvals::Answer` has `Woken`, for a request a note ended, so an exhaustive `match` on it
  needs an arm for it. `remote::Approval` has `RemoveIndex`.
- `remote::JobSummary` has `paused`, and its `expires_at` is `None` for a monitor that doesn't
  expire.
- `build_info` gives the build's ID and what `build.rs` recorded of it.
- `code_mode::CALLABLE` holds `apply_diff` as well as `edit_file`, eight tools in all.
- `code_mode::Tools` has `Code`, so an exhaustive `match` on it needs an arm for it, and
  `Tools::ALL` holds three. `Tools::calls_only_from_code` says whether a setting reaches the tools
  of `CALLABLE` only from code, and `code_mode::file_tool_definitions()` gives the definitions of
  arko_agents' file tools among them.
- `RunCodeTool::new` takes the session's tool definitions (`ToolDefinition`) in place of their
  names.

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
