# Worker profiles and workers that start workers

The main agent starts workers with `start_agent`. A worker runs on the session's worker model
unless the main agent chooses otherwise, either with a profile or with a model and an effort.
A profile is a named setup for workers. It holds a model, an effort and instructions, and the
main agent starts a worker with it by name. A worker can start workers of its own too, which
[the last section](#workers-that-start-workers) describes.

## A file for each profile

A profile is a Markdown file with a few `key: value` lines in front, then the instructions every
worker started with it gets. The file's name without `.md` is the profile's name.

```markdown
---
description: Judges 3D results against a reference image
model: sol
effort: high
---
You judge 3D models. Compare the render with the reference image on shape, proportions and
materials. Say what is wrong, in order of how much it matters.
```

| Key | Meaning |
|---|---|
| `description` | One line of up to 120 characters, shown in lists. Required |
| `model` | A model as `/model` takes it, such as `opus`, `sol`, `cc:haiku` or `openrouter:<slug>`. Without one, the session's worker model |
| `effort` | `low`, `medium`, `high`, `xhigh` or `max`, as the model takes it |
| `fast` | `true` for fast mode, where the model has it |
| `cache` | `5m` or `1h`, the prompt cache of Claude workers that take a choice |
| `nested` | `true` to let the worker start workers of its own, down to the session's worker depth. The call's `nested` wins, so `nested: false` leaves it out for one worker |
| `made_by` | `agent` when the main agent saved the profile |

A name is lowercase letters, digits and hyphens, up to 32 characters, and starts with a letter
or a digit. `default`, `main`, `worker`, `lead` and `all` are taken. The instructions can run to
4,000 characters, and a file over 16 KB is left out. A file that can't be read shows in
`/profiles` with the reason and hides no other profile. Windows line endings and a byte order
mark are fine. A profile doesn't say whether a worker works in a git worktree. The main agent
chooses that for each worker with `isolated` in `start_agent`. A key Arko doesn't know, such as
`isolated` in a file from an earlier version, is kept when Arko rewrites the file.

## Where profiles live

| Place | Folder | Who uses it |
|---|---|---|
| This session | `profiles` in the session's folder | This session |
| This project | `.agents/profiles` in the working directory | Everyone who works on the project |
| Every project | `profiles` in Arko's folder | You, in every project |

When a name is in more than one place, the nearest wins, so a session's profile beats the
project's and the project's beats yours. Arko reads the files each time a worker starts, so an
edit reaches the next worker without a restart. A worker keeps the setup it started with, also
when its profile is edited or deleted later and when the session is resumed.

## How a worker's setup is chosen

For the model, the effort and the prompt cache, the call to `start_agent` wins, then the
profile, then the session's worker model, which `/model worker` and `--worker-model` set.

- A model without an effort keeps the nearest effort it takes to the one that would have applied.
- An effort the model doesn't take is refused, and the main agent is told which ones it takes.
- Fast mode is on when the worker runs the session's worker model and the session has fast mode
  on, whether it has a profile or not. A worker on another model is fast only when its profile says
  `fast: true`.
- Each worker hands over at the size of its own model, and `/handover-at workers SIZE` still
  applies to all of them, each up to what its model leaves room for.
- A profile's instructions open the worker's first message, after the project's instructions.
- A model the session can't reach is refused with what it needs. A Claude model needs
  `ANTHROPIC_API_KEY`, a `cc:` model needs Claude Code signed in, and so on.

## What the main agent can do

Two tools help it choose. `worker_options` lists the profiles and the models usable now, with
each model's efforts, price, handover size and prompt cache, and the plan usage next to models on
a plan. `save_worker_profile` saves a setup under a name. A profile for the session needs no
approval. Before Arko saves one for the project or for every project, it puts you a question that
shows the whole profile, and only a plain yes saves it (a "please" or "thanks" after the yes is
fine). Anything else you write goes back to the agent as your feedback, and the transcript says
nothing was saved. The agent can't replace a profile you made.

The question guards what Arko writes for the agent. The agent's own file tools and shell can write
a file in the project's `.agents/profiles` folder as they can write any file in the project, and
in your Arko folder once you turn outside access on. So treat a project's profiles as you treat its
`AGENTS.md`, as text the project's authors and the agent can change.

## Commands and settings

| Command | What it does |
|---|---|
| `/profiles` | Lists the profiles, those they hide, those that can't be read, and the folders |
| `/profiles NAME` | Shows one profile whole, with its file |
| `/profiles add NAME MODEL [EFFORT] [DESCRIPTION]` | Writes a profile for every project and prints its file to edit |
| `/profiles remove NAME` | Deletes the profile `/profiles` shows under that name |
| `/worker-models agent`, `profiles` or `session` | Who chooses the workers' models. See below |
| `/worker-depth 1` to `4` | How many levels of workers the session allows |
| `/message-copies on` or `off` | Whether the agent that started a worker gets a copy of the messages you send the worker |

`/worker-models` (also `--worker-models`) takes three values. It is the "Models for workers" row in
`/settings` and the setting of that name on the web page. With `agent`, the default, the main agent chooses a profile, model and effort for
each worker and can save profiles. With `profiles`, it starts workers with a profile only. With
`session`, every worker runs the session's worker model and none starts workers. The setting applies to the next start.
A session saved before the setting existed keeps `session`, which is how its workers always ran.

`/worker-depth` (also `--worker-depth`) is saved with the session and defaults to 2. It is the
"Worker depth" row in `/settings` and the setting of that name on the web page. It sets how many
levels of workers the session allows, and it applies to the next worker started. With 1 only the
main agent starts workers. With 2 or more, a worker can start workers of its own when the agent
that starts it says so, down to that level.

`/message-copies` (also `--message-copies` and `--no-message-copies`) is saved with the session and
is on by default. It is the "Message copies" row in `/settings` and the setting of that name on the
web page. The agent that started a worker gets a copy of each message you send the worker, with its
next news and without a turn of its own. With it off, that agent doesn't see your messages. A change
applies from the next message, and a session saved before the setting existed has it on.

## Workers that start workers

The main agent starts a worker with `nested: true` in `start_agent`, or with a profile that says
`nested: true`, to let that worker start workers of its own. It is meant for a part of the work that
splits again. The call's `nested` wins over the profile's, and `worker_options` marks the profiles
that have it.

A worker started this way has `start_agent`, `list_agents`, `stop_agent` and `link_agents` besides
the tools every worker has, and its first message opens with a short paragraph about its workers.
It has no `worker_options` or `save_worker_profile`, so the main agent names the profiles and
models in its task. `list_agents` and the result of a start say what each worker runs.

Its workers report to it, and the main agent reads the worker's own report. The worker can't
report while its workers are unfinished, and it is told which ones. Stopping a worker, or the worker
ending, stops the workers under it too.

| Limit | What it is |
|---|---|
| Worker depth | The levels of workers the session allows, 2 by default and 4 at most. A worker at the last level starts no workers, and a start that asks for `nested` there is refused with the reason. At the last level a profile's `nested` is ignored |
| Workers of one worker | At most 8 running or queued at once |
| Models for workers | With `session`, every worker runs the session's worker model and none starts workers. With `profiles`, a worker starts its workers with a profile only |

A worker's workers are listed under it in the agent list, `/workers` and the web page. A notice of
a stop or a failure says "r2 (under r1) stopped" in every view, and the line prompt and `-p` also
say "r2 (under r1) reported". `/workers` and the web page's cards give what a worker cost with its
workers, and `/cost` counts each worker once.

The user's message to a worker is copied to the worker that started it, as news that doesn't start
a turn, unless message copies are off. A worker reaches the one that started it with `message_agent`
to `lead`, and `link_agents` lets a worker's own workers message each other. A worker can't message
another worker's workers. `/stop-workers r1` stops r1 and the workers under it, and only the worker
that started a worker can restart it with a message.

A session that closed with a tree of workers running continues it, each worker with the setup it
started with. A worker whose starter can't run again, because it failed, is stopped instead of
relaunched, and its starter can restart it with a message. An older Arko opens such a session and
lists every worker as the main agent's.

A conversation in Claude Code keeps the tools it started with. A session on Claude Code that was
made before workers could start workers resumes without `nested` in `start_agent`, and Arko says so
when it opens. A new session has it.

## For apps

`arko::profiles` reads and writes the files without a session, as `arko::extensions` does for
MCP servers and skills. Call it off the UI thread because it reads files.

```rust
use arko::profiles::{self, Author, Place, Profile, Roots};

let roots = Roots::new(arko_folder).workspace(project).session(session_folder);
let listed = profiles::list(&roots);   // active, hidden and unreadable
let found = profiles::read(&roots, "3d-judge")?;

let mut profile = Profile::new("3d-judge", "Judges 3D results");
profile.model = Some("sol".into());
profiles::save(&roots, Place::User, &profile, Author::User)?;
profiles::delete(&roots, Place::User, "3d-judge")?;
```

`Profile::check` tells whether a profile can be saved, with a model Arko knows and an effort it
takes. `profiles::folders` gives the folders for an "open the folder" button, and
`profiles::approval_text` and `profiles::is_approval` are the question the agent's save puts to
the user and the rule for its answer. `arko::workers` has the names, commands and sentences of the
two settings, and `ModelChoice::label` and `about` the words for each value, so an app labels and
sets them as the terminal and the web page do.

A worker's summary (`WorkerSummary`) names the worker that started it in `parent`, its `depth`
(1 for the main agent's workers), and what it and the workers under it cost in
`family_cost_billed` and `family_cost_list`. `arko::remote::family_order` puts a list of summaries
in the order a list shows a family, with each worker's workers after it.
