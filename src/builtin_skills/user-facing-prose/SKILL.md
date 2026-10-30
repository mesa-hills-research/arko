---
name: user-facing-prose
description: How to write any text a person reads, such as UI text (labels, buttons, tooltips, notifications, errors, empty states, settings descriptions), terminal messages, web pages, docs, READMEs and commit messages. Load it before writing or reviewing any of these. It asks for the fewest words that keep the meaning, written as plain sentences without colons or semicolons joining clauses.
---

# User-facing prose

These rules cover every word a user reads. That means UI strings, notifications, errors, tooltips, settings descriptions, terminal messages, web pages, docs and commit messages. Messages between agents are exempt. For documentation in a repository, the `repo-docs-style` skill adds its own rules on top of these.

## Use the fewest words that keep the meaning

Every word a user reads costs attention, so prefer the minimal prose that is still complete. Cut whatever the layout, the context or the user's own action already says.

- **A label is a word or two,** not a sentence, and it doesn't describe itself. A button in a dialog titled "Delete profile" says "Delete", not "Delete this profile".
- **Don't repeat what the surroundings show.** A message that waits to be read is marked "Pending", not "Note · Not read yet".
- **Name a state only when it differs from the usual one,** and name it with one word where one fits ("Pending", "Failed", "Queued"), not a sentence about it ("This message has not been read yet").
- **Choose the shorter word when both are right.** "Use" over "utilize", "Stop" over "Stop the running task", "Pending" over "Not read yet".
- **Drop filler.** Openings and closings such as "Please note that" or "You can now", "Successfully", hedges, and any sentence that restates the one before it.
- **Cut for the reader, not for the count.** Keep the words that remove doubt. An error still says what happened and what to do ("Couldn’t share the page. Port 8765 is in use.").
- **Don't lose detail, and don't turn curt.** Fewer words never means dropping a fact the reader needs, and it never means clipped, telegraphic text. Write plainly, the way a person would say it to someone beside them, and let it read as if a person wrote it, not a language model. Skip fancy formatting too (colons and semicolons between clauses, arrows, bold lead-ins, decorative symbols).

## No colons between clauses

Colons that join two statements ("X: Y") are a strong sign that a language model wrote the text. Rewrite the sentence instead. A period, a comma, "so", "because", parentheses or a reordered sentence almost always reads better.

The test is whether a semicolon could stand where the colon is. "The worker gets the skills if configured: it can load them at will" could take a semicolon, so rewrite it. "Optional: the directory it works in" couldn't, so a short label like that before a parameter's description is fine.

| Instead of | Write |
|---|---|
| The page isn’t encrypted: share it on networks you trust. | The page isn’t encrypted, so share it only on networks you trust. |
| A branch starts from a conversation: send a message first. | Send a message first, then branch from it. |
| The app re-reads this conversation without its cache: about 180k tokens. | The app re-reads about 180k tokens of this conversation without its cache. |
| A new key: these links stop working | A new key ends these links. |
| A sample session: it shows, but doesn’t run. | A sample session that shows but doesn’t run. |
| Signed in: ~/.config/app/auth.json (yours). | Signed in with ~/.config/app/auth.json (yours). |
| The app asks: Tabs or spaces? | The app asks “Tabs or spaces?” (or show the question by itself under a heading) |

The same goes for "Label: explanation" patterns in prose and lists.
- No **Bold lead:** followed by an explanation. Write the item as a sentence ("**New key** ends the old links").
- No "Note:", "Tip:", "Warning:" or "Important:" prefixes. Say the thing. If it's a warning, the UI's warning style shows that.
- No trailing colons on form labels, settings names or headings ("Model", not "Model:").

Colons are fine where they aren't prose.
- Times, ratios and identifiers, such as 10:30, 16:9, `cc:sonnet` or `key:value` in code.
- Keyboard shortcuts, and code, paths and commands shown as code.
- Verbatim machine text in a technical detail line, where a rewrite would garble it. One example is an error the system returned, shown inside a disclosure or log view. Keep the sentence around it colon-free where you can ("Couldn’t read config.json (permission denied)").
- In docs, a colon may introduce a list or a code block when a full sentence leads into it.

## Also avoid

- **Semicolons.** Use a period, a comma or a dash, or rewrite.
- **"It does X. It does not do Y."** Lists of what something doesn't do. Cut negatives a reader wouldn't assume in the first place.
- **Emphatic negatives tacked on.** "Completes words from what you've typed, never from pasted text or replies" sounds like it's insisting. Give the scope a plain sentence of its own, such as "Completes words from what you've typed. Excludes pasted text and replies." A "not" that corrects a value in an error ("takes high, not max") is fine, and so is "never" where it's the plain fact.
- **Stacked qualifiers and hedges.** Say what happens, plainly.
- **Inflated words.** Prefer "use" to "leverage", "lets you" to "empowers", "shows" to "surfaces".

## In UI text especially

- **Say what happened or what to do,** in the fewest plain words. "Couldn’t share the page. Port 8765 is in use." beats a paragraph.
- **One idea per sentence.** Two short sentences beat one sentence joined with a colon or semicolon.
- **Name things the way the UI names them.** Use the same words as the menu item or button the text refers to.
- **No filler states.** No "LIVE", "CONNECTED" or "Ready!" badges, and no decorative status dots. Show a state only when it's unusual or the user must act on it.

## Check before you finish

1. Search the strings or the text for `: ` and `;`. Rewrite every colon that joins clauses or follows a label. Keep only the allowed cases above.
2. Search for "Note", "Tip", "Warning", "Important" used as prefixes, and for bold-lead bullets.
3. Read each string aloud. Does it sound like a person telling you something?
4. Cut each string once more. Delete every word whose removal loses nothing, and every label that repeats its surroundings.
