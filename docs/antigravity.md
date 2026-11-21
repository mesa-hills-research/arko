# Antigravity

Install Antigravity CLI 1.5.14 or later and run `agy` to sign in to your Google AI plan. Then start Arko with an `agy:` model.

```sh
arko -m agy:neon
arko -m agy:neon --worker-model agy:flash
```

Use `/model` to see which models and efforts your account offers. The short names are `agy:neon`,
`agy:argon`, `agy:hydrogen`, `agy:flash`, `agy:opus`, `agy:sonnet` and `agy:gpt-oss`. Full names
work too, such as `agy:gemini-4-neon`. A worker profile takes the same names.

The CLI runs the model. Arko supplies the instructions, tools and approvals. Antigravity's project
customizations do not run, so put project instructions in `AGENTS.md` for Arko to read.

`/cost` shows how much of the plan's windows you have used. It also estimates what the session's
calls would cost at API prices. The plan covers those calls.
