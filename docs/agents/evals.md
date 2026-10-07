# Eval workflow

Read this before changing agent behavior, fixture tasks or running paid browser checks.

## Iterate and verify

Agent changes are iterated against `dev` only. Measure `heldout` before and after a round, and never debug
its tasks. If a held-out task was used to design a fix, disclose that loss of independence in the PR.
Fix the general cause, then verify the local and mock fixture suites and the relevant dev tasks.

The internal live suite, external corpora, comparator adapters and recording tools live in Parallax under
`parallax.browser_use`. Run its `parallax browser internal --help`, `official --help`, `data --help` and
`track --help` in the maintainer checkout. The internal comparison uses Browser Use hosted default and Jev
Ultrafast; label the Python SDK agent separately when used. Unsupported Ultrafast tasks use hosted default
and retain their coverage limitation. Never pool the best score from separate agents.

Parallax imports source benchmark grades as source reports. Its own semantic judgments use Jev with an LLM
fallback. Imported predicates, weighted rubrics and full-task judgments retain their original meanings and
are never relabeled as Parallax semantic verdicts.

## Before spending

Use an approved total budget. Update and record the agent, comparator, dataset and judge revisions before
running, and verify current provider availability and pricing. Preserve the campaign ledger across resumes;
unknown spend stays reserved until reconciled. A provider outage is recorded as an ungraded infrastructure
failure, not silently converted into a task failure or removed from the denominator.

The scheduled fixture job checks configured providers before running. A gateway key alone covers Jev and
the LLM; direct TypeSafe and OpenRouter routes are also supported. Keys stay in ignored configuration.
Heavy local checks run through `~/scripts/agent-heavy`, with owned browsers and containers stopped on exit.

## Evidence and release

Detailed runs and validation live in the private
[fastbrowse-evals Langfuse project](https://us.cloud.langfuse.com/project/cmuwjxra401iyad0cymgswes5).
Keep raw output in ignored `artifacts/evals/`, with private filesystem permissions. Git stores compact
approved baselines and source identity receipts, not logs, recordings, decrypted tasks or credentials.

Parallax exports the internal catalog into Fastbrowse and checks it byte-for-byte. The Fastbrowse publication
gate requires the matching catalog digest, clean committed runner and agent builds, current task versions,
complete task coverage, three measured repeats and every physical run in the ledger. Matched task success,
time and cost regressions block publication. No result is published until complete runs are reviewed and
the maintainer explicitly approves it.

The fixture workflow must be green on the release build. A changed comparison must also be rerun on that
build. CI validates changed result rows and their ledgers against base-branch baselines. The release guard
refuses a missing fixture receipt or executable changes after a comparison measurement.

Public projections contain ids, source hashes, grades, completion, build identity, time and cost only. Answers,
page content, credentials, local paths and private trace links never enter a public dataset or page. Langfuse
upload is not publication approval. Read-back checks must verify an upload before marking tracking complete.
