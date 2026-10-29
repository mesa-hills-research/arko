# The .arko folder

What agents save in a project goes in the project's `.arko` folder, so other tools that read
`.agents` or `.claude` never see Arko's files.

| In `.arko` | Holds |
|---|---|
| `skills` | Skills the agent [saves](skills.md#skills-the-agent-saves) for the project |
| `mcp.json` | MCP servers the agent [adds](mcp.md#servers-the-agent-adds) for the project |
| `profiles` | [Worker profiles](worker-profiles.md#where-profiles-live) the agent saves for the project |

Arko reads what you put there yourself in the same way. A `.arko`, or a folder or `mcp.json` in
it, that is a link is refused, so a save can't land outside the project. The agent replaces or
removes a skill or a server only in the very folder it saved it in, so a `.arko` moved in from
elsewhere keeps what it holds.

## Keeping it out of git

When Arko makes `.arko`, it writes a `.gitignore` in it that holds a comment and `*`. Git then
ignores the folder and that file, and your repository's own `.gitignore` stays as it is.

To share the files with your team, delete `.arko/.gitignore`. Arko writes it only when it makes
the folder, so it doesn't come back. To stop writing it for every project, turn off "Keep agents'
project files out of git" with `/extensions keep-out-of-git off`, or in `/settings` under
"Project files" and on the web page. Turning it off removes Arko's `.gitignore` from the
session's project and from other projects at the next save there. A `.gitignore` you edited stays.
Turning it on writes one into an `.arko` that has none.

The choice is kept for every project in `profiles.json` in Arko's folder. With it off, a worker
profile saved in a git repository asks for your [approval](approvals.md), since git no longer
ignores the folder.
