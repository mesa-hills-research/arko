# Session names

A new session is named by the first words of its first message, cut at a word within 30 characters
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
or signed in, no login, no key), fails or takes more than 20 seconds hands over to the next. When none
works, the first words stay the name. Naming runs in the background after the first message, so the
turn never waits for it, and only the first 2,000 characters of the first message are sent.

The menu says under each way what it lacks. The words work in any front end:

| Command | What it does |
|---|---|
| `/session-names` | Opens the menu. Outside the full-screen view, says the setting. |
| `/session-names words` | Names sessions by their first words. |
| `/session-names WAY...` | Names them by a model, trying the ways in the order given. |

The setting is the list of ways in `session-names.json` in Arko's folder, in the order they are
tried. A missing or empty list means the first words. It applies to sessions started after the
change.

## In an app

`arko::session_names` has `load` and `save` for the list, `Way` with its words and labels, `arranged`
for the order a menu shows, `missing` for what each way lacks now, and `short_name` for the cut.
`missing` runs `claude` to check Claude Code, so call it off a UI thread. A session's name comes
from `sessions::list` and the status as before.
