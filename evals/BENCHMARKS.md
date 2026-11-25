# Browser benchmarks

The browser benchmark is a Parallax plugin. Install the eval environment from
`evals/`, then use `parallax run` for a campaign and
`fastbrowse-evals browser publish-candidate` for the publication projection.

## Plugin entry points

The package registers these entry points:

- `fastbrowse`: the fastbrowse agent on a Browser Use Cloud browser.
- `fastbrowse-pool`: the same agent on a browser leased from the fastbrowse pool.
- `fastbrowse-hosted`: the fastbrowse hosted API, kept as a diagnostic product arm.
- `fastbrowse`: one `parallax.task_sources` source containing the core, dev, stretch-dev, heldout, and sweep task records.

Every task is world-less, uses `Area.BROWSER`, and names Parallax's
`browser-evidence-v2` policy. The task record carries the start URL, task
metadata, authorization setting, and secret names. Secret values stay in the
fastbrowse harness.

Parallax supplies the browser harnesses `browser-use-cloud`, `browser-use-oss`,
`browser-harness` (Codex), `browser-harness-claude-code`, and `cua-codex`. New campaign figures use Parallax's final
page observer, retry rules, judging identity, typed timing, component costs,
details, and run artifacts.

## Development and held-out suites

`core`, `dev` and `stretch-dev` are development sets: fastbrowse has been iterated against their tasks, so their
scores are development-set results. `heldout` holds tasks the agent has never been tuned on; run it before and after
a change, and never debug against it. It runs on the same arms as the other suites, so a comparison gets a held-out signal by naming `heldout` in the
campaign's suites. A task in it moves to a development suite as soon as a fix is shaped by it.

Every static-site task records `data.intended_outcome`, which the judge reads beside the run's evidence. A task whose
answer moves with its site (the PyPI tasks) records the rule that finds the answer instead of a value.

## Running a campaign

Run one invocation for each comparison. The runner writes `manifest.json`,
`results.jsonl` and the trial artifacts into the run directory.

```sh
uv run parallax run fastbrowse \
    --harness fastbrowse \
    --repeats 3 \
    --retry-invalid 2 \
    --retry-wait 60 \
    --out runs/internal-fastbrowse
```

The backend comparison uses:

```sh
uv run parallax run fastbrowse \
    --harness fastbrowse,fastbrowse-pool \
    --repeats 3 \
    --out runs/internal-backend
```

The configured provider key limits apply across campaigns that share a key.
`--retry-invalid` and `--retry-wait` cover environment interruptions. The
offline test suite runs without paid campaigns.

## Arm mapping

Publication maps Parallax results to the existing public arm names in one place:

| Parallax result | Public arm |
| --- | --- |
| `fastbrowse` | `fastbrowse` |
| `fastbrowse-pool` | `fastbrowse-pool` |
| `browser-use-cloud` with model `bu-ultrafast` | `browser-use-ultrafast` |
| `browser-use-cloud` otherwise | `browser-use` |
| `browser-harness` | `browser-harness` |
| `browser-use-oss` with model `bu-4.0` (browser-use 1.0.6's default), or `bu-fast` from browser-use 1.0.5 | `browser-use-oss` |
| `fastbrowse-hosted`, `cua-codex` and `browser-harness-claude-code` | diagnostic only |

The public schema remains 7. `GROUPS`, site groups, task versions, catalog
coverage, and published history are unchanged. The candidate projection reads
Parallax run directories and does not rewrite `docs/results` history.

## Candidate projection

```sh
uv run fastbrowse-evals browser publish-candidate \
    runs/internal-fastbrowse \
    --campaign-id release-0.11.0 \
    --out benchmark-candidate.json
```

The projection sums physical attempts for a task and repeat, retains typed
timing and cost components, and rejects a candidate that does not pass the
existing publication gate.

## Official suites

Parallax 2.0.0.dev26 supplies the official sources `bu-bench` and `online-mind2web`,
with policies `bu-bench-upstream-v1` and `online-mind2web-upstream-v1`. The
official workflow checks out the pinned upstream judge for each source, installs
its dependencies in that checkout's own environment, and runs:

```sh
parallax run bu-bench --harness fastbrowse
parallax run online-mind2web --harness fastbrowse
```

The workflow passes `--retry-invalid` and `--retry-wait`. It uploads an encrypted private run and a sanitized run
directory with provenance for the publication stage. Official results use the same mapping as
internal results, keyed by the run selection, and keep the upstream score in
`TrialResult.score`. Published history is not regenerated.
