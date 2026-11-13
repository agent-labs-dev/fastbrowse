# Eval workflow

Read this before changing agent behavior, fixture tasks or running paid browser checks. Publishing a result
has its own rules in [publish-evals.md](publish-evals.md).

## Iterate and verify

Agent changes are iterated against `dev` only. The separate held-out suite is gone: its tasks moved
into `dev`, so any dev task may be one a fix was designed against. When a task shaped the fix, say so in the PR,
and check the result against the core suite and the local and mock fixtures rather than the task alone.
Fix the general cause (the no eval tuning rule in [AGENTS.md](../../AGENTS.md)) before the before-and-after run.

The internal live suite, external corpora, comparator adapters and the official runner live in `evals/`
(`fastbrowse_evals`, a Parallax plugin). Run `uv run fastbrowse-evals browser internal --help`, `official --help`,
`track-ledger --help` and `track-official --help` from `evals/`. The internal comparison runs fastbrowse live and
reads Browser Use hosted default, BU Ultrafast and Browser Harness from the competitor reference; label the Python SDK agent
separately when used. An agent shows tasks it cannot attempt as not run. Never pool the best score from separate agents.

Parallax imports source benchmark grades as source reports. Its own semantic judgments use Jev with an LLM
fallback. Imported predicates, weighted rubrics and full-task judgments retain their original meanings and
are never relabeled as Parallax semantic verdicts.

## Measure before claiming

A claim about the agent rests on a run made for it, on the build it names:

- Smoke a few tasks live before a full suite. A full suite is for a build whose smoke already passes.
- Compare a candidate with a baseline measured on the same harness, judge and task set. Figures from
  different harnesses, judges or task sets are not a comparison. `benchmark-campaign.yml` measures the build at
  `fastbrowse_ref` with the tasks at `harness_sha`, and refuses a build whose grading differs from those tasks'
  versions (`grading changed without a task version update`), so take the baseline from a build with the same
  task versions, not an older release. Only a build that ships the harness's catalog can be published.
- Report LLM calls by purpose beside pass rates; the ledger records each call's purpose.
- Change a default model only on measured runs: measure the candidate on the fixture suites against the
  current default, and record the figures beside the default (`crates/fastbrowse-clients/src/environment.rs`
  and [docs/evals.md](../evals.md)).

## Environment interruptions

A transient site problem the agent did not cause (an error page, a 5xx, a 429, maintenance, a page that never
finishes loading, a network error) is an environment interruption. The agent may retry it in general ways, and
the harness judges it as not an agent failure. Headless local Chrome meets bot checks on sites such as search
engines and package indexes; a run a bot check stopped is ungraded, not an agent failure.
[`evals/BENCHMARKS.md`](../../evals/BENCHMARKS.md) has how each is recorded.

## Before spending

Use an approved total budget. Update and record the agent, comparator, dataset and judge revisions before
running, and verify current provider availability and pricing. Preserve the campaign ledger across resumes;
unknown spend stays reserved until reconciled, and each in-flight task holds its own reservation. A provider
outage is recorded as an ungraded infrastructure failure, not silently converted into a task failure or
removed from the denominator.

Before the first paid call of a campaign:

- The key's own cap is the spend control. Neither a workflow nor the request session refuses to start because a
  key reports less credit left than the approved total: the session records what each key answered at
  `GET https://openrouter.ai/api/v1/key` in `wallet.credit.json`, as evidence, and the wallet still bounds what
  the run may commit. A key that reaches its limit mid-run turns every later call into an HTTP 403.
- Check every secret and variable the workflow names in its header. The campaign workflows check theirs
  before any paid call; a new workflow does the same.

The scheduled fixture job checks configured providers before running. OpenRouter is the recommended
route, one key for Jev and the LLM. A gateway key alone covers the LLM and also serves Jev as a last resort; beside
another Jev key it is a backup for an outage. Keys stay in ignored configuration.
Run local checks directly, with owned browsers and containers stopped on exit.

