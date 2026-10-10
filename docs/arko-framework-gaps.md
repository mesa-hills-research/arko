# What fin needs from arko_framework

fin is built on arko_ai, arko_agents and arko_workflows. This page lists the places where fin works around something the framework can't do yet. For each one it gives what arko does today, what fin does instead, and the change that would let fin drop its workaround. The list starts with the biggest gaps.

Paths are relative to the arko_framework repository. Line numbers are from upstream commit `023c92c` (origin `HEAD` when this was written). fin itself builds against a local checkout at `8f8569a`, 142 commits behind it. Where the two differ, the entry says so.

| # | Gap | Priority |
|---|-----|----------|
| 1 | Subagents block the agent that starts them | High |
| 2 | Lead and subagents can't talk while a subagent runs | High |
| 3 | Every run in a workflow shares one working directory | High |
| 4 | No isolated workspaces (git worktrees) for parallel subagents | Medium |
| 5 | Stopping a single run (upstream only) | Medium |
| 6 | No way to know whether a note or message was read | Medium |
| 7 | Resumed runs keep their old tools and instructions | Medium |
| 8 | MCP servers: stdio only, no working directory | Medium |
| 9 | A failing `ask_user` handler ends the run | Low |
| 10 | Terminal tool rough edges | Low |
| 11 | The mock model server is private to arko's tests | Low |
| 12 | Base URLs can't be set from the environment | Low |
| 13 | Missing re-exports | Low |
| 14 | Skills: paths, scripts and sources | Low |
| 15 | A reply without a tool call counts as a failure | Low |

## 1. Subagents block the agent that starts them

**Today.** `run_workflow_agent` runs the target agent to completion inside the tool call. `execute` awaits `WorkflowEngine::run_nested` (`arko_workflows/src/services/mod.rs:243-249`), and the tool's own doc says "The call runs another agent to completion, however long that takes" (`:675`). Its parameters are `agent`, `prompt` and `resume_run` (`:664-667`), with nothing that returns early. The result comes back only as the tool result.

The tool is marked `parallel_safe: false` (`:669`), so several calls in one response run one after another unless the host overrides it per tool. Calls running at once are capped by `WorkflowServicesConfig::max_active_calls`, default 4 (`:47-58`), and a call over the cap fails at once instead of waiting (`:260-270`). The cap can't be turned off: `max_active_calls(0)` is a build error.

Hosts can start runs without blocking (`Workflow::spawn_agent`, `run_agents_parallel`), but an agent can't.

**Effect.** A lead that hands out work sits idle until every subagent finishes, even when it could carry on with something else or answer the user.

**fin's workaround.** fin has its own lead tools in `src/crew.rs`: `start_worker` returns a worker id at once, and each worker runs in the background as its own `Workflow`. A worker's report reaches the lead as a note while its turn runs, or starts a new turn when it's idle. `wait_for_workers`, `list_workers` and `stop_worker` complete the set. In `fin -p`, fin waits for the workers' reports before it exits.

**Request.**
- A way for an agent to start a subagent in the background: a `start_agent` tool, or a `background: true` parameter on `run_workflow_agent`. It returns a run id at once.
- The run's result (its `complete_task` summary, or its failure) delivered to the caller at its next turn boundary, the way notes are delivered, plus a `WorkflowEvent` so the host can start a turn for an idle caller.
- A `wait_for_agents` tool that returns when any of the given runs finishes, with its result, and a tool that lists the caller's runs and their state.
- An unlimited setting for `max_active_calls`, and an option to queue calls over the cap instead of failing them.

## 2. Lead and subagents can't talk while a subagent runs

**Today.** `send_message` exists (`arko_workflows/src/messaging/tools.rs:44-56`). It works between agents of the same `Workflow`, is addressed by agent name (every active run of that name gets it), and is delivered at the recipient's next turn boundary. `wait_for_message` (`messaging/wait.rs`) returns when a message arrives, but its result only names the sender. The text arrives at the next turn boundary.

