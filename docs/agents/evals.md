# Eval workflow

Read this before changing agent behavior, fixture tasks or running paid browser checks. Publishing a result
has its own rules in [publish-evals.md](publish-evals.md).

## Iterate and verify

Agent changes are iterated against `dev` only. Measure `heldout` before and after a round, and never debug
its tasks. If a held-out task was used to design a fix, disclose that loss of independence in the PR.
Fix the general cause (the no eval tuning rule in [AGENTS.md](../../AGENTS.md)), then verify the local and mock
fixture suites and the relevant dev tasks.

The internal live suite, external corpora, comparator adapters and recording tools live in Parallax under
`parallax.browser_use`. Run its `parallax browser internal --help`, `official --help`, `data --help` and
`track --help` in the maintainer checkout. The internal comparison uses Browser Use hosted default and Jev
Ultrafast; label the Python SDK agent separately when used. Unsupported Ultrafast tasks use hosted default
and retain their coverage limitation. Never pool the best score from separate agents.

Parallax imports source benchmark grades as source reports. Its own semantic judgments use Jev with an LLM
fallback. Imported predicates, weighted rubrics and full-task judgments retain their original meanings and
are never relabeled as Parallax semantic verdicts.

## Measure before claiming

A claim about the agent rests on a run made for it, on the build it names:

- Smoke a few tasks live before a full suite. A full suite is for a build whose smoke already passes.
- Compare a candidate with a baseline measured on the same harness, judge and task set. Figures from
  different harnesses, judges or task sets are not a comparison.
- Report LLM calls by purpose beside pass rates; the ledger records each call's purpose.
- Change a default model only on measured runs: measure the candidate on the fixture suites against the
  current default, and record the figures beside the default (`crates/fastbrowse-clients/src/environment.rs`
  and [docs/evals.md](../evals.md)).

## Environment interruptions

A transient site problem the agent did not cause (an error page, a 5xx, a 429, maintenance, a page that never
finishes loading, a network error) is an environment interruption. The agent may retry it in general ways, and
the harness judges it as not an agent failure. Headless local Chrome meets bot checks on sites such as search
engines and package indexes; a run a bot check stopped is ungraded, not an agent failure. Parallax's
`docs/browser-benchmarks.md` has how each is recorded.

## Before spending

Use an approved total budget. Update and record the agent, comparator, dataset and judge revisions before
running, and verify current provider availability and pricing. Preserve the campaign ledger across resumes;
unknown spend stays reserved until reconciled, and each in-flight task holds its own reservation. A provider
outage is recorded as an ungraded infrastructure failure, not silently converted into a task failure or
removed from the denominator.

Before the first paid call of a campaign:

- Check that each provider key has credit for the whole campaign. A key that reaches its limit mid-run turns
  every later call into an HTTP 403 and invalidates the runs after it. OpenRouter reports a key's remaining
  limit at `GET https://openrouter.ai/api/v1/key`.
- Check every secret and variable the workflow names in its header. The campaign workflows check theirs
  before any paid call; a new workflow does the same.

The scheduled fixture job checks configured providers before running. OpenRouter is the recommended
route, one key for Jev and the LLM. A gateway key alone covers the LLM and also serves Jev as a last resort; beside
another Jev key it is a backup for an outage. Keys stay in ignored configuration.
Run local checks directly, with owned browsers and containers stopped on exit.

## Campaigns in CI

- `gh workflow run` dispatches only a workflow whose file is on the default branch. A new or renamed workflow
  merges to `main` before its first dispatch; `--ref` then picks the code it runs.
- A campaign measures the ref it was asked to. Parallax pins a fastbrowse commit, so
  `benchmark-campaign.yml` installs the requested build over that pin and runs every later command with
  `uv run --no-sync`; a plain `uv run` would sync back to the pin and measure the wrong build.
- GitHub artifact downloads and job logs return 502 or 503 under load. Retry them; a failed download is not a
  failed run.

## Evidence and release

Detailed runs and validation live in the private
[fastbrowse-evals Langfuse project](https://us.cloud.langfuse.com/project/cmuwjxra401iyad0cymgswes5).
Keep raw output in ignored `artifacts/evals/`, with private filesystem permissions. Git stores compact
approved baselines and source identity receipts, not logs, recordings, decrypted tasks or credentials.

Parallax exports the internal catalog into fastbrowse and checks it byte-for-byte. The fastbrowse publication
gate requires the matching catalog digest, clean committed runner and agent builds, current task versions,
complete task coverage, three measured repeats and every physical run in the ledger. Regressions in matched
task success, completion, LLM calls, model throughput or cost block publication; wall time is recorded
but moves with cloud load, so it never blocks, and Jev calls are recorded but never block, since Jev decides every step. No result is published until complete runs are reviewed and
the maintainer explicitly approves it.

The fixture workflow runs nightly and on demand; it does not gate a release. CI validates changed result rows
and their ledgers against base-branch baselines.

Public projections contain ids, source hashes, grades, completion, build identity, time, cost, call counts,
model throughput and the input and output token counts behind that throughput only. Answers, page content,
credentials, local paths and private trace links never enter a public dataset or page. Langfuse upload is
not publication approval. Read-back checks must verify an upload before marking tracking complete.
