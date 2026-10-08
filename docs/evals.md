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

Results remain private until failures are resolved, full runs are reviewed and the maintainer explicitly
approves publication. Approved figures are generated from the result feed on the benchmark page. They are
not copied into the README or generated Markdown tables.

Read [the eval workflow](agents/evals.md) before changing agent behavior or running paid checks.

## Codex with Cua Driver

The internal comparison catalog includes the opt-in `cua-codex` arm, using Codex CLI 0.160.1
and Cua Driver 0.34.0. Parallax runs it through browser MCP tools on a private Linux desktop,
with the existing source task graders. Credential and safe-stop tasks are excluded.

Rows record the local browser environment, and billed spend remains unknown. Local timings
are not a controlled comparison with cloud arms. Adding the arm publishes no new figures.
See Parallax's browser benchmark documentation for setup and retained attempt traces.
