---
name: arko-taste
description: >
  Twelve checks for anything a person sees in an interface, such as a transcript row, a fold, a
  label, a status or a control. Load it before designing, building or reviewing a screen or a change
  to one. It asks for rows that earn their place, nothing shown twice, controls that keep their
  promise, the effect rather than the mechanism, and a rendering of every state before any code.
license: MIT
---

# Taste

Twelve checks for anything a person sees. Apply them to a rendering of every state a thing can
reach (closed, open, running, failed, merged with its neighbours, on a phone), before writing code
and again when reviewing it.

1. Every row earns its place. A row that tells the reader nothing they need doesn't exist.
2. Nothing shows twice. What a line says never appears inside it, and a control or a status appears
   once for each thing.
3. A control keeps its promise. A triangle opens to content worth the tap, a cut line opens to the
   rest, and "N more" is a door, never a dead end.
4. Show the effect, not the mechanism. The reader wants what was read, changed and run. Tool names,
   parameter names and ids are mechanism and stay out of sight.
5. Say what's unusual. A failure, a count, a running state, a cost. Success is the past tense.
6. Nothing moves under the reader. A row keeps its place and height, and opening it adds below.
7. Two levels at most. A row, then an item in it, then content. Deeper is a design mistake.
8. Same thing, same words, same shape on every surface. When two surfaces differ, one of them is
   wrong.
9. Fewest words, sentence case, no final period, and no label that repeats its surroundings.
10. Aligned to the pixel and the column. Equal gaps are equal, and markers share one column.
11. It looks right on the device it's for, at its width and pixel density, in its browser.
12. A design decision stands only with a rendering. Prose that describes a screen is a guess.

## Before code

Draw every state as the user would see it, as text for a terminal and as screenshots for a page or
a window, and get a yes on the drawings. Then build to them.

## When reviewing

Look at renderings of the built change on a realistic session, not at the code, against the twelve
checks. Name the check that fails and show the state that fails it.
