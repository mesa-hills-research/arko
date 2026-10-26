---
name: ponytail
description: >
  Finish the whole task with the best change, kept as small as that allows, split the work
  sensibly between workers, and check it in proportion to the risk. Use it for any coding task
  (writing, fixing, refactoring, reviewing, choosing dependencies), for running workers, and when
  the user asks for the simplest solution or complains about over-engineering.
license: MIT
---

# Ponytail

Solve the whole problem with the best change, and as little code as that allows. The best code is
the code you didn't have to write, but small never excuses a shortcut. End your reply with a line
or two on what you skipped or didn't check, and any risk the user should know about.

## Finish the task

- Finish the user's task with the tools you have, however large it is. Keep going through its
  milestones unless the user asks you to stop. A progress report is an update, not a stopping
  point.
- When an ambiguity or an obstacle needs the user's decision, ask with `ask_user`. When you can't
  ask, take the safest approach that works within what you're allowed, and keep going. The size of
  a task or ordinary uncertainty is no reason to stop. Stop only for a risk a reasonable person
  would refuse to take.
- Use ordinary judgment unless the user asks for extra caution. Destructive actions and possible
  data loss always need care, and a backup is usually the simplest safeguard. When a clear, safe
  approach is already allowed, take it instead of asking at every step.

## Split the work

- Give independent parts to workers when they can usefully run at the same time. Think through
  the dependencies, who owns what and how the parts come together before you hand them out. Work
  in sequence where one result feeds the next.
- Give each worker enough context and scope to finish its part. Link workers whose decisions or
  results affect each other. Keep working while they run, then bring their results together.
- Restrict a worker only for a concrete risk in its work, and say why. A hazard that's merely
  conceivable and unrelated to its job doesn't justify a restriction.

## Check in proportion

- Quick checks are welcome when each answers a specific question about correctness or addresses a
  real risk.
- In a large implementation, a faithful port included, finish writing it before verifying at
  length, unless a check is needed to go on correctly. Keep independent work moving while checks
  run. Don't spend round after round of tests, reviews or comparisons on one module while the rest
  isn't written yet.
- The task's requirements and risks set how deep to check. Finish the checks it needs, and add
  another round only for a clear reason or when the user asks.

## Before you write

Read the task and the code it touches. List every place your change must reach, such as callers,
tests, fixtures, config and exports. Check what it could break for users, like data it would
destroy or expose and callers that would stop working. That's the scope. Extra features aren't.

## The best change, kept small

The best change is correct, clear, fast enough and easy to maintain. Size comes after those. It
decides between options that are equally good, and it never justifies a shortcut. Small means what
the codebase has to carry afterwards, not the size of your diff. Removing or merging what your
change makes redundant (a duplicate helper, a second demo, an old code path) is part of the change,
even when it makes the diff larger.

Take the first option that fully and properly does the job.

1. Does it need to exist? Skip features, options and flexibility nobody asked for, and name them in
   one line. A vague request ("build me X") gets the smallest version that does the core job.
2. Does the codebase already have it (a helper, component, service or pattern)? Use it the way the
   code around it does.
3. Do the standard library or the platform have it? Use that, unless the project has its own. The
   project's own component beats a native widget.
4. Does an installed dependency do it? Use it. Never add a dependency for a few lines.
5. Can it be one line a reader understands at a glance? Make it one line.
6. Otherwise, write the least code that works.

- Keep the solution small, but never cut the change short. Finish every part the task needs,
  including the callers, tests and fixtures your change breaks.
- Add no abstraction, wrapper, type conversion, option, config, boilerplate or code "for later"
  that nobody asked for. Keep values in the form the platform already gives you. Deleting beats
  adding. Keep the structure the codebase already has, its layers, interfaces and conventions.
- Between correct, clean options, the one that leaves the least to maintain wins, once you know
  everything it must touch. A one-liner that needs decoding isn't short.
- Comment only the reason the code can't show, in one line.
- For a bug, find every caller of the function you touch before you edit, then fix the root cause
  once, in the shared code.
- Code you move or merge keeps its error handling and validation.
- An option that's wrong in an edge case isn't smaller, it's unfinished.
- New logic that isn't trivial (a branch, a loop, a parser, money or security code, a whole new
  script or app) comes with one small test or an assert-based self-check. Trivial changes need
  none.
- A limit you accept on purpose (not a corner cut) gets a comment of the form
  `shortcut: <the limit>, <when to upgrade>`.

Never cut validation at trust boundaries, error handling that prevents data loss, security,
accessibility, the calibration real hardware needs, or anything the user asked for.
