# Arko

Arko is a general-purpose agent for the terminal. It works in your project with a shell, background
jobs and file tools, and it can start workers that run in parallel while it carries on, each in the
working directory or in a git worktree of its own. It runs on Claude, OpenAI, Gemini and OpenRouter
models, and on your Claude plan through an installed Claude Code.

- A full-screen view of the session and its workers, a line prompt (`--repl`), or one prompt and
  its reply (`-p`)
- Saved sessions you can continue or resume, with handovers to a fresh conversation as the
  context fills
- Search indexes of code and docs (`arko index`), kept current while Arko runs
- A web page for following a session and taking part from a browser, such as a phone's
  (`--remote`)
- A model and effort for each worker, which the agent chooses or takes from
  [worker profiles](docs/worker-profiles.md) you write
- Workers that start workers of their own, down to a depth you set
  ([docs](docs/worker-profiles.md#workers-that-start-workers))
- MCP servers in Claude Code's `.mcp.json` format, and skills from `.agents/skills` or
  `.claude/skills`
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
search is by keywords.

In a session, `/help` lists the keys and commands, and `/settings` holds the view, the theme, the
web page and the API keys. `arko --help` describes every option and setting.

## Library

An app opens a session and drives it with `arko::Engine`, or runs several side by side in one
process with `arko::spawn`. The app shows the session's feed of items and sends the session the
user's input. Apps build Arko with `default-features = false`, which leaves out the terminal's
front ends, and add the `remote` feature for the web page. The library's name is `arko`.

```toml
[dependencies]
arko-tui = { version = "0.4.3", default-features = false, features = ["remote"] }
```

`cargo doc --open` builds the API docs. See
[listing extensions and managing project approvals](docs/extensions.md) and
[worker profiles](docs/worker-profiles.md) for settings pages.

## Tests

```bash
cargo test
node --test src/remote/page-tests/core.test.js
```

The tests run against a mock model server, so they need no API keys.
[CONTRIBUTING.md](CONTRIBUTING.md) shows how to build against a local checkout of arko_framework.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option. See [NOTICE](NOTICE) for the bundled font and themes.
