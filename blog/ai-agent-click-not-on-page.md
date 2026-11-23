---
title: Why an AI agent clicks the wrong button
description: An agent that clicks screen coordinates can hit a control that is not there. How an indexed control set bounds what fastbrowse can click.
topic: How the agent acts
published: 2026-10-31
updated: 2026-11-22
verifiedAgainst: v0.11.2
verifiedOn: 2026-11-22
---

An agent that clicks screen coordinates can hit a control that is not there. The click lands on something real, just not the button the task meant, and the run reports a wrong answer.

fastbrowse takes the coordinate out of the loop. The agent can only name a control the page offered it.

## What the agent chooses from

At each step Jev chooses an operation and a target from the page's indexed buttons, links and fields. For a large set of targets, Jev picks a group first, then a control within it. The browser checks that the chosen control is still there before it acts.

A fill decision can also name other offered fields in the same form that the task needs filled. Those are filled in page order without a request each, and only while the page still shows each one as it was offered; anything else goes back to a full decision.

A read decision can cover a whole list through its observed next-page links. Each page is checked before it is read; a different list or a page that needs an action returns to an ordinary decision. Each read and navigation still spends its own step.

- Hit-testing runs before every input. A control that moved or was covered between the read and the click is caught at the click.
- The text and pictures for each choice share Jev's input limit. On a dense page, a relevance check drops controls the task cannot use. A page that is still too dense offers the controls that fit and scrolls for the rest.

## The authorization gate

Jev judges model-selected clicks, drags and key chords, navigation to a supplied address, and acceptance of confirm, prompt and before-unload dialogs. Code-selected pagination is exempt. A refusal is recorded as a `refused` step with a reason.

Code permits an action when Jev judges it not irreversible, or when the run is authorized and Jev judges the task asked for it. When confirmation is required, the run stops at `needs_confirmation` with exit code 3. In a browser that stays open, such as a running Chrome reached over its DevTools port or through the extension, a run stopped at `needs_confirmation` leaves its tab on the step it stopped at. The judge can be wrong, so read the step before you authorize it.

## See what the agent was offered

`Config(step_choices=True)` adds `StepResult.choices` to each step: the controls the agent was offered for that decision, with its pick marked. When recovery overrides a decision, the step still records what the first chooser saw.

For a run that clicked something unexpected, check whether the right control was in that list:

- It was offered but not picked: reword the task.
- It was never offered: inspect the capture to see whether the control was missing, hidden or left out of the index.

The control the agent clicked and the quote a result is checked against come from one reading of the page. [How it works](/docs/design/) covers the loop, [run statuses and exit codes](/docs/results/) lists each ending, and [the comparison page](/compare/) shows how other agents read a page.

```sh
uvx fastbrowse "cancel the subscription and confirm the page says cancelled" --start https://example.com/account
```

Next: [your first run](/docs/getting-started/).
