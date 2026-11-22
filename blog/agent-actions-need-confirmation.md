---
title: When a browser agent should stop, not act
description: fastbrowse stops before actions classified as irreversible, ending needs_confirmation with exit 3. What that default protects, and when to use --authorize.
topic: How the agent acts
published: 2026-11-14
updated: 2026-11-22
verifiedAgainst: v0.12.0
verifiedOn: 2026-11-22
---

A browser agent needs to distinguish a price lookup from a purchase. fastbrowse stops before actions classified as irreversible, such as submitting an order, paying, deleting data or sending a message, unless the run is authorized. It can still fill fields and make reversible changes. Classification can miss a change, so state read-only constraints in your task and check important actions yourself.

## The default is to stop

When the next action is classified as irreversible, an unauthorized run ends with the status `needs_confirmation` and exit 3 before dispatching that action. A script that tests for a non-zero exit sees the stop without parsing the answer.

| Status | Exit | What it means |
|:--|--:|:--|
| `complete` | 0 | Jev judged the task done |
| `needs_confirmation` | 3 | stopped before an irreversible action; re-run with `--authorize` |
| `needs_login` | 4 | a sign-in wall that no `--secret` covers |
| `blocked` | 6 | a bot check that did not clear |
| `budget_exceeded` | 7 | a resource limit was reached |

Only `complete` exits 0. The full table, including the statuses that stop before anything is typed, is in [run statuses and exit codes](/docs/results/).

## Opting in, one run at a time

`--authorize` lets a run submit, pay, delete or send when Jev judges that the task asks for the action. It does not override a task that says to change nothing, not to download, or not to take an action, such as "do not click Upload", so a task carrying its own constraint is still refused. After a run makes a requested commit, it refuses an extra unrequested commit and gives Jev a chance to choose another step or finish.

```sh
export SAUCE_PASSWORD=secret_sauce
fastbrowse "Log in as standard_user with the saved password and add the backpack to the cart." \
  --start https://www.saucedemo.com/ --secret password=SAUCE_PASSWORD --authorize
```

Without `--authorize`, the run stops if an action is classified as irreversible. The exact stopping point depends on that classification. Which flags the run accepts is in the [command-line reference](/docs/flags/).

## The same default behind an MCP server

`fastbrowse-mcp` is started without `--allow-authorize`, so every call that reaches an irreversible step stops at `needs_confirmation` and the calling model cannot opt itself in. With the flag on, and `--max-steps`, `--max-dollars` and `--max-seconds` set, you know the most a call can do and the most it can cost before the first one runs. [Setting those limits server-side](/blog/mcp-browser-tool/) covers the flags.

## What to branch on in your own code

1. Stop the loop on exit 3 rather than retrying it. A retry spends another run to arrive at the same refusal.
2. Inspect the stopped result and let a person complete the step, or re-run with `--authorize` once you have decided the task may complete on its own.
3. Keep `--json`. It carries the status, the step count, the cost and each quote behind the answer, which is what an audit of the run reads.

```sh
uvx fastbrowse "what is the current price of the pro plan" --start https://example.com/pricing
```

Next: [your first run](/docs/getting-started/).
