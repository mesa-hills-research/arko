# Skills

A skill is a folder with a `SKILL.md`, which starts with a name and a description in YAML front
matter and goes on with instructions. The agents see each skill's name and description, and load one
with `load_skill` when a task calls for it. A skill with `auto_load: true` in its front matter is
always in their instructions instead.

Arko reads skills from the project's `.agents/skills`, `.claude/skills` and `.skills`, from `skills`
in Arko's folder, and from `~/.agents/skills` and `~/.claude/skills`. `/skills` lists them.

## Built-in skills

Arko has two skills built in, so no files are needed.

| Skill | Load it for |
|---|---|
| `user-facing-prose` | Any text a person reads, such as UI text, messages, errors, docs and commit messages |
| `repo-docs-style` | READMEs, docs pages, design notes and changelogs |

The agents load them on demand like any other skill, and `/skills` marks them as built in.

### Replace one

A skill of the same name in any skill folder replaces the built-in one. `/skills` says where the
replacement is.

### Leave them out

| Leave out | Every new session | One session |
|---|---|---|
| All | `ARKO_BUILTIN_SKILLS=0` | `--no-builtin-skills` |
| Some | `ARKO_BUILTIN_SKILLS_OFF=repo-docs-style` | `--no-builtin-skill repo-docs-style` |

The settings go in the environment or in a `.env` file. Separate several names with commas, and
quote the list in a `.env` file if it has spaces. A flag beats the setting.

The session keeps its choice. Resuming with a flag changes it for the conversations that start
after, and a conversation already started keeps the skills it began with. A name that isn't a
built-in skill keeps the session from opening.

## For apps

`Options::builtin_skills` and `Options::builtin_skills_off` do what the flags do. The facts from
`Details::skills()` mark each built-in skill (`builtin`), give the path of a skill that replaces one
(`replaces_builtin`) and list the built-in skills left out (`builtin_off`).
`arko::extensions::list` includes the built-in skills as the environment's settings leave them,
with `builtin` set and an empty `path`.
