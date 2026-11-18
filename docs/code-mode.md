# Code mode

With code mode on, the agents have one more tool, `run_code`. It runs a short JavaScript program
that calls their tools, so a loop over many files, a filter or a chain of calls happens in the
program, and only what the program returns goes back to the model.

```sh
arko --tools both
```

`ARKO_TOOLS=both`, in the environment or a `.env` file, does the same for every new session.
`classic`, without `run_code`, is the default. A resumed session takes `--tools` for the
conversations and workers that start after.

## What the code does

The code is the body of an async function. `await tools.read_file({ path })` calls a tool with
the arguments it takes as a tool and gives its result as text. `return` gives the result, and
`console.log` adds lines above it.

```js
const hits = [];
for (const path of ["src/engine.rs", "src/harness.rs"]) {
  const text = await tools.read_file({ path });
  const count = (text.match(/TODO/g) ?? []).length;
  if (count) hits.push(`${count} ${path}`);
}
return hits.join("\n");
```

Code can call `read_file`, `list_files`, `index_search`, `index_status`, `shell`, `write_file`
and `apply_diff`. Jobs that outlive a run, workers, questions, messages, skills, notes and MCP
servers stay direct calls.

Each call passes the same checks as a call of the model's, meaning outside access, the command
guard, the path and approval policies, and every hook. A call that would wait for your approval
is refused at once with a note to call the tool directly. A failed or refused call throws an
`Error` the code can catch, and the third refused call ends the run.

## Limits

| | |
|---|---|
| Computing time, waits for tools not counted | 30 seconds |
| The whole run | `timeout_seconds`, 5 minutes by default and at most an hour |
| Memory | 64 MB of JavaScript heap |
| Tool calls | 500 |
| Refused calls | 3 |
| The code | 100 KB |
| The result | 30,000 characters, cut with a line saying how much was left out |
| One result into the code, one call's arguments | 8 MB each |

A run that fails says its line in the code and what it had changed (files written or edited,
commands run).

## The code runner

The code runs in a process of its own, Arko started again as a code runner, one for each run. It
gets none of Arko's environment, so none of its keys, and it has no files, network or processes
of its own. It reaches the tools only through Arko. Before it reads the code it sets its limits
with the operating system.

| | Linux | macOS | Windows |
|---|---|---|---|
| Memory | `RLIMIT_AS` (1 GB above its start) and `RLIMIT_DATA` (512 MB) | `RLIMIT_AS` | A Job Object's process memory (512 MB) |
| Processor time | `RLIMIT_CPU` (60 seconds) | `RLIMIT_CPU` | The Job Object's user time |
| Other | No core dumps or file writes, and `oom_score_adj` 1000 | No core dumps or file writes | One process, no error dialog |

Arko also ends a runner whose resident memory passes 512 MB on Linux and macOS. A runaway
script, or a fault in the engine, ends the runner and never Arko, and the agent reads why.

## In an app

The runner is the same program as the one that runs the session. An app built on the library
whose sessions run code calls `arko::code_mode::run_if_asked()` first in `main`. Until it does,
`run_code` says the app can't run code.
