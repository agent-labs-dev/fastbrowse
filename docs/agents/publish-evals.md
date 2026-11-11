# Publish benchmark results

Full browser campaigns run through the harness, `evals/`. Its exporter validates clean builds, scheduled slots, source
identity, physical ledgers and private Langfuse read-back before producing the campaign's `benchmark-candidate.json`.
The file contains sanitized per-run grades, completion, time, cost, LLM and Jev call counts and model
throughput. Official rubric scores retain their source meaning.

No attempt of any agent has a time, step or spend limit: it runs until the agent ends it or a provider does. The harness
records the absence of limits in each run and manifest, and publication refuses a run that records any limit. A limit
a provider imposes and no request can lift, such as Browser Use's 4 hour session maximum, is stated with the figures.

## What may be published

- Only a figure we measured, on the build the candidate names. A vendor's own figure appears only as a
  reference, labelled vendor-published with its source, never beside ours as if measured the same way.
- Claude Opus 5.5 through OpenRouter is the one judge model behind published figures: Parallax's LLM fallback
  (`PARALLAX_JUDGE_MODELS`) and the official suites' judges (`PARALLAX_BU_BENCH_JUDGE`,
  `PARALLAX_MIND2WEB_JUDGE`) all name it, and a group judged by any other model is not published.

## The candidate contract

Three programs share the candidate's schema: `fastbrowse-evals browser publish-candidate` writes it,
`fastbrowse.evals.benchmarks` validates and installs it, and `agent-labs-dev/fastbrowse-site` parses it
(`src/lib/benchmark-feed.ts`). A schema change lands in all three. Before any of them merges, run one candidate written by
the changed writer through both readers.

## Schemas

## What may be published

- Only a figure we measured, on the build the candidate names. A vendor's own figure appears only as a
  reference, labelled vendor-published with its source, never beside ours as if measured the same way.
- Claude Opus 5.5 through OpenRouter is the one judge model behind published figures: Parallax's LLM fallback
  (`PARALLAX_JUDGE_MODELS`) and the official suites' judges (`PARALLAX_BU_BENCH_JUDGE`,
  `PARALLAX_MIND2WEB_JUDGE`) all name it, and a group judged by any other model is not published.

## The candidate contract

Three programs share the candidate's schema: `fastbrowse-evals browser publish-candidate` writes it,
`fastbrowse.evals.benchmarks` validates and installs it, and `agent-labs-dev/fastbrowse-site` parses it
(`src/lib/benchmark-feed.ts`). A schema change lands in all three. Before any of them merges, run one candidate written by
the changed writer through both readers.

## Schemas

Schema 1 requires all four original groups. Schema 2 accepts nonempty subsets of those four groups.
Schema 3 also supports `internal-fastbrowse`: every supported internal task in the current catalog (53 since
`stretch-wizard-correction` was retired; figures approved earlier ran 54), three repeats each, with
fastbrowse alone. The fastbrowse-only group does not require comparator reruns. Official
fastbrowse suites do not require new comparator runs. Previously published comparator figures can be linked
as references with their task and build differences stated. Every group inside one candidate still comes
from the same measured agent and runner build. Adding a suite must retain the published groups and their
regression checks; results from different builds cannot be combined under one build receipt.
Schema 4 adds `internal-backend`: fastbrowse on a Browser Use Cloud browser against fastbrowse on its own
browser pool. From schema 5, `internal-ultrafast` pairs fastbrowse on its own pool with `jev-ultrafast`; figures
approved under an older schema keep fastbrowse on a Browser Use Cloud browser there.
Schema 2 and later record each group's agent and runner commits and require them to match the candidate's build,
unless the group lists its `sources`: the campaign runs its rows came from, newest first, one entry per run and arm,
each with its commits, date and the tasks it supplied for that arm, and for an arm other than fastbrowse the pin it
ran (`version`) and the models its rows reported (`models`). Its commits are then the newest source's. An
earlier source stays valid on any build, including a branch commit a squash merge left outside `main`. A slot our own harness or
judge failed is listed in `unjudged` with its reason, outside every figure; a run the site's bot check or an outage
stopped is a failed row naming why, and a legacy `blocked` slot reads as one. Each task the catalog says an arm cannot run is listed in `excluded` with the catalog's
reason and counts as `repeats` failed attempts in every pass rate. A row whose agent connects to its browser where the
harness cannot see (hosted Browser Use, Browser Harness) states what of that setup stayed in its seconds in
`timing_note`. The site's parser (`src/lib/benchmark-feed.ts`) must accept these fields before a candidate that has
them is published.

