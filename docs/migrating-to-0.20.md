# Moving to 0.20

Arko 0.20 changes what the feed says about a call's result, how the Quiet view's rows are made and
how a message carries files. An app that shows the feed, such as Arko Studio, needs these changes.

## The framework

Arko 0.20 takes arko_agents 0.8 and arko_workflows 0.9. An app that depends on them as well moves
to the same versions. `AgentEvent::ToolResult` has a new field, `receipt`, so a pattern that names
its fields needs `..`.

## A result's content

A result item (`Kind::Result`) holds its call's content, in the form that fits the tool.

| Field | What it holds |
|---|---|
| `content` | Its form, a `ContentKind` (`Plain`, `Muted`, `Lines` or `Diff`) |
| `text` | Its first 16 lines (`content::PREVIEW_LINES`) |
| `more` | The line that opens the rest ("… 24 more lines"), when there's more |
| `language` | The language of the code it holds |
| `whole` | All of it, while the feed keeps it |

`Feed::content(id)` gives the whole by the result's ID, and `None` once the feed no longer keeps
it, which an app shows as "No longer kept". A `Lines` content holds a file's lines as `12\ttext`,
and a `Diff` content holds each line as its mark (`-`, `+` or a space), its number and a tab. An
edit without a receipt from its tool (Claude Code's edits, a restored session) has no numbers. The
new `remote::content` module reads these lines (`content::line`) and makes them.

The page's `ItemData` has the same `content`, `more` and `language`.

## Files sent from the page

The web page sends files with a message, saved in the project's `.arko/attachments`.

- `remote::Action::Message` has fields, `Message { text, files }`, where `files` are
  `attachments::Attached`. `Action::message(text)` makes one without files.
- `Server::start` takes a `remote::Project`, the project's folder (`workspace`) and Arko's folder
  (`root`), whose settings the attachments folder follows.
- `Item.files` holds what a user's message carried, each an `attachments::SentFile`.
  `Feed::user_with_files` and `Feed::note_with_files` add a message with them, and `user_with` and
  `note` add one without.
- The page's `ItemData.files` has them as `FileData`, with each file's name, type, size, lines and
  the `url` that downloads it.

## A call's end

`Act.ended` moved to `Item.ended` (and `ItemData.ended`). It is the ID the next item had when the
call failed, and a call from that ID on starts a Quiet row of its own. `Act.detail` is the one muted
line a row shows where its words cut something, such as a command's whole text.

## Quiet rows

- `quiet::Call` has three more fields, `content`, `ended` and `sealed`, so build it with
  `..Default::default()`. `sealed` is set for a call whose row of one call the reader opened, which
  keeps that form.
- `quiet::Row` has `merged`, for a row of several calls that opens to each call's Full line, and
  `opens`, false for a row with no fold.
- `quiet::Words.failed` is a bool, for the one ✗ at the row's right. What went wrong is in the
  call's content.
- `quiet::WorkerRow::of` says whether a worker's item is a row in its lead's view, a report or a
  message to that lead, and `WorkerRow::words` words it from its call's state (`quiet::Sending`).

The builds between 0.19.0 and 0.20 had `Act.body` with `Fact`, `Row.calls` with `quiet::Opened`,
and `Call.output`. They are gone. A row opens to its calls' content instead, through `Row.merged`
and each call's result item.
