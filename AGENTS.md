# Notes for coding agents

Arko is a general-purpose agent for the terminal, with a remote web page in `web/` (Rust and Leptos) and a library that Arko Studio builds on. CONTRIBUTING.md says how to run the tests, build against a local framework checkout and rebuild the web page.

- A change in behaviour comes with a test. The tests run against a mock model server and need no keys.
- Tests, fixtures, examples and docs use made-up paths and names (`/home/ada/proj`, "Example Studio"), never a real machine's or person's.
- Commit subjects are plain sentences saying the behaviour ("A forced end skips the reset when the usual close finished first"). Messages are public, so they say what changed and why for any user of the project.
- Every word a user reads (terminal messages, the web page, tool descriptions, the agents' instructions, docs) uses the fewest plain words that keep the meaning, as a person would explain it to a colleague. Join clauses with a period or a comma, not a colon or a semicolon. The built-in `prose` skill has the full rule, for docs too. What the tools say in their descriptions isn't repeated in the instructions, and a test keeps it so.
- A conversation's tool list and system prompt never change while it runs, so a prompt cache stays warm. Anything that may change mid-session goes in a message, as the project's instructions do.
- After changing `web/`, rebuild the page with `web/build.sh` and commit `src/remote/page/` with the change.
- Saved sessions must stay loadable. Give new fields serde defaults and keep reading old ones.
- The interfaces (terminal, web page, Studio) follow `docs/interface-design.md` and the `arko-taste` skill.
