# Running evals

Read this page before changing a task, the browser plugin, or a benchmark
publication result. Do not run a paid campaign while iterating on code.

## Time limit

Every run an eval harness starts sets `max_seconds`. Without it nothing bounds the run's length: a wait on a page
that never changes lasts the seconds the driver asked for, but the run may wait again, and only the stall rule stops it.
The fixture bench and runner derive the run's limit from their one
client timeout (`TASK_SECONDS` in `src/fastbrowse/evals/runner.py`), less the agent's ending-frame allowance, so
the agent stops itself as `budget_exceeded` with its evidence before the client's backstop fires.

## Local checks

From the repository root, install the pinned eval environment and the local
fastbrowse build:

```sh
uv sync --project evals --locked --no-install-package fastbrowse
uv pip install --python evals/.venv/bin/python -e .
```

The offline smoke is a Parallax task discovery check:

```sh
uv run --project evals --no-sync parallax run fastbrowse \
  --harness fastbrowse --repeats 1 --budget 1 --reserve 1 --dry-run
```

The focused publication tests use only synthetic `TrialResult` values and a
synthetic run directory:

```sh
CARGO_TARGET_DIR=target/codex-parallax \
  uv run --project evals --no-sync pytest evals/tests/test_parallax_publication.py -q
```

Run the fast eval lint and type checks for changed files:

```sh
uv run --project evals --no-sync ruff check evals/src evals/tests
(cd evals && .venv/bin/ty check src/fastbrowse_evals/backend.py src/fastbrowse_evals/catalog.py \
  src/fastbrowse_evals/cli.py src/fastbrowse_evals/hosted_api.py src/fastbrowse_evals/live.py \
  src/fastbrowse_evals/live_tasks.py src/fastbrowse_evals/more_tasks.py \
  src/fastbrowse_evals/publication.py src/fastbrowse_evals/sweep_tasks.py \
  src/fastbrowse_evals/task_specs.py)
```

Do not run the full repository suite or a paid browser campaign locally.

## Campaign protocol

The campaign workflow installs the harness checkout first, then installs the
measured fastbrowse SHA. It runs `parallax run` with the plugin installed.
Keep the pool GCP service account and the `harness_sha` and `FASTBROWSE_SHA`
install order intact.

Before paid work, each workflow authenticates its OpenRouter credentials with
`GET https://openrouter.ai/api/v1/key`. Configured provider key limits apply across runs sharing a key. Browser Use Cloud
and fastbrowse-hosted benchmark keys have no per-key spend limit.

Parallax writes `manifest.json`, `results.jsonl`, and trial artifacts.
Resume a stopped run with `parallax run --resume RUN_DIR`. Environment
interruptions are retried with `--retry-invalid` and `--retry-wait`. Agent failures are recorded separately.

After the run artifacts pass validation, each group's job summary contains
trial tallies, costs and failed check IDs from `parallax report RUN_ID`. Task evidence and judge commentary
are excluded. When both `PARALLAX_REPORT_WEBHOOK_URL` and
`PARALLAX_REPORT_WEBHOOK_SECRET` are configured, the same summary is sent as
JSON with `X-Webhook-Signature` containing the hex HMAC-SHA256 of the exact
request body. Reporting failures do not change campaign acceptance or
publication. The report's status is the campaign matrix outcome, not a
claim that every task passed.

New task records belong in the combined task source. A record must use
`Area.BROWSER`, require `Capability.BROWSER`, and name `browser-evidence-v2`.
Task identity comes from `versions.json`. Do not put a password or token in a
record or prompt. The fastbrowse harness resolves task secrets by origin.

## Publication

Publication reads Parallax run directories through one mapping in
`fastbrowse_evals.publication`. The mapping converts `TrialResult` to the
existing public `Row`, sums physical retry measurements, and keeps typed timing
and cost details. It then runs `fastbrowse.evals.benchmarks.check_catalog`.

The mapping preserves these public arm names:

- `fastbrowse` to `fastbrowse`;
- `fastbrowse-pool` to `fastbrowse-pool`;
- `browser-use-cloud` with model `bu-ultrafast` to `browser-use-ultrafast`;
- other `browser-use-cloud` results to `browser-use`;
- `browser-harness` to `browser-harness`;
- `agent-browser` on its pinned model to `agent-browser`, a campaign arm that no publication group carries yet;
- `browser-use-oss` to `browser-use-oss`.

`fastbrowse-hosted`, `cua-codex` and `browser-harness-claude-code` remain diagnostic. Candidate schema 7,
`GROUPS`, site groups, and all files under `docs/results` stay as recorded.

The official benchmark workflow uses the pinned Parallax's `bu-bench` and
`online-mind2web` sources ([evals/PARALLAX.md](../../evals/PARALLAX.md)). It clones the pinned upstream judge for each source,
installs its dependencies in that checkout's own virtual environment, and
passes the official source's sanitized run directory to the normal publication
mapping. Do not restore the deleted fastbrowse official runner or row adapter.
