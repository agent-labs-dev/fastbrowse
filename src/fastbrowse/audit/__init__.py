"""The capability audit suite: cases as data, run by tier, with a per-case budget.

    uv run python -m fastbrowse.audit --tier 0
    uv run python -m fastbrowse.audit --tier 1 --spend --max-dollars 0.05

The audit suite layers on top of the repository's own tests and evals rather than replacing them. It
checks the contracts an operator relies on when they run fastbrowse (through the terminal, the embedding
API or the MCP server), and it measures how a run spends time and money.

Tiers
-----

- Tier 0 is contract and static checks. It never spends model money, so it is safe in CI.
- Tier 1 drives the agent against the repository's local fixture sites with a headless browser.
- Tier 2 drives the agent against real sites on the open web.
- Tier 3 exercises the integration surfaces: the MCP server and the embedding API.

Tiers 1 to 3 call paid models, so the runner refuses them unless `--spend` is given. The default tier is
tier 0.

The status-to-exit-code contract (T0.10)
----------------------------------------

The terminal maps a run's status to a process exit code, and the mapping was implicit in the code until
this suite wrote it down. Every `fastbrowse.models.Status` member maps as follows.

- `complete` exits 0. It is the only status that reports success.
- Every other status exits 1: `unverified`, `needs_confirmation`, `needs_login`, `blocked`, `needs_input`,
  `stuck`, `budget_exceeded`, `observation_limit`, `unavailable` and `error`.
- A configuration refusal, such as a missing key or a limit the run cannot use, exits 1 as well.
- A command line usage error, such as a flag choparse cannot parse, exits 2.

The decision lives at `src/fastbrowse/cli.py:238` (`return 0 if result.succeeded else 1`), the `succeeded`
property at `src/fastbrowse/models.py:254` (`return self.status is Status.COMPLETE`), and the
configuration refusal at `src/fastbrowse/cli.py:265`. The `--json` refused shape is built at
`src/fastbrowse/cli.py:241` (`_refused`). The audit case `T0.10` asserts this mapping is total and stable.
"""
