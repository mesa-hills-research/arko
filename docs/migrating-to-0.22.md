# Moving to 0.22

Arko 0.22 names the worker a call set going, counts a command's extra lines in new words and adds a
dictation setting. Arko 0.22.1 renames a built-in skill. An app that shows the feed, such as Arko
Studio, needs these changes.

## A worker's start

`remote::Item` has two new public fields, `sets` and `task_title`. Code that builds an `Item` field
by field needs them. `Item::new` gives both `None`.

| Field | What it holds |
|---|---|
| `sets` | The worker a call set going. A `start_agent` call has it once its result says it started or queued the worker. A `message_agent` call has it once its result says the message continued or restarted a worker that ended, or that the worker goes on with it when an agent run ends. Any other call has `None`. |
| `task_title` | On a `start_agent` call, its task's first line that has words, without the marks of a heading or a list (`#`, `-`, `*`), on one line and cut after 72 characters with a "…". A start with no task has "Start a worker". |

The terminal draws a start, and a message with `sets`, as the worker's row. A start's row says
`task_title` until the worker has a title of its own (`WorkerSummary.title`, worded the same way),
then says how the worker stands. A message's row says "Messaged r2". An app that draws such rows
finds the worker by `sets`.

Neither field goes to the web page. `started` is as before, the ID of a worker that started at
once, so a queued worker has `sets` and no `started`. A session restored from its history gives
`sets` the same way. When the restored session has no record of the worker, its start keeps `sets`
and loses `started`.

## Counts

`remote::plural(count, one, many)` gives a count with its noun ("0 lines", "1 line", "3 lines"),
and `remote::noun(count, one, many)` gives the noun alone, for a count written its own way ("1,240
lines"). `count` is any integer type that converts from `u8`, such as `usize`, `u32`, `u64`, `i32`
and `i64`. The terminal and the page count with them.

Two wordings changed, which matters to an app that matches on the text.

| Where | Was | Is |
|---|---|---|
| The text of a command call with more lines, for any count | `$ cd a  (+1 lines)`, `(+2 lines)` | `$ cd a  (1 more line)`, `(2 more lines)` |
| The notice of events the feed missed (`Feed::missed`), for a count of one | `(1 events not shown)` | `(1 event not shown)` |

## Dictation

`dictation::settings::Settings` has the new `workspace_paths`, `true` by default, also when
`dictation.json` leaves it out. Code that builds a `Settings` field by field needs it, or
`..Settings::default()`. It writes spoken symbols as code and a dictated path as the workspace's
file or folder it names, in English ([Dictation](dictation.md#paths-and-symbols)). `false` leaves
them as the model writes them. The cleanup has `settings::set_cleanup`, but this setting has no
setter, so an app that offers it edits `dictation.json`.

## The interface skill

The built-in skill `arko-taste` is `ui-taste`, so `arko::builtin_skills::names()` gives `prose`,
`ponytail` and `ui-taste`. An app that kept a choice to turn `arko-taste` off passes `ui-taste`
instead. A session skips the old name in `Options::builtin_skills_off` and warns that it isn't a
built-in skill, and `extensions::list_with` skips it quietly. Either way `ui-taste` stays on, as it
does for a session saved with `arko-taste` turned off.
