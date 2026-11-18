# Code mode, first deliverable: rules to keep

Working notes for the `code-mode` branch (Arko) and the `code-mode` branch of arko_framework.
They come from DESIGN.md (sections 1, 3 and 7) and DECISIONS.md, with the owner's later
changes: the engine runs in a child process, every coding tool is callable through the gate
from the start, and the language is JavaScript only. This file goes in the last commit.

## Scope

- `run_code(code, purpose, timeout_seconds)`, offered beside today's tools under
  `--tools both` (`ARKO_TOOLS`, `tools` in `session.json`). New sessions default to `classic`
  until the quick regression check passes, and then to `both` (DECISIONS.md). Sessions saved
  without the setting read as `classic` either way.
- `run_code` is always registered, as a resume-only tool for the main agent and the workers. A
  conversation whose configuration lists it is offered it.
- JavaScript only. The description teaches the API in plain JavaScript terms.
- No transcript drawing beyond the minimum. The row of step 2 waits for the owner.

## Callable from code

Exactly these seven, a constant in one place, and nothing else:

| Tool | Read-only |
|---|---|
| `read_file`, `list_files`, `index_search`, `index_status` | yes |
| `shell`, `write_file`, `edit_file` | no |

Never callable, whatever the agent has: `index_create`, `index_manage`, `terminal_start`,
`terminal_status`, `terminal_input`, `terminal_control`, `monitor_start`, `monitor_control`,
every worker tool (`start_agent`, `message_agent`, `wait_for_agents`, `stop_agent`,
`list_agents`, `link_agents`, `worker_options`, `save_worker_profile`, `run_workflow_agent`),
`ask_user`, `close_questions`, `message_user`, `complete_task`, `handover_report`, `load_skill`,
`add_skill`, `edit_skill`, `remove_skill`, `keep_note`, `forget_note`, `mcp` and every
MCP server's tool, `earlier_conversation`, `apply_diff`, and `run_code` itself. These start
jobs that outlive the run, spend money or plans, wait for a person, or are the turn's own
protocol.

## The gate (arko_agents)

Every call from code goes through one function in arko_agents, reached through `TaskContext`
so the native loop and the Claude Code runtime both have it. There is no direct path, for
reads either. It keeps the design's six rules.

1. Only a tool in the agent's registry, never `complete_task`, `ask_user`, `close_questions`,
   `handover_report` or the calling tool itself. `run_code` narrows that to the seven.
2. The path policy and the approval policy, by the same function the loop uses, so the two
   can't drift. A call that would wait for approval is refused at once, with a message to call
   the tool directly. It never waits.
3. The middleware's `before` and `after` around the tool, so the outside-access hook and every
   later hook see each call. A nested decision to fail, cancel or pause the task does that at
   the outer call.
4. The tool's own time limit, and a `TaskContext` of its own with a new call ID and the outer
   call's stop signal. A nested call gets no gate of its own.
5. Calls run one at a time, in the order made.
6. Two new events, a start and an end, each naming the outer call. They are not
   `ToolCallFormatted` and `ToolResult`. An end holds at most 4 KB of the result and its full
   length.

A refusal inside a tool (the command guard) reaches the gate as a refusal, not a failure.
The framework change ships with arko_agents 0.9.0. Arko takes it through a `[patch.crates-io]`
git dependency at a full SHA, in a commit marked TEMPORARY.

## The child process

- The interpreter never runs in Arko's process. Each run starts a fresh child, the same binary
  with the hidden argument `code-runner`, and talks to it over stdin and stdout in frames, a
  4-byte little-endian length and then UTF-8 JSON. Only strings cross.
- A tool call from the script is a frame to Arko. Arko runs it through the gate and sends the
  answer back. The child holds no key, credential or tool.
- The child sets its limits before any code runs, and the run fails if it can't:
  - Linux and macOS: `RLIMIT_DATA` for memory, `RLIMIT_CPU` above the computing limit,
    `RLIMIT_CORE` and `RLIMIT_FSIZE` at 0
  - Linux: `oom_score_adj` raised to 1000, so the OOM killer takes it first
  - Windows: a Job Object with a process memory limit, a user-time limit, one active process,
    and death on an unhandled exception
