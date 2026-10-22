---
name: repo-docs-style
description: Rules for repository documentation, such as READMEs, docs/ pages, design notes and changelogs. Load it before creating or editing any project docs, and when reviewing docs another agent wrote. It covers where content goes, what to write about, how short to keep it and how to word commit messages.
---

# Repository documentation style

These rules apply to every repo doc you write or edit. The `user-facing-prose` skill covers the wording of every text a user reads, and these rules add what is particular to documentation.

## Where content goes

- **README is an overview.** It says what the project is, why it exists, how to install it and a minimal usage example, and links to the docs. Short lists are fine there (for example the supported languages of a parser library). Option tables, internals, methodology, edge cases and long caveats are not.
- **Headline evidence belongs in the README.** When a number is the reason to use the project, show it up front as a short table a reader takes in at a glance. A parser library that is faster than the reference implementation shows a small speed table against it (overall plus a handful of key languages) and one line on how it was verified, because "pure Rust and faster than C" is the pitch. Keep it to roughly 5–10 rows. The full per-language comparison, the methodology, the hardware and the noise go in `docs/` with a link.
- **Heavy material goes in `docs/`.** Reference pages, design notes, troubleshooting. Link to them from the README with one line each.
- **A public repo holds what its users need.** That is the library, its tests, benchmarks a reader can run, examples and docs. Internal tooling stays in private work folders, such as dataset builders that need private tools or paid keys, machine setup scripts, planning documents for the agents that built the project, and migration runbooks.

## What to write about

- **Describe the project, not how it was made.** Don't mention that an AI, an agent swarm or a workflow built it, the specification or reference implementation it was built against, its plan steps, gates or scores, or when decisions were made and by whom. Readers care what it does and how well. Names inside the code follow the same rule. A test file is named for what it tests (`quantization.rs`), never for the plan step it came from (`u07_quant.rs`).
- **Don't dwell on verification.** How rigorously something was checked (oracles, conformance counts, "bit-identical on 727,612 inputs", fuzzing campaigns, independent re-checks) is not the pitch. Give the test command and, where correctness is the point (a port claiming identical output), one plain sentence. Overemphasis on verification and on how a project was built is a common weakness of docs written by a model.
- **Write for every user, not the project it was first built for.** A general-purpose library is described as general-purpose. A vector store that began inside a search tool is a vector store for anyone, so the tool that prompted it gets one clause at most, or nothing.

## Say it in the fewest words

The `user-facing-prose` skill asks for the fewest words that keep the meaning, and docs follow it. Every sentence in a doc has to earn its place.

- **Start with the thing.** "The page says what it is" is not a sentence. Cut openings such as "This section describes" and "In this guide you will learn", and closings that sum up what was just said.
- **Don't restate the heading.** A heading and its first sentence say different things.
- **Prefer the smaller form.** A short table or list beats a paragraph for options and settings, and one example beats three sentences explaining it.
- **One page, one job.** If a page needs a long preamble to explain what it is for, it is two pages or none.
- **Delete what a reader can see.** Don't describe what a table, a screenshot or a command already shows, and don't repeat the same fact in the README, the docs page and the changelog.
- **Cut for the reader, not for the count.** Keep the words that remove doubt, such as a command, a number, a name or the one caveat that would bite.
- **Keep the detail, and keep it human.** Shorter is not terse. Don't drop what a user needs to get it right, and don't write clipped fragments. Write direct, plain sentences in the voice of a person explaining something to a colleague, with little fancy formatting (no colons or semicolons between clauses, no bold lead-ins, no arrows or decorative symbols).

## Commit messages in public repos

Commit messages are public too, and they follow the same rules as the docs. Write them for any user of the project, saying what changed and why in general terms. "Autocomplete waits for a pause in typing" or "Improvements to autocomplete" is right. Leave out anything about one person or one setup. That means no "the maintainer", "as requested" or "on my machine", no names, and no review rounds, agents or plan steps. A bug report from one user becomes a description of the bug ("Suggestions came up on every keystroke").

## Prose that doesn't read like a model wrote it

- **Avoid semicolons.** People use them far less than language models do, even where they are grammatically correct. Use a period, a comma or a dash, or rewrite the sentence.
- **Avoid colons between clauses** ("X: Y"), "**Bold lead:** explanation" bullets and "Note:" prefixes. The `user-facing-prose` skill has the full rule, with examples and the few places a colon is fine.
- **Avoid the "It does X. It does not do Y or Z." pattern.** Lists of what something doesn't do are a strong tell.
- **Cut obvious negatives.** Before writing any negative, ask whether a reasonable reader would assume the opposite. If not, delete it. If the negative really matters, keep it to one short clause, or move the specifics to `docs/`.
- **Let results speak for themselves.** Phrases like "repositories no agent ever saw" or "on held-out data" make a result sound more concrete, but overemphasis on held-out benchmarking is another tell. State the number and the inputs plainly. A benchmark page may give the machine and the method in a few lines.
- Say what the thing does, plainly. Keep sentences direct.

## Check before you finish

1. Is the README still an overview? Is anything detailed moved to `docs/` and linked? Does a headline table support the pitch, and is it short enough to read at a glance?
2. Search for words about how it was built ("swarm", "agent", "oracle", "verified", "identical", "conformance"), and for names and dates. Does each one tell a user something they need? Delete the rest.
3. Would someone with no connection to the original project see it as meant for them?
4. List the repo's files. Is anything there only because of how the project was built?
5. Search the text for `;` and rewrite every one.
6. Find every "not", "doesn't", "never" and "no". Is each one something a reader would otherwise get wrong? Delete the rest.
7. Cut the page once more. Delete every sentence and every word whose removal loses nothing.
8. Read the doc aloud in your head. Does it sound like a person explaining their project?

## Example

Before:

> codeindex chunks source files with tree-sitter and does not call a model. It does not require a GPU, and it does not upload your code.

After:

> codeindex splits source files into chunks with tree-sitter and indexes them for search, all on your machine.
