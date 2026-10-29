# MCP servers

MCP servers are programs that give the agents more tools. Arko starts them when a session opens,
talks to them over standard input and output, and gives the agents one tool, `mcp`, to reach all of
them. The tool never changes, so a server can start, stop, come or go in the middle of a
conversation without losing the prompt cache, and a large server's tool list stays out of every
request.

## Setting servers up

Arko reads servers from these files, in this order. The first file that names a server wins.

| File | Notes |
|---|---|
| Files named with `--mcp-config FILE` | Can be given more than once |
| `.arko/mcp.json` in the project | Where the agent [adds servers](#servers-the-agent-adds) |
| `.mcp.json` in the project | Claude Code's file for a project |
| `mcp.json` in Arko's folder | Servers for every project |

Arko asks before it starts the servers in a project's `.arko/mcp.json` or `.mcp.json`, since they
run programs as soon as Arko opens there, and remembers your answer until the file's servers change.

A file uses Claude Code's format.

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" },
      "workers": false
    }
  }
}
```

A list of servers that each have a `name` works too. `${VAR}` and `${VAR:-default}` in a command,
its arguments and its environment come from Arko's environment. Servers reached by a URL and
entries with `"disabled": true` are skipped. A server gets 60 seconds to start and list its tools,
and a chat starts none.

Two fields are Arko's own.

| Field | Effect |
|---|---|
| `"workers": false` | Only the main agent reaches the server. Workers get the others |
| `"shared": true` | Sessions open in one app share one copy. See [Shared servers](#shared-servers) |

## The mcp tool

The main agent's `mcp` has seven actions, and a worker's has the first four.

| Action | Does |
|---|---|
| `servers` | Lists the servers with their state, how many tools each has and what it is for |
| `tools` | Lists a server's tools, and gives the full input schema of those it names |
| `call` | Runs a tool, starting its server first if it stopped |
| `start`, `stop` | Starts a server that stopped or failed, or stops one. `stop` ends the programs the server started too |
| `add`, `remove` | Sets up a new server or takes out one an agent added. Main agent only |

A worker has the servers of the agent that started it. See
[Skills and MCP servers for a worker](worker-profiles.md#skills-and-mcp-servers-for-a-worker)
to give it fewer. A worker with no servers isn't offered `mcp`.

A conversation saved when each server tool was a tool of its own keeps those tools, also after a
handover, while new conversations and new workers get `mcp`. When a server of theirs doesn't
start, its tools keep the definitions they had when the session last ran it, so the prompt cache
holds, and a call says the server isn't running. A session an older Arko opened last has none of
those definitions kept, so its tools get plain ones and the conversation's prompt cache starts
again.

## Servers the agent adds

The main agent adds a server with `mcp add`, for the project or for every project. The server
goes in the project's `.arko/mcp.json` (see [the .arko folder](arko-folder.md)) or in `mcp.json` in
Arko's folder. The agent names the environment variables a server needs and never their values.
Arko writes each as `"NAME": "${NAME}"`, so the value comes from your environment when the server
starts.

Adding a server waits for your OK in the [approval tray](approvals.md). The request shows the
command, the variables and which ones aren't set, whether workers get the server, where it is
kept, and the agent's reason. Once you approve, the server starts at once, unless a variable it
needs isn't set, and the project's file is remembered as approved, so the next session starts it
without asking. A server runs with your
permissions, outside the command guard, so read the command before you approve. You can let agents
add servers without asking with `/extensions ask-mcp off` (see [Approvals](approvals.md#what-asks-first)).

An add that names a server another file sets up is refused. An add that names a server the agent
added before replaces it. The agent can remove only servers that agents added, and doesn't ask.

## Shared servers

Each session runs its own copy of every server and stops it when the session closes. A server
marked `"shared": true` runs once for all the sessions open in one app, as long as their command,
arguments and environment match, and it stops when the last one closes. A session that stops it
only lets go of it.

Sharing suits a server that keeps nothing for a particular client. A browser server, one that
indexes a project, or one that remembers state for each client mixes the sessions up when shared.
The agent proposes sharing only after it has checked that the server

- keeps no state for a client or a session,
- holds no browser or open files,
- uses no paths, environment or credentials that belong to one project, and
- behaves the same in every session.

When it isn't sure, it leaves the server unshared. Its reason says what it checked, and the request
shows that reason under "Why shared". Read it before you approve, and set `shared` in a file by
hand for a server you know is safe.

## Commands

| Command | Does |
|---|---|
| `/mcp` | Lists the servers with their command, file and tools |
| `/mcp NAME` | Shows one server whole, with its variables ("set" or "not set"), whether workers get it, its tools, and who added it and why |
| `/mcp remove NAME` | Removes a server an agent added from its file and stops it. For any other server, it names the file to edit |

When agents may add servers without asking, `/mcp` ends with a line saying so.

## For apps

`arko::extensions` lists servers and their files without a session, and removes what an agent
added. See [listing extensions](extensions.md).
