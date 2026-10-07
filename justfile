# Run the local fixture suite (also uses paid model APIs).
evals-local *args:
    uv run python -m fastbrowse.evals.runner {{args}}

# Validate and publish rows under their recorded release, then regenerate all outputs.
evals-publish rows:
    uv run python -m fastbrowse.evals.versions --publish auto {{quote(rows)}}

# Regenerate the approved site feed; optional arguments include --bump TASK_ID.
evals-docs *args:
    uv run python -m fastbrowse.evals.versions --docs {{args}}
