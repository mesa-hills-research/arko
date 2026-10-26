# The agent's questions

The main agent asks you with `ask_user` when it needs a decision only you can make. Its questions
don't stop its work. It asks, goes on with what doesn't depend on the answers, and reads them when
they come. Each question has an id in its conversation (`q1`, `q2`, ...), and may come with
suggested answers and the default the agent goes with if you don't answer. When the agent can't go
on at all without an answer, it asks with `wait`, and waits.

## Answering in the full-screen view

New questions open the answer panel above the message box when you aren't typing. While you type,
a chip says "Arko asks 2 questions" instead, and Alt+Q opens the panel.

| Key | In the answer panel |
|---|---|
| 1 to 9 | Pick a suggested answer, or "You decide" after them (the agent goes with its default) |
| Letters | Type an answer of your own (Other) |
| ↑ ↓, Tab | The next question, and "Anything else" at the end |
| Enter | Send what you filled in. The questions you left out stay open |
| Esc | Answer in a message instead. The questions go with your next message ("Replying to 2 questions"), and Backspace in the empty message box takes them off |
| Ctrl+N | Not now. The agent goes on with its defaults |

Open questions you haven't answered show as a quiet chip, "2 open questions", where Alt+Q and
Ctrl+N work too. The terminal's bell rings for new questions while its window isn't focused. On a
Mac, Alt+Q is Option+Q, which needs Option to work as Alt (in the Terminal app, turn on Use Option
as Meta key).

Each ask stays in the transcript as a card that shows what became of each question. It can be
answered, answered in your message, closed by the agent (it found the answer or the question no
longer matters), or dismissed.

## What the agent reads

Each time you send answers, the agent gets one note. An idle agent starts a turn with it, as it does
with a worker's report.

```
The user answered q3 and q4.
q3 Tabs or spaces? Spaces
q4 Push now? You decide (you said you'd assume: not yet)
Anything else: Keep the old API one more release.
Still open: q5.
```

A message you send with the questions attached starts with "(This message answers q3 and q4.)".
Any other message ends with the questions still open, "(Still open: q5.)". "Not now" tells the
agent "The user dismissed q5."

The session keeps its questions in its folder (`questions.json`), so open questions are open again
when you resume it. After a handover or a compaction the agent is reminded of the questions still
open.

Workers ask the agent that started them, not you, with the same ids, suggested answers and defaults.

## The web page and apps

The web page's status (`Reading::questions`) lists the open questions with their ids, suggested
answers, `multiple`, default and state, and each ask's feed item (`Item::questions`) has all of its
questions, updated as they change. `Activity::question` is the card the agent waits on, if any.

| Request | Body |
|---|---|
| `POST /api/questions/answer` | `{"answers": [{"id": "q3", "choices": ["Spaces"]}, {"id": "q4", "you_decide": true}, {"id": "q5", "text": "main"}], "anything_else": "..."}` |
| `POST /api/questions/dismiss` | `{"ids": ["q5"]}`, or `{}` for every open question |
| `POST /api/message` | `{"text": "...", "answers": ["q3", "q4"]}` answers them in a message |

Both answer a 409 when none of the questions is open.

An app does the same with `Input::AnswerQuestions`, `Input::DismissQuestions` and
`Message::answers`. The types are in `arko::questions`.
