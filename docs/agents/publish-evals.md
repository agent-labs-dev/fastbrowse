# Publish benchmark results

Full browser campaigns run through Parallax. Its exporter validates clean builds, scheduled slots, source
identity, physical ledgers and private Langfuse read-back before producing the campaign's `benchmark-candidate.json`.
The file contains sanitized per-run grades, completion, time, cost, LLM and Jev call counts and model
throughput. Official rubric scores retain their source meaning. Internal hosted and Ultrafast comparisons use
separate matched task groups.

A candidate is written under schema 5; schemas 3 and 4 are read only so that the approved figures and a staged
candidate written under them keep validating. A candidate holds one or more of the groups. `internal-fastbrowse`
is every supported internal task in the current catalog (53 since `stretch-wizard-correction` was retired;
figures approved earlier ran 54), three repeats each, with fastbrowse alone, so it needs no comparator reruns.
Official fastbrowse suites do not require new comparator runs either. Previously published comparator figures
can be linked as references with their task and build differences stated. `internal-backend` is fastbrowse on a
Browser Use Cloud browser against fastbrowse on its own browser pool. `internal-ultrafast` pairs fastbrowse on its
own pool with `jev-ultrafast`; figures approved under schema 3 or 4 keep fastbrowse on a Browser Use Cloud browser
there. Every group records its agent and runner commits, which must match the candidate's build: adding a suite
must retain the published groups and their regression checks, and results from different builds cannot be
combined under one build receipt.
`internal-hosted` is the head-to-head: one Parallax invocation over the 52 tasks fastbrowse and the hosted Browser
Use agent both run, the navigation tasks included, read on each hosted agent's final page
through its browser; figures approved at 46 or 47 tasks still read. It holds rows from whichever of `fastbrowse`,
`browser-use`, `jev-ultrafast` and `browser-harness` ran, each with every repeat of every task it runs. Ultrafast returns
no answer, so it runs only the tasks of the set graded without one. An arm that did not run has no rows and the site
shows it as not measured. An arm that reports neither its model nor its spend carries `"dollars": null`
and the site shows its pass rate and seconds with "cost not reported"; fastbrowse rows always carry a cost. The
official suites publish the fastbrowse arm, with a vendor arm's rows only where Parallax marks them publishable, and
accept any positive repeat count: every task and repeat is one row with its upstream score kept as the upstream
defines it, the group records the repeat count, and a task's figure is the mean of its repeats' scores.

CI validates the candidate's exact coverage and compares matched scores, completion, LLM calls, model
throughput and cost with the previous approved candidate, using the release gate's own efficiency comparison; wall time and Jev calls are recorded but not compared. A group whose
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

A minor release (`vX.Y.0`) dispatches `benchmark-campaign.yml` on its tag once it is published; a patch release
does so only when its annotated tag message has the line `Benchmarks: run`, and anyone with write access can
dispatch it by hand. When every group has run, the campaign builds the candidate and calls `publish-evals.yml`,
which opens a pull request on agent-labs-dev/fastbrowse-site. The pull request carries the candidate byte for
byte as `data/benchmark-<campaign>.json` and the site's group files under `data/benchmarks/`, written by

```sh
uv run python -m fastbrowse.evals.site_groups benchmark-candidate.json run-facts.json ../fastbrowse-site
```

A group file holds no figure. It names the candidate group, campaign, build and candidate SHA256, and the site
computes every figure from the candidate's per-attempt rows. Its `systems` maps each site system with rows to its
arm, `cost_not_reported` lists those of them whose rows carry no cost, and `not_measured` lists the site systems
with no rows. The live suite card shows the head-to-head group
(`internal-hosted`) when the campaign ran it, else `internal-fastbrowse`. A card the candidate has no group for
keeps its file only while that file carries no measured figures: the site reads one candidate's rows, so a card still
measured by an earlier campaign stops the write, and that campaign's group must be measured again. Merging the site pull request is the approval: no other dispatch or receipt is needed. To
publish a campaign again, dispatch `publish-evals.yml` with the campaign run's id.

An upload to Langfuse does not approve publication.

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
