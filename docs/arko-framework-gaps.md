# What Arko needs from arko_framework

Arko is built on arko_ai, arko_agents and arko_workflows. This page lists the places where Arko works around something the framework can't do yet. For each one it gives what the framework does now, what Arko does instead, and the change that would let Arko drop its workaround. The list starts with the biggest gaps.

Paths are relative to the arko_framework repository, and line numbers refer to its commit `023c92c`. Resolved entries describe the APIs Arko now uses.

| # | Gap | Status / priority |
|---|-----|----------|
| 1 | Subagents block the agent that starts them | Resolved |
| 2 | Lead and subagents can't talk while a subagent runs | Resolved |
| 3 | Every run in a workflow shares one working directory | Resolved |
| 4 | No isolated workspaces (git worktrees) for parallel subagents | Resolved |
| 5 | Stopping a single run | Resolved |
| 6 | No way to know whether a note or message was read | Resolved |
| 7 | Resumed runs keep their old tools and instructions | Medium |
| 8 | MCP servers start over stdio only, without a working directory | Medium |
| 9 | A failing `ask_user` handler ends the run | Low |
| 10 | Terminal tool rough edges | Low |
| 11 | The mock model server is private to arko_framework's tests | Low |
| 12 | Base URLs can't be set from the environment | Low |
| 13 | Missing re-exports | Low |
| 14 | Skill paths, scripts and sources | Low |
| 15 | A reply without a tool call counts as a failure | Resolved |
| 16 | ChatGPT's prompt cache is reported as 5 minutes | Resolved |
| 17 | A host can't ask a workflow's run for a handover | Low |

## Native workers (1–6), resolved

`arko_workflows::subagents` now runs background agents inside one workflow. Arko uses `start_agent`, `message_agent`, `wait_for_agents`, `list_agents` and `stop_agent`. Runs above the concurrency cap queue, and the cap can be unlimited. Mail and run state survive checkpoints. Reports reach a running lead at a turn boundary, and `NewsWaiting` wakes an idle lead. `resume_subagents` continues interrupted workers.

Each run has a saved working directory. `GitWorktreeProvider` creates Arko's isolated workers under `<session>/worktrees`, on branches `arko/<session>/<id>`. Worker IDs are `r1`, `r2`, and so on. The provider records the workspace for later follow-up and removal.

Single-run stops leave other agents running. `stop_subagent` also stops queued runs and reports the reason to the lead. Arko uses a lead-only stop for Ctrl-C and transfers workers when `/new` starts a new lead conversation.

`ContextInjected`, `notes_given` and the native mailbox's delivery receipts replace searches through conversation text to find out whether news was read. All worker costs are included in the workflow ledger.

## 7. Resumed runs keep their old tools and instructions

Continuing, resuming and recovering a run start from its saved task (`arko_workflows/src/workflow/engine.rs:3066`, `:3262`). The runtime offers only the saved tool list (`arko_agents/src/runtime/loop.rs:222-224`) and writes the system prompt only when the conversation has none (`runtime/loop/start.rs:107-108`). `resume_from` refuses a changed agent configuration (`workflow/mod.rs:2456`). The documented alternative is to register the new configuration under another agent name. So after an upgrade, resumed sessions keep the tools and instructions they were created with.

Saved sessions keep their original agent configurations in Arko. Sessions with old worker tools keep their workers and worktrees as archives, and calls to retired worker tools return a migration notice. A new Arko session uses native workers. `/new` resets a conversation inside the same saved session and keeps its original tools.

The framework could offer an opt-in way to continue a run with the current configuration (the new tool list and instructions), with the system message replaced or a short "your tools changed" message added.

## 8. MCP servers start over stdio only, without a working directory

`McpServerConfig` has `name`, `command`, `args` and `env` (`arko_agents/src/mcp/config.rs:7-15`). `connect_server` sets no `current_dir`, so a server inherits the host's directory (`mcp/manager.rs:203-211`). rmcp is built with only `client` and `transport-child-process` (`arko_agents/Cargo.toml:26`), so HTTP and SSE servers aren't supported. Unknown keys such as `cwd` or `url` are ignored without a warning.

Arko reads Claude Code's `.mcp.json` format, skips `http` and `sse` servers with a warning, and starts stdio servers in its own directory.

Two changes would help:
- `cwd: Option<PathBuf>` for stdio servers.
- The streamable HTTP transport, with headers (and later OAuth), so remote servers in `.mcp.json` work.

## 9. A failing `ask_user` handler ends the run

When the host's `UserInputHandler` returns `Err`, the error goes up with `map_err(WorkflowError::Channel)?` (`arko_workflows/src/workflow/engine.rs:1168-1172`), and the workflow cancels the run with a channel error. `arko_workflows/INDEX.md:716` documents this.

Arko's handler always returns `Ok`. When nobody can answer (`arko -p`, or no terminal) it returns text telling the agent to decide for itself.