`internal-hosted` is the head-to-head over the 53 tasks fastbrowse runs; figures approved at 46, 47 or 52 tasks still
read. A campaign runs `fastbrowse` and `jev-ultrafast` live in one harness invocation, on Browser Use Cloud browsers
with the same settings. Each arm runs every repeat of every task it can attempt: `browser-use` skips the task that
passes only by stopping before an unauthorized step, and `browser-harness` also skips the login tasks, so it runs 47.
Ultrafast returns no answer, only DONE or BLOCKED, so it runs only the six navigation tasks, which are graded on the
page a run ends on. The catalog states each reason and the site shows those tasks as not run. An arm that did not run has no rows and the site shows it as not measured. An arm
that reports neither its model nor its spend carries `"dollars": null` and the site shows its pass rate and seconds with
"cost not reported". Fastbrowse receipts retain known dollar figures, but an unknown or unsettled line marks
the total incomplete. A provisional figure is a lower bound, not a settled bill.

`browser-use` and `browser-harness` rows come from `docs/results/competitor-reference.json`, which only
`publish-candidate` reads. It holds their public rows with the vendor, date, version, browser and campaign of the one
run that measured them, and the site labels them as measured once on that date, not on this release. A dispatch with
`refresh_competitors` runs both live on a full head-to-head and leaves `competitor-reference.json` in the candidate
artifact; commit that file to `docs/results/` for later campaigns. Publication refuses the head-to-head, naming each
stale task, when the reference is missing, is graded under another policy, or no longer matches the catalog.

The official suites publish fastbrowse alone, beside the vendor's own published figure, and accept any
positive repeat count: every task and repeat is one row with its upstream score kept as the upstream defines it, the
group records the repeat count, and a task's figure is the mean of its repeats' scores.

## Validation and approval

Candidate rows declare transport counts with `llm_calls_basis: physical_requests`. Historical rows without
a basis retain their cost-line meaning. A declared basis without a count, or a null count, is unknown;
the omitted-count zero default remains only for historical rows without a basis. Retry totals and summaries
must retain unknown counts and keep different bases separate.