- Arko kills the child at the whole run's time limit, on a stop (the dropped future and
  `TaskContext::stop`), on a protocol error, and at the refusal cap. The child exits when its
  stdin closes, so it dies with Arko.
- A child that dies, is killed or breaks the protocol is a clean tool error, never an Arko
  failure. No panic, unwrap or blocking call on a tokio worker on Arko's side.
- One child per run. Its start (process plus engine) is measured and reported.

## The engine seam

- rquickjs 0.14.0 with `std` only, behind a Cargo feature `code-mode` that is on by default.
  Without it, `run_code` is a stand-in that says the build has none.
- One file names rquickjs. Everything else talks to the six functions: `start(limits, stop,
  to_host)`, `eval(source)`, `call(function, text)`, `run_jobs()`, `heap_bytes()`, and the
  `stop` it polls. Strings are the only values that cross.
- The prelude is standard ECMAScript only, nothing QuickJS-ng adds. `tools` and `console` are
  frozen, and the prelude's entry points can't be replaced.
- The host decides why a run ended from its own flags (computing time, the call cap, a stop),
  never from the engine's error text.
- In the child, JavaScript runs on one thread of its own (`arko-code`, 2 MB of stack). The
  interpreter is made on it and never leaves it. No lock is shared with it.
- No panic crosses into the engine. Engine failures are values.
- Line numbers in errors are the code's own, so the wrapper adds no line before it.

## Limits

Set before any code runs, as constants in one place.

| Limit | Value | Enforced by |
|---|---|---|
| Computing time, waits for tools not counted | 30 s | The interrupt handler, and `RLIMIT_CPU` or the Job Object as a backstop |
| Whole run | `timeout_seconds`, the shell's default and maximum | Arko kills the child |
| Each tool call | The tool's own | The gate |
| Heap | 64 MB | QuickJS, and the OS limit above it as a backstop |
| Stack | 512 KB on a 2 MB thread | QuickJS |
| Tool calls in a run | 500 | The child's native function, and Arko as a backstop |
| Refused calls in a run | 3, then the run ends | Arko |
| Code size | 100 KB | The tool, before a child starts |
| Result | 30,000 characters, cut with a line saying how much was left out | The tool |
| One result into the code | 8 MB | Arko |

## Results

- What the code returns, after its `console.log` lines. A string as it is, anything else as
  compact JSON.
- A failed call rejects with an `Error` whose message is the tool's. A refused one has
  `refused` set. Code may catch either.
- Calls not awaited finish before a successful result goes back, and a failure among them is
  reported.
- An uncaught error gives its message, its line in the code, the log so far, and one sentence
  on what the run had changed (files written or edited, commands run).
- A run that reaches a limit names it, with the same sentence.
- No state between runs.

## Tests that gate it

- The engine tests from the spike, with outcomes asserted: a value, a thrown error and its
  line, the deadline in a loop, inside `try`, in a regex and after an `await`, the heap cap,
  the stack cap, a promise nothing settles, the allowlist of globals, `import`,
  `Atomics.wait`, a replaced entry point, 600 calls against the cap of 500.
- The child: its OS limits hold, a crash or a kill is a clean error, a stop kills it within a
  second (while it loops and while a call is in flight), several runs at once on a
  multi-thread runtime with a progress timeout.
- The tool on a temporary folder: a fan-out of reads, a missing file that rejects and is
  caught, an uncaught error's line, a long result cut.
- The gate, each test written to fail if the gate is bypassed: a write outside the working
  directory refused while outside access is off, a guarded command refused and the third
  refusal ending the run, a call that needs approval refused instead of waiting, a middleware
  that rejects, one that fails the task at the outer call, the forbidden tools, the events in
  order with the 4 KB cut, and the same through the Claude Code runtime's harness.
- With the mock model, a session under `--tools both` that calls `run_code`, and one under
  `classic` whose request doesn't offer it.
- The description as a golden.

## Process

- Dependencies as the design's table says, added to the existing lock without `cargo update`,
  checked with `check_dependency_ages.py` before the first build, everything at least 30 days
  old.
- Builds, tests and clippy only through `~/Desktop/arko-framework-work/bin/cargo-locked`.
  `fmt` and `clippy --all-targets -D warnings` before the last commit.
- A CHANGELOG entry under Unreleased.
- Small commits, no push or merge of Arko's branch. Only the framework branch is pushed.
