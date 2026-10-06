# Fresh comparison validation

The live comparison measured clean core commit `bae3e92fd2f9c73a83710b39f497cc0dddee0d65` with
three repeats, each arm's default models and step limits, and concurrency two. The rows retain all
117 selected results and 121 physical attempts, including four retries. No selected attempt is unpriced.

The publication gate blocks these rows: flight-search median cost is $0.0051 against $0.0024 in the
matched baseline, and GitHub-open median time is 11.9 seconds against 9.5 seconds. These are diagnostic
results, not published benchmark figures. The canonical results feed and website headline remain unchanged.

`core` `c0785e8b`: fastbrowse against Browser Use agent, on the same 13 tasks.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.19) | 39/39 | 39/39 | 14.5s | 16.9s | $0.0057 | $0.0068 | $0.27 |
| Browser Use agent | 38/39 | 38/39 | 18.8s | 48.3s | $0.3798 | $0.9309 | $36.31 |

Each arm has 39 selected results, excluding earlier retries. Runs: `807c1c72b986` at `bae3e92`.

`core` `c0785e8b`: fastbrowse against Browser Use Ultrafast, on the same 6 tasks.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.19) | 18/18 | 18/18 | 8.0s | 10.8s | $0.0027 | $0.0032 | $0.06 |
| Browser Use Ultrafast | 12/18 | 12/18 | 10.8s | 33.0s | $0.0013 | $0.0128 | $0.23 |

Each arm has 18 selected results, excluding earlier retries. Runs: `807c1c72b986` at `bae3e92`.

`core` `c0785e8b`: fastbrowse alone, on the 1 task only it ran.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.19) | 3/3 | 3/3 | 25.6s | 26.1s | $0.0039 | $0.0039 | $0.01 |

Each arm has 3 selected results, excluding earlier retries. Runs: `807c1c72b986` at `bae3e92`.

The same clean build passed all 96 default-route fixture attempts: all 32 tasks passed each of three
repeats. A separate gateway-route campaign recorded 95/96, retaining its report-download recovery failure.
Neither campaign is pooled with another build or route.

Interrupted diagnostic campaigns retained unreconciled in-flight charges before the live cancellation
fix. Reported campaign spend is a lower bound, not a complete billing reconciliation. The approved total
API and cloud-browser budget is $200. Every failed campaign remains in local evidence.

The external corpus uses a separately pinned native grader and a predeclared balanced draw. Its completion
claims and independent correctness are counted separately from this live suite.

## Extraction fix and release checks

A subsequent gateway fixture campaign at `7284c3c6d090f29f215e796d995871265b9fb23e`
passed 95/96 attempts. The cheapest-product answer quoted the right value, but typed extraction lost
its evidence from an earlier page. Commit `3f66928718c88af21a520a800d9f18f23d8aa623`
reads scalar candidates from retained source quotes, excludes stale evidence and bounds every choice
batch. Its local fixture round passed 96/96, including all three cheapest-product repetitions with
`{"name":"Paper Filters","price":4.6}`. Both failed campaigns remain in the evidence.

The heldout suite passed 9/9 before and 9/9 after this change; development passed 13/13 after it.
These single-repeat checks are diagnostics, not comparison figures. Local Python tests passed 2034,
and seven hosted integration checks passed against this core build with real local Postgres and Redis.
The final release guard adds exact-commit fixture attestation for every release, including releases
without new comparison rows. Commit `ae54d1d9e92e88f5f268932b0af12aa1239f472c` passed 2053 tests on each of Python 3.13 and 3.14 in CI.
A feature-branch fixture run cannot authorize a tag: the release commit on main must itself pass Evals.

The hosted dashboard passed 17 browser tests, including all eight widths from 320 to 1920 pixels and
88 console route visits. Shared table layouts become labelled cards in narrow containers. The site
passed 38 unit checks and 40 browser page-width checks. Its gate also rejects missing exports, runtime
errors, missing local assets, clipped content and inaccessible mobile navigation.

The live comparison above predates the extraction fix. It cannot establish the final build's performance.
No regression threshold was waived, and no fresh headline was substituted for the existing valid feed.

The exact final code build's gateway fixture run, GitHub Actions `37386069955`, passed 89/96.
Every failed row was `unavailable` during provider HTTP 503 errors: repeat zero passed 32/32, repeat
one passed 25/32, and repeat two passed 32/32. All 96 attempts were priced, totaling $0.45140.
Both completed cheapest-product attempts returned the corrected typed data. The release gate correctly
refused this round. A recovery rerun was started only after the final repeat demonstrated provider recovery;
the failed round remains retained and counted.

## Native corpus protocol

The external comparison measures clean build `7284c3c6d090f29f215e796d995871265b9fb23e`,
which predates the extraction fix. Its draw was chosen before results: seed 75, three tasks per
stratum, 12 tasks covering all eight sites, two default-model arms and three paired repeats.
The arms use their own defaults: Fastbrowse records `jev openrouter`, and Browser Use records
`claude-opus-4.7` with SDK 3.11.3. This compares the default products, including their different models.
Every physical attempt resets the selected capsule. Retries remain separate physical attempts. The draw,
archive, capsule reset code and native grader are pinned; observed native state is scored by the
upstream predicates rather than a model.

