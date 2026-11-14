# Moving to 0.21

Arko 0.21 changes how the Quiet view words its rows and where a thought goes, where Messages puts
a turn's count of calls, and what a call's outcome says while its job runs. An app that shows the
feed, such as Arko Studio, needs these changes.

## The terminal

The full-screen view draws on [termelon](https://github.com/mesa-hills-research/termelon) in place
of ratatui and crossterm. An app built with `default-features = false` builds none of them.

## A result's content

`content::preview(kind, text)` takes the content's kind. A line over 400 characters
(`content::PREVIEW_LINE_CHARS`) ends the preview at its cut, and the door then counts the
characters that follow ("… 1,240 more characters"), the line's rest and every line after.

## Quiet rows

- `quiet::Words` has `news`, how the row's calls went in a few words, such as a test run's counts
  ("1 failed, 23 passed"). Build it with `..Default::default()`.
- `Words.text` says what the calls did and nothing more. The news, what came while a wait waited
  and "running x more" are in `news` and `note`, and `Words::said()` gives the whole line. A client
  cuts `text` to its width and shows `news` (in parentheses) and `note` whole after it.
- `quiet::COMMAND_CHARS` is gone. A command's words are its whole first line.
- A row holds the reads and changes of one file ("Read app.rs and made 3 edits"), besides the
  calls of one kind.
- A thought is in a row only between two calls of that row. Any other thought is a step of its
  own, and one the reader opened on its own line goes to `quiet::rows` as `Step::Other`, so it
  stays there.
- Open, a row of one call whose line cut its words shows the call as written, then its detail.
  A command's detail, where it has one, is the whole command, so it shows without the call as
  written. A monitor's `Act.detail` is its settings, after its command only where its words leave
  some of the command out.

## A call's job

- A call that moved to the background has the outcome "in the background", without the job's ID.
- A `terminal_start` call's outcome is "running" while its job runs, then "finished", "failed",
  "interrupted" or "stopped", and `Item.job` names its job.

## A turn's count in Messages

`quiet::counts` places every turn's count of calls, so the terminal, the page and an app agree.
Give it one `quiet::Seen` per item in the order Messages shows them.

| `Seen` | For |
|---|---|
| `Call { id, turn, status }` | A call of the agent, which its turn's count counts |
| `Thinking { id, turn }` | The agent's thinking, which Messages leaves out |
| `Shown` | Anything Messages shows, a worker's report or message to its lead too |
| `Hidden` | Anything else it leaves out |

Each `quiet::Count` has its `turn`, the call it opens by (`first`), its `calls`, `failed` and
`running`, and the item its line stands at (`at`). That is the thought of the same turn that led
to its first call with nothing shown between them, else the first call. Open, a count shows its
turn's work from there, as Quiet does. Find a count by the item being drawn (`at`), since it may
stand at an item before the turn's first call. A thought leads only to a call of its own turn, so a
turn Arko starts for news right after a turn that ended in a thought still has its line.
