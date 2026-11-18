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
- Each note is posted with `post_note_once`, keyed by the ended run, how it ended, its model
  calls so far and the peer. The same end gives the same key, whichever path tells it.
- Posted from the crew's event task, through `crate::blocking` (posting writes the event log),
  with no crew lock held. Nothing is posted while the workflow stops, since its workers stop
  with it.
- After a resume, once the workers are relaunched, every ended worker's peers that run again
  are told (with the same keys). This covers a worker the checkpoint saved running whose
  result says it ended (the framework settles it without an event), an end during a stop of
  the workflow, and a note lost when Arko died. A peer told before isn't told again.
- Events lost to a lagging channel (1024 behind) lose their notes until the next resume.
  Accepted.

## 3. Background jobs after a restart

- When a session resumes, each worker whose background jobs (monitors, commands, moved calls)
  stopped with Arko gets one note naming them, in the words the main agent's first message uses,
  so it starts again the ones it still needs.
- Posted with `post_note_once`, keyed by the first stopped job's id, so a workflow opened again
  in the same process posts nothing new.
- Posted before `resume_subagents`, so a relaunched worker reads it at its first step. One that
  isn't relaunched reads it when it's continued.
- The main agent's first message is unchanged.

## 4. A paused or failed peer blocks no one

- Covered by 2. Its peers are told and their waits end.
- Run slots stay as they are. Arko's default sets no run limit.

## Tests

- 1: parsing and wording, the value reaching the workflow config, saved and read back, the
  command before and after the conversation starts.
- 2: the recipients, words and keys. Two linked workers where one ends: a running peer gets the
  note and its wait ends, a finished peer stays finished. A checkpoint that saved a worker
  running whose result finished: its relaunched peer is told once, also when the workflow opens
  again.
- 3: a resumed session posts a worker's note naming its monitor, once.
