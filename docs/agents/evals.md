# Evals

Unit tests cannot tell you whether the agent still browses well. The suites can.

```sh
uv run python -m fastbrowse.evals.runner                      # local fixtures, headless Chrome, ~$0.005 a task
uv run --extra browser-use python -m fastbrowse.evals.live    # live head-to-head, three arms
uv run --extra browser-use python -m fastbrowse.evals.live --arms fastbrowse --suite heldout --repeat 3
```

`--suite` picks the set: `core` (the published suite), `dev`, `heldout`, and the harder `stretch-dev` and
`stretch-heldout`. **Agent changes are iterated against `dev` only.** `heldout` is run before and after a
round of changes and never debugged, so its score says whether a round improved the agent or only its dev
score. A change made to fix a named held-out task spends that set's value; say so in the PR when it happens.

The fastbrowse arm uses `OPENROUTER_API_KEY` for the LLM when set, otherwise `AI_GATEWAY_API_KEY`.
Jev uses direct TypeSafe when keyed, otherwise OpenRouter or the gateway. The other live arms require
OpenRouter, and the live suite also needs `BROWSER_USE_API_KEY`. Upstream outages look
exactly like regressions, so re-read a red run before believing it.

The scheduled fixture job forwards `OPENROUTER_API_KEY`, `TYPESAFE_API_KEY` and `AI_GATEWAY_API_KEY`, then checks
them with `--check-providers` before it runs a task. A gateway key alone covers both Jev and the LLM.
Missing credentials fail that check without producing a task score.

The OpenRouter latency benchmark in `fastbrowse.evals.latency` still requires `OPENROUTER_API_KEY`.

Every result row records its build (`run`: version, commit, dirty tree, models) and the `task_version` it ran;
compare rows only at equal task versions. Changing what a task asks or how it grades means
`python -m fastbrowse.evals.versions --bump TASK_ID --docs`, and a test fails until you do. Results are
published as committed rows (`--publish`). The README headline, the results and task tables and the suite
versions in [docs/evals.md](../evals.md) are generated from the code and those rows, and a test fails when
they differ; regenerate them in the same PR ([versions](../evals.md#versions)).

A published comparison has one route: rows committed with `--publish` at the task versions they ran, every
physical attempt kept, and retries and their spend recorded with them. Never type a number into prose.
`.github/workflows/evals.yml` runs the fixture suites on a schedule and on demand; a release that publishes a
comparison needs that job green and, for a head-to-head figure, the comparison re-run on the same build.

`fastbrowse.evals.publication` is the gate that route runs, and it refuses before it advises. Deterministic
checks refuse a row that is not from a clean committed build at the release, a current task version and a
current suite version, that has no attempt ledger reconciling every physical attempt and retry, that claims a
number where cost is unknown, that repeats one `(arm, task, repeat)` key, or whose dataset pin does not match.
An interrupted live run retains each in-flight attempt as an unselected row before closing its ledger. Its
cost stays unknown unless the arm already returned its report; queued slots and truth-only preflight do not
claim a paid attempt. Interrupted comparisons cannot supply a published figure.
The regression check compares fastbrowse against fastbrowse only, per suite, task and task version, and only
against the latest published release that ran them at the same protocol and model route, so a comparison never
crosses arms, task versions, protocols or routes silently. Every live task needs three distinct measured
repeats before any baseline: fewer is a diagnostic and blocks even a new task, a bumped task version or a
re-routed task with no baseline to compare against. A per-task pass rate that declines, and a median wall time
or cost that rises by more than 20% and more than 1 second or $0.001, block. The newest matching release with at
least three attempts is the baseline, so a thin newest release does not hide an older full one; a task with no
matching baseline, a matching baseline under three attempts, and a baseline at another protocol or route are
reported and never block once the candidate has its three repeats. `versions --publish` enforces the gate with
the ledger required; the scheduled job runs the same module over its fixture rows:

```sh
uv run python -m fastbrowse.evals.publication --rows artifacts/evals/nightly.jsonl --baseline docs/results
```

`.github/workflows/ci.yml` runs the gate over the `docs/results` files a pull request adds or changes, against
the rows at the base branch, so a published row cannot be hand-edited past `--publish`; a new results file with
no attempt ledger fails there. The release workflow runs `scripts/release_guard.py` before the GitHub release
and PyPI: a release that commits a comparison needs the Evals workflow green on the release build and its
measured build to be an ancestor that runs the same `src/`, tests, lockfiles, browser code and workflows. A
README, docs or changelog commit may follow the measurement; any code or config change may not. The Evals
workflow itself is the protocol the green run attests: a dispatch under three repeats fails before it spends and
the fixture target is fixed at 100%.


## Matched external corpus

`fastbrowse.evals.corpus_compare` runs the same live arm adaptors on one pinned, seeded draw. Every arm sees
one URL map, task text and independent grader. Reset the isolated capsule before every physical attempt,
including retries, with a reviewed program pinned by SHA-256. Hosted browsers need public endpoints; the
local preflight checks reachability but does not prove that a remote browser can load their assets.

```sh
uv run --all-extras python -m fastbrowse.evals.corpus_compare windtunnel \
  --site-urls local-sites.json --public-site-urls public-sites.json \
  --cache artifacts/corpus-cache --seed 75 --per-stratum 3 \
  --arms fastbrowse browser-use --repeat 3 --out artifacts/corpus-preflight
```

Execution adds `--execute --authorize`, `--reset-command 'reset.py {site} {run_id}'`, `--reset-code reset.py`,
`--reset-sha256 HASH`, and a pinned grader (`--grader-code`, `--grader-sha256`, then `--grader-command` last).
Obtain an approved total API and cloud-browser budget before execution. The default live adaptors keep their
normal models and step limits; record capsule boot, endpoint setup and preflight separately from agent time.

WindTunnel excludes its declared API-only task from browser comparisons. Its declared fixture logins become
named, origin-scoped secrets; unexpected login instructions fail before spending. Upstream task references
and digests stay intact. Native state probes determine correctness independently of each arm's completion
claim. A reset or grader error remains ungraded, and every physical attempt, retry and unknown cost stays in
the ledger. Report only task-repeat pairs that all named arms can grade, with at least three repeats per task;
never choose the best repeat or compare totals with different task sets. Keep the external report separate
from the canonical live-suite feed and disclose endpoint adapters and excluded tasks with it.

A stopped draw cannot supply a comparison headline. Truncated runs, interrupted runs and runs without a task
that has three paired repeats exit non-zero. One- and two-repeat runs retain diagnostic rows but cannot pass
the comparison gate.
