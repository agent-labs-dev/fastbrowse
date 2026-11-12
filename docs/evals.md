# Measuring fastbrowse

We measure fastbrowse with browser fixture tests and benchmark tasks run through the `evals/` project, a Parallax plugin.

The [benchmark page](https://fastbrowse.ai/benchmarks) describes these measurements:

| Comparison | Arms | Measurement |
|---|---|---|
| C1, agent against agent (`internal-hosted`) | fastbrowse and Jev Ultrafast live, Browser Use hosted default and Browser Harness from the competitor reference | Source task success on each task an agent can attempt |
| C2, backend against backend (`internal-backend`) | fastbrowse on its own browser pool, fastbrowse on a Browser Use Cloud browser | Source task success on matched tasks |
| C3, against Ultrafast (`internal-ultrafast`) | fastbrowse on its own browser pool, Jev Ultrafast | Source task success on the tasks both run |

These three are the only comparisons; [benchmark results](benchmarks.md) records the figures each has. The
upstream suites below and the Codex with Cua Driver arm are diagnostics, not comparisons:

| Diagnostic | Agents | Measurement |
|---|---|---|
| BU Bench | fastbrowse, with Browser Use's published figures as a separate reference | Official weighted rubric score |
| Online-Mind2Web | fastbrowse, with published figures as a separate reference | Official full-task success |

No attempt has a time, step or spend limit. An official benchmark's partial
rubric score is not a solved-task percentage. A reported comparison needs matching task revision, scope,
judge and browser protocol; missing reference configuration is disclosed rather than assumed equivalent.

A model cost receipt records its physical provider request count, including retries. The `llm_calls` efficiency metric sums those counts and is omitted when any LLM receipt has no count. Its `llm_calls_basis` is `physical_requests`. Historical rows without a basis counted cost lines. The release gate compares call counts only on the same basis; a changed or unknown basis produces an advisory. Historical receipts remain unknown; a logical cost line is not assumed to be one provider request.

## Fixture checks

Local and mock suites exercise real browsers, completion rules and authorization gates. They are regression
checks, not claims about performance across the web. `.github/workflows/evals.yml` runs them nightly and on
demand, and a release publishes only after a dispatched run on its tree passed. Each run's trace is kept in the
`nightly.traces.jsonl` artifact (`--traces`), so a failure seen only in CI can be replayed from its own requests.

The recorded run: `uv run python -m fastbrowse.evals.runner --suite local --repeat 1` exercises real local Chrome and grades
what the fixture server recorded: **6/6 passed** (recorded 2026-10-15). The fixture receipt a published
result rests on, its head sha and workflow run id, is recorded in `docs/results/benchmarks.json`, and the local
run is listed in `docs/results/evidence.manifest.json`. The mock suite is the larger companion suite.

```sh
uv run python -m fastbrowse.evals.runner --help
uv run python -m fastbrowse.evals.publication --help
```

## Task identity and result evidence

`evals/` owns internal prompts, truth functions and graders. Its generated metadata export,
`src/fastbrowse/evals/benchmark-catalog.json`, contains task ids, versions, grader fingerprints, eligible
agents, the reason each head-to-head agent that skips a task cannot run it, and source hashes. `evals/` checks the export against its canonical
sources; fastbrowse validates it against the public version lock.

`src/fastbrowse/evals/public-tasks.json` holds the text of all 53 live tasks and is maintained by hand. Once a task can be read it no longer measures unseen work, so a separate private set does that job.

Recorded results name the task catalog digest, `evals/` harness commit, fastbrowse build and provider route.
Comparisons report task coverage, repeat counts, physical attempts, costs and regressions. Full logs,
recordings, answers and page content stay out of Git and the public feed.

Headlines use complete suites. Diagnostic campaigns report their coverage and limitations separately.

`docs/results/evidence.public.json` contains public run summaries, with exact content hashes
pinned by `evidence.manifest.json`. The site validates both files at one commit before rendering.

Read [the eval workflow](agents/evals.md) before changing agent behavior or running paid checks.

## Default LLM measurements

`DEFAULT_LLM` in `crates/fastbrowse-clients/src/environment.rs` is `openai/gpt-6-luna` for every purpose, at low
reasoning. The LLM proposes a first address, writes field values the task does not state word for word, and composes answers; Jev makes every other call.
On 8 tasks where the text model matters, on build `bc2940d9` with Browser Use Cloud, it passed 18 of 22 runs, as
`deepseek/deepseek-v4.1-flash-1017` with reasoning off did, at a lower median passing time (5.30s against 5.95s)
and about half the LLM spend per run ($0.0023 against $0.0041).

DeepSeek V4.1 Flash 1017 was the default before it. On the local and mock suites, one run each on the same build and
Jev, it passed 35 of 38 tasks, as `anthropic/claude-sonnet-5.5` did, at a tenth of Sonnet's LLM spend ($0.0018
against $0.0191) and a lower mean time per task (3.9s against 4.2s).

Gemini 3.5 Flash Lite at low reasoning was also measured as a candidate default against DeepSeek V4.1 Flash
1017 on the Sauce Demo checkout: one task, 3 repeats per arm, identical prompt and limits, local Chrome.

