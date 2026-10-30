# Kept notes

The main agent can keep notes during a session, such as "tear the build box down when the last build
is done". A handover or a Claude Code compaction can lose what the agent was told, so its notes come
back on their own. They sit in the session's folder (`kept-notes.json`), so a resumed session has
them.

The agent keeps a note with `keep_note` and removes one with `forget_note`. `keep_note` with
`replaces` (a note's number) updates that note in place, so a running status stays one note. Each
note is for the main agent, for chosen workers by ID (`r1`), or for every worker. The main agent's
own list holds every note, so a note for workers is also its own, and an audience such as `lead, r1`
is the same as `r1`.

| Agent | Gets its notes |
|---|---|
| Main agent | In the first message of each conversation, and again after a handover, after a compaction and when the session goes back to a conversation (`/resume N`, `arko -c`) |
| Worker | In its first message, as they are kept while it runs, and again after its own handover or compaction |

The notes hold 16 KB in all, and a replaced note counts at its new size. A note that would pass that
is refused with each note's number, size and age, so the agent can replace or forget one. A list
longer than an agent can take in at once while it works reaches it in parts, one after another, and
a long note goes on from one part into the next.

`/notes` lists the notes, and `/notes remove N` removes one. The main agent and the running workers
the note was for are told in one line. An agent that already read the note keeps what it read until
its next handover or compaction, which brings the current list. A note the main agent forgets is
announced to the workers the same way. When it replaces a note, the running workers it was or is
for get their list again.

An app reads and changes the notes of a session folder with `arko::kept_notes::KeptNotes`. That
changes the folder only. To remove a note from a session that is running, an app sends it the line
`/notes remove N`, as the terminal does.
