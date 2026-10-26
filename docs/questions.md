# The agent's questions

The main agent asks you with `ask_user` when it needs a decision only you can make. Its questions
don't stop its work. It asks, goes on with what doesn't depend on the answers, and reads them when
they come. A question may come with suggested answers and the default the agent goes with if you
don't answer. When the agent can't go on at all without an answer, it asks with `wait`, and waits.

Arko asks you too, before the agent saves a worker profile for the project or for every project.
That approval shows the profile's name, where it is kept, its model and effort, what it is for and
its instructions, with Save and Don't save. Only Save saves it. Anything you type instead goes back
to the agent as changes to make, and it asks again with the changed profile. Profiles the agent
saves together come in one card, with a choice for each.

## Answering in the full-screen view

New questions open the answer panel above the message box when you aren't typing. While you type,
a chip says "Arko asks 2 questions" instead, and Alt+Q opens the panel. The questions of one ask are
numbered 1, 2, 3, and a question asked alone has no number.

| Key | In the answer panel |
|---|---|
| 1 to 9 | Pick a suggested answer, or "You decide" after them (the agent goes with its default) |
| ↓, Tab | From a question's answers to its Other field, then on to the next question, and from the last back to the first |
| ↑, Shift+Tab | Back |
| Typing | An answer of your own (Other). A question without suggested answers takes typing at once |
| PgUp, PgDn | Scroll a long question, such as a profile's instructions |
| Enter | Send what you filled in. The questions you left out stay open |
| Esc | Answer in a message instead. The questions go with your next message ("Replying to 2 questions"), and Backspace in the empty message box takes them off |
| Ctrl+N | Not now. The agent goes on with its defaults |

Open questions you haven't answered show as a quiet chip, "2 open questions", where Alt+Q and
Ctrl+N work too. Alt+Q works over the settings and other windows, which come back when the panel
closes. The terminal's bell rings for new questions while its window isn't focused. On a Mac, Alt+Q
is Option+Q, which needs Option to work as Alt. In the Terminal app, turn on Use Option as Meta key.

A question shows in one place at a time. While it is open it is in the answer panel, and the
transcript has one quiet line where it was asked ("Arko asked 2 questions"). Click that line to
open the panel. Once a question is answered, closed or dismissed, the line shows it with what became
of it. It can be answered, answered in your message, closed by the agent (it found the answer or
the question no longer matters), or dismissed.

## What the agent reads

Each question has an id that is unique in the session (`q1`, `q2`, ...), which the agent uses and
you don't see. Each time you send answers, the agent gets one note. An idle agent starts a turn with
it, as it does with a worker's report.

```
The user answered q3 and q4.
q3 Tabs or spaces? Spaces
q4 Push now? You decide (you said you'd assume: not yet)
Still open: q5.
```

A message you send with the questions attached starts with "(This message answers q3 and q4.)".
Any other message ends with the questions still open, "(Still open: q5.)". "Not now" tells the
agent "The user dismissed q5." A resumed session's transcript shows your messages as you wrote
them.

The session keeps its questions in its folder (`questions.json`), with the answers the agent hasn't
read yet, so open questions are open again when you resume it, and an answer given before you quit
reaches the agent with its next turn. After a handover or a compaction the agent is reminded of the
questions still open.

Workers ask the agent that started them, not you, with the same ids, suggested answers and defaults.

## The web page and apps

The web page's status (`Reading::questions`) lists the open questions with their ids, numbers,
suggested answers, `multiple`, default and state, and each ask's feed item (`Item::questions`) has
all of its questions, updated as they change. `Activity::question` is the card the agent waits on,
if any.

| Request | Body |
|---|---|
| `POST /api/questions/answer` | `{"answers": [{"id": "q3", "choices": ["Spaces"]}, {"id": "q4", "you_decide": true}, {"id": "q5", "text": "main"}, {"id": "q6"}]}` |
| `POST /api/questions/dismiss` | `{"ids": ["q5"]}` |
| `POST /api/message` | `{"text": "...", "answers": ["q3", "q4"]}` answers them in a message |

Both answer a 409 when none of the questions is open. An answer names every question its panel
shows, an unanswered one by its id alone (`{"id": "q6"}`), and one with nothing filled in gets a
400.

An app does the same with `Input::AnswerQuestions`, `Input::DismissQuestions` and
`Message::answers`. The types are in `arko::questions`.
