# The first publication on `evals/`

The workflows run the campaign and official benchmarks from `evals/` in this repository. What the first
candidate they publish needs:

- The committed catalog's `task_sources["more_tasks.py"]` moved because the file imports from
  `fastbrowse_evals` now. A catalog that moves only its source hashes is a protocol change to the publication
  gate (`benchmarks._retirement_only`), so the first candidate after the move needs the owner's
  `protocol_change` approval. Task versions and fingerprints are unchanged.
- A run records the catalog hash the harness renders (`fastbrowse_evals.versions.provenance`) and measures any
  build. Publication refuses a candidate whose measured build ships a different catalog
  (`fastbrowse_evals.publication.export`). 0.10.0 ships the old hash, so the first build this harness can
  publish is the first one that contains the regenerated catalog.
- The candidate's `runner_sha` is now a fastbrowse commit. The publication check that it is in the
  checkout's history (`runner_pinned`) runs inside this repository, so it holds by construction.
