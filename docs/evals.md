# Measuring Fastbrowse

Fastbrowse keeps browser fixture tests and release gates in this repository. Maintainer benchmarks run in
Parallax, under the `browser-use` family. Detailed runs, grades, costs and validation belong in the private
[fastbrowse-evals Langfuse project](https://us.cloud.langfuse.com/project/cmuwjxra401iyad0cymgswes5).

The [benchmark page](https://fastbrowse.ai/benchmarks) describes three separate measurements:

| Set | Agents | Measurement |
|---|---|---|
| Fastbrowse internal | Fastbrowse, Browser Use hosted default, Jev Ultrafast | Source task success on matched tasks |
| BU Bench | Fastbrowse, with Browser Use's published figures as a separate reference | Official weighted rubric score |
| Online-Mind2Web | Fastbrowse, with published figures as a separate reference | Official full-task success |

Ultrafast coverage and hosted-default fallbacks are reported separately. An official benchmark's partial
rubric score is not a solved-task percentage. A reported comparison needs matching task revision, scope,
judge and browser protocol; missing reference configuration is disclosed rather than assumed equivalent.

## Fixture checks

Local and mock suites exercise real browsers, completion rules and authorization gates. They are regression
checks, not claims about performance across the web. `.github/workflows/evals.yml` runs them on a schedule
and on demand. A release needs a green fixture run on its exact build.

```sh
uv run python -m fastbrowse.evals.runner --help
uv run python -m fastbrowse.evals.mock --help
uv run python -m fastbrowse.evals.publication --help
```

## Task identity and publication

Parallax owns internal prompts, truth functions and graders. Its generated metadata export,
`src/fastbrowse/evals/benchmark-catalog.json`, contains task ids, versions, grader fingerprints, eligible
agents and source hashes. It deliberately omits task text. Parallax checks the export against its canonical
sources; Fastbrowse validates it against the public version lock.

Recorded live results must name that catalog digest and a clean committed Parallax runner, alongside the
Fastbrowse build and provider route. The publication gate checks task coverage, repeat counts, the complete
physical-run ledger, known costs and matched regressions before accepting compact result rows. Full logs,
recordings, answers and page content stay out of Git and the public feed.

Benchmark headlines require the full publication gate and maintainer approval. The maintainer can also
approve recorded diagnostic campaigns for [the eval browser](https://fastbrowse.ai/evals), with their
coverage and limitations visible. These campaigns do not satisfy the benchmark headline gate.

`docs/results/evidence.public.json` contains the approved sanitized projection, with exact content hashes
pinned by `evidence.manifest.json`. The site validates both files at one commit before rendering. Detailed
traces remain private in Langfuse; archive receipts retain source identities without their contents.

Read [the eval workflow](agents/evals.md) before changing agent behavior or running paid checks.

## Codex with Cua Driver

The internal comparison catalog includes the opt-in `cua-codex` arm, using Codex CLI 0.160.1
and Cua Driver 0.34.0. Parallax runs it through browser MCP tools on a private Linux desktop,
with the existing source task graders. Credential and safe-stop tasks are excluded.

Rows record the local browser environment, and billed spend remains unknown. Local timings
are not a controlled comparison with cloud arms. The eval browser keeps the historical full round,
the later Fastbrowse round and the focused dev regression checks separate. Source grades, completion,
physical attempts and known costs are reported independently; unknown cost is never treated as zero.
See Parallax's browser benchmark documentation for setup and retained attempt traces.

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
infrastructure and any runs without complete usage. They never satisfy the official publication
contract's requirement for recorded cost.
