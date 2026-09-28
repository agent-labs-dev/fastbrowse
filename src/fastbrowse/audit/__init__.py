"""Capability checks for the CLI, MCP and embedding API.

    uv run python -m fastbrowse.audit --tier 0
    uv run python -m fastbrowse.audit --tier 1 --spend --max-dollars 0.05

Tier 0 checks contracts without model calls. Tier 1 uses local browser fixtures, tier 2 uses live sites,
and tier 3 exercises MCP and embedding. Cases that call models require --spend and carry a per-case budget.
Each report records the observed status, exit code, cost and assertion evidence.
"""
