# What fin needs from arko_framework

fin is built on arko_ai, arko_agents and arko_workflows. This page lists the places where fin works around something the framework can't do yet. For each one it gives what arko does today, what fin does instead, and the change that would let fin drop its workaround. The list starts with the biggest gaps.

Paths are relative to the arko_framework repository. Line numbers in the remaining requests refer to upstream commit `023c92c`. Resolved entries describe the APIs fin now uses.

| # | Gap | Status / priority |
|---|-----|----------|
| 1 | Subagents block the agent that starts them | Resolved |
| 2 | Lead and subagents can't talk while a subagent runs | Resolved |
| 3 | Every run in a workflow shares one working directory | Resolved |
| 4 | No isolated workspaces (git worktrees) for parallel subagents | Resolved |
| 5 | Stopping a single run (upstream only) | Resolved |
| 6 | No way to know whether a note or message was read | Resolved |
| 7 | Resumed runs keep their old tools and instructions | Medium |
| 8 | MCP servers: stdio only, no working directory | Medium |
| 9 | A failing `ask_user` handler ends the run | Low |
| 10 | Terminal tool rough edges | Low |
| 11 | The mock model server is private to arko's tests | Low |
| 12 | Base URLs can't be set from the environment | Low |
| 13 | Missing re-exports | Low |
| 14 | Skills: paths, scripts and sources | Low |
| 15 | A reply without a tool call counts as a failure | Resolved |
| 16 | ChatGPT's prompt cache is reported as 5 minutes | Resolved |

## Resolved: native workers (1–6)

`arko_workflows::subagents` now runs background agents inside one workflow. fin uses `start_agent`, `message_agent`, `wait_for_agents`, `list_agents` and `stop_agent`. Runs above the concurrency cap queue, and the cap can be unlimited. Mail and run state survive checkpoints. Reports reach a running lead at a turn boundary, and `NewsWaiting` wakes an idle lead. `resume_subagents` continues interrupted workers.

Each run has a saved working directory. `GitWorktreeProvider` creates fin's isolated workers under `<session>/worktrees`, on branches `fin/<session>/<id>`. Worker IDs are `r1`, `r2`, and so on. The provider records the workspace for later follow-up and removal.

Single-run stops leave other agents running. `stop_subagent` also stops queued runs and reports the reason to the lead. fin uses a lead-only stop for Ctrl-C and transfers workers when `/new` starts a new lead conversation.

`ContextInjected`, `notes_given` and the native mailbox's delivery receipts replace searches through conversation text to find out whether news was read. All worker costs are included in the workflow ledger.

## 7. Resumed runs keep their old tools and instructions

**Today.** Continuing, resuming and recovering a run start from its saved task (`arko_workflows/src/workflow/engine.rs:3066`, `:3262`). The runtime offers only the saved tool list (`arko_agents/src/runtime/loop.rs:222-224`) and writes the system prompt only when the conversation has none (`runtime/loop/start.rs:107-108`). `resume_from` refuses a changed agent configuration (`workflow/mod.rs:2456`). The documented alternative is to register the new configuration under another agent name.

**Effect.** After an upgrade, resumed sessions keep the tools and instructions they were created with.

**fin's workaround.** Saved sessions retain their original agent configurations. Sessions with old worker tools preserve their workers and worktrees as archives. Calls to retired worker tools return a migration notice. Start a new fin session to use native workers. `/new` resets a conversation inside the same saved session and keeps its original tools.

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

## Resolved: messages to the user (15)

arko_agents supplies `message_user` and a `MessageToUser` event. A message can reach the user while the agent keeps working. Hosts can still override the tool, as fin does. The runtime also offers an opt-in setting for treating text-only responses as progress messages.

## Resolved: ChatGPT prompt cache (16)

`LLMClient::prompt_cache_lifetime` distinguishes ChatGPT OAuth's 30-minute cache from OpenAI API keys' 5-minute cache. Native worker waits use that value. Claude workers retain fin's `cache` choice of `5m` or `1h`, supplied through a `start_agent` parameter and preparation hook.

## fin's own dependency setup

fin builds against the local arko checkout through path dependencies. Native subagents replace fin's per-worker workflows, mailbox and `workers.json` runtime.