Native correctness and agent completion are separate. An attempt can achieve the requested database
state and then end with an error; its native grade may pass while its completion remains unsuccessful.
The unavailable outage attempt is ungraded, and a stopped attempt retains that status even when native state
passes. A partial draw or a task with fewer than three paired graded repeats cannot headline.

The measured capsule setup subtotal is 1021.25 seconds. Complete cold setup time is unknown, and
agent times exclude setup. Failed setup and endpoint startup logs are retained locally. The protocol
reports the unknown total explicitly instead of presenting the subtotal as complete cold setup time.

The raw evidence uses ASCII JSON escapes for transport. Decoded source quotes, offsets, capture hashes
and provenance are unchanged. Interrupted calls with unreconciled receipts remain unknown costs,
never zero-cost attempts. Reported totals are lower bounds and the approved $200 budget includes
model API calls, cloud browsers and retries.

The third LearnHouse attempt for Browser Use stopped before a paid browser was started: reset exited
nonzero after 5.06 seconds. It remains an ungraded, zero-cost infrastructure row. PostgreSQL logs
recorded `database "learnhouse" does not exist` during initialization. The pinned capsule's readiness
loop calls `pg_isready` before restoring the dump, which suggests a startup readiness race. The pinned
upstream evaluator was not modified during measurement. Browser Use therefore lacks a third graded
LearnHouse repeat, and this draw cannot headline even if every other planned attempt finishes.

The recovery run, GitHub Actions `37388098451`, passed 96/96 with all 32 tasks at 100% across three
repeats on the same exact code commit. All 96 attempts were priced, totaling $0.80740. Stability and
publication checks passed. The prior outage round remains retained and counted in the budget.

## Complete native attempt record

The run finished its planned schedule and exited 1 because of the ungraded reset failure. It retained
73 records: 72 browser attempts, including one paid outage retry, plus one reset failure before browser
startup. All records have known prices. The all-attempt cost was $55.50114. These diagnostics describe
the measured 7284c3c build, not the later extraction build.

| Arm | Scheduled | Graded | Native pass | Completed | Both native pass and completion | All-attempt cost |
|:--|--:|--:|--:|--:|--:|--:|
| fastbrowse | 36 | 36 | 23 | 21 | 19 | $0.52930 |
| browser-use | 36 | 35 | 33 | 35 | 33 | $54.97184 |

Each task has three scheduled slots per arm. Native-pass denominators below count graded slots only;
completion denominators count every scheduled slot, including the infrastructure failure. Fastbrowse
completion requires `complete`; Browser Use completion uses its own reported ending. Raw endings are retained.

| Task | Fastbrowse native pass | Fastbrowse completed | Browser Use native pass | Browser Use completed |
|:--|--:|--:|--:|--:|
| blog-read-author | 3/3 | 3/3 | 3/3 | 3/3 |
| directory-filter | 3/3 | 2/3 | 3/3 | 3/3 |
| ea-6 | 3/3 | 3/3 | 3/3 | 3/3 |
| ea-7 | 3/3 | 0/3 | 3/3 | 3/3 |
| hev-8 | 3/3 | 3/3 | 3/3 | 3/3 |
| hev-9 | 0/3 | 0/3 | 3/3 | 3/3 |
| id-2 | 3/3 | 3/3 | 3/3 | 3/3 |
| lh-8 | 1/3 | 3/3 | 2/2 | 2/3 |
| md-2 | 1/3 | 1/3 | 1/3 | 3/3 |
| md-4 | 3/3 | 3/3 | 3/3 | 3/3 |
| md-8 | 0/3 | 0/3 | 3/3 | 3/3 |
| react-auth-boundary | 0/3 | 0/3 | 3/3 | 3/3 |

The original CLI correctly exited nonzero but incorrectly offered a headline over the remaining 11
tasks after LearnHouse lost its third grade. That original report is retained for audit. The coverage
fix refuses a headline whenever any planned slot on an initially shared eligible task is ungraded or
missing; a successful graded retry may recover its slot. The corrected report is regenerated from the
same recorded attempts without rerunning agents, resetting state or changing native grades.

[Raw evidence index](2026-10-05-fresh-evidence.md)

Reported model and cloud-browser charges for this effort total $138.28974, including recovered receipts.
This is a lower bound because earlier interrupted calls still have unreconciled charges. The $55 reserve
remains within the approved $200 cap. All paid runs for this effort have finished.

The missing-slot regression failed against the old reporter: it offered a paired headline after an
eligible task lost a graded repeat. The corrected reporter passed all 46 focused comparison checks,
including successful outage recovery and exclusions decided by initial arm eligibility. Replaying all
73 original records now refuses the headline and preserves the original all-attempt costs. The original
report, corrected report and correction receipt remain separate artifacts.
