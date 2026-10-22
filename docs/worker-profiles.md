# Worker profiles and a model for each worker

The main agent starts workers with `start_agent`. A worker runs on the session's worker model
unless the main agent chooses otherwise, either with a profile or with a model and an effort.
A profile is a named setup for workers. It holds a model, an effort and instructions, and the
main agent starts a worker with it by name.

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
| `isolated` | `true` to start the worker in a git worktree of its own |
| `nested` | `true` to let the worker start workers of its own, once workers can |
| `made_by` | `agent` when the main agent saved the profile |

A name is lowercase letters, digits and hyphens, up to 32 characters, and starts with a letter
or a digit. `default`, `main`, `worker`, `lead` and `all` are taken. The instructions can run to
4,000 characters, and a file over 16 KB is left out. A file that can't be read shows in
`/profiles` with the reason and hides no other profile. Windows line endings and a byte order
mark are fine.

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
- Fast mode is on only when the profile says `fast: true`, or when the worker runs the session's
  worker model and the session has fast mode on.
- Each worker hands over at the size of its own model, and `/handover-at workers SIZE` still
  applies to all of them, each up to what its model leaves room for.
- A profile's instructions open the worker's first message, after the project's instructions.
- A model the session can't reach is refused with what it needs. A Claude model needs
  `ANTHROPIC_API_KEY`, a `cc:` model needs Claude Code signed in, and so on.

## What the main agent can do

Two tools help it choose. `worker_options` lists the profiles and the models usable now, with
each model's efforts, price, handover size and prompt cache, and the plan usage next to models on
a plan. `save_worker_profile` saves a setup under a name. A profile for the session needs no
approval. One for the project or for every project is put to you as a question that shows the
whole profile, and only a plain yes saves it. Anything else you write goes back to the agent as
your feedback. The agent can't replace a profile you made.

## Commands and settings

| Command | What it does |
|---|---|
| `/profiles` | Lists the profiles, those they hide, those that can't be read, and the folders |
| `/profiles NAME` | Shows one profile whole, with its file |
| `/profiles add NAME MODEL [EFFORT] [DESCRIPTION]` | Writes a profile for every project and prints its file to edit |
| `/profiles remove NAME` | Deletes the profile `/profiles` shows under that name |
| `/worker-models agent`, `profiles` or `session` | Who chooses the workers' models. See below |
| `/worker-depth 1` to `4` | How many levels of workers the session allows |

`/worker-models` (also `--worker-models`, and a row in `/settings` and on the web page) takes
three values. With `agent`, the default, the main agent chooses a profile, model and effort for
each worker and can save profiles. With `profiles`, it starts workers with a profile only. With
`session`, every worker runs the session's worker model. The setting applies to the next start.

`/worker-depth` (also `--worker-depth`) is saved with the session and defaults to 2. It sets how
many levels of workers the session allows, and it takes effect once workers can start workers of
their own.

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
the user and the rule for its answer. `arko::workers` has the names and commands of the two
settings, so an app labels and sets them as the terminal does.
