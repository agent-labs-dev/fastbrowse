---
name: sift-project
description: "Quality gate, audit evidence, live roots and cleanup boundaries for fastbrowse. Load before sift audits."
---

# fastbrowse audit guidance

fastbrowse is a Python 3.13+ package with CLI, MCP, embedding and `serve` entry points. Two owned JavaScript
expressions run inside browser pages through CDP. Python browser tests exercise them against Chrome.
`packages/sdk` is the TypeScript SDK published to npm as `fastbrowse`. It starts `fastbrowse serve --stdio`,
and its tests run against that server from the uv environment.

## Gate

From the repository root, after `uv sync --all-extras` and `npm ci --ignore-scripts`:

```sh
uv run ruff check .
uv run ruff format --check .
uv run ty check
uv run python scripts/changelog.py --check "$(uv version --short)"
uv run python scripts/npm_versions.py
uv run python scripts/no_slop.py
uv run vale sync
uv run vale README.md CHANGELOG.md AGENTS.md CONTRIBUTING.md docs skills src scripts tests
uv run pytest -q
npm run check:browser
npm run generate:sdk && git diff --exit-code -- packages/sdk/src/protocol.ts
npm run check:sdk
uv run actionlint
uv run python .sift/agents.py check
uv run python .sift/gate.py --base origin/main
```

Every command must exit 0. Node.js 22+ is needed only for development. CI retains the required `check`
status, which depends on the Python matrix, secret scan, sift job and SDK job, and on the five binary builds
when a change touches what is frozen. The sift job uses the PR base
rather than main for its changed-file rules. No ast-grep dependency is needed while there are no rules.
The vendored sift helpers are version 0.3.0; do not rewrite their source locally.

## Evidence

These commands produce candidates, not blocking verdicts. Run them from the repository root.

```sh
uvx vulture==2.16 src tests scripts --min-confidence 80 --sort-by-size
npx --yes jscpd@4.0.8 --silent --reporters json --output .sift/runs/evidence/jscpd src/fastbrowse/*.py src/fastbrowse/clients src/fastbrowse/adapters src/fastbrowse/evals/*.py src/fastbrowse/audit/*.py scripts tests
uvx zizmor==1.30.1 --offline .github/workflows
npx --yes markdownlint-cli2@0.23.3 README.md AGENTS.md CONTRIBUTING.md SECURITY.md 'docs/*.md'
npx --yes -p typescript@7.0.2 tsc --allowJs --checkJs --noEmit --target es2022 --lib es2022,dom,dom.iterable src/fastbrowse/browser/capture.js src/fastbrowse/browser/snapshot.js
gitleaks detect --source . --no-banner --redact
```

- Vulture reports six unused context-manager exception parameters. Their protocol signatures are live.
- Clone detection reports shared test setup and client protocol shapes. Read both contracts before merging them.
- Zizmor remains advisory in CI: the Jev review executes base code and treats the PR as diff data;
  the release uses a local reusable site workflow. No ignore markers conceal those warnings.
- Markdownlint's initial scan reported 1,077 issues, including line length and table layout. Vale remains the prose gate.
- JavaScript type checking initially reported 156 issues, including DOM narrowing, implicit parameters and
  the injected window registry. It is evidence until those annotations can be added separately.
- Gitleaks is pinned to 8.28.0 in CI. No secret or detector baseline is installed.

## Live roots

- `fastbrowse.run_task`, package exports, public Pydantic models and callback protocols are consumed by embedders.
- `pyproject.toml` registers `fastbrowse.cli:main` and `fastbrowse.mcp_server:main` console scripts.
- `fastbrowse serve` is reached through `cli.main`; `fastbrowse.protocol` models are the wire the npm SDK reads.
- `packages/sdk/src/index.ts` exports are the npm package's public API. `fastbrowse.scripted` is named at run
  time by `serve --run-task` in the smoke scripts and is a hidden import of the frozen build.
- `scripts/` stand-alone programs are called from `ci.yml`, `binaries.yml` and `release.yml`.
- MCP registers its browse tool and uses its schema/docstrings as model-visible descriptions.
- `src/fastbrowse/browser/page.py` loads snapshot and capture JavaScript by file path; both are wheel assets.
- CDP event registrations, context-manager methods, pytest fixtures and test discovery call symbols indirectly.
- Evals and audit modules have `python -m` entry points; scripts are also called from CI and the justfile.
- Approved benchmark rows, sanitized evidence projections, README and changelog content are consumed by
  fastbrowse.ai outside this repository.

## Model-read text

Prompts in agent, planner, policy, retrieval, verification, safety and shortcut modules, MCP tool descriptions,
LLM response schemas, Jev questions and task text are behavior. Never shorten them as cleanup.
Validate behavior changes against dev evals; heldout runs are before and after a round and are not debugged.
See [the evaluation workflow](../../../docs/agents/evals.md) for paid-run prerequisites.

## Zones

| Paths | Zone |
|---|---|
| src/fastbrowse Python and browser/capture.js, browser/snapshot.js | production |
| packages/sdk/src/ except protocol.ts | production |
| packages/sdk/test/ | test |
| packages/sdk/src/protocol.ts | generated |
| scripts/ | script |
| tests/ Python | test |
| tests/browser/sites/, evals/fixtures/, audit/fixtures/ | fixture |
| docs/results/, src/fastbrowse/evals/versions.json | generated |
| src/fastbrowse/browser/autoconsent/, .sift/gate.py, .sift/agents.py | vendor |
| .github/, packaging/, tool manifests and lockfiles | config |
| Markdown and .agents/skills/ | docs |

## Conventions

Keep Python compatible with 3.13, Ruff at 120 columns and typed seam models. Preserve status, authorization,
secret-origin and citation invariants. Comments explain the constraint behind code. Use ASCII punctuation.
Parallax exports benchmark metadata; `fastbrowse.evals.versions` checks identities and generates the approved result feed.
Detailed runs remain in private Langfuse. See `docs/agents/evals.md` before changing benchmark contracts.

## Risk order

1. Tool configuration and documentation.
2. Reporting scripts and eval infrastructure, preserving versioned grading contracts.
3. Clients, adapters and browser indexing.
4. Reading, citation memory and verification.
5. Run loop, authorization and secret handling.

## Settled

- Context-manager exception arguments are protocol parameters, even when unused: adapters/browser_use_cloud.py.
- Similar provider client shapes implement different wire protocols: clients/typesafe.py and clients/vercel.py.
- Browser scripts return CDP-serializable values and use DOM globals; they are not Node.js modules.
- snapshot.js stays a parenthesized function expression because Python appends its mode arguments.
  capture.js is embedded inside parentheses and cannot have a leading or trailing semicolon.
  Their format markers preserve those contracts; JavaScript lint rules still apply, but the formatter
  cannot enforce the expression bodies without also rewriting their wrappers.

## Anti-patterns

None recorded yet.

## Project rules and lenses

Existing Ruff rules cover Python style and correctness. Biome lists its JavaScript correctness rules explicitly.
Tests already enforce eval fingerprints, generated results, provider contracts, status semantics and safety behavior.
The changelog check, no_slop and Vale own release entries and prose. Do not duplicate those in sift rules.
No ast-grep rules, script rules or project lenses have been added: the audit found no new mechanical defect shape.
