---
name: worker-choice
description: >
  How to choose a worker's model, effort and billing before starting one. Load it before starting
  a worker or saving a worker profile.
license: MIT
---

# Choosing a worker

## Billing comes first

- Prefer models on the user's plans (Claude Code's `cc:` models, OpenAI models on the ChatGPT
  login) whenever one can do the job.
- Before starting a worker billed per token (an API key, OpenRouter, Gemini), ask the user and wait
  for a yes for that worker. Ask even when the session's worker billing would start it without
  asking. Say which model, why no plan model will do, and roughly what it may cost.
- Before starting many workers, check how much of each plan is left (`worker_options`). When a
  plan is nearly used up, ask before moving the work to another plan or to the API, and say what
  waiting for the reset would mean instead.

## Model

- Open-ended work, such as root causes, designs, new features and anything that needs
  judgment, goes to the strongest general model the user's plans offer.
- Bug hunts and bug fixes go to that same strong model, however small the bug looks.
- Routine, clearly defined changes (change this function to do that, a small interface tweak, a
  mechanical refactor) and research go to a smaller, cheaper model at a high effort, not to the
  strong model at a low one.
- Where quality matters above all, such as reviews of designs, documents that set standards, and
  the final call when workers disagree, use the best model available.
- A second opinion or a code review goes to a model from another maker, so its blind spots
  differ from the author's.

## Effort

- Run workers at a high effort by default. Lowering it to save time or cost usually costs more in
  rework.
- A low effort fits only mechanical one-off work, such as a lookup or a rename across files.
- Use the maximum effort only when the user asks for it.

## Profiles

When several workers share a setup (a model, an effort, standing instructions, their skills and
servers), save it once as a profile and start them by its name.
