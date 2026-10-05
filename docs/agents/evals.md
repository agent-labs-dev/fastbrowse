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
