# Migration notes

Fastbrowse evals now consume Parallax dev20 directly from PyPI. The old live
runner, duplicate evidence and judging code, request wallets, spend proxy,
tracking ledger, and competitor adapter modules are deleted.

The task functions were converted into one immutable task specification table.
`live_tasks.py` exposes its `TaskRecord` list to Parallax, while
`more_tasks.py` and `sweep_tasks.py` remain small suite views for catalog and
documentation code. Task versions continue to come from `versions.json`.

Publication reads Parallax result directories through
`fastbrowse_evals.publication`. It maps every selected result and its physical
retries to the existing `fastbrowse.evals.benchmarks.Row`, then runs the
existing schema 7 publication gate. Historical candidate and evidence files
are not regenerated.

The official suite runner, dataset loader, upstream bridge, and official rows
adapter were deleted. Parallax now supplies `bu-bench` and
`online-mind2web`, and the official workflow installs each pinned upstream
judge in its own environment. Official run directories use the same
TrialResult-to-Row mapping as internal campaigns, keyed by the task source in
the run selection.

Offline verification uses synthetic `TrialResult` values and a synthetic
Parallax run directory. It makes no browser, provider, judge, or Langfuse call.
