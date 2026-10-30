<img src="src/remote/page/icon.svg" alt="" width="96" align="right">

# Arko

Arko is a general-purpose agent for the terminal. It works in your project with a shell, background
jobs and file tools, and it can start workers that run in parallel while it carries on, each in the
working directory or in a git worktree of its own. It runs on Claude, OpenAI, Gemini and OpenRouter
models, and on your Claude plan through an installed Claude Code.

- A full-screen view of the session and its workers, a line prompt (`--repl`), or one prompt and
  its reply (`-p`)
- Saved sessions you can continue or resume, with handovers to a fresh conversation as the
  context fills
- Another model for the main agent mid-session (`/model`), in a new conversation that starts from
  the old model's report and a brief of the old conversation, reads the rest as it needs, and keeps
  the workers running
- [Questions](docs/questions.md) from the agent, answered in a panel or in a message when you're ready
- An [approval tray](docs/approvals.md) for what the agent needs your OK for, such as an MCP server,
  a skill or a worker profile, answered all at once
- Search indexes of code and docs (`arko index`), kept current while Arko runs
- A web page for following a session and taking part from a browser, such as a phone's
  (`--remote`)
- Any model on OpenRouter, from a list you can filter, with [routing settings](docs/openrouter.md)
  for zero data retention, a region and the providers that serve it
- A model and effort for each worker, which the agent chooses or takes from
  [worker profiles](docs/worker-profiles.md) you write, and the skills and MCP servers each
  worker gets ([docs](docs/worker-profiles.md#skills-and-mcp-servers-for-a-worker))
- Workers that start workers of their own, down to a depth you set
  ([docs](docs/worker-profiles.md#workers-that-start-workers))
- Workers on your Claude and ChatGPT plans first, with your OK before one is billed per token,
  and an optional daily API budget ([docs](docs/worker-profiles.md#worker-billing))
- [Notes](docs/kept-notes.md) the main agent keeps for itself and its workers, which come back
  after a handover or a compaction, or when a conversation is resumed
- [MCP servers](docs/mcp.md) in Claude Code's `.mcp.json` format, reached through one tool so the
  prompt cache holds, which the agent can add with your OK
- [Skills](docs/skills.md) from `.agents/skills` or `.claude/skills`, for some models or for the
  main agent only, which the agent can save with your OK, and three built in (two for writing, and
  ponytail for coding tasks and running workers)
- What the agent saves in a project goes in a [`.arko` folder](docs/arko-folder.md) that git
  ignores by default
- Themes for the full-screen view, and your own in GPUI Kit's format
- A library for apps with front ends of their own

## Install

Arko needs Rust 1.89 or later and a C compiler. Its package on crates.io is `arko-tui`, and the
command it installs is `arko`.

```bash
cargo install arko-tui
```

To build from source, clone the repository and install from the checkout.

```bash
git clone https://github.com/mesa-hills-research/arko.git
cd arko
cargo install --path .
```

## Usage

```bash
arko                                 # start a session in the current directory
arko -m sonnet                       # pick the model
arko -p "List the TODOs in src/"     # send one message, print the reply and exit
arko -c                              # continue the last session here
arko --remote 0.0.0.0                # also serve the web page to your network
```

Arko picks a model from the credentials it finds. Set `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY` or `OPENROUTER_API_KEY`, sign in to Codex for a ChatGPT login, or sign in to Claude
Code for `cc:` models. When the key for a model is missing, Arko asks for it on a setup screen. Keys
can also go in a `.env` file. Semantic search in the indexes uses `VOYAGE_API_KEY`, and without it
search is by keywords. A second ChatGPT login takes over when the first reaches its usage limit, and `/chatgpt` manages
them (see [ChatGPT logins](docs/chatgpt-logins.md)).

In a session, `/help` lists the keys and commands, and `/settings` holds the view, the theme, the
web page, the API keys and the ChatGPT logins. `arko --help` describes every option and setting.
Sessions are named by the first words of their first message, or by a model if you turn that on
(see [session names](docs/session-names.md)).

## Library

An app opens a session and drives it with `arko::Engine`, or runs several side by side in one
process with `arko::spawn`. The app shows the session's feed of items and sends the session the
user's input. Apps build Arko with `default-features = false`, which leaves out the terminal's
front ends, and add the `remote` feature for the web page. The library's name is `arko`.

```toml
[dependencies]
arko-tui = { version = "0.13.0", default-features = false, features = ["remote"] }
```

`cargo doc --open` builds the API docs. See
[listing extensions and managing project approvals](docs/extensions.md),
[approvals](docs/approvals.md), [worker profiles](docs/worker-profiles.md) and
[skills](docs/skills.md) for settings pages.

## Tests

```bash
cargo test
```

The tests run against a mock model server, so they need no API keys.
[CONTRIBUTING.md](CONTRIBUTING.md) shows how to build against a local checkout of arko_framework.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option. See [NOTICE](NOTICE) for the bundled font and themes.
