---
name: prose
description: How to write anything someone reads, from UI text, errors, help and messages between agents to a project's README, docs, commit messages and comments in code. Load it before writing or reviewing any of these.
---

# Prose

Write the way a person explains something to a colleague beside them, in the fewest plain words that keep the meaning. Every word a reader meets costs attention, and anything that reads as if a model wrote it costs trust. That goes for everything you write, messages to other agents and reports included.

## The fewest words

- Cut what the layout, the context or the reader's own action already says. A label is a word or two and doesn't describe itself, so a button in a dialog titled "Delete profile" says "Delete". A state is named only when it's unusual, in one word where one fits ("Pending", "Failed").
- Call things what the interface calls them, in the words of the menu item or button the text refers to.
- Choose the shorter word when both are right. Drop openings, closings, hedges, "Successfully", and any sentence that restates the one before.
- Cut for the reader, not for the count. Keep every fact that removes doubt. An error still says what happened and what to do ("Couldn't share the page. Port 8765 is in use."). Shorter is never clipped or telegraphic.
- One idea per sentence. Two short sentences beat one joined with a colon or a semicolon.

## The tells of a model

These are the patterns that keep turning up, so check for each one by hand.

- A colon joining two clauses ("X: Y"). The test is whether a semicolon could stand there. If it could, rewrite with a period, a comma, "so", "because", parentheses or a reordered sentence. A short label before a parameter ("Optional: the directory") is fine, and so are colons in times, ratios, identifiers, shortcuts, code, paths, URLs, and machine text shown verbatim. In docs, a full sentence may end in a colon that leads into a list or a code block.
- Semicolons. People use them far less than models do.
- "**Bold lead:** explanation" bullets, "Note:", "Tip:", "Warning:" and "Important:" prefixes, trailing colons on labels and headings, arrows and decorative symbols.
- Lists of what something doesn't do, and emphatic negatives ("never from pasted text"). Before any negative, ask whether a reasonable reader would assume the opposite. If not, delete it.
- Inflated words. "Use" over "utilize", "lets you" over "empowers", "shows" over "surfaces".
- Filler states in a UI. No "LIVE", "CONNECTED" or "Ready!" badges and no decorative dots. Show a state only when it's unusual or the user must act on it.

| Instead of | Write |
|---|---|
| The page isn't encrypted: share it on networks you trust. | The page isn't encrypted, so share it only on networks you trust. |
| A new key: these links stop working | A new key ends these links. |
| Signed in: ~/.config/app/auth.json (yours). | Signed in with ~/.config/app/auth.json (yours). |
| The app asks: Tabs or spaces? | The app asks "Tabs or spaces?" |
| It chunks files with tree-sitter. It does not call a model or upload your code. | It splits files into chunks with tree-sitter and indexes them, all on your machine. |

## Docs and commit messages

Readers of a public repo care what the project does and how well, so describe the project, never how it was made. Leave out agents, swarms, plans, reviews, finding IDs, gates, scores, oracles, "the maintainer", "as requested", "on my machine", names and dates, in docs, comments, test names and commit messages. Don't dwell on how rigorously something was verified. Give the test command and, where correctness is the point, one plain sentence. Give a result as its number and its inputs, since phrases such as "on held-out data" only oversell it. A general-purpose library is described for every user, and the project that prompted it gets one clause at most.

A repo's files (AGENTS.md, docs, comments) name only skills, tools and files its contributors have. A skill from a personal skill folder lives on one machine, not in the repo, so state the rule itself in a sentence or two, or link to a doc in the repo. A skill that ships inside the repo itself may be named.

Tests, fixtures, examples and docs use made-up paths, user names, machine names and project names (`/home/ada/proj`, `C:\Users\ada`, `D:\Work`, "Example Studio"), never those of a real machine or person. A path copied from a real session gets renamed before it's committed.

The README is an overview. It says what the project is, why it exists and how to install it, with a minimal example, a headline table if a number is the pitch (5 to 10 rows), and links. Reference, design notes, methodology and troubleshooting go in `docs/`, one page per job. Internal tooling, such as setup scripts for one machine and planning documents, stays out of the repo.

A doc starts with the thing, and its first sentence says something its heading doesn't. A short table or list beats a paragraph for options, and one example beats three sentences explaining it. A fact lives in one place, so the README, a docs page and the changelog don't repeat each other.

A commit message says what changed and why for any user of the project. "Autocomplete waits for a pause in typing" is right. A bug found in review becomes a description of the bug ("Suggestions came up on every keystroke").

## Before you finish

1. Search the text for `: ` and `;`, comments included, and rewrite each one that isn't an allowed case.
2. Search for "Note", "Tip", "Warning", "Important", bold-lead bullets, names, dates and words about how the project was made ("swarm", "agent", "maintainer", "verified").
3. Find every "not", "doesn't", "never" and "no". Keep each only where a reader would otherwise get it wrong.
4. In a repo, check that the README is still an overview, that someone new to the project would see it as meant for them, and that no file is there only because of how the project was made.
5. Read it aloud in your head. Does it sound like a person? Cut once more.
