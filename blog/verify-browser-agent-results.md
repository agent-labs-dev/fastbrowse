---
title: How to verify browser agent results
description: What each fastbrowse run status and exit code means, how citations back an answer, and what to do when a run ends unverified.
topic: Results and cost
published: 2026-10-31
updated: 2026-11-12
verifiedAgainst: v0.10.0
verifiedOn: 2026-11-12
---

fastbrowse marks an answer it could not back with a quote as `unverified` and exits 10. A caller that checks the exit code never mistakes a guess for a result.

A fact the page states comes back with the quote it came from, as a numbered link and as an entry in `RunResult.citations`. A fact the page does not state, such as a count or a winner, is derived: it is backed by the records it draws on, and each of those is cited.

## The three endings to branch on

- `complete` (exit 0): every information requirement is backed by a quote, and every action that changes the site is confirmed on the page.
- `unverified` (exit 10): the run finished, but code could not confirm the result. For example, a claim failed its check against the quotes it cites, part of a list the task covers went unread, or the site refused a requested step.
- `budget_exceeded` (exit 7): a resource limit was reached, or a missing cost prevented enforcing the dollar cap.

Only `complete` exits 0, so a script can test for zero instead of parsing the status. The full table, including the statuses that stop before an irreversible action, is in [run statuses and exit codes](/docs/results/).

## How the agent picks a control

At each step the agent reads the page's buttons, links and fields and picks one of them. It never clicks a screen coordinate, so it cannot click something the page does not have. The quote an answer is checked against comes from the same reading of the page. [The design](/docs/design/) covers the loop.

When a site changes its layout, the run sees a different set of controls instead of clicking the wrong one.

## What to check in your own integration

1. Branch on the run status before reading the answer.
2. Keep the citations. They are the record of what the agent read and did.
3. Read the error on an `unverified` run before retrying. It names each claim the answer left out and the check that claim failed. Either the page did not hold the fact, which a different start page or a narrower task fixes, or the run could not find, read or check it, which is worth investigating first.

```sh
uvx fastbrowse "what is the current price of the pro plan" --start https://example.com/pricing
```

Next: [your first run](/docs/getting-started/).
