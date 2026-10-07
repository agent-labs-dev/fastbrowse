# Publish benchmark results

Full browser campaigns run through Parallax. Its exporter validates clean builds, scheduled slots, source
identity, physical ledgers and private Langfuse read-back before producing `docs/results/benchmark-candidate.json`.
The file contains sanitized per-run grades, completion, time and cost. Official rubric scores retain
their source meaning. Internal hosted and Ultrafast comparisons use separate matched task groups.

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

The website reads the candidate and approval from one resolved Fastbrowse commit. It checks the exact-byte
digest and the completed owner-dispatched workflow receipt before deriving any figure. A changed
candidate needs a new approval. Raw answers, page content, recordings and private trace links stay in
Langfuse and ignored local artifacts.