A handler could instead decline a question (for example with `Err(UserInputError::Unavailable(reason))`), which the agent would receive as the tool's answer while its run carries on.

## 10. Terminal tool rough edges

- `terminal_input` accepts 1 to 4096 bytes (`arko_agents/src/terminal/mod.rs:660-665`), and the agent sees "input must contain 1–4096 bytes". Agents that write files by sending heredocs to a shell job hit it often.
- The `wait` action of `terminal_control` needs `revision`. The parameter's description says so, but the schema marks it optional (`arko_agents/src/terminal/tools.rs`), so a model that leaves it out gets a "Missing required parameter" error.

Arko gives its agents `write_file` and `apply_diff` and tells them to use those for file content.

Two changes would help:
- An error message that suggests `write_file`, or sending the text in parts.
- `revision` marked as required for `wait`, or defaulting to the job's current revision.

## 11. The mock model server is private to arko_framework's tests

arko_workflows' Responses mock server is `#[cfg(test)] pub(crate) mod test_support` (`arko_workflows/src/lib.rs:47-48`), and no crate has a test-support feature. Arko keeps its own copy in `src/mock_model.rs` for its end-to-end tests.

A `test-support` feature could expose the mock Responses server (and an Anthropic one), with helpers for scripted tool calls and `complete_task`.

## 12. Base URLs can't be set from the environment

`OpenAIClient::with_base_url` and `AnthropicClient::with_base_url` exist (`arko_ai/src/providers/openai.rs:580`, `anthropic.rs:57`), but `LLMClient::from_env_with` reads only keys and OAuth variables (`arko_ai/src/client.rs:82-131`). Installing a client with `with_openai_client` also drops the OAuth accounts (`client.rs:679`).

So a built binary can't be pointed at a proxy or a mock server without code changes, and Arko's end-to-end tests run the harness in-process instead of the binary.

`from_env_with` could read `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL`, or a builder option could set the base URL and keep the OAuth accounts.

## 13. Missing re-exports

- `PathSafetyPolicy` is only reachable as `arko_agents::agent_config::PathSafetyPolicy` (`arko_agents/src/agent_config.rs:151`). Every other type a tool configuration needs is re-exported at the crate root (`lib.rs:18-26`).
- The terminal tool names are only available as `TerminalTool::NAMES`. `TERMINAL_START_TOOL_NAME` is private (`agent_config.rs:518`).

The fix is to re-export `PathSafetyPolicy` at the root and make the terminal tool name constants public.

## 14. Skill paths, scripts and sources

`SkillsConfig` (`arko_agents/src/skill/config.rs:35-63`) takes one workspace source and several global sources.
- Global paths are used as given, so `~` and environment variables aren't expanded (`skill/loader.rs:32`).
- `scripts/` folders are skipped and only `.md` resources are loaded (`loader.rs:194-205`). The model never sees the skill's folder, so skills that ship scripts or assets can't use them.

Arko resolves every skill folder (the project's `.agents/skills`, `.claude/skills` and `.skills`, its own `skills`, and `~/.agents/skills` and `~/.claude/skills`) to an absolute path and passes them all as global sources.

Three changes would help:
- Several workspace sources.
- The skill's folder shown to the model when the skill loads, with `scripts/` and other files left in place for it to run or read.
- `~` expansion in global paths.

## 17. A host can't ask a workflow's run for a handover

`UserCommand::RequestHandover` asks a running agent for a handover at its next turn boundary (`arko_agents/src/runtime/command.rs`). A workflow host reaches a run's command channel only through `AgentHandle::into_parts` (`arko_workflows/src/workflow/mod.rs`), which returns the raw event and command channels, so the workflow's hooks and budget tracking no longer apply. `run_agent`, `continue_agent` and `restart` keep the channel to themselves. So a host can't ask a running agent to hand over now.

The handover size is solved. `WorkflowBuilder::agent_handover_threshold` gives an agent's runs a `HandoverThreshold` the host changes while they run, in place of the size saved in `AgentConfig::handover`. Arko's `/handover-at` sets the main agent's.

Arko's shell tool keeps each run's command channel when the runtime binds it (`Tool::bind_runtime`, which gives every offered tool the run's channel as the run starts). For `/handover`, Arko sends `RequestHandover` on that channel. This works only for runs offered one of Arko's tools.

`Workflow::request_handover(run_id)` could send the command to a running run, or keep it for the run's next start.

## Messages to the user (15), resolved

arko_agents supplies `message_user` and a `MessageToUser` event. A message can reach the user while the agent keeps working. Hosts can still override the tool, as Arko does. The runtime also offers an opt-in setting for treating text-only responses as progress messages.

## ChatGPT prompt cache (16), resolved

`LLMClient::prompt_cache_lifetime` distinguishes ChatGPT OAuth's 30-minute cache from OpenAI API keys' 5-minute cache. Native worker waits use that value. Claude workers keep Arko's `cache` choice of `5m` or `1h`, given through a `start_agent` parameter and a preparation hook.
