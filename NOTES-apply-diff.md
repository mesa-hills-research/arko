# apply_diff in Arko, the rules the change keeps

Deleted in the branch's last commit.

- New sessions offer the main agent and workers `apply_diff`, not `edit_file`. A session saved
  while the agents had `edit_file` resumes with it (still registered), isn't told it lacks the
  file tools, and its outside-access checks and transcript rows work as before. Sessions saved
  before `edit_file`, with `apply_diff`, match the new configuration.
- Everything that names `edit_file` follows: the agents' instructions, code mode's callable
  list, the outside-access checks, the changed-file summary of earlier conversations, the
  transcript rows, the Quiet labels, the docs and the CHANGELOG.
- An apply_diff row names the file and how many blocks applied, once per surface. Opening it
  shows the changes as an edit_file row does, from the tool's receipt, and a session saved
  without receipts shows the blocks.
- The framework comes from its apply-diff branch through a `[patch.crates-io]` git dependency
  at a full SHA, in its own commit marked TEMPORARY.
- Builds only through `cargo-locked`. fmt and clippy `--all-targets -D warnings` clean. Tests
  check behaviour, not wording.
