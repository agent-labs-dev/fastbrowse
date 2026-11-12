---
title: Cap browser automation cost per run
description: What a fastbrowse run reports it spent, what each charge is for, and how to cap a run with --max-dollars and --max-steps.
topic: Results and cost
published: 2026-10-31
updated: 2026-11-12
verifiedAgainst: v0.10.1
verifiedOn: 2026-11-12
---

Every fastbrowse run reports what it spent. `--json` breaks the total down by component, so model calls and verification calls show up as separate lines. Measured cost per run for benchmark tasks is on [the benchmarks page](/benchmarks/).

## An unknown price stops a capped run

When a model endpoint reports no price, the cost of those calls is unknown. A run with a dollar cap that meets an unknown price stops with `budget_exceeded` instead of spending past a cap it cannot check.

The result names the resource that ran out in `budget.resource` and the limit it hit in `budget.limit`, so a caller can tell a spent budget from a step limit without parsing an error.

## Charges on a run

- Model calls, which pick each action and write the answer. A stalled run can also ask for a fresh known route before stopping.
- Verification calls, which check each claim against the page text it cites.
- Optional web search, reported as a search cost line.
- The browser. A cloud browser is billed by time and added when the run stops.

`--max-dollars` caps model and web-search spend and `--max-steps` caps the number of actions. Neither changes the browser bill. Both are unset by default, so set them when a task is one of many.

```sh
uvx fastbrowse "find the cheapest nonstop flight to Lisbon next Friday" \
  --max-dollars 0.50 --max-steps 40
```

In the Python library the same cap is `Limits(max_dollars=0.10)`, and the result carries `cost.known_dollars` for your own accounting.

## Comparing cost across tools

Two costs per run compare only when they come from the same tasks, at the same task versions, with the same browser. A vendor's cost per task means little until it names the suite and says whether the browser is included. [Benchmark results](/benchmarks/) lists the campaign, the agents and the limits behind every number, and [the comparison page](/compare/) covers licence and hosting for each alternative.

Next: [your first run](/docs/getting-started/).
