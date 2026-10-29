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
| `cache` | `long` or `short` (`1h` and `5m` mean the same), the prompt cache of workers whose model has a long one. Long is an hour on Claude models and 12 hours on OpenAI GPT-5.6 and later with an API key. Short is 5 minutes on Claude and 30 minutes on OpenAI |
| `nested` | `true` to let the worker start workers of its own, down to the session's worker depth. The call's `nested` wins, so `nested: false` leaves it out for one worker |
| `skills` | Skill names separated by commas, or `none`. Workers with the profile get only these, as far as the agent that starts them has them. See [Skills and MCP servers for a worker](#skills-and-mcp-servers-for-a-worker) |
| `mcp` | MCP server names, the same way |
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
| This project | `.arko/profiles` in the working directory | Every session in the working directory |
| This repository | `.arko/profiles` in the root of the git repository | Every session in the repository |
| Every project | `profiles` in Arko's folder | You, in every project |

When a name is in more than one place, the nearest wins, in the order above. Arko reads the files
each time a worker starts, so an edit reaches the next worker without a restart. A worker keeps
the setup it started with, also when its profile is edited or deleted later and when the session
is resumed.

The main agent saves a profile for the project unless it says `session` or `user`, so profiles
stay with the workspace and a new session there finds them. A worker in a worktree of its own
uses the profiles of the session's working directory, as a worktree has the committed files only.

Earlier versions saved the project's and the repository's profiles in `.agents/profiles`, which other
tools read too. Arko still reads that folder, after the `.arko/profiles` of the same place, and
`/profiles` names it next to the profiles it holds. Saves go to `.arko/profiles` only, and Arko
never moves a file there by itself. Saving a profile that lives in `.agents/profiles` writes the new
file in `.arko/profiles` and deletes the old one, so the name isn't listed twice. An `.agents/profiles`
reached through a link is only read, so its copy stays, and the agent is told.

Arko keeps the project's `.arko` folder out of git by default. See [the .arko folder](arko-folder.md).
A `.arko` or `.arko/profiles` that is a link is refused, so a save can't land outside the project.

In a subfolder of a git repository, Arko also reads the repository root's `.arko/profiles` and
`.agents/profiles`, after the working directory's own. "Repo root" under "Worker profiles" in
`/settings` turns off this lookup. It is on by default and kept for every project in
`profiles.json` in Arko's folder.

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
approval, and neither does one for a project folder that git ignores (`git check-ignore` says),
which is the folder Arko makes by default. Before Arko saves one in a folder git tracks, in a
project outside a git repository, or for every project, it waits for your OK in the
[approval tray](approvals.md), which shows the whole profile and the folder it goes in. Reject with
words in the message box sends them back to the agent, and the transcript says nothing was saved.
The agent makes the changes they ask for, or replies when they are about something else, and asks
again. The agent can't replace a profile you made.

The approval guards what Arko writes for the agent. The agent's own file tools and shell can write
a file in the project's `.arko/profiles` folder as they can write any file in the project, and
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
| `/worker-billing plans`, `ask` or `api` | Whether workers may run on models billed per token. See [Worker billing](#worker-billing) |
| `/api-budget DOLLARS`, `on` or `off` | A daily budget for what is billed per token |
| `/worker-depth 1` to `4` | How many levels of workers the session allows |
| `/worker-retry-cap off`, `on` or a number | How many failed model requests in a row stop a worker |
| `/message-copies on` or `off` | Whether the agent that started a worker gets a copy of the messages you send the worker |

`/worker-models` (also `--worker-models`) takes three values. It is the "Models for workers" row in
`/settings` and the setting of that name on the web page. With `agent`, the default, the main agent chooses a profile, model and effort for
each worker and can save profiles. With `profiles`, it starts workers with a profile only. With
`session`, every worker runs the session's worker model and none starts workers. The setting applies to the next start.
A session saved before the setting existed keeps `session`, which is how its workers always ran.

`/worker-depth` (also `--worker-depth`) is saved with the session and defaults to 2. It is the
"Worker depth" row in `/settings` and the setting of that name on the web page. It sets how many
levels of workers the session allows, and it applies to the next worker started. With 1 only the
main agent starts workers. With more, a worker can start workers of its own when the agent
that starts it says so, down to that level.

`/worker-retry-cap` (also `--worker-retry-cap [N]`) is saved with the session and is off by default. It
is the "Worker retry cap" row in `/settings` and the setting of that name on the web page. With a
number N, `on` or the flag without a number (which mean 10), a worker fails after N failed model
requests in a row, and the workers it started stop with it. A model response starts the count
again. The main agent has no cap. A change applies at once, and a worker already running has the
new cap at its next failed request.

`/message-copies` (also `--message-copies` and `--no-message-copies`) is saved with the session and
is off by default. It is the "Message copies" row in `/settings` and the setting of that name on the
web page. With it on, the agent that started a worker gets a copy of each message you send the
worker, with its next news and without a turn of its own. With it off, that agent doesn't see your
messages. A change applies from the next message, and a session saved before the setting existed has
it on.

## Worker billing

`cc:` models run on your Claude plan, and OpenAI models on your ChatGPT plan when the session uses
the ChatGPT login. Models on an API key, Gemini and OpenRouter are billed per token.
`/worker-billing` (also `--worker-billing`, and `ARKO_WORKER_BILLING` for new sessions) says
whether workers may run on those. It is the "Worker billing" row in `/settings` and the setting of
that name on the web page, and it applies to the next worker started.

| Value | A worker on a model billed per token |
|---|---|
| `plans` | Is refused, and the main agent is told which plan models it can use |
| `ask` (the default) | Starts once you approve it in the [approval tray](approvals.md), which names the model and its price |
| `api` | Starts without asking |

Approve starts that worker, and with "Allow MODEL for this session" turned on in its details, that
model's next workers in the session too. Reject refuses it. The main agent waits for your answer, and
the words in the message box reach it with the answer. Only the main agent's starts can ask, so a worker's own
start on a model billed per token is refused unless you allowed that model for the session. With
`plans`, `/model worker` and the profiles the main agent saves take a plan's model only. The main
agent's own model stays your choice, and its instructions tell it to prefer your plans' models for
workers.

`/api-budget` (also `--api-budget`, and `ARKO_API_BUDGET` for new sessions) sets a daily budget in
dollars, off by default and $10 with `on`. It counts what your sessions were billed per token since
local midnight, the main agent's calls included. Each session shares its spend with the others in
`api-spend.json` in Arko's folder. Once the day's spend reaches the budget, a new worker on a model
billed per token is refused with `api`, and asked about with `ask` even for a model you allowed for
the session. You're told once, and running workers go on.

`worker_options`, the `/model` picker and the web page's `/model` choices show `API, $5/$25` (the
price per million input and output tokens) beside a model billed per token, and the result of a
start says it too.

## Skills and MCP servers for a worker

A worker has the skills and MCP servers of the agent that started it, except a skill whose front
matter says `workers: false` and a server whose entry says `"workers": false`, which stay with the
main agent (see [skills](skills.md) and [MCP servers](mcp.md)). `start_agent` takes `skills` and
`mcp` to give a worker fewer. Each is names separated by commas, a JSON array of names or `none`.

```
skills: ponytail, release-notes
mcp: none
```

- A worker started with names gets exactly those. Each must be one its starter has and can give,
  or the start is refused with the names the starter can give.
- A worker started with neither has what its starter has. A worker with a list passes the list on,
  so a set carries through every depth and each level can only narrow it.
- A profile's `skills` and `mcp` work the same way when the call leaves them out. A call's names win
  over the profile's. A profile serves many starters, so its names are crossed with the starter's,
  and the start result says which ones were left out.
- `save_worker_profile` takes both and refuses names the main agent can't give a worker. The
  profile's approval says what workers with it get.
- A worker keeps its set when its session resumes. One with everything sees skills saved and servers
  added later. One with a list sees only its list.

The start result, `list_agents` and `/workers` name a set only when it is narrowed. A worker with
no MCP servers isn't offered the `mcp` tool.

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
a turn, when message copies are on. A worker reaches the one that started it with `message_agent`
to `lead`, and `link_agents` lets a worker's own workers message each other. So does starting them
with the same `group` in `start_agent`. A worker can't message another worker's workers.
`/stop-workers r1` stops r1 and the workers under it, and only the worker that started a worker can
restart it with a message.

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

`profiles::Preferences` loads and saves the profile settings, and `profiles::needs_approval` tells
whether the agent's save asks you first. `Preferences::save_in` also adds or removes Arko's
`.gitignore` in a project's `.arko` folder when the keep-out-of-git choice changes
(`arko::arko_dir::apply_setting` does that alone). After saving it, or the switches of
`arko::extension_prefs`, call `Details::refresh_settings` on each open session, so its web page
shows the change with nothing in its transcript. `Profile::check` tells whether a profile can be
saved, with a model Arko knows and an effort it takes. `profiles::folders` gives the folders for an
"open the folder" button, the project's `.arko/profiles` and then its older `.agents/profiles`, and
`Found::reach` names the older one for a profile read from it. `profiles::approval` is the request
the agent's save puts in the approval tray. `arko::workers` has the names, commands and sentences
of the workers' settings, and `ModelChoice::label` and `about` and `WorkerBilling::label` and
`about` the words for each value, so an app labels and sets them as the terminal and the web page
do. `arko::models::billing` gives the label a picker shows beside a model billed per token.

A worker's summary (`WorkerSummary`) names the worker that started it in `parent`, its `depth`
(1 for the main agent's workers), whether it may start workers of its own in `nested`, and what it
and the workers under it cost in `family_cost_billed` and `family_cost_list`. A worker that failed
has the whole reason in `error`, and the feed sends `Update::WorkerEnded` when a worker finishes,
fails or stops. `arko::remote::family_order` puts a list of summaries in the order a list shows a
family, with each worker's workers after it. `family_order_with_levels` also gives each worker's
level, counted from the roots in the list.
