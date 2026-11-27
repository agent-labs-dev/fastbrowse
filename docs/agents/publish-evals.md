# Publish benchmark results

Internal browser campaigns run through the Parallax plugin in `evals/`. The
campaign writes a Parallax run directory. The publication adapter reads those
directories and writes the existing `benchmark-candidate.json` contract.
Merging the fastbrowse-site pull request remains the approval step.

## Run and collect

Install the plugin environment and the fastbrowse build it will measure:

```sh
uv sync --project evals --locked --no-install-package fastbrowse
uv pip install --python evals/.venv/bin/python -e .
```

The campaign workflow installs the harness checkout first, then installs the
measured fastbrowse SHA. Keep that order, the `harness_sha`, and the pool GCP
service account unchanged.

Spend limits are configured on provider keys and are shared by campaign groups that use the same key.

```sh
uv run --project evals --no-sync parallax run fastbrowse \
  --harness fastbrowse --repeats 3 --retry-invalid 2 --retry-wait 60 \
  --out runs/internal-fastbrowse
```

Parallax writes `manifest.json`, `results.jsonl`, and private trial artifacts.
Retry settings cover environment interruptions. The campaign keeps the complete
run directory as the source for publication.

A narrowed rerun can use the workflow's `only` input for affected task ids. `candidate_only`
skips new trials and builds from the campaign's downloaded run artifacts. The
stage selects, for each task and repeat, the newest run that measured it in full.

## Arms and task coverage

The plugin registers `fastbrowse`, `fastbrowse-pool`, and `fastbrowse-hosted`.
Parallax supplies `browser-use-cloud`, `browser-use-oss`, `browser-harness`
(Codex), `browser-harness-claude-code`, `agent-browser` (Codex), and `cua-codex`. The publication mapping is:

| Parallax result | Public arm |
| --- | --- |
| `fastbrowse` | `fastbrowse` |
| `fastbrowse-pool` | `fastbrowse-pool` |
| `browser-use-cloud` with model `bu-ultrafast` | `browser-use-ultrafast` |
| other `browser-use-cloud` results | `browser-use` |
| `browser-harness` | `browser-harness` |
| `agent-browser` on its pinned model | `agent-browser`, a partial arm of `internal-hosted` |
| `browser-use-oss` | `browser-use-oss` |
| `browser-use-1-luna` on `openai/gpt-6-luna` | `browser-use-1-luna`, a partial arm of `internal-hosted` |
| `fastbrowse-hosted`, `cua-codex` and `browser-harness-claude-code` | diagnostic only |

Every task record is world-less, uses `Area.BROWSER`, requires browser
capability, and names `browser-evidence-v2`. Parallax owns evidence splitting,
screenshots, the fallback judge, judge identity, interruption retries, typed
timing, cost components, and harness details. The fastbrowse harness only
provides the agent integration and final page observation.

## Build the candidate

The adapter reads one or more Parallax run directories. It runs on the harness's own fastbrowse, not
the measured build: the candidate schema and catalog are the harness's, and each run's provenance names
the build it measured. `--catalog-sha` names the commit of that harness checkout: the candidate records
it as `catalog_sha`, and the site reads the catalog there to check `catalog_sha256`.

```sh
uv run fastbrowse-evals browser publish-candidate \
  runs/internal-fastbrowse runs/internal-hosted \
  --campaign-id release-0.11.0 \
  --catalog-sha "$(git rev-parse HEAD)" \
  --out benchmark-candidate.json
```

`TrialResult` becomes one public `Row`. Physical attempts are retained in
`physical_runs`, with their typed timing, usage, and costs summed on the row.
The adapter then runs `fastbrowse.evals.benchmarks.check_catalog` before it
writes the candidate. A candidate uses schema 9, the existing `GROUPS`, the
existing site groups, and the existing task versions. Schema 9 adds `catalog_sha`; a candidate before it has
its catalog read at `agent_sha`. Schema 8 adds `agent-browser` and `browser-use-1-luna` to `internal-hosted`;
a schema 7 candidate keeps that group's five arms.

The candidate carries an export receipt containing the run bundle digest,
campaign id, measured build, group ids, row count, and Parallax run trace. It
does not rewrite `docs/results`, publication baselines, evidence manifests, or
past releases. Those files remain exactly as recorded.

## Site publication

The campaign stage uploads the candidate and run facts. The site publication
workflow copies the candidate byte for byte into the site data and generates
the site group files. The site computes figures from per-attempt rows. A run
artifact is evidence for the candidate, not approval. The site pull request is
the approval boundary.

The existing gate still checks exact task coverage, task versions, arm sets,
repeats, scores, completion, timing bases, costs, and regression limits. A
protocol change must update the schema and gate together, and must preserve
the old figures as recorded history.

## Official suites

The official workflow runs Parallax 2.0.0.dev26's `bu-bench` and
`online-mind2web` sources with policies `bu-bench-upstream-v1` and
`online-mind2web-upstream-v1`. It clones each pinned upstream judge at the
revision named by Parallax, installs its own dependencies in its own virtual
environment, and maps the existing judge secrets to Parallax's official bridge.

The workflow sends the encrypted private run to the benchmark artifact and
uploads a sanitized run directory containing the manifest, provenance,
and results. Provenance records the measured fastbrowse and harness
commits, so the release gate can verify the exact producer run. The campaign
stage downloads those runs and passes them to the same
`TrialResult` to `Row` mapping used by internal campaigns. The run selection
identifies the official group, and the upstream score remains the row score.
Published history, baselines, and evidence manifests remain unchanged.

## Local verification

Use synthetic values and a synthetic Parallax run directory for offline checks:

```sh
uv run --project evals --no-sync pytest \
  evals/tests/test_parallax_publication.py -q
uv run --project evals --no-sync parallax run fastbrowse \
  --harness fastbrowse --repeats 1 --budget 1 --reserve 1 --dry-run
```

Do not run a paid browser campaign while iterating on code. Do not regenerate
published files as part of a plugin change.
