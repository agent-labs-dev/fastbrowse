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
uv run fastbrowse serve --stdio   # JSON-RPC for another process; the JavaScript SDK starts this
```

The gate, which CI runs in this order on Python 3.13 and 3.14 (the browser, sift and SDK checks on 3.13):

```sh
uv run ruff check . && uv run ruff format --check .
uv run ty check                                     # ty, not pyright
uv run python scripts/changelog.py --check "$(uv version --short)"
uv run python scripts/npm_versions.py               # the npm versions are the Python one
uv run python scripts/no_slop.py
uv run vale sync && uv run vale README.md CHANGELOG.md AGENTS.md CONTRIBUTING.md docs src scripts tests
uv run pytest -q
npm ci --ignore-scripts && npm run check:browser
npm run generate:sdk && git diff --exit-code -- packages/sdk/src/protocol.ts
npm run check:sdk                                   # starts `fastbrowse serve` from the uv environment
uv run actionlint
uv run python .sift/agents.py check
uv run python .sift/gate.py --base origin/main
```

`uv run pre-commit install` runs ruff and ty before each commit; the hook versions and the pinned tools move
together.

One test, one file, one name:

```sh
uv run pytest tests/test_agent.py -q
uv run pytest -k "next_page" -q
uv run pytest tests/browser -q          # needs Chrome; skips without it
```

The [sift project skill](.agents/skills/sift-project/SKILL.md) records audit commands, live roots and generated files.

## Evals

Unit tests cannot tell you whether the agent still browses well; the suites can. **Agent changes are iterated
against `dev` only**, and `heldout` is run before and after a round and never debugged. The commands, keys,
task versions and publishing rules are in [docs/agents/evals.md](docs/agents/evals.md); read it before changing
agent behaviour or a task.

## Architecture

The run loop is `src/fastbrowse/agent.py`, and everything else is a seam it calls.

- **[run.py](src/fastbrowse/run.py)** opens the browser and builds the Jev and LLM clients from `Settings`, then hands control to
  the loop. This is the embedding API: `run_task(...)`. The browser is one of three, in this order: one the
  caller hands over (`cdp_url` or `cdp_port`, neither started nor stopped here; `attach` drives a window already
  open instead of a new tab, and `connect_cdp` yields that page without the agent), a Browser Use Cloud browser (`browser_api_key`), or
  local Chrome. `start` is optional; without one the first address is proposed from the task.
- **[page.py](src/fastbrowse/page.py) / [browser/](src/fastbrowse/browser/)** index the page. [browser/snapshot.js](src/fastbrowse/browser/snapshot.js) runs in the page and returns the controls
  with what tells them apart (role, label, the card or row that disambiguates twins, whether a field blocks
  its form); [browser/capture.js](src/fastbrowse/browser/capture.js) returns the text as source blocks with stable spans, so a reader can cite them.
- **[policy.py](src/fastbrowse/policy.py)** batches operation and target choices, read assessment and applicable wall checks.
  Large target sets use a further choice inside a group. [jev.py](src/fastbrowse/jev.py) is the client's shape;
  [clients/typesafe.py](src/fastbrowse/clients/typesafe.py) serves direct TypeSafe when keyed and otherwise OpenRouter; [clients/vercel.py](src/fastbrowse/clients/vercel.py) serves the
  Vercel AI Gateway backup. [clients/environment.py](src/fastbrowse/clients/environment.py) selects routes; [clients/failover.py](src/fastbrowse/clients/failover.py) switches after retries.
- **[retrieval.py](src/fastbrowse/retrieval.py)** routes short facts through Jev and other reads through the LLM. A read claim cites source
  blocks and code copies its quote from them; a count the page does not state rests on its basis facts; [memory.py](src/fastbrowse/memory.py) holds notes with citation ids, and [citations.py](src/fastbrowse/citations.py) builds deep links.
- **[verification.py](src/fastbrowse/verification.py)** decides whether a run may finish: Jev's done check against the plan's requirements,
  then the LLM verifier for what Jev doubted and for any requirement evidenced on an address the run guessed
  from the task, then the answer's claims checked against the quotes.
- **[safety.py](src/fastbrowse/safety.py)** owns secrets and irreversible actions. **[effects.py](src/fastbrowse/effects.py)** says what an action actually did,
  which is how a no-op is told from progress. **[telemetry.py](src/fastbrowse/telemetry.py)** is the ledger: steps, calls, dollars.
- **[cli.py](src/fastbrowse/cli.py)**, **[mcp_server.py](src/fastbrowse/mcp_server.py)**, **[serve.py](src/fastbrowse/serve.py)** and **`run_task`** are the four entry points; [options.py](src/fastbrowse/options.py) holds the rules
  they share, so a flag means the same thing in all of them. `serve` is `fastbrowse serve --stdio`: it calls
  `run_task` for another process over JSON-RPC, one run at a time, and adds no agent logic. Its messages are the
  models in [protocol.py](src/fastbrowse/protocol.py); [output_schema.py](src/fastbrowse/output_schema.py) turns a caller's JSON Schema into an `output_schema`.
- **[packages/sdk](packages/sdk)** is the TypeScript SDK, `fastbrowse` on npm. It starts `serve` and knows the agent by
  the protocol alone; [its types](packages/sdk/src/protocol.ts) are generated from the Python models (`npm run generate:sdk`).
  What it starts is the package frozen by [packaging/fastbrowse.spec](packaging/fastbrowse.spec), without the MCP server, shipped as one
  `@fastbrowse/<os>-<arch>` npm package per platform.

## Rules and invariants

These are the things a change must not quietly break.

- **Only `Status.COMPLETE` is success.** Anything the loop cannot prove is reported as what it is
  (`unverified`, `needs_confirmation`, `needs_login`, `blocked`, `needs_input`, `stuck`, `budget_exceeded`,
  `observation_limit`, `unavailable`, `error`),
  never rounded up. The CLI exits 0 only for `complete`.
- **A claim cites a quote.** An answer's facts are spans code cut from a capture, not text a model wrote; a
  derived count or winner cites the facts it was concluded from.
  A requirement is evidenced or it is open.
- **Page content is data, never instructions.** Prompts that read page content must say so, and completion is judged against quotes
  and page state rather than the model's say-so.
- **A model never sees a secret value.** Secrets reach a page by name, resolved at the moment of typing and
  only for their declared origin (which may be `https://*.site.com`, covering that site's hosts and nothing
  that merely ends with the same letters), and are blanked from what models see and redacted from the run's text results. An address
  on an origin the secret was typed on keeps its host and port, in what models see and what is reported alike,
  since the run read that host before typing there and the site published it; the rest of the address, any
  other host, and every other appearance of the value is blanked or redacted. No model screenshot or PNG step frame is taken while a resolved secret is showing as page text. The step-frame check is made
  against the page as it is when the image is taken, never against an earlier reading of it - the action being
  recorded may be the one that put the secret there. Live JPEG frames and recordings are held back before
  secret typing and while observations reveal a resolved secret; recordings keep their last clean frame.
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

