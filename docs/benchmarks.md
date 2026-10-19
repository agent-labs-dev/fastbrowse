# Benchmark results

Measured results for fastbrowse against the agents it competes with. Every figure is traceable to a named
source; where a comparison has not been run, this file says so rather than estimating. The
[benchmark page](https://fastbrowse.ai/benchmarks) renders only an approved feed and withholds every
measurement slot until the maintainer approves one; this file records the measurements behind that feed.

These are recorded benchmark comparisons, not a controlled experiment: the associations below do not
establish cause.

Benchmarking compares exactly three things, and nothing else is a headline:

| | Comparison | Arms | Publication group | Status |
|:--|:--|:--|:--|:--|
| C1 | agent against agent | `fastbrowse` against `browser-use`, the hosted Browser Use agent | `internal-hosted` | recorded below |
| C2 | backend against backend | `fastbrowse-pool`, fastbrowse on its own browser pool, against `fastbrowse` on a Browser Use Cloud browser | `internal-backend` | not yet measured |
| C3 | fastbrowse and its backend against Browser Use Ultrafast | `fastbrowse-pool` against `jev-ultrafast`, over the 6 tasks both run | `internal-ultrafast` | not yet measured |

The Codex with Cua Driver arm and the upstream suites (BU Bench, Online-Mind2Web) are diagnostics, described in
[measuring fastbrowse](evals.md); none of them is one of the three comparisons.

## C1: fastbrowse against the hosted Browser Use agent

### Recorded head-to-head

Same 14 answer tasks (lookups, sign-ins, checkout, Google Flights), three passes each, on identical cloud
browsers under identical limits (30 steps, $0.25 cap, 300s).

| Metric | fastbrowse | Browser Use (hosted) | Delta |
|:--|:--|:--|:--|
| Tasks passed | 40 / 42 (95%) | 14 / 42 (33%) | 2.9x the pass rate |
| Median execution time | 17.0s | 27.5s | 1.6x faster |
| Average cost per task | $0.011 | $0.400 | 35x cheaper |

All 28 of Browser Use's failures ran past the $0.25 cap. Source: the recorded head-to-head in the
`fastbrowse-site` repository (`src/app/page.tsx`). The live benchmark page withholds every measurement slot
until the maintainer approves a candidate feed, so this file, not that page, carries the recorded figures.

### Internal matched-task campaigns

Medians and means are over each arm's recorded attempts on the tasks both arms ran, the matched subset named
in the Matched tasks column (`docs/results/<build>.jsonl`). Cost is metered provider spend; the ratio is
Browser Use's mean divided by fastbrowse's mean.

| Build | Matched tasks | fastbrowse pass | Browser Use pass | fastbrowse med s | Browser Use med s | fastbrowse mean $ | Browser Use mean $ | Cost ratio |
|:--|:--|:--|:--|:--|:--|:--|:--|:--|
| 0.5.6 | 39 | 85.5% | 92.3% | 28.7 | 19.6 | $0.0192 | $0.3808 | 19.9x |
| 0.5.7 | 39 | 94.9% | 97.4% | 21.7 | 15.8 | $0.0258 | $0.3866 | 15.0x |
| 0.5.8 | 47 | 100.0% | 97.9% | 13.9 | 18.6 | $0.0081 | $0.3771 | 46.3x |
| 0.5.9 | 20 | 98.3% | 100.0% | 23.9 | 57.6 | $0.0081 | $0.3591 | 44.5x |

Source: `docs/results/publication-baselines.json`, which names each build's `docs/results/<build>.jsonl`
and its sha256.

**The arms are not the same model.** In these campaigns the fastbrowse arm selected actions with Jev; the
`browser-use` arm ran `claude-opus-4.7`. Task sets and revisions changed between builds, so rows compare
within a build, not across.

## C2: fastbrowse's own browser pool against a Browser Use Cloud browser

Not yet measured. The `internal-backend` group runs fastbrowse on the 53 fastbrowse tasks twice, once on its own
hosted browser pool and once on a Browser Use Cloud browser, so the browser backend is the only difference. No
approved campaign of it exists, so no speed or cost delta between the two backends is claimed here. Running it
needs `BROWSER_USE_API_KEY` (the cloud arm) and the private Parallax live tasks.

### Browser cost basis

| | fastbrowse's own fleet | Browser Use Cloud |
|:--|:--|:--|
| Browser time | no per-browser-hour fee (self-hosted Chrome, or the hosted product's own browser pool) | $0.02/browser-hour pay-as-you-go, no subscription |
| Billing granularity | n/a | billed per whole minute, 1-minute minimum |
| Proxy | own egress | residential proxies, $5/GB, on by default |

A recorded fastbrowse run settles at a median of **$0.0051 to $0.0103** in metered model+run cost across
the campaigns above. One billed Browser Use Cloud browser minute ($0.0003 at the current $0.02/hour rate) is
a fraction of that, so on fastbrowse's own cost scale the browser backend is not the cost driver: model
routing is. Source: the campaign rows and Browser Use's published cloud pricing, checked 2026-10-18.

## C3: fastbrowse on its own browser pool against Browser Use Ultrafast (`jev-ultrafast`)

Not yet measured. The `internal-ultrafast` group compares the two products whole, each on its own backend:
fastbrowse on its browser pool against `jev-ultrafast`, on the 6 core tasks both run, three repeats each.
From publication schema 5 the gate holds every new candidate to that pairing.

The earlier record is not a C3 figure: the 0.5.14 core suite ran fastbrowse on a Browser Use Cloud browser, not on
its own pool, against `jev-ultrafast` (fastbrowse 58 of 60, median 8.6s; `jev-ultrafast` 40 of 60, median 12.6s;
source: `docs/results/0.5.14.jsonl`). Figures approved under that pairing keep it, under their own schema.

## What is not claimed here

- No speed, cost or success delta is claimed for C2 or C3 until an approved campaign of each exists.
- Browser Use Cloud rates are published and may change.
- fastbrowse's `dollars` are metered provider cost. The hosted product's customer price is separate
  (`fastbrowse-hosted/docs/billing.md`).
