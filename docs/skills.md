# Skills

A skill is a folder with a `SKILL.md`, which starts with a name and a description in YAML front
matter and goes on with instructions. The agents see each skill's name and description, and load one
with `load_skill` when a task calls for it. A skill with `auto_load: true` in its front matter is
always in their instructions instead.

Arko reads skills from the project's `.agents/skills`, `.claude/skills`, `.skills` and
`.arko/skills`, from `skills` in Arko's folder, and from `~/.agents/skills` and `~/.claude/skills`.
When two skills share a name, the first one read keeps it and the other loads as `name-2`. `/skills`
lists them with their descriptions and folders, and `/skills NAME` shows one with its files.

A worker has the skills of the agent that started it, or fewer when that agent says so. See
[Skills and MCP servers for a worker](worker-profiles.md#skills-and-mcp-servers-for-a-worker).

## Skills for some models

Two more fields of the front matter decide who sees a skill.

| Field | Effect |
|---|---|
| `models` | The models whose agents list the skill, one name or a list. Other agents can still load it by name. |
| `workers: false` | Only the main agent gets the skill. |

An entry of `models` is matched against the whole name, ignoring case, and `*` and `?` work as in
file names. An exact ID such as `claude-haiku-5-5` pins the skill to that release, and a short name
or pattern such as `haiku` or `gpt-6*` follows the family. `cc:haiku` reaches only Haiku in Claude
Code. A plain name reaches a model on OpenRouter only through Arko's table of the slugs OpenRouter
gives the models Arko knows, so `haiku` lists the skill for `openrouter:anthropic/claude-haiku-5.5`
and not for another publisher's lookalike. For any other slug, write the pattern against it, such as
`openrouter:openai/gpt-6*`. `/skills` shows the models after a skill's folder ("for haiku"), and
"main agent only" for `workers: false`. Claude Code itself ignores both fields in a folder it
shares with Arko.

## Skills the agent saves

The main agent saves a skill with `add_skill`, from a folder or Markdown file it wrote outside the
skill folders, a `.zip` or `.skill` bundle, or an https URL of one. A skill for the project goes in
its `.arko/skills`, which git ignores unless you say otherwise (see
[the .arko folder](arko-folder.md)). A skill for every project goes in `skills` in Arko's folder.

| Save | Asks you |
|---|---|
| For every project | Always |
| From a URL | Always |
| For the project | Only with `review-skills` on (see [Approvals](approvals.md#what-asks-first)) |

The [approval tray](approvals.md) shows the skill's text and files, where it came from and who gets
it. Arko checks the skill before it asks and refuses one with `auto_load`, a link, more than 200
files or more than 1 MB, or the name of a skill you made. A skill the agent saved before under that
name is replaced, and the request says so. Nothing is written before you approve, and what's saved
is the copy the tray showed, even if the source changes meanwhile. If a skill of that name appears
while you decide, or the agent's skill it would replace is edited, Arko doesn't save it and the
agent asks again. An edit in the last moment keeps the old skill's folder, hidden beside the new
one, and the agent is told where. An agent's skill you filled with more than 1,000 files and
folders or 16 MB isn't replaced. A `.arko`, `.arko/skills` or `skills` folder that is a link is
refused, so a save can't land outside its place. The agent's `load_skill` finds the skill at once,
and its skill list shows it from the next conversation. Workers started after the save get it
unless it says `workers: false`.

Arko records each save in `agent-added.json` in its folder, so `/skills` marks the skill "added by
the agent" with the day, and `/skills NAME` says why and from where. The agent's `remove_skill` and
your `/skills remove NAME` remove only such a skill, without asking. Delete your own skills by hand.

## Built-in skills

Arko has three skills built in, so no files are needed.

| Skill | Load it for |
|---|---|
| `user-facing-prose` | Any text a person reads, such as UI text, messages, errors, docs and commit messages |
| `repo-docs-style` | READMEs, docs pages, design notes and changelogs |
| `ponytail` | Any coding task, and running workers |

The agents load them on demand like any other skill, and `/skills` lists them as built in.

### Replace one

A skill of the same name in any skill folder replaces the built-in one, and `/skills` shows its
folder.

### Leave them out

| Leave out | Every new session | One session |
|---|---|---|
| All | `ARKO_BUILTIN_SKILLS=0` | `--no-builtin-skills` |
| Some | `ARKO_BUILTIN_SKILLS_OFF=repo-docs-style` | `--no-builtin-skill repo-docs-style` |

The settings go in the environment or in a `.env` file. Separate several names with commas, and
quote the list in a `.env` file if it has spaces. A flag beats the setting.

`/builtin-skills on` or `off` changes all of them as you go, and `/builtin-skills repo-docs-style off`
one. Both are rows in `/settings` and on the web page. Before the conversation starts the change
reaches the agents. After that the conversation going on keeps its skills, and new workers and new
conversations get the change.

The session keeps its choice. Resuming with a flag changes it for the conversations that start
after, and a conversation already started keeps the skills it began with. A name that isn't a
built-in skill in the environment setting or in `Options` is skipped, and Arko says so. The
`--no-builtin-skill` flag refuses a name it doesn't know.

## For apps

`Options::builtin_skills` and `Options::builtin_skills_off` do what the flags do, and
`arko::builtin_skills::names()` gives the names they take. The facts from `Details::skills()` mark
each built-in skill (`builtin`), give the path of a skill that replaces one (`replaces_builtin`),
give the folder or file of each skill that isn't built in (`path`) and list the built-in skills left
out (`builtin_off`). `arko::extensions::list` includes the built-in skills as the environment's
settings leave them, and `arko::extensions::list_with` as the app's `builtin_skills` and
`builtin_skills_off` leave them. Each has `builtin` set and an empty `path`. The live settings
(`remote::LiveSettings`) hold the choice in `builtin_skills` and `builtin_skill_list`.

Each `SkillFacts` also has `models`, `workers` and `added`, the record of an agent's save
(`arko::agent_added::SkillRecord`, with the day, the session, the reason and the source) or `None`
for a skill no agent saved. [Listing extensions](extensions.md) has the same on `SkillEntry`, and
`remove_added` removes a skill an agent saved without a session.
