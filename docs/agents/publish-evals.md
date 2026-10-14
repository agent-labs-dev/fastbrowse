# Publish benchmark results

Full browser campaigns run through Parallax. Its exporter validates clean builds, scheduled slots, source
identity, physical ledgers and private Langfuse read-back before producing `docs/results/benchmark-candidate.json`.
The file contains sanitized per-run grades, completion, time and cost. Official rubric scores retain
their source meaning. Internal hosted and Ultrafast comparisons use separate matched task groups.

Schema 1 requires all four original groups. Schema 2 accepts nonempty subsets of those four groups.
Schema 3 also supports `internal-fastbrowse`: all 54 supported internal tasks, three repeats each, with
fastbrowse alone. The fastbrowse-only group does not require comparator reruns. Official
fastbrowse suites do not require new comparator runs. Previously published comparator figures can be linked
as references with their task and build differences stated. Every group inside one candidate still comes
from the same measured agent and runner build. Adding a suite must retain the published groups and their
regression checks; results from different builds cannot be combined under one build receipt.
Schemas 2 and 3 record each group's agent and runner commits and require them to match the candidate's build.

CI validates the candidate's exact coverage and compares matched scores, time and cost with the previous
approved candidate. Unresolved costs, missing grades, incomplete coverage and a failed fixture workflow
block publication. These checks detect recorded regressions; they do not prove that every website or
grader is free of bugs. Review unexpected outcomes before approving.

To approve reviewed results, dispatch `publish-evals.yml` on `main` with the SHA256 of the exact candidate
bytes. For the first publication, also approve establishing the initial baseline. Changed task or judge
protocols require a matched baseline; changing a limit note cannot skip regression checks.
Only the maintainer can issue this approval. The workflow revalidates coverage and the fixture
receipt and returns a `benchmark-approval` artifact. Download `benchmark-approval.json`, copy the reviewed candidate to `docs/results/benchmarks.json` and include both in
the reviewed data PR and wait for green CI before merging. An upload to Langfuse does not approve
publication.

The website reads the candidate and approval from one resolved fastbrowse commit. It checks the exact-byte
digest and the completed owner-dispatched workflow receipt before deriving any figure. A changed
candidate needs a new approval. Raw answers, page content, recordings and private trace links stay in
Langfuse and ignored local artifacts.