| Arm (resolved model) | Build | Verified | Median wall | Mean cost | Endings |
|:--|:--|:--|--:|--:|:--|
| Flash Lite, low reasoning | `16e13b68` | 1/3 | 34.9s | $0.0307 | 1 complete, 2 stuck |
| DeepSeek V4.1 Flash 1017 | `16e13b68` | 1/3 | 91.9s | $0.0555 | 1 complete, 1 budget_exceeded, 1 stuck |
| main default (3.8-flash + 3.5-flash-lite) | `e1d478b` | 2/3 | 45.5s | $0.0532 | 2 complete, 1 stuck |
| 0.5.25 published campaign (reference) | `48c964d` | 3/3 | not recorded | not recorded | |

At n=3 this shows no improvement: Flash Lite is level with DeepSeek on completions and behind main, though
62% faster and 45% cheaper than DeepSeek. Both Flash Lite failures placed the order, then could not read the
order total from the confirmation page and gave up. That is the separately filed unsatisfiable-requirement
shape, not a model defect.

## Codex with Cua Driver

A diagnostic arm, not one of the three comparisons.

The internal comparison catalog includes the opt-in `cua-codex` arm, using the Codex CLI and Cua Driver
releases pinned in `evals/src/fastbrowse_evals/adapters/arm-pins.json`. Parallax runs it through
browser MCP tools on a private Linux desktop, with the existing source task graders. Credential and safe-stop tasks are excluded.

Rows record the local browser environment, and billed spend remains unknown. Local timings
are not a controlled comparison with cloud arms. The recorded-results view shows each campaign separately. Source grades, completion,
physical attempts and known costs are reported independently; unknown cost is never treated as zero.
See `evals/` and [docs/agents/evals.md](agents/evals.md) for setup and retained attempt traces.

### API-equivalent estimates

An optional `estimated_api_cost` accompanies a recorded run without filling its `dollars` field.
It names the model, usage digest, rate source and rate-check date. Missing usage or an unknown
service tier stays unknown. Grade, completion and recorded time remain unchanged.

Recover Cua usage from exact artifact paths in a retained private ledger:

```sh
uv run python -m fastbrowse.evals.api_cost --ledger <ledger.jsonl> --artifact-root <checkout> --out <recovery.json>
```

The recovery file binds estimates to the ledger digest and row index. It contains token counts and
cost ranges, without messages, answers or local artifact paths. It does not publish results or edit
the ledger. A publication must retain the original sources and verify that digest before joining rows.

GPT-6 Astra standard API rates are $10 per million uncached input tokens, $1 for cached input,
$12.50 for cache writes and $50 for output. Reasoning tokens are included in output tokens.
[OpenAI pricing](https://developers.openai.com/api/docs/models/gpt-6-astra) doubles input and cache
rates and multiplies output rates by 1.5 for requests above 272,000 input tokens. CLI usage totals
combine requests, so totals above that threshold yield a short-to-long-context range. Totals below
it prove every request used short-context rates. Estimates exclude subscription charges, browser
infrastructure and runs without reported turn usage. Codex may omit startup prewarm tokens from
turn totals, so these bounds cover only recorded tokens, not the complete run. The basis is
`recorded-token-api-equivalent-range`. Estimates remain separate from recorded spend.

## Desktop apps

`fastbrowse-evals desktop OUT` (in `evals/`) measures `fastbrowse --desktop` against Claude driving
[deskpilot](https://github.com/cjber/deskpilot)'s MCP tools itself, on a GTK order form. Each arm starts a fresh form
with deskpilot's `launch`, on its own agent's hidden workspace, and each task grades the form's own saved state: the
order it placed, field by field, or that it placed none. A read-only task's answer must name the value. A run also fails
if the form's window ever takes keyboard focus. It needs a Hyprland session with a deskpilot that has `launch` and its
plugin loaded, PyGObject with GTK 3 for the form (`--python`), `OPENROUTER_API_KEY` for fastbrowse and a signed-in
`claude` for the baseline; `--arm` and `--task` narrow a run. Recipes are not measured, since `--desktop` refuses them.

## Timing

A row's `seconds` runs from the browser session being ready to the agent's final answer, for every arm. Opening and
closing the session are outside it and are recorded apart as `session_open_seconds` and `session_close_seconds`. Each
row records `timing_basis`, currently `session-ready-to-answer`; a row without one was measured on an older clock. A
row whose agent connects to its browser where the harness cannot see (hosted Browser Use, Browser Harness) keeps that
connection in its seconds and says so in `timing_note`.
`benchmarks.py` refuses a group that mixes bases, the release gate never matches rows across them, and `summary`
reports no seconds for a comparison across them. `bench.py`'s `wall` is a separate diagnostic: spawn to reply on a
shared Chrome.

The internal live observer retains a PNG of the final viewport alongside document text and control values. It
captures the selected tab before lifting modal visibility attributes, and supplies the image through the judge's
existing image evidence contract. An ambiguous final tab yields no image. Tasks declaring secrets, including stored
credentials, withhold the image entirely; callers that do not declare secret usage also withhold it. A failed image
capture records its reason and keeps the text and control observations. Viewport images cover only what was visible,
so document text and controls remain part of the evidence. Each image records its observed viewport dimensions and
device pixel ratio separately: an arm may retain document evidence from before its browser daemon resets the
viewport, while the image shows the layout at the later observer read. Existing rows keep their original observations.
