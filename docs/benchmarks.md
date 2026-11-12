# Benchmark results

Measured results for fastbrowse against the agents it competes with. Every figure is traceable to a named
source; where a comparison has not been run, this file says so rather than estimating. The
[benchmark page](https://fastbrowse.ai/benchmarks) publishes campaign `release-0.10.0`; the figures in the
published record below are the page's, and this file adds the sources, the cost basis, the runs each arm
excludes and the earlier records the page no longer carries.

These are recorded benchmark comparisons, not a controlled experiment: the associations below do not
establish cause.

## Where the published campaign stands against the release

The page publishes `release-0.10.0`: fastbrowse-evals at commit `2e94d24`, run 2026-11-09, judged by
anthropic/claude-opus-5.5 through OpenRouter. 0.10.0, tagged 2026-11-10, is the current release, so the
published campaign measures the build that ships rather than lagging a release behind it. No campaign measured
on a 0.10.1 build has been published, so no figure here describes 0.10.1.

A campaign becomes published only when its groups have run and its candidate export has been verified and
merged, and since `bd02773b` a release does not wait on an eval run for its build, so a later tag can again run
ahead of the page. When it does, the page is the source of truth and this file follows it: a figure the page
does not publish does not go into a comparison here.

Benchmarking compares exactly three things, and nothing else is a headline:

| | Comparison | Arms | Publication group | Status |
|:--|:--|:--|:--|:--|
| C1 | agent against agent | `fastbrowse` and `jev-ultrafast` live, `browser-use` and `browser-harness` from the competitor reference | `internal-hosted` | published record below |
| C2 | backend against backend | `fastbrowse-pool`, fastbrowse on its own browser pool, against `fastbrowse` on a Browser Use Cloud browser | `internal-backend` | earlier campaign record below; not on the published page |
| C3 | fastbrowse and its backend against Browser Use Ultrafast | `fastbrowse-pool` against `jev-ultrafast`, over the 6 navigation tasks both run | `internal-ultrafast` | Ultrafast measured on the page as an arm of C1's live suite (below); the 6-task own-pool pairing is not yet measured |

The Codex with Cua Driver arm and the upstream suites (BU Bench, Online-Mind2Web) are diagnostics, described in
[measuring fastbrowse](evals.md); none of them is one of the three comparisons.

## C1: fastbrowse against the hosted Browser Use agent

### Published campaign record (`release-0.10.0`, run 2026-11-09)

53 live tasks at three repeats each, every arm on Browser Use cloud browsers under identical limits at its own
defaults, judged by anthropic/claude-opus-5.5 through OpenRouter. Source: the published campaign
`release-0.10.0`, fastbrowse-evals at commit `2e94d24` (full SHA
`2e94d242d1933f69816277a9a109b06aa33c3940`), measured in workflow run 42720659664 and computed from 555
recorded attempts.

| System | Attempts passed | Cost per run | Fastest on | Runs the figure excludes |
|:--|:--|:--|:--|:--|
| Browser Use agent (V3 API, Claude Opus 4.7) | 94.1% (144 of 153) | $0.5508 | 11 of 50 | cannot attempt 1 of 53 tasks (saucedemo-pause: its API has no way to pause before an irreversible step), counted as failures; 6 of 156 runs not judged because our harness failed (earlier task version), left out of the score |
| fastbrowse 0.10.0 | 89.9% (143 of 159) | $0.0150 | 35 of 50 | blocked by the site's bot check, 6 of 159 runs, counted as failures |
| Browser Harness | 88.2% (135 of 153) | $0.0046 | 2 of 50 | cannot attempt 6 of 53 tasks (expandtesting-login, practice-login, saucedemo-cart, saucedemo-checkout, saucedemo-locked-out, saucedemo-pause: we give it no login credentials, because it would hold them in its own shell, and it has no way to pause before an irreversible step), counted as failures; 6 of 141 runs not judged because our harness failed (earlier task version), left out of the score |
| Jev Ultrafast | 4.6% (6 of 129) | $0.0017 | 2 of 50 | cannot attempt 6 of 53 tasks (the same six: it has no way to be given login credentials and no way to pause before an irreversible step), counted as failures; 4 of 141 runs blocked by the site's bot check, counted as failures; 30 of 141 runs not judged because our harness failed (evidence unavailable), left out of the score |

Browser Harness's $0.0046 is our estimate from list prices applied to its token counts, not a charge we paid.

Cost is the mean over every run, failures included, so lower is better. Fastest on counts the tasks where a
system's median time over its passing runs was the lowest, out of the 50 tasks at least two systems passed.
Attempts passed is the share of attempts the judge accepted, so higher is better. The page states the campaign
as 53 tasks x 3 repeats = 159 attempts per system; an arm that cannot attempt a task carries fewer, which is
why the denominators differ.

On this campaign the hosted Browser Use agent passed the most attempts, at the highest cost and the fewest
fastest-on tasks: 94.1% against fastbrowse's 89.9%, $0.5508 a run against $0.0150 (36.6x cheaper), and fastest
on 11 of the 50 shared tasks against fastbrowse's 35.

Jev Ultrafast is Browser Use's open source agent (MIT), measured at commit `1231850a`. It cannot return an
answer, so every task that asks a question fails, which is most of what is left after the six credential and
pause tasks it cannot attempt. Its 4.6% is a fact about a product that is not built to answer, not a like for
like result against the other three arms.

Timing is not like for like across arms. fastbrowse's and Jev Ultrafast's times run from the moment the browser
session is ready to the final answer; the Browser Use agent's and Browser Harness's also include connecting to
the browser and opening the first page, which our harness cannot separate from their runs. Median session
opening and closing, as the page reports them: fastbrowse 3.0s and 0.2s, Browser Use agent 2.0s and 0.1s, Jev
Ultrafast 2.7s and 0.2s, Browser Harness 0.5s and 0.2s.

The page's other live-suite card measures fastbrowse alone on all 53 tasks: 90.5% (144 of 159) at $0.0149 a
run, against 0.7.1's 99.3% (158 of 159) at $0.0268. The two cards publish different fastbrowse numerators for
the same 159 attempts (143 of 159 in the head-to-head, 144 of 159 alone); the page does not reconcile them and
neither does this file. The 0.7.1 figures are not like for like either: 0.7.1 was graded by Jev on 2026-10-25
and 0.10.0 by anthropic/claude-opus-5.5 through OpenRouter, and every task's version changed between the
releases, 9 of the 53 to a different result.

### Superseded record (`release-0.7.1`, 2026-10-25), no longer the published campaign

An earlier head-to-head, kept for the record and not as the published campaign. Both agents attempted every
catalog task the hosted agent supports (46), three repeats each, on Browser Use cloud browsers under identical
limits, with the hosted agent at its defaults. Source: the group recorded for campaign `release-0.7.1`, agent
commit `7b8b0e8f`, data sha256 `7d6ac26f23fa6d354128fa7e4bcc8190125c441f4f136dc39d0e3c35cec5d4e2`.

| Metric | fastbrowse | Browser Use (hosted) | Delta |
|:--|:--|:--|:--|
| Task success | 99.2% (137 of 138 counted) | 96.3% (133 of 138) | 2.9 points higher |
| Cost per run | $0.0296 | $0.3180 | 10.7x cheaper |

Task success is the share of scored results the source grader passed. Completion is reported separately:
fastbrowse completed 133 of 138 results to the hosted agent's 138 of 138. The page publishes no run time for
this campaign and carries its head-to-head nowhere, so this file quotes no time and no time delta for it. The
page now carries 0.7.1 only in the solo 53-task card above. No README claim rests on this record.

An earlier record in the `fastbrowse-site` repository (`src/app/page.tsx`) measured 14 answer tasks (lookups,
sign-ins, checkout, Google Flights) under a 30-step, $0.25 cap, 300s regime: fastbrowse passed 40 of 42 and the
hosted agent 14 of 42, median 17.0s against 27.5s, $0.011 against $0.400 a task, with all 28 of the hosted agent's
failures running past the $0.25 cap. That is a different task set under different limits, so it is not the C1
headline, and the page carries it nowhere.

### Internal matched-task campaigns (earlier builds, not on the published page)

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

Ultrafast is measured on the published page, as the fourth arm of the C1 live suite above: 4.6% of 129
attempts, $0.0017 a run, fastest on 2 of the 50 tasks two or more agents passed. That arm ran on a Browser Use
Cloud browser like every other arm of the campaign, so it is the agent comparison, not this one.

The `internal-ultrafast` pairing this section owns is not yet measured. It compares the two products whole,
each on its own backend: fastbrowse on its browser pool against `jev-ultrafast`, on the 6 navigation tasks both
run, three repeats each; `jev-ultrafast` returns no answer. From publication schema 5 the gate holds every new
candidate to that pairing.

The earlier record is not a C3 figure: the 0.5.14 core suite ran fastbrowse on a Browser Use Cloud browser, not on
its own pool, against `jev-ultrafast` (fastbrowse 58 of 60, median 8.6s; `jev-ultrafast` 40 of 60, median 12.6s;
source: the `docs/results/0.5.14.jsonl` entry in `docs/results/publication-baselines.json`). Figures approved
under that pairing keep it, under their own schema.

### Browser Use Ultrafast (`bu-ultrafast`): vendor figures, not measured by us

Browser Use put Ultrafast on every Cloud plan in the US on 2026-10-30 and publishes a vendor-reported median of
22 seconds and $0.0064 a task (source: <https://browser-use.com/posts/bu-ultrafast-is-live>, read 2026-11-04).
Those are Browser Use's figures about Browser Use, and they cover the model and the search only: browser time at
$0.02 a browser-hour and proxy traffic at $5/GB are billed separately. They are not a measurement of ours, they
are not the same kind of number as our `dollars` (metered provider spend, browser included), and this file puts
them in no column above.

What we do measure under that name is the open source `jev-ultrafast` agent at the pinned commit, which the page
carries as the Jev Ultrafast arm. The vendor's hosted `bu-ultrafast` model has no arm of its own: the runner's
arm set is `fastbrowse`, `fastbrowse-pool`, `browser-use`, `jev-ultrafast` and `browser-harness`, fixed by
`src/fastbrowse/evals/benchmarks.py`, and each publication group validates the arms it may carry -
`internal-hosted` carries all four head-to-head arms, with `jev-ultrafast` and `browser-harness` partial to the
tasks each can attempt, and `internal-ultrafast` pairs `fastbrowse-pool` with `jev-ultrafast` at the commit
pinned in `evals/src/fastbrowse_evals/adapters/arm-pins.json`. Measuring the vendor's hosted model therefore
still takes an arm added to the arm set and to a group, a patch pin of the revision it runs, and a cost meter
that adds its browser-hour and proxy GB, so its per-task cost is the same quantity as ours. Until that exists
its vendor figures stay labelled as the vendor's.

## What is not claimed here

- No speed, cost or success delta is claimed for the 6-task own-pool C3 pairing until an approved campaign of it exists.
- The page publishes two fastbrowse numerators for the same 159 attempts (143 of 159 in the head-to-head card,
  144 of 159 in the solo card); this file quotes each where the page publishes it and picks neither.
- Browser Use Cloud rates are published and may change.
- fastbrowse's `dollars` are metered provider cost. The hosted product's customer price is separate
  (`fastbrowse-hosted/docs/billing.md`).
