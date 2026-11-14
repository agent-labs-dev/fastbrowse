# Running evals

Read this page before changing a task, the browser plugin, or a benchmark
publication result. Do not run a paid campaign while iterating on code.

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
measured fastbrowse SHA. It runs `parallax run` with the plugin installed,
`--budget` from the campaign input, and `--reserve` for each trial. Keep the
pool GCP service account and the `harness_sha` and `FASTBROWSE_SHA` install
order intact.

Parallax writes `manifest.json`, `results.jsonl`, `budget.jsonl`, and trial
artifacts. Resume a stopped run with `parallax run --resume RUN_DIR` and keep
the same budget and reserve. Environment interruptions are retried with
`--retry-invalid` and `--retry-wait`; an agent failure is not retried as an
environment interruption.

New task records belong in the combined task source. A record must use
`Area.BROWSER`, require `Capability.BROWSER`, and name `browser-evidence-v1`.
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
- `browser-use-oss` to `browser-use-oss`.

`fastbrowse-hosted` and `cua-codex` remain diagnostic. Candidate schema 7,
`GROUPS`, site groups, and all files under `docs/results` stay as recorded.

The official benchmark workflow uses Parallax dev20's `bu-bench` and
`online-mind2web` sources. It clones the pinned upstream judge for each source,
installs its dependencies in that checkout's own virtual environment, and
passes the official source's sanitized run directory to the normal publication
mapping. Do not restore the deleted fastbrowse official runner or row adapter.
