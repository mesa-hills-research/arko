# Rules for the ui-fixes branch

Working notes for the live review, deleted in the branch's last commit.

## Scope

1. Terminal: opening a result's "N more lines" moves the rows above it.
2. Dropped by the owner: the finished-command shift at 60x24 in Full. DESIGN.md row 22 stays.
3. Page on a phone: about half a screen of empty space below a running call. Diagnose first,
   feed or display, with evidence, and report before changing anything.
4. Page on a phone: the running row's "Move to background" runs past the right edge.
5. Page: a worker's report sometimes shows twice ("r234 reported" row, then a plain line).

Studio is checked for 3 and 5 and left unchanged.

## Rules every change keeps

- Zero tolerance for shifts. Nothing above an item the reader acts on moves, up or down, in the
  frame of the action. An open item has its blank row below it only.
- Following never moves a line down (FOLLOWING.md rules 1 to 10, DESIGN.md section 9's table).
  Blank rows below the end stay until new output fills them or the reader goes to the end.
- A fold toggled keeps the lines above it on their rows. For a content, whose fold is its last
  line (the door, or "↑ show the first lines only"), the lines above that are its preview and the
  rows before it, so those stay. Opening adds below the preview. Closing keeps the preview's
  rows where any shows. Where none shows, the door takes the clicked row, so nothing that stays
  on the screen moves.
- A content's preview and its whole draw their shared lines the same, column for column (the
  number gutter fits the whole's widest number), so opening moves nothing sideways either.
- The shift meters (`src/tui/shift_tests.rs`, `web/shift/meter.mjs`) gain the cases fixed here
  and stay at zero for them.
- A row fits a phone's width (390 px). Its outcome and controls stay whole, and its words are
  cut with "…". A control keeps an accessible name.
- Nothing shows twice. A worker's report is one row in its lead's view, on every surface.
- Same words and shapes on every surface (terminal, page, Studio).
- Prose rules for every string and comment: no semicolons, no colon-joined clauses, no
  "word, its description" appositions.

## Evidence

- Renderings of each state before code, the page at 390 px before and after in
  `~/Desktop/arko-studio-work/scratch/web-page-1118/after/`.
- Builds and tests only through `~/Desktop/arko-framework-work/bin/cargo-locked`, at most 4 jobs.
  fmt and clippy `--all-targets -D warnings` clean.
