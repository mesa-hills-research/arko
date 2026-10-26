# Session names

A new session is named by the first words of its first message, cut at a word within 30 display columns
("Fix the flaky scroll test in…"). Arko can ask a model for a better name instead. The name you give
a session with a rename always comes first, and a branch keeps "Branch of …".

## Naming by a model

`/session-names` opens a menu with two choices, First words and Named by a model. With a model, the
menu lists four ways to ask one. Space turns a way on or off, Shift+↑ and Shift+↓ move it, and Enter
saves.

| Way | What it uses |
|---|---|
| `claude-plan` | Haiku 5.5 in Claude Code, on your Claude plan |
| `chatgpt-plan` | GPT-6 Luna through your ChatGPT login |
| `claude-key` | Haiku 5.5 with your Anthropic API key, billed per token |
| `openai-key` | GPT-6 Luna with your OpenAI API key, billed per token |

Choosing a model turns on the two plan ways. The key ways start off, since they bill per token. Each
way asks at low effort for a name of 2 to 5 words in the message's language.

The first message goes to the first way that works. A way that can't run (Claude Code isn't installed
or signed in with a Claude plan, no login, no key), fails, gives no usable name, or sends nothing for
two minutes hands over to the next. A way that is still working is never cut off, so what it already
used isn't wasted. When none works, the first words stay the name. Naming runs in the background
after the first message, so the turn never waits for it, and it goes on after the session closes. It
stops when the session is deleted (no further way is asked), when the first message is edited (the new
one is named instead, also if the session was closed and opened again meanwhile), and when Arko quits,
which also ends the Claude Code it runs. A name is shown only for the first message it was made for.
A way turned off is not sent the message, also while Claude Code is still being checked. Only the first
2,000 characters of the first message are sent.

The Claude plan way asks Claude Code with the variables that could bill it or sign it in elsewhere
(`ANTHROPIC_*`, Bedrock, Vertex and Foundry settings) empty, and runs only when Claude Code is signed
in with a Claude plan. A name the model gives has one to five words, at most 40 display columns.

The menu opens at once and says under each way what it lacks as soon as that is checked. The words
work in any front end:

| Command | What it does |
|---|---|
| `/session-names` | Opens the menu. Outside the full-screen view, says the setting. |
| `/session-names words` | Names sessions by their first words. |
| `/session-names WAY...` | Names them by a model, trying the ways in the order given. |

The setting is the list of ways in `session-names.json` in Arko's folder, in the order they are
tried. A missing or empty list means the first words. Arko reads it again before it asks each way,
so a way turned off is not asked, and a name from a way turned off meanwhile is not kept.

## In an app

`arko::session_names` has `load` and `save` for the list, `Way` with its words and labels, `arranged`
for the order a menu shows, `missing` for what each way lacks now, and `short_name` for the cut. A name
is cut by what a user sees as one character, so an emoji or a flag is never split. `missing` runs
`claude` to check Claude Code, so call it off a UI thread. A session's name comes from
`sessions::list` and the status as before. A session an app runs with `spawn` keeps its thread for a
name still being asked after the session closes. Call `session_names::stop_all` when the app quits. It
ends every naming under way and the Claude Code each runs, and returns when they have ended, or after
two seconds.
