# Evals

Fastbrowse's internal browser benchmark is a Parallax plugin in `evals/`.
Parallax is the one generic eval engine. It owns task loading, world-less task
records, browser evidence, judging, retries, budgets, run artifacts, and the
browser harnesses.

## Run the benchmark

Install the eval environment first:

```sh
cd evals
uv sync --locked
```

Run a local dry run without credentials or network calls:

```sh
uv run parallax run fastbrowse \
  --harness fastbrowse --repeats 1 --budget 1 --reserve 1 --dry-run
```

A campaign uses the same command without `--dry-run`, normally with three
repeats and an approved budget:

```sh
uv run parallax run fastbrowse \
  --harness fastbrowse,fastbrowse-pool \
  --repeats 3 --budget 300 --reserve 3 \
  --retry-invalid 2 --retry-wait 60 --out runs/internal-backend
```

The plugin registers `fastbrowse`, `fastbrowse-pool`, and
`fastbrowse-hosted`. Parallax also provides `browser-use-cloud`,
`browser-use-oss`, `browser-harness`, and `cua-codex`.

Each task is world-less, has `Area.BROWSER`, and uses
`browser-evidence-v1`. A browser harness stores the final page under trial
metadata key `browser`, using the `plain-cdp` observer. Timing, cost
components, and secret-free harness details use Parallax trial hooks.

## Results and publication

Parallax writes a run directory containing `manifest.json`, `results.jsonl`,
`budget.jsonl`, and private trial artifacts. The publication adapter reads those
files and maps `TrialResult` values to the existing
`fastbrowse.evals.benchmarks.Row` schema. Physical retries are retained in the
row's `physical_runs` count.

The mapping is:

- `fastbrowse` to `fastbrowse`;
- `fastbrowse-pool` to `fastbrowse-pool`;
- `browser-use-cloud` with `bu-ultrafast` to `browser-use-ultrafast`;
- other `browser-use-cloud` results to `browser-use`;
- `browser-harness` to `browser-harness`;
- `browser-use-oss` to `browser-use-oss`.

`fastbrowse-hosted` and `cua-codex` are diagnostic and have no public arm.
Candidate schema 7, publication groups, site groups, task versions, and
published history remain unchanged.

Use `fastbrowse-evals browser catalog PATH` to export or check the public task
catalog. Use `fastbrowse-evals browser publish-candidate RUN_DIR ...` to build a
candidate from Parallax run directories. The command runs the existing
publication gate before writing the candidate.

The official benchmark workflow runs Parallax dev20's `bu-bench` and
`online-mind2web` task sources. It checks out each pinned upstream judge in its
own environment, maps the upstream judge key through Parallax's official
bridge, and passes sanitized Parallax run directories with provenance to the
same publication mapping. Published history remains unchanged.
