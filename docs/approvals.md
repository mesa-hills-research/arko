# Approvals

Some of what the main agent does waits for your OK first. Today that is adding an MCP server,
saving a [skill](skills.md#skills-the-agent-saves) for every project or from a download, saving a
[worker profile](worker-profiles.md) in a folder git tracks or for every project, and starting a
worker on a model billed per token under [worker billing](worker-profiles.md#worker-billing) `ask`.
`/extensions` lets servers be added without asking, or has every skill save ask.
Each such request waits in the approval tray above the message box, apart from the agent's
[questions](questions.md), until you approve or reject it. Requests that come while others wait join
them, so the saves of one response, for example, are answered at once.

Each request is a row with its kind (profile, skill, server or worker), its name, where it applies
and a short summary. A row opens to its details. A profile shows its model, what it is for, the
skills and MCP servers it limits its workers to, and its instructions. A worker billed per token
shows the model's price, what your sessions were billed per token today and the worker's task, with
the option "Allow MODEL for this session", which Approve covers when you turn it on.

With several requests each row has a box, and all start checked. Approve approves the rows checked and
rejects the others in the same answer ("Approve 2, reject 1"), and Reject all rejects them all.
Whatever the message box holds when you answer goes to the agent with each answer, and the box
empties. A message you send while requests wait is an ordinary message, which answers none of them.

## Keys in the full-screen view

Alt+A gives the tray the keys, and Alt+A or Esc gives them back to the message box. Until then,
what you type goes to the message box. On a Mac, Alt+A is Option+A, which needs Option to work as
Alt. In the Terminal app, turn on Use Option as Meta key.

| Key | In the tray |
|---|---|
| ↑ ↓ | Move between the rows |
| Space | Check or uncheck a row |
| A | Check all, or none |
| V | Open a row's details, or close them |
| O | Turn a row's option on or off, in its details |
| PgUp, PgDn | Scroll a profile's instructions or a skill's SKILL.md |
| Enter | Approve the rows checked, and reject the others |
| R | Reject all |

The status line says "needs approval" while requests wait, and the terminal's bell rings for new
ones while its window isn't focused. On the web page the tray has the same rows, boxes and buttons,
and the header says "Needs approval".

At the line prompt (`--repl`), each request prints with its details, and commands answer them.

| Command | Does |
|---|---|
| `/approve` | Approves them all |
| `/approve 1 3` | Approves the first and the third and rejects the rest. `2+` also turns the second one's option on |
| `/reject` | Rejects them all |

Words after the numbers, or after `/reject`, go to the agent with the answer.

## What the agent reads

Each request's tool call reads its own answer. A profile approved is saved, rejected isn't, and
your words come with either. A rejection with words asks the agent to change the profile as they
say and save it again, which asks you again. A worker rejected doesn't start, and the agent reads
why with your words. A request whose turn stops leaves the tray, and when no one can answer (`-p`,
or Arko without a terminal) the agent is told so and nothing is saved or started.

The tray isn't kept with the session. A tool that runs again after a resume asks again.

## The web page and apps

The status's `approvals` (`Reading::approvals`) lists the requests in the order they came, each with
its id (`a1`, `a2`, ...), its `batch` and its `approval`, tagged by `kind` (`profile`, `skill`,
`mcp_server` or `worker_billing`). The types are in `arko::approvals`. `Approval::title`,
`kind_word`, `name`, `scope`, `summary`, `details` and `option` give the words every front end
shows, and `approvals_heading`, `approve_label` and `reject_label` those of the tray's heading and
buttons.

`POST /api/approvals` answers them, naming every request the tray shows.

```json
{"choices": [{"id": "a1", "approve": true, "option": true}, {"id": "a2", "approve": false}], "words": "Keep it to this project."}
```

A request it doesn't name keeps waiting. It answers 409 when none of the requests waits any more,
and 413 when the words are over 16 KB (`arko::approvals::MAX_APPROVAL_WORDS`), with how short to
make them. An app sends the same as `Input::AnswerApprovals`.