## Campaigns in CI

- `gh workflow run` dispatches only a workflow whose file is on the default branch. A new or renamed workflow
  merges to `main` before its first dispatch; `--ref` then picks the code it runs.
- A campaign measures the ref it was asked to. The harness lock pins this checkout's fastbrowse, so the
  workflows sync with `--no-install-package fastbrowse`, install the requested build over it and run every later
  command with `uv run --no-sync`; a plain `uv run` would sync back to the pin and measure the wrong build.
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

The fixture workflow runs nightly and on demand, and a release needs a passing dispatched run on its tree. CI validates changed result rows
and their ledgers against base-branch baselines.

Public projections contain ids, source hashes, grades, completion, build identity, time, cost, call counts,
model throughput and the input and output token counts behind that throughput only. Answers, page content,
credentials, local paths and private trace links never enter a public dataset or page. Langfuse upload is
not publication approval. Read-back checks must verify an upload before marking tracking complete.

### Manual CI diagnostics

The existing `Evals` workflow accepts optional `probe` and `fill_tasks` inputs on
`workflow_dispatch`. Both run on Blacksmith after the fixture checks. Every successful
invocation still covers all 41 fixture tasks at least three times, stability and
publication. Diagnostic results never enter the fixture rows or a publication candidate.

`probe` is a JSON object with `task`, optional `start`, `fetch`, `allowed_origins`, `output_schema` and `settings`.
`settings` accepts only `FASTBROWSE_DECISION_SOURCE`, `FASTBROWSE_DECISION_MODEL`,
`FASTBROWSE_LLM_PROVIDER`, `FASTBROWSE_LLM_MODEL` and `FASTBROWSE_LLM_REASONING`.
Credentials come from CI secrets. Defaults match the campaign backend's `run_settings`:
provider keys excluding AI Gateway, fastbrowse options and an explicitly selected Jev
source. Set the model options to the retained request's values when comparing runs;
a new request is a controlled diagnostic, not a replay of a missing original request.
The schema has an object root and uses the public API's supported JSON Schema subset;
an offline public API preflight refuses unsupported constraints before fixture spending.
Its HTTP transport and browser access callback refuse all network and browser work. The helper
calls public `run_task` with a local browser or fetch, the supplied schema and
`Limits(max_dollars=...)`. Supply a separate finite positive `probe_budget`.
The encrypted receipt retains the result, full `traced()` events, costs, checkout SHA
and loaded native extension hash. Exceptions and unknown costs retain the full budget
as unresolved, rather than counting a missing charge as zero.

`fill_tasks` is a JSON array of existing fixture task IDs. Supply a separate finite
positive `fill_budget`. This delegates to `bench fill`: its existing budget checks
between tasks, so an active task and its passes can exceed it. It is not a hard
spending ceiling. Admit that exposure separately before dispatch. The helper refuses
a successful diagnostic if coverage is incomplete, a cassette has misses, grading
fails, costs are unpriced or reported spending exceeds the approved figure. The
receipt conservatively retains the full unresolved budget hold for reconciliation.
No scheduled invocation enables either diagnostic or adds its spending.

Both diagnostics require `diagnostic_certificate`, a public PEM encryption certificate
whose private key the maintainer holds. Input validation tests CMS encryption before
any paid call. Raw results, traces, fill logs and cassettes are archived and encrypted
with OpenSSL CMS AES-256; only `diagnostics.cms` enters the workflow artifact beside
the existing fixture files. Do not put secrets in dispatch inputs. Decrypt the downloaded
file in private scratch with `openssl cms -decrypt -binary -inform DER -in diagnostics.cms
-recip recipient.pem -inkey recipient.key -out diagnostics.tar.gz`. Keep the private key
outside GitHub. These options do not dispatch, publish results or update cassettes in Git.
