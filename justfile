# Run a named live suite; extra arguments go to the existing CLI.
evals-run suite="dev" *args:
    uv run --extra browser-use python -m fastbrowse.evals.live --suite {{suite}} {{args}}

# Run the local fixture suite (also uses paid model APIs).
evals-local *args:
    uv run python -m fastbrowse.evals.runner {{args}}

# Validate and publish rows under their recorded release, then regenerate all outputs.
evals-publish rows:
    uv run python -m fastbrowse.evals.versions --publish auto {{quote(rows)}}

# Regenerate tables and the site feed; optional arguments include --bump TASK_ID.
evals-docs *args:
    uv run python -m fastbrowse.evals.versions --docs {{args}}

# Fetch a pinned dataset, optionally sample and check HTTP reachability. No agent calls.
evals-data source *args:
    uv run --extra eval-data python -m fastbrowse.evals.datasets {{source}} {{args}}
