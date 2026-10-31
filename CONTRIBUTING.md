# Contributing to Arko

## Tests

```bash
cargo test
cargo test --manifest-path web/Cargo.toml
```

The tests run against a mock model server, so they need no API keys. The Claude Code tests run a
scripted stand-in for the `claude` CLI, `tests/fixtures/fake_claude_agent.py`, and need Python 3
(`ARKO_TEST_PYTHON` picks the interpreter). The stand-in is a copy of arko_agents' test fixture,
so update it when arko_agents changes the protocol it speaks.

## Building against a local arko_framework

`Cargo.toml` takes the framework crates from crates.io. To work on Arko and the framework together,
clone [arko_framework](https://github.com/mesa-hills-research/arko_framework) beside this
repository and add this override to `.cargo/config.toml`, which git ignores.

```toml
[patch.crates-io]
arko_ai = { path = "../arko_framework/arko_ai" }
arko_agents = { path = "../arko_framework/arko_agents" }
arko_workflows = { path = "../arko_framework/arko_workflows" }
arko_index = { path = "../arko_framework/arko_index" }
```

The checkout's crate versions must match the ones `Cargo.toml` asks for. Cargo records the
override in `Cargo.lock`, so regenerate the lock without the override before committing.

## Dictation

The tests that run the real speech model skip unless `ARKO_TEST_SPEECH_PACK` names Granite
Speech's int8 pack and `ARKO_TEST_SPEECH_WAV` a LibriSpeech clip (`260-123286-0018`), and the full
install test needs `ARKO_TEST_GRANITE_DIR`, a folder of Granite Speech's files.

```bash
cargo test --features dictation
```

## The web page

The remote web page is written in Rust with Leptos, in `web/`. Its built files are in
`src/remote/page/`, so building Arko needs no web tools. After changing the page, rebuild it with
`web/build.sh` and commit the output with the change. [web/README.md](web/README.md) says how.
