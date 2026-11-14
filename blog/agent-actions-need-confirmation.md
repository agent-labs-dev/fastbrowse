---
title: When a browser agent should stop instead of acting
description: fastbrowse runs read-only steps and stops before submit, pay, delete and send, ending needs_confirmation with exit 3. What that default protects, and when --authorize is the right call.
topic: Safety
published: 2026-11-14
updated: 2026-11-14
verifiedAgainst: v0.11.0
verifiedOn: 2026-11-14
---

A browser agent that cannot tell a price lookup from a purchase will eventually make one. fastbrowse runs the read-only steps of a task and treats every action that changes something on the site, such as submit, pay, delete or send, as one it does not take on its own.

## The default is to stop

The first irreversible step ends the run with the status `needs_confirmation` and exit 3. Nothing on the page is left holding a half-finished click, and a script that only tests for a non-zero exit sees the stop without parsing the answer.

| Status | Exit | What it means |
|:--|--:|:--|
| `complete` | 0 | Jev judged the task done |
| `needs_confirmation` | 3 | stopped before an irreversible action; re-run with `--authorize` |
| `needs_login` | 4 | a sign-in wall that no `--secret` covers |
| `blocked` | 6 | a bot check that did not clear |
| `budget_exceeded` | 7 | a resource limit was reached |

Only `complete` exits 0. The full table, including the statuses that stop before anything is typed, is in [run statuses and exit codes](/docs/results/).

## The tab stays open where it stopped

A run that stops at `needs_confirmation`, `needs_input` or `needs_login` leaves the tab it stopped in open, outside its tab group, so you can finish the step in the page it already filled. That is the difference between an agent that refuses and an agent that makes you start over: the form is still there, with the values the run typed into it.

## Opting in, one run at a time

`--authorize` allows submit, pay, delete and send. It does not override a task that says to change nothing or not to download, so a task carrying its own constraint is still refused.

```sh
export SAUCE_PASSWORD=secret_sauce
fastbrowse "Log in as standard_user with the saved password and add the backpack to the cart." \
  --start https://www.saucedemo.com/ --secret password=SAUCE_PASSWORD --authorize
```

Without `--authorize` the same task stops after the login and the add, at the first step that changes the cart. Which flags the run accepts is in the [command-line reference](/docs/flags/).

## The same default behind an MCP server

`fastbrowse-mcp` is started without `--allow-authorize`, so every call that reaches an irreversible step stops at `needs_confirmation` and the calling model cannot opt itself in. With the flag on, and `--max-steps`, `--max-dollars` and `--max-seconds` set, you know the most a call can do and the most it can cost before the first one runs. [Setting those limits server-side](/blog/mcp-browser-tool/) covers the flags.

## What to branch on in your own code

1. Stop the loop on exit 3 rather than retrying it. A retry spends another run to arrive at the same refusal.
2. Hand the open tab to a person when the step is one they should take, or re-run with `--authorize` once you have decided the task may complete on its own.
3. Keep `--json`. It carries the status, the step count, the cost and each quote behind the answer, which is what an audit of the run reads.

```sh
uvx fastbrowse "what is the current price of the pro plan" --start https://example.com/pricing
```

Next: [your first run](/docs/getting-started/).
