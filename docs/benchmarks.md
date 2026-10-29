# Benchmark results

Measured results for fastbrowse against the agents it competes with. Every figure is traceable to a named
source; where a comparison has not been run, this file says so rather than estimating. The
[benchmark page](https://fastbrowse.ai/benchmarks) publishes campaign `release-0.5.25-48c964d`; this file
records each comparison's sources and the records that are no longer on that page.

The published campaign is 0.5.25 on purpose. It is the newest release whose page figures come from a campaign
bound to a Langfuse export (`langfuse_verified` true, fixture 108 of 108). The 0.7.1 campaign was rendered from a
local candidate that nothing bound to an export, and no campaign at all exists for 0.7.3, 0.7.4 or 0.8.x, so there
is no recorded evidence that those builds perform better. On task success the two campaigns tie: 0.5.25 passed 158
of 159 and 137 of 138, 0.7.1 the same 158 of 159 and 137 of 138, with the hosted Browser Use arm scoring lower in
the 0.7.1 run (133 of 138 against 135 of 138). 0.5.25 is also the faster of the two and the one with a full C2
record.

These are recorded benchmark comparisons, not a controlled experiment: the associations below do not
establish cause.

Benchmarking compares exactly three things, and nothing else is a headline:

| | Comparison | Arms | Publication group | Status |
|:--|:--|:--|:--|:--|
| C1 | agent against agent | `fastbrowse` against `browser-use`, the hosted Browser Use agent | `internal-hosted` | recorded below, on the published campaign |
| C2 | backend against backend | `fastbrowse-pool`, fastbrowse on its own browser pool, against `fastbrowse` on a Browser Use Cloud browser | `internal-backend` | recorded below, on the published campaign |
| C3 | fastbrowse and its backend against Browser Use Ultrafast | `fastbrowse-pool` against `jev-ultrafast`, over the 6 tasks both run | `internal-ultrafast` | not yet measured |

The Codex with Cua Driver arm and the upstream suites (BU Bench, Online-Mind2Web) are diagnostics, described in
[measuring fastbrowse](evals.md); none of them is one of the three comparisons.

## C1: fastbrowse against the hosted Browser Use agent

### Recorded head-to-head

Both agents attempted every catalog task the hosted agent supports (46), three repeats each, on Browser Use
cloud browsers under identical limits, with the hosted agent at its defaults. Source: the published
`fastbrowse-internal, hosted Browser Use` group, campaign `release-0.5.25-48c964d`, agent commit `48c964d8`, data
sha256 `46dfe0123d6626cb909290064f2ed84cb56fe187e204d1daed0e741aedac4e2c`.

| Metric | fastbrowse | Browser Use (hosted) | Delta |
|:--|:--|:--|:--|
| Task success | 99.3% (137 of 138 counted) | 97.8% (135 of 138) | 1.4 points higher |
| Execution time | 16.4s | 16.1s | 0.3s slower |
| Cost per run | $0.0510 | $0.3267 | 6.4x cheaper |

Task success is the share of scored results the source grader passed. Completion is reported separately:
fastbrowse completed 135 of 138 results to the hosted agent's 138 of 138. The same campaign also publishes the
`fastbrowse-internal` group, fastbrowse alone on the 53-task internal catalog: 99.4% (158 of 159), an execution
time of 14.5s and $0.0383 a run.

The 0.7.1 campaign measured the same task success as this one (137 of 138 against a hosted arm that scored 133 of
138), 18.9s, $0.0296 a run, and 99.3% (158 of 159) at 17.0s and $0.0268 on the internal catalog. It is faster to
quote and cheaper per run, and it is not on the page because its figures were never bound to a Langfuse export.

An earlier record in the `fastbrowse-site` repository (`src/app/page.tsx`) measured 14 answer tasks (lookups,
sign-ins, checkout, Google Flights) under a 30-step, $0.25 cap, 300s regime: fastbrowse passed 40 of 42 and the
hosted agent 14 of 42, median 17.0s against 27.5s, $0.011 against $0.400 a task, with all 28 of the hosted agent's
failures running past the $0.25 cap. That is a different task set under different limits, so it is not the C1
headline.

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

### Recorded on the published campaign (release-0.5.25)

The `internal-backend` group runs fastbrowse on the 53 fastbrowse tasks three repeats each, once on its own
hosted browser pool and once on a Browser Use Cloud browser, so the browser backend is the only difference.
Source: `docs/results/benchmark-candidate.json`, campaign `release-0.5.25-48c964d`, data sha256
`46dfe0123d6626cb909290064f2ed84cb56fe187e204d1daed0e741aedac4e2c`. It is the only campaign with this group, so
no later build has a recorded C2 comparison.

| Metric | fastbrowse, Browser Use Cloud browser | fastbrowse, own pool | Delta |
|:--|:--|:--|:--|
| Task success | 98.1% (156 of 159 counted) | 96.2% (153 of 159) | 1.9 points lower on the pool |
| Median time per result | 15.0s | 15.9s | 0.9s slower on the pool |
| Median cost per result | $0.0200 | $0.0192 | $0.0008 cheaper on the pool |
| Total cost | $6.76 | $7.14 | $0.38 more on the pool |

This is a matched comparison of browsers, not of agents: both arms run the same fastbrowse build, tasks and
repeats. On this backend fastbrowse passes fewer tasks on its own pool than on the Browser Use Cloud browser.

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
source: the `docs/results/0.5.14.jsonl` entry in `docs/results/publication-baselines.json`). Figures approved
under that pairing keep it, under their own schema.

## What is not claimed here

- No speed, cost or success delta is claimed for C3 until an approved campaign of it exists.
- Browser Use Cloud rates are published and may change.
- fastbrowse's `dollars` are metered provider cost. The hosted product's customer price is separate
  (`fastbrowse-hosted/docs/billing.md`).
