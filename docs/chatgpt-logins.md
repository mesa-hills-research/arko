# ChatGPT logins

Arko signs in to ChatGPT with a Codex `auth.json`. Several logins can take turns, and Arko uses
them in order.

## Setting them

`ARKO_CODEX_AUTH` names the files, separated as `PATH` is (`:`, and `;` on Windows). One path
works as it always did. Without it, Arko uses Codex's own login, `$CODEX_HOME/auth.json` or
`~/.codex/auth.json`. `--codex-auth FILE` names one for a single session.

While Arko runs, `/chatgpt` manages the list and saves it in Arko's `.env`.

| Command | What it does |
|---|---|
| `/chatgpt` | Lists the logins, which one serves and which are set aside. |
| `/chatgpt add PATH` | Adds a file after the others. |
| `/chatgpt remove N` | Removes login N, by its number or its path. |
| `/chatgpt first N` | Puts login N first. |

`/settings` shows the list. A change is used from the next request of the main agent and the
workers. The web page lists the logins but changes nothing.

## Taking over

Requests go to the first login. When the backend says it reached its usage limit, or its credits
or quota ran out, Arko sets it aside and goes on with the next one at once. It comes back to the
first after the reset. A login with credits left keeps serving on them. When every login is set
aside, the agents wait for the earliest reset, as they do with one.

A conversation goes on when another login takes over. Each login is sent only the reasoning it
produced during the run, and the conversation keeps everything. Reasoning saved from an earlier run
counts as the first login's, so a resumed conversation sends it to the first login, even when a
second one produced it.

## Credits and usage

The status line, `/cost`, `worker_options` and the web page show the plan usage of the login that
served last, with its credits when the backend reports them, and how many other logins are set aside.

The full-screen status line names the plans when more than one was used. Credits have at most two
decimals, and billed costs are split into `main` and `workers`. In `/settings`, turn on **API prices**
to estimate plan-covered calls at API list prices.

```text
Claude 5h: 0% · 7d: 98% · ChatGPT 5h: 12% · 7d: 40% · credits: 12.50 · workers: $8.40
```

`/cost`, `worker_options` and the web page keep the longer form.

```text
ChatGPT plan 5 h 12%, 7 d 82%, credits left 12.50, 1 set aside
ChatGPT plan 5 h 12%, 7 d 82%, unlimited credits
```

It says "has credits" when the backend gives no amount, and nothing about credits when there are
none. The Claude plan has no credits. It shows its extra usage instead when that is on, with the
amounts when Claude Code gives them.

```text
Claude plan 5 h 12%, 7 d 71%, extra usage $271.40 of $500.00
Claude plan 5 h 12%, 7 d 71%, extra usage on
```

## When a file changes

`codex login` can sign a file in to another account. Arko reads the file again before the next
request, and once more when a request is rejected as revoked. The login then signs in as the
account the file holds, and the rejected request is sent again. A file that can't be read, holds no
login, or holds an account another login already has changes nothing, and the error shows as
before.

## In an app

`arko::accounts` has `chatgpt_logins_in`, `add_chatgpt_login`, `remove_chatgpt_login` and
`first_chatgpt_login` for the list of an Arko folder, `set_chatgpt_login` for a list of one, and
`clear_chatgpt_login` for Codex's own. `login_changes` tells when a folder's list changes, and the
sessions open on the folder in the same process use the change at once.

`remote::PlanStatus` holds each plan's usage as `windows`, `credits`, `set_aside` and `extra_usage`
beside the `text` line.
