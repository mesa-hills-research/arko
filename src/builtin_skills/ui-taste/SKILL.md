---
name: ui-taste
description: >
  Checks for anything a person sees in an interface, graphical or terminal, such as a row, a label,
  a status, a panel or a control. Load it before designing, building or reviewing a screen or a
  change to one. It puts ease on the eyes first (no layout shifts), then asks for coherent colour,
  clear hierarchy, a natural flow, consistency, elements that earn their place, nothing shown
  twice, and a rendering of every state before any code.
license: MIT
---

# Taste

The core measure for any interface, graphical or terminal, is how easy it is on the eyes, and that
means as few layout shifts as possible. A button that goes from "Save" to "⠋ Saving…" grows and
pushes everything beside it. Reserve the room from the start (a fixed width, a spinner slot that is
always there, a status drawn over the label), so nothing moves when the state changes.

A tasteful, carefully designed interface is what sets a well-made project apart from a rushed one,
so every check below is held to, not traded away for speed.

Sixteen checks for anything a person sees. Apply them to a rendering of every state a thing can
reach (empty, loading, open, closed, failed, at its longest, at its narrowest, on each device it's
for), before writing code and again when reviewing it.

1. Every element earns its place. One that tells the person nothing they need doesn't exist.
2. Nothing shows twice. A label doesn't repeat what its surroundings say, and each thing has one
   status and one control.
3. A control keeps its promise. Something that looks openable opens to content worth the click, and
   a note such as "12 more lines" shows those lines when clicked, never only says they exist.
4. Show the effect, not the mechanism. The person wants what happened to their work. Internal
   names, ids and implementation terms stay out of sight.
5. Say what's unusual. A failure, a count, a running state, a cost. Success needs little or no
   announcement.
6. Nothing moves under the person. An element keeps its place and size through every state, and
   whatever opens or grows adds below or after it, never above or before. Measure shifts frame by
   frame with zero tolerance. A tolerance is a design decision, shown as a recording of the movement
   it allows and agreed before any test accepts it.
7. Keep nesting shallow. Two levels inside a list or a panel is plenty, and deeper is usually a
   design mistake.
8. Same thing, same words, same shape everywhere it appears. When two places differ, one of them is
   wrong.
9. Fewest words, sentence case, no final period on labels, and no label that repeats its
   surroundings.
10. Aligned to the pixel and the column. Equal gaps are equal, and like elements share an edge.
11. It looks right on every device it's for, at their widths and pixel densities, in their
    browsers or terminals, and only those. A desktop app with no phone version needs no phone
    checks.
12. A design decision stands only with a rendering. Prose that describes a screen is a guess.
13. Colour is coherent. Text and controls meet contrast in every theme, and the colours work
    together, with one palette, few accents, and each colour meaning one thing everywhere.
14. Hierarchy is clear. The eye lands on what matters most first, and size, weight and colour say
    importance, never decoration.
15. The flow is natural. Things read in the order the task happens, and related things sit
    together.
16. It's consistent. Text never wraps where it shouldn't (labels, buttons and table cells are sized
    for what they hold), numbers in a column align on their last digit, and the same gesture does
    the same thing everywhere.

## Before code

Find out where the interface will be used, meaning which platforms, devices, screen sizes, browsers
or terminals. Look first in the project's own documents and notes. If they don't say, ask the user,
and record the answer where later sessions will find it (the project's AGENTS.md or its design
document), so the question is asked once.

Then draw every state as the person would see it, on each of those surfaces, as text for a terminal
and as screenshots for a page or a window, and get a yes on the drawings. Then build to them.

## When reviewing

Look at renderings of the built change with realistic content, not at the code, against the sixteen
checks. Name the check that fails and show the state that fails it.