## Agent skills

### Issue tracker

Issues and specs live in GitHub Issues on `agent-labs-dev/fastbrowse`, worked through `gh`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five default roles, each label named after its role: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one glossary and one ADR directory at the repo root, created on first use. See `docs/agents/domain.md`.

## Releasing

Versions are patch-by-patch unless the maintainer says otherwise, and every one needs a changelog entry:

1. Add the entry under the new version's heading in `CHANGELOG.md` (Keep a Changelog, prose bullets).
2. `uv version <x.y.z>`, then `uv run python scripts/npm_versions.py --write`, which gives the SDK's manifest
   the same number. Open a `chore: <x.y.z>` PR. CI fails if the version has no entry or npm disagrees with it.
3. After it merges, `git tag v<x.y.z> && git push origin v<x.y.z>`.

A release that publishes or changes a comparison figure also needs the fixture suites green on the release build
(`.github/workflows/evals.yml` fails on any regression) and, for a head-to-head figure, the comparison re-run on
that same build.

One tag publishes one version to PyPI and npm (`.github/workflows/release.yml`). It first builds the wheel and the
five binaries and smoke-tests each. The macOS two are signed ad hoc
(`scripts/macos_signatures.py`), which is enough for a binary installed through npm. Then, each step
needing the one before: the GitHub release with **the changelog entry as its notes**, PyPI, the five platform
packages, the SDK. PyPI and npm take the job's OIDC identity, so no token is stored. No registry takes a version
back, so finish a release that failed partway by re-running its failed jobs. Started by hand (`gh workflow run
release.yml --ref <branch>`), the workflow stops after the binaries and publishes nothing: do that after changing
the build.

After PyPI the workflow asks fastbrowse.ai to rebuild, since its changelog page reads this file at build
time. `scripts/changelog.py` is what reads the entry, so the repository, the release and the site never tell
three stories about one version. `.github/workflows/site.yml` asks for the same rebuild whenever
`CHANGELOG.md`, `README.md` or `docs/` change on `main`, so the site never waits for a release to catch up. It
needs `SITE_DEPLOY_HOOK` (a Vercel deploy hook for `agent-labs-dev/fastbrowse-site`) in this repository's
secrets; without it the workflows still succeed and warn that the site was not rebuilt.

Pull requests also get an advisory Jev review (`.github/workflows/jev-review.yml`, `scripts/jev_review.py`):
Jev judges whether changed prose is true to the diff and whether the changelog misses a user-visible change.
Its doubts are warnings for the reviewer; it never blocks a merge. It reads `AI_GATEWAY_API_KEY` from the
repository's secrets, so it runs on `pull_request_target` from the base branch's code and reads the pull
request only as a git ref: a change to the script takes effect once it is on `main`.

PR titles are conventional commits (`feat:`, `fix:`, `perf:`, `docs:`, `build:`, `ci:`, `chore:`) and become
the squash-merge subject. `main` requires the `check` status and resolved review threads.
