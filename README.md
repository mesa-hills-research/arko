# fin

fin is a general-purpose agent for the terminal. It works in your project with a shell, background
jobs and file tools, and it can start workers that run in parallel while it carries on, each in the
working directory or in a git worktree of its own. It runs on Claude, OpenAI, Gemini and OpenRouter
models, and on your Claude plan through an installed Claude Code.

- A full-screen view of the session and its workers, a line prompt (`--repl`), or one prompt and
  its reply (`-p`)
- Saved sessions you can continue or resume, with handovers to a fresh conversation as the
  context fills
- Search indexes of code and docs (`fin index`), kept current while fin runs
- A web page for following a session and taking part from a browser, such as a phone's
  (`--remote`)
- MCP servers in Claude Code's `.mcp.json` format, and skills from `.agents/skills` or
  `.claude/skills`
- Themes for the full-screen view, and your own in GPUI Kit's format

## Install

fin needs Rust 1.89 or later and a checkout of
[arko_framework](https://github.com/mesa-hills-research/arko_framework) next to it, since it builds
against that through path dependencies.

```bash
git clone https://github.com/mesa-hills-research/arko_framework.git
git clone https://github.com/mesa-hills-research/fin.git
cd fin
cargo install --path .
```

## Usage

```bash
fin                                  # start a session in the current directory
fin -m sonnet                        # pick the model
fin -p "List the TODOs in src/"      # send one message, print the reply and exit
fin -c                               # continue the last session here
fin --remote 0.0.0.0                 # also serve the web page to your network
```

fin picks a model from the credentials it finds. Set `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY` or `OPENROUTER_API_KEY`, sign in to Codex for a ChatGPT login, or sign in to Claude
Code for `cc:` models. When the key for a model is missing, fin asks for it on a setup screen. Keys
can also go in a `.env` file. Semantic search in the indexes uses `VOYAGE_API_KEY`, and without it
search is by keywords.

In a session, `/help` lists the keys and commands, and `/settings` holds the view, the theme, the
web page and the API keys. `fin --help` describes every option and setting.

## Tests

```bash
cargo test
node --test src/remote/page-tests/core.test.js
```

The tests run against a mock model server, so they need no API keys.

## Docs

- [What fin needs from arko_framework](docs/arko-framework-gaps.md): where fin works around the
  framework, and the changes that would remove each workaround.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option. See [NOTICE](NOTICE) for the bundled font and themes.
