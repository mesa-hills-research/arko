# Rules this change keeps

Paired work (a worker and its reviewer, or any two linked workers) hit four snags. The fixes
stay in Arko, on arko_workflows 0.9.0 as it is.

## 1. The peer-message limit

- `/peer-messages N` sets how many messages two linked workers may send each other between two
  of their lead's messages to either. It's a session setting, saved with the session, and 200
  by default. A whole number from 1.
- It feeds `SubagentsConfig::max_peer_messages` when the workflow is built. Before the
  conversation starts the agents are built again, as other settings do, so it applies at once.
  After that it applies when the session next opens, since 0.9.0 has no live setter, and the
  command's reply says so.
- A session saved before the setting gets the new default.
- The refusal keeps the framework's words. TODO-LATER gets a line for the next arko_workflows
  release: the refusal should say how the lead lifts the limit.

## 2. Peers hear when a worker ends

- When a worker finishes, fails or is stopped, each run linked with it (the same lead, and
  named in `peers`, in its group or `all`) gets one note: `Note::for_run(peer)`, from the host,
  for the worker agent.
- A note never wakes a run that isn't running, and never touches the framework's peer counts
  (only mailbox messages do). A running peer reads it at its next step, and its
  `wait_for_agents` returns at once. A finished peer reads it when something else continues it.
- A finish note gives the first lines of the report (at most 6 lines and 600 characters) and
  says the whole report went to the lead.
- A failure or stop note gives the reason on one line and says not to wait for it. A plan's
  usage limit ends a worker as failed (its usage-limit policy pauses, and a subagent's pause
  fails the run), so it lands here too.
- Each note is posted with `post_note_once`, keyed by the ended run, its state, its record's
  `since` (kept whole by a checkpoint) and the peer. The same end gives the same key, whichever
  path tells it, and a later end, even one just like it, another.
- The notice is built from the record as it is when it's told, never from the event. A late
  event finds the record as it is by then. It tells the latest end (whose own event makes the
  same key), or nothing while the run goes on.
- Posted from the crew's event task, through `crate::blocking` (posting writes the event log),
  with no crew lock held. Nothing is posted while the workflow stops, since its workers stop
  with it.
- After a resume, once the workers are relaunched, every ended worker's peers are told (with the
  same keys), whether they run again or not. This covers a worker the checkpoint saved running
  whose result says it ended (the framework settles it without an event), an end during a stop
  of the workflow, and a note lost when Arko died. A peer told before isn't told again.
- Accepted (lead, 11-18): a record the checkpoint saved running whose result says it ended is
  settled again with a fresh `since` when the workflow is built. If its peers were told of that
  end (Arko died just after the run was continued), they're told again. Matching the words
  would lose a new end that reads like an old one, which is worse. A `shortcut:` comment and a
  test say so, and TODO-LATER has the framework fix.
- Events lost to a lagging channel (1024 behind) lose their notes until the next resume.
  Accepted.

## 3. Background jobs after a restart

- Each worker whose background jobs (monitors, commands, moved calls) stopped with Arko gets a
  note naming them, in the words the main agent's first message uses, so it starts again the
  ones it still needs.
- A clean close stops a worker's monitors and moved calls as its run ends (the framework's
  `stop_owner_watchers`), before the jobs are saved, so the next open never lists them. They're
  read at once before the crew stops the workflow (`Crew::shutdown_and`'s `before`, no await or
  output between) and told once the workers have stopped, before its last checkpoint, so no
  worker reads the note and starts a monitor that the close then stops. A start still under way
  at the stop is cancelled, and the framework's reopen message tells the worker so.
- The next open tells the jobs `interrupted_at_open` lists: a worker's commands after a clean
  close, and everything after Arko died.
- Each note is posted with `post_note_once`, keyed by its first job's id, so a workflow opened
  again in the same process posts nothing new.
- The open's notes are posted before `resume_subagents`, so a relaunched worker reads them at
  its first step. One that isn't relaunched reads them when it's continued.
- The main agent's first message is unchanged.

## 4. A paused or failed peer blocks no one

- Covered by 2. Its peers are told and their waits end.
- Run slots stay as they are. Arko's default sets no run limit.

## Tests

- 1: parsing and wording, the value reaching the workflow config, saved and read back, the
  command before and after the conversation starts.
- 2: the recipients, words and keys. Two linked workers where one ends: a running peer gets the
  note and its wait ends, a finished peer stays finished, and a late event of an old end tells
  nothing new. A checkpoint that saved a worker running whose result finished: its relaunched
  peer is told once, also when the workflow opens again.
- 3: a closed Arko (its worker waiting for mail, and still waiting when it stops) and a dead
  one: the resumed worker reads one note naming its monitor, also when the workflow opens
  again.