Combined with gap 1, this means:
- A lead inside `run_workflow_agent` can't make another tool call, so it can't message the subagent it's waiting on.
- A subagent's message to its lead is queued until `run_workflow_agent` returns, because context is "added at the next turn boundary… including while a tool call ran" (`arko_agents/README.md:789-792`). A subagent that waits for the lead's answer times out.
- Each subagent in its own `Workflow` (fin's workaround for gap 3) can't use `send_message` at all, because the `MessageBus` belongs to one workflow (`arko_workflows/src/context/mod.rs:26`).

**fin's workaround.** fin's crew has its own mailbox. The lead has `message_worker` and `wait_for_workers`, and workers have `message_lead` and `wait_for_lead`. Messages to a running agent are posted with `Workflow::post_note` and reach it at its next turn boundary. A wait tool returns the message text itself. A message to a finished worker continues its run with `continue_agent`, and one to a stopped worker restarts it.

**Request.**
- Messages addressed to a run id, so a lead can tell apart two runs of the same agent.
- `wait_for_message` (and the proposed `wait_for_agents`) returning the message text, so a waiting agent can act on it in the same call.
- With background subagents (gap 1) and per-run workspaces (gap 3), the existing `send_message` would cover the rest.

## 3. Every run in a workflow shares one working directory

**Today.** The engine creates each run's task with the workflow's workspace: `Task::new(config, String::new(), ctx.workspace.clone(), None)` (`arko_workflows/src/workflow/engine.rs:2571`). `WorkflowBuilder::workspace` is set once per workflow (`workflow/mod.rs:1344`), and `AgentConfig` has no workspace field. arko_agents already supports a per-task directory (`Task.current_working_directory` drives the terminal tools and path checks, `arko_agents/src/terminal/mod.rs:553`), but arko_workflows never sets it per run.

**Effect.** Parallel subagents that change files all work in the same checkout, and their edits and builds collide.

**fin's workaround.** One `Workflow` per worker, each with its own workspace (the working directory or a git worktree), checkpoints in `<session>/workers/<id>/run`, and cost ledger. fin keeps its own records of the workers (`workers.json`), adds up their costs, reloads them when a session resumes, and needs its own mailbox because `send_message` can't cross workflows (gap 2).

**Request.** A `workspace` (or `cwd`) option on `run_agent`, `spawn_agent` and the background subagent tool. It would set the run's task directory for the shell, terminal and file tools and the path-safety checks, and be saved in the checkpoint so continued and recovered runs keep it.

## 4. No isolated workspaces for parallel subagents

**Today.** Nothing in arko_agents or arko_workflows creates an isolated workspace for a run.

**fin's workaround.** When the lead asks for one, `start_worker` makes a git worktree at `<session>/worktrees/<id>` on a new branch `fin/<session-id>/<id>`, one at a time under a lock, and runs the worker's workflow there. `stop_worker` can remove it. A worktree starts from the last commit, so uncommitted changes in the main checkout aren't in it, and each worktree builds its own `target/`.

**Request.** An `isolation` option on the background subagent tool (none, or a git worktree), backed by a host-pluggable trait (something like `WorkspaceProvider` with `create(run) -> PathBuf` and `remove(run)`), so hosts can plug in vws_rs, a copy or a container. The run's workspace (gap 3) would be set to what it returns, and the run's record would keep the path and branch so a later message or restart reuses it.

## 5. Stopping a single run

**Today.** Upstream has `Workflow::stop_run(agent_run_id, request, intent)` and `stop_agent(agent_name, …)`, which stop one run while the rest of the workflow carries on (`arko_workflows/src/workflow/mod.rs:385-450`, also on `WorkflowCancelHandle`). The local checkout at `8f8569a` only has whole-workflow `cancel` and `stop`.

**fin's workaround.** Built against the local checkout, fin stops a worker by stopping that worker's own workflow.

**Request.** This is already upstream. One addition would help background subagents: when a run is stopped, its caller should hear about it (a note or event saying which run stopped and why). Today fin's lead isn't told when the user stops a worker.

## 6. No way to know whether a note or message was read

**Today.** Per-run note tracking exists (`NotesSnapshot.seen`, `arko_workflows/src/notes/mod.rs:181-190`), but only in saved checkpoints. The live `NoteBoard` is `pub(crate)` (`context/mod.rs:61`), and no `AgentEvent` reports that context was injected. Messages are acknowledged only when the recipient's run completes. arko's own recovery code checks whether a run got a note by searching the conversation for its text (`holds`, `notes/mod.rs:643`).

**Effect.** When a run finishes right as a note arrives, the host can't tell whether the agent saw it.

**fin's workaround.** fin searches the finished run's conversation for the note header arko writes, ``Note {seq} from agent `{name}` `` (format at `notes/mod.rs:706-714`). If it isn't there, fin continues the run once so it reads the note. A change to that format would break the check silently, and a fallback stops it from looping.

**Request.** An `AgentEvent::ContextInjected { note_seqs, message_ids }` event, or `NotesHandle::given(run_id) -> u64` on the live board.

## 7. Resumed runs keep their old tools and instructions

**Today.** Continuing, resuming and recovering a run start from its saved task (`arko_workflows/src/workflow/engine.rs:3066`, `:3262`). The runtime offers only the saved tool list (`arko_agents/src/runtime/loop.rs:222-224`) and writes the system prompt only when the conversation has none (`runtime/loop/start.rs:107-108`). `resume_from` refuses a changed agent configuration (`workflow/mod.rs:2456`). The documented alternative is to register the new configuration under another agent name.

**Effect.** After an upgrade, resumed sessions keep the tools and instructions they were created with.

**fin's workaround.** A session saved before workers became asynchronous keeps the blocking `run_workflow_agent`, and fin tells the agent which newer tools it lacks.

**Request.** An opt-in way to continue a run with the current configuration: the new tool list and instructions, with the system message replaced (or a short "your tools changed" note added).

## 8. MCP servers: stdio only, no working directory

**Today.** `McpServerConfig` has `name`, `command`, `args` and `env` (`arko_agents/src/mcp/config.rs:7-15`). `connect_server` sets no `current_dir`, so a server inherits the host's directory (`mcp/manager.rs:203-211`). rmcp is built with only `client` and `transport-child-process` (`arko_agents/Cargo.toml:26`), so HTTP and SSE servers aren't supported. Unknown keys such as `cwd` or `url` are ignored without a warning.

**fin's workaround.** fin reads Claude Code's `.mcp.json` format, skips `http` and `sse` servers with a warning, and starts stdio servers in its own directory.

**Request.**
- `cwd: Option<PathBuf>` for stdio servers.
- The streamable HTTP transport, with headers (and later OAuth), so remote servers in `.mcp.json` work.

## 9. A failing `ask_user` handler ends the run

**Today.** When the host's `UserInputHandler` returns `Err`, the error goes up with `map_err(WorkflowError::Channel)?` (`arko_workflows/src/workflow/engine.rs:1168-1172`), and the run is cancelled ("Stopped by the workflow: Channel error: …"). `arko_workflows/INDEX.md:716` documents this.

**fin's workaround.** fin's handler always returns `Ok`. When nobody can answer (`fin -p`, or no terminal) it returns text telling the agent to decide for itself.

**Request.** A way for a handler to decline a question (for example `Err(UserInputError::Unavailable(reason))`), which the agent receives as the tool's answer while its run carries on.

## 10. Terminal tool rough edges

**Today.**
- `terminal_input` accepts 1 to 4096 bytes (`arko_agents/src/terminal/mod.rs:660-665`), and the agent sees "input must contain 1–4096 bytes". Agents that write files by sending heredocs to a shell job hit it often.
- `terminal_control`'s `wait` action needs `revision`, but the schema marks it optional (`terminal/tools.rs:168-172` against `:266`). The model finds out from a "Missing required parameter" error.

**fin's workaround.** fin gives its agents `write_file` and `apply_diff` and tells them to use those for file content.

**Request.**
- An error message that suggests `write_file`, or sending the text in parts.
- `revision` documented as required for `wait`, or defaulting to the job's current revision.

## 11. The mock model server is private to arko's tests

**Today.** arko_workflows' Responses mock server is `#[cfg(test)] pub(crate) mod test_support` (`arko_workflows/src/lib.rs:47-48`), and no crate has a test-support feature.

**fin's workaround.** fin keeps its own copy in `src/mock_model.rs` for its end-to-end tests.

**Request.** A `test-support` feature that exposes the mock Responses server (and an Anthropic one), with helpers for scripted tool calls and `complete_task`.

## 12. Base URLs can't be set from the environment

**Today.** `OpenAIClient::with_base_url` and `AnthropicClient::with_base_url` exist (`arko_ai/src/providers/openai.rs:580`, `anthropic.rs:57`), but `LLMClient::from_env_with` reads only keys and OAuth variables (`arko_ai/src/client.rs:82-131`). Installing a client with `with_openai_client` also drops the OAuth accounts (`client.rs:679`).

**Effect.** A built binary can't be pointed at a proxy or a mock server without code changes, so fin's end-to-end tests run the harness in-process instead of the binary.

**Request.** `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` read by `from_env_with`, or a builder option that sets the base URL and keeps the OAuth accounts.

## 13. Missing re-exports

- `PathSafetyPolicy` is only reachable as `arko_agents::agent_config::PathSafetyPolicy` (`arko_agents/src/agent_config.rs:151`). Every other type a tool configuration needs is re-exported at the crate root (`lib.rs:18-26`).
- The terminal tool names are only available as `TerminalTool::NAMES`. `TERMINAL_START_TOOL_NAME` is private (`agent_config.rs:518`).

**Request.** Re-export `PathSafetyPolicy` at the root and make the terminal tool name constants public.

## 14. Skills: paths, scripts and sources

**Today.** `SkillsConfig` (`arko_agents/src/skill/config.rs:35-63`) takes one workspace source and several global sources.
- Global paths are used as given, so `~` and environment variables aren't expanded (`skill/loader.rs:32`).
- `scripts/` folders are skipped and only `.md` resources are loaded (`loader.rs:194-205`). The model is never shown the skill's folder, so skills that ship scripts or assets can't use them.

**fin's workaround.** fin resolves every skill folder (the project's `.agents/skills`, `.claude/skills` and `.skills`, its own `skills`, and `~/.agents/skills` and `~/.claude/skills`) to an absolute path and passes them all as global sources.

