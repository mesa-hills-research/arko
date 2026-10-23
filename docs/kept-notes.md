# Kept notes

The main agent can keep notes during a session, such as "tear the build box down when the last build
is done". A handover or a Claude Code compaction can lose what the agent was told, so its notes come
back on their own. They sit in the session's folder (`kept-notes.json`), so a resumed session has
them.

The agent keeps a note with `keep_note` and removes one with `forget_note`. Each note is for the main
agent, for chosen workers by ID (`r1`), or for every worker.

| Agent | Gets its notes |
|---|---|
| Main agent | In the first message of each conversation, and again after a handover or a compaction |
| Worker | In its first message, as they are kept while it runs, and again after its own handover or compaction |

The notes hold 8 KB in all. A note that would pass that is refused until another is forgotten.

`/notes` lists the notes, and `/notes remove N` removes one. An agent that already read a removed note
keeps what it read.

An app reads and changes the notes of a session folder with `arko::kept_notes::KeptNotes`.