CI validates the candidate's exact coverage and compares matched scores, completion, LLM and Jev calls, model
throughput and cost with the previous approved candidate; wall time is recorded but not compared. A group whose
catalog differs from the approved one only by tasks listed under the catalog's `retired` (a site that is gone)
is compared on the tasks both ran; the earlier catalog is found in Git history by its digest. Any other catalog,
task or limit difference is a protocol change, published only as described
under [Changing the protocol](#changing-the-protocol). Unresolved costs, missing grades, incomplete coverage and
a failed fixture workflow block publication. The fixture receipt is a successful maintainer dispatch of
`evals.yml` whose head commit is the measured build. When `main` has moved past that build, push a branch at
it and dispatch the workflow there; a branch run counts only when its commit is in `main`'s history, so the
workflow and runner it used are ones `main` held. A candidate can carry a verified export receipt in its `export`
field in place of the fixture receipt, never both. The export receipt names the measured build and campaign, the
bundle's digest and the trace it was read back from, covers exactly the published groups and counts every
published row. `uv run python -m fastbrowse.evals.benchmarks --status` prints what blocks the staged candidate
and the next action. These checks detect recorded regressions; they do not prove that
every website or grader is free of bugs. Review unexpected outcomes before approving.

## From a campaign to the site

A release runs `benchmark-campaign.yml` as `release-<x.y.z>` on its commit before it is tagged, and the release
workflow's `gate` job will not publish without it (see [release.md](release.md)); anyone with write access can
dispatch a campaign by hand. An integration holding a token with `contents: write` starts one with a `repository_dispatch`
through `client_payload`, which takes the same values as the manual inputs and is refused before any spend when a
value is outside them. Any campaign that is not a smoke is refused before any spend when its build has no passing or
running Evals dispatch. When every group has run, the campaign builds the candidate from this run and the campaign's
earlier finished runs, each task from the newest run that measured it in full, and calls `publish-evals.yml`,
which opens a pull request on agent-labs-dev/fastbrowse-site. The pull request carries the candidate byte for
byte as `data/benchmark-<campaign>.json` and the site's group files under `data/benchmarks/`, written by

```sh
uv run python -m fastbrowse.evals.site_groups benchmark-candidate.json run-facts.json ../fastbrowse-site
```

A group file holds no figure. It names the candidate group, campaign, build and candidate SHA256, and the site
computes every figure from the candidate's per-attempt rows. Its `systems` maps each site system with rows to its
arm, `cost_not_reported` lists those of them whose rows carry no cost, `not_measured` lists the site systems
with no rows, and `setups` names each shown system's version and models where the candidate records them. The live suite has two cards: `live-suite` shows the head-to-head group (`internal-hosted`) and
`live-suite-fastbrowse` shows `internal-fastbrowse`. A `reference` block in a group file is a vendor figure on another
task set: the site labels it and never compares it, and a new campaign keeps it. A card the candidate has no group for
keeps its file only while that file carries no measured figures: the site reads one candidate's rows, so a card still
measured by an earlier campaign stops the write, and that campaign's group must be measured again. Merging the site pull request is the approval: no other dispatch or receipt is needed. To
publish a campaign again, dispatch `publish-evals.yml` with the campaign run's id.

Official results reach the same candidate through their public rows. A run directory holds decrypted BU Bench goals
and upstream records, so it leaves `official-benchmarks.yml` only encrypted, and that job also writes the
`official-rows-<suite>` artifact: opaque task ids, scores, seconds, dollars and protocol facts, after the harness
has held the run to every publication rule. Dispatch it first, then dispatch `benchmark-campaign.yml` on the same
`fastbrowse_ref` and `harness_sha` with `official_runs` set to its run id, or give it `campaign_id` and it
dispatches the campaign itself once every job has passed, as a release does. The campaign's own stage adds each
suite as a group; its run facts take each suite's date and judge from those rows.

Both workflows send every result to Langfuse even when the run stopped. The campaign sends each group's ledger with
`fastbrowse-evals browser track-ledger` in its own job, which "Re-run failed jobs" repeats from the ledger artifact
without a paid run; the official workflow sends each suite with `track-official` as a step. The candidate is refused
unless Langfuse holds them. An upload to Langfuse does not approve publication.

## Retiring a task or changing the protocol

Neither path exists to improve a score. A task is retired only for an external reason that stops it measuring
any agent, such as a site that no longer exists, and its catalog entry records that reason, with evidence
(for example, the DNS answers showing the name is gone) and a date. A task is never retired, swapped or
regraded because of how an agent scored on it, and a protocol change never drops or replaces tasks for that
reason. A protocol change states its cause in its note, such as #356's uncapped runs.

## Figures in docs/results

`docs/results/benchmarks.json` and `docs/results/benchmark-approval.json` hold figures approved before the site
pull request became the approval. `python -m fastbrowse.evals.benchmarks --check-diff` still refuses any change to
them, their archives or their protocol-change records that lacks the owner receipt they were approved with. A new
campaign is not published there.