**Request.**
- Several workspace sources.
- The skill's folder shown to the model when the skill loads, with `scripts/` and other files left in place for it to run or read.
- `~` expansion in global paths.

## 15. A reply without a tool call counts as a failure

**Today.** When a response has text but no tool call, the runtime treats it as unusable output and answers "Your response did not call a tool, so the task is still open…" (`NO_NATIVE_TOOL_CALL`, `arko_agents/src/runtime/loop/texts.rs:15`). The reply goes through the runtime's error recovery (`handle_model_output_failure`, `runtime/loop/recovery.rs:380`, called from `runtime/loop/native.rs:584`). This happens most when the user types a question while the agent works and the agent answers it in plain text.

**fin's workaround.** fin gives the main agent a `message_user` tool that shows the user a message without ending the turn, and its instructions say every response must call a tool.

**Request.** A built-in tool for messages to the user, like `ask_user` without the wait, or a per-agent option that turns a text-only reply into a progress message and lets the run carry on.

## fin's own dependency setup

fin builds against the local checkout through path dependencies. Its arko_index is older and searches by keywords only, so fin takes arko_index from upstream by git (`rev = "023c92c"` in `Cargo.toml`). Moving the other three crates to upstream would bring single-run stops (gap 5) and drop the split. Several of the requests above (1, 2, 3 and 6) would together let fin replace `src/crew.rs` with the framework's own subagents.
