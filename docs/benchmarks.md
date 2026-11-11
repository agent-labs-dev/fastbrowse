# Benchmark results

Measured results for fastbrowse against the agents it competes with. Every figure is traceable to a named
source; where a comparison has not been run, this file says so rather than estimating. The
[benchmark page](https://fastbrowse.ai/benchmarks) publishes the approved campaign `release-0.7.1`; this file
records each comparison's sources and the earlier records that are no longer on that page.

These are recorded benchmark comparisons, not a controlled experiment: the associations below do not
establish cause.

## Why the published campaign lags the release

The page publishes `release-0.7.1` (agent commit `7b8b0e8f`, run 2026-10-25). The current release is 0.9.1,
tagged 2026-10-30. No campaign measured on a 0.9.x build has been approved and published, so every figure here
and on the page is a 0.7.1 figure and none of it describes the current build. A campaign becomes published only
when its groups have run and its candidate export has been verified and merged, and since `bd02773b` a release
does not wait on an eval run for its build, so nothing keeps a tag and a published campaign in step.

The next campaign to publish measures a 0.9.x build on the same three comparisons: C1 head-to-head against the
hosted Browser Use agent over the 46 live tasks, three repeats each, reporting task success and mean cost per
run; C2 fastbrowse on its own browser pool against one Browser Use Cloud browser; and C3 against
`jev-ultrafast` over the 6 tasks both run.

Benchmarking compares exactly three things, and nothing else is a headline:

| | Comparison | Arms | Publication group | Status |
|:--|:--|:--|:--|:--|
| C1 | agent against agent | `fastbrowse` and `jev-ultrafast` live, `browser-use` and `browser-harness` from the competitor reference | `internal-hosted` | recorded below |
| C2 | backend against backend | `fastbrowse-pool`, fastbrowse on its own browser pool, against `fastbrowse` on a Browser Use Cloud browser | `internal-backend` | earlier campaign record below; not on the published page |
| C3 | fastbrowse and its backend against Browser Use Ultrafast | `fastbrowse-pool` against `jev-ultrafast`, over the 6 navigation tasks both run | `internal-ultrafast` | not yet measured |

The Codex with Cua Driver arm and the upstream suites (BU Bench, Online-Mind2Web) are diagnostics, described in
[measuring fastbrowse](evals.md); none of them is one of the three comparisons.

## C1: fastbrowse against the hosted Browser Use agent

### Recorded head-to-head

Both agents attempted every catalog task the hosted agent supports (46), three repeats each, on Browser Use
cloud browsers under identical limits, with the hosted agent at its defaults. Source: the published
`fastbrowse-internal, hosted Browser Use` group, campaign `release-0.7.1`, agent commit `7b8b0e8f`, data sha256
`7d6ac26f23fa6d354128fa7e4bcc8190125c441f4f136dc39d0e3c35cec5d4e2`.

| Metric | fastbrowse | Browser Use (hosted) | Delta |
|:--|:--|:--|:--|
| Task success | 99.2% (137 of 138 counted) | 96.3% (133 of 138) | 2.9 points higher |
| Cost per run | $0.0296 | $0.3180 | 10.7x cheaper |

Execution time is not published for this campaign. The benchmark page shows no run time for either arm of this
group, both reading "Not recorded", so this file quotes no time and no time delta for it. The 18.9s to 15.3s
this file used to carry is still in the campaign's recorded data, but a figure the page does not publish is not
one this file puts in a comparison: the two surfaces carry the same numbers or neither does.

Task success is the share of scored results the source grader passed. Completion is reported separately:
fastbrowse completed 133 of 138 results to the hosted agent's 138 of 138. The same campaign also publishes the
`fastbrowse-internal` group, fastbrowse alone on the 53-task internal catalog: 99.3% (158 of 159) of results
and $0.0268 a run.

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

### Earlier campaign record (release-0.5.25), not on the published page

The `internal-backend` group runs fastbrowse on the 53 fastbrowse tasks three repeats each, once on its own
hosted browser pool and once on a Browser Use Cloud browser, so the browser backend is the only difference.
Source: `docs/results/benchmark-candidate.json`, campaign `release-0.5.25-48c964d`. The published benchmark page
does not currently carry this group.

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
fastbrowse on its browser pool against `jev-ultrafast`, on the 6 navigation tasks both run, three repeats each:
`jev-ultrafast` returns no answer.
From publication schema 5 the gate holds every new candidate to that pairing.

The earlier record is not a C3 figure: the 0.5.14 core suite ran fastbrowse on a Browser Use Cloud browser, not on
its own pool, against `jev-ultrafast` (fastbrowse 58 of 60, median 8.6s; `jev-ultrafast` 40 of 60, median 12.6s;
source: the `docs/results/0.5.14.jsonl` entry in `docs/results/publication-baselines.json`). Figures approved
under that pairing keep it, under their own schema.

### Browser Use Ultrafast (`bu-ultrafast`): deliberately unmeasured

Browser Use put Ultrafast on every Cloud plan in the US on 2026-10-30 and publishes a vendor-reported median of
22 seconds and $0.0064 a task (source: <https://browser-use.com/posts/bu-ultrafast-is-live>, read 2026-11-04).
Those are Browser Use's figures about Browser Use, and they cover the model and the search only: browser time at
$0.02 a browser-hour and proxy traffic at $5/GB are billed separately. They are not a measurement of ours, they
are not the same kind of number as our `dollars` (metered provider spend, browser included), and this file puts
them in no column above.

We have not run that arm, and no C1 or C3 figure for it is claimed. The arms the runner can measure are fixed
by `src/fastbrowse/evals/benchmarks.py` (`fastbrowse`, `fastbrowse-pool`, `browser-use`, `jev-ultrafast`), and
each publication group validates the arms it may carry: `internal-hosted` pairs `fastbrowse` with `browser-use`,
and `internal-ultrafast` pairs `fastbrowse-pool` with `jev-ultrafast` at the commit pinned in
`evals/src/fastbrowse_evals/adapters/arm-pins.json`. Measuring Browser Use Ultrafast therefore takes an arm
added to the arm set and to a group, a patch pin of the revision it runs, and a cost meter that adds its
browser-hour and proxy GB, so its per-task cost is the same quantity as ours. Until that exists the comparison stays unmeasured, and its vendor figures stay labelled as the vendor's.

## What is not claimed here

- No speed, cost or success delta is claimed for C3 until an approved campaign of it exists.
- Browser Use Cloud rates are published and may change.
- fastbrowse's `dollars` are metered provider cost. The hosted product's customer price is separate
  (`fastbrowse-hosted/docs/billing.md`).
