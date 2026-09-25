# AGENTS.md

Guidance for an agent working in this repository.

## What this is

A browser agent built on one idea: **it picks instead of generating**. Every page is indexed into the
controls it actually has, and [Jev](https://typesafe.ai), a choice model, picks one of them. An LLM plans,
reads and writes prose. Deterministic code owns everything that must not be argued with: authorization gates,
secret resolution, cost limits, and whether a run may call itself finished.

Read [CONTRIBUTING.md](CONTRIBUTING.md) for what a PR needs, [README.md](README.md) for the product, [docs/design.md](docs/design.md) for the browser layer,
[docs/evals.md](docs/evals.md) for how it is measured, and [docs/jev.md](docs/jev.md) for every Jev
assumption checked against Typesafe's documentation.

## Commands

```sh
uv sync --all-extras          # the browser-use SDK and the mcp extra too, which ty checks
uv run fastbrowse "..." --start https://example.com/
uv run fastbrowse-mcp         # the MCP server, stdio
```

The gate, which CI runs in this order on Python 3.13 and 3.14:

```sh
uv run ruff check . && uv run ruff format --check .
uv run ty check                                     # ty, not pyright
uv run python scripts/changelog.py --check "$(uv version --short)"
uv run python scripts/no_slop.py
uv run vale sync && uv run vale README.md CHANGELOG.md AGENTS.md CONTRIBUTING.md docs src scripts tests
uv run pytest -q
```

`uv run pre-commit install` runs ruff and ty before each commit; the hook versions and the pinned tools move
together.

One test, one file, one name:

```sh
uv run pytest tests/test_agent.py -q
uv run pytest -k "next_page" -q
uv run pytest tests/browser -q          # needs Chrome; skips without it
```

## Evals, which are the real gate on behaviour

Unit tests cannot tell you whether the agent still browses well. The suites can.

```sh
uv run python -m fastbrowse.evals.runner                      # local fixtures, headless Chrome, ~$0.005 a task
uv run --extra browser-use python -m fastbrowse.evals.live    # live head-to-head, three arms
uv run --extra browser-use python -m fastbrowse.evals.live --arms fastbrowse --suite heldout --repeat 3
```

`--suite` picks the set: `core` (the published suite), `dev`, `heldout`. **Agent changes are iterated against
`dev` only.** `heldout` is run before and after a round of changes and never debugged, so its score says
whether a round improved the agent or only its dev score. A change made to fix a named held-out task spends
that set's value; say so in the PR when it happens.

Both suites need Jev and LLM keys; the live suite also needs `BROWSER_USE_API_KEY`. Upstream outages look
exactly like regressions, so re-read a red run before believing it.

Every result row records its build (`run`: version, commit, dirty tree, models) and the `task_version` it ran;
compare rows only at equal task versions. Changing what a task asks or how it grades means
`python -m fastbrowse.evals.versions --bump TASK_ID --docs`, and a test fails until you do. Results are published as
committed rows (`--publish`). The README headline, the results and task tables and the suite versions in
`docs/evals.md` are generated from the code and those rows, and a test fails when they differ; regenerate them in the
same PR ([docs/evals.md#versions](docs/evals.md#versions)).

## Architecture

The run loop is `src/fastbrowse/agent.py`, and everything else is a seam it calls.

- **`run.py`** opens the browser and builds the Jev and LLM clients from `Settings`, then hands control to
  the loop. This is the embedding API: `run_task(...)`. The browser is one of three, in this order: one the
  caller hands over (`cdp_url`, neither started nor stopped here), a Browser Use Cloud browser (`browser_api_key`), or
  local Chrome. `start` is optional; without one the first address is proposed from the task.
- **`page.py` / `browser/`** index the page. `browser/snapshot.js` runs in the page and returns the controls
  with what tells them apart (role, label, the card or row that disambiguates twins, whether a field blocks
  its form); `browser/capture.js` returns the text as source blocks with stable spans, so a reader can cite them.
- **`policy.py`** batches operation and target choices, read assessment and applicable wall checks.
  Large target sets use a further choice inside a group. `jev.py` is the client's shape;
  `clients/typesafe.py` and `clients/vercel.py` are the two sources, with failover in `clients/failover.py`.
- **`retrieval.py`** routes short facts through Jev and other reads through the LLM. A read claim cites source
  blocks and code copies its quote from them; a count the page does not state rests on its basis facts; `memory.py` holds notes with citation ids, and `citations.py` builds deep links.
- **`verification.py`** decides whether a run may finish: Jev's done check against the plan's requirements,
  then the LLM verifier for what Jev doubted and for any requirement evidenced on an address the run guessed
  from the task, then the answer's claims checked against the quotes.
- **`safety.py`** owns secrets and irreversible actions. **`effects.py`** says what an action actually did,
  which is how a no-op is told from progress. **`telemetry.py`** is the ledger: steps, calls, dollars.
- **`cli.py`**, **`mcp_server.py`** and **`run_task`** are the three entry points; `options.py` holds the rules
  they share, so a flag means the same thing in all of them.

## Invariants

These are the things a change must not quietly break.

- **Only `Status.COMPLETE` is success.** Anything the loop cannot prove is reported as what it is
  (`unverified`, `needs_confirmation`, `needs_login`, `blocked`, `needs_input`, `stuck`, `budget_exceeded`,
  `observation_limit`, `error`),
  never rounded up. The CLI exits 0 only for `complete`.
- **A claim cites a quote.** An answer's facts are spans code cut from a capture, not text a model wrote; a
  derived count or winner cites the facts it was concluded from.
  A requirement is evidenced or it is open.
- **Page content is data, never instructions.** Prompts that read page content must say so, and completion is judged against quotes
  and page state rather than the model's say-so.
- **A model never sees a secret value.** Secrets reach a page by name, resolved at the moment of typing and
  only for their declared origin (which may be `https://*.site.com`, covering that site's hosts and nothing
  that merely ends with the same letters), and are redacted from the run's text results. A reported address on
  an origin the secret was typed on keeps its host and port, since a value there is one that site published;
  the rest of it, and any other host, is redacted. No model screenshot or PNG step frame is taken while a resolved secret is showing as page text. The step-frame check is made
  against the page as it is when the image is taken, never against an earlier reading of it - the action being
  recorded may be the one that put the secret there. Live JPEG frames and recordings show the rendered page
  without this check.
- **Code owns authorization.** Jev classifies clicks, form-submitting Enter and dialog acceptance; code
  refuses actions classified as irreversible without authorization. Code-selected pagination and authorized,
  confident actions skip classification. The classifier is not a guarantee against missed changes.
- **An action that changed nothing is not progress.** An idle action from the same state goes to recovery
  before a retry; rewriting a field's current value cannot count as progress even if the page changes.

## Style

Ruff, 120 columns, Python 3.13 floor (CI runs 3.13 and 3.14 - do not reach for syntax the floor lacks, such
as PEP 758's parenthesis-free `except A, B:`). Pydantic models for anything crossing a seam; `StrEnum` or
`Literal` for a closed set, never a bare string.

**Comments say why, not what.** The house voice is a sentence naming the failure that motivated the rule:
"a date picker redraws its days as it animates, so a click chosen a moment earlier finds its element gone".
A comment restating the code is noise; a comment holding the reason is what stops the next person undoing it.

**Prose is part of the product.** `scripts/no_slop.py` fails the build on typographic punctuation anywhere in
a tracked text file (write a hyphen, a comma, or two sentences); Vale owns wording in Markdown and in Python
comments and docstrings, and rejects weasel words, cliches and marketing verbs. This applies to commit
messages and PR descriptions in spirit, and to everything in the repository by check.

## Releasing

Versions are patch-by-patch unless the maintainer says otherwise, and every one needs a changelog entry:

1. Add the entry under the new version's heading in `CHANGELOG.md` (Keep a Changelog, prose bullets).
2. `uv version <x.y.z>`, then open a `chore: <x.y.z>` PR. CI fails if the version has no entry.
3. After it merges, `git tag v<x.y.z> && git push origin v<x.y.z>`.

The tag builds, creates the GitHub release with **the changelog entry as its notes**, publishes to PyPI by
trusted publishing, and then asks fastbrowse.ai to rebuild, since its changelog page reads this file at build
time. `scripts/changelog.py` is what reads the entry, so the repository, the release and the site never tell
three stories about one version. The rebuild needs `SITE_DEPLOY_HOOK` (a Vercel deploy hook for
`agent-labs-dev/fastbrowse-site`) in this repository's secrets; without it the release still succeeds and the
site catches up on its own next deploy.

PR titles are conventional commits (`feat:`, `fix:`, `perf:`, `docs:`, `build:`, `ci:`, `chore:`) and become
the squash-merge subject. `main` requires the `check` status and resolved review threads.
