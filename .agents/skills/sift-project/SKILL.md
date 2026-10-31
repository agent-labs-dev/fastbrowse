---
name: sift-project
description: "Quality gate, audit evidence, live roots and cleanup boundaries for fastbrowse. Load before sift audits."
---

# fastbrowse audit guidance

fastbrowse's core is Rust, in `crates/` (ADR 0004). The `fastbrowse` wheel carries it as `fastbrowse._native`, and
`src/fastbrowse` is the thin Python layer: models, the native bridge and the evals. Two owned JavaScript expressions,
`crates/fastbrowse-browser/js/snapshot.js` and `capture.js`, run inside pages through CDP. `packages/sdk`, `go`,
`java` and `dotnet` are clients of `fastbrowse serve --stdio`.

## Gate

From the repository root, after `uv sync --all-extras` and `npm ci --ignore-scripts`:

```sh
uv run ruff check .
uv run ruff format --check .
uv run ty check
uv run python scripts/changelog.py --check "$(uv version --short)"
uv run python scripts/npm_versions.py
uv run python scripts/no_slop.py
uv run python scripts/docs_sync.py
uv run vale sync
uv run vale README.md CHANGELOG.md AGENTS.md CONTRIBUTING.md docs skills src scripts tests
uv run python -m fastbrowse.evals.storage --manifest docs/results/evidence.manifest.json --check
uv run pytest -q
uv run python -m fastbrowse.evals.benchmarks --check-diff origin/main
uv run python -m fastbrowse.evals.publication --check-diff origin/main
npm run check:browser
npm run generate:sdk && git diff --exit-code -- packages/sdk/src/protocol.ts
npm run check:sdk
uv run actionlint
uv run python .sift/agents.py check
uv run python .sift/gate.py --base origin/main
just rust
```

Every command must exit 0. Node.js 22+ is needed only for development. CI retains the required `check`
status, which depends on the Python matrix, Rust job, AUR job, secret scan, sift job and SDK job, and on the five
binary builds when a change touches what is built. The sift job uses the PR base
rather than main for its changed-file rules. No ast-grep dependency is needed while there are no rules.
The vendored sift helpers are version 0.3.0; do not rewrite their source locally.

## Evidence

These commands produce candidates, not blocking verdicts. Run them from the repository root.

```sh
uvx vulture==2.16 src tests scripts --min-confidence 80 --sort-by-size
npx --yes jscpd@4.0.8 --silent --reporters json --output .sift/runs/evidence/jscpd src/fastbrowse/*.py src/fastbrowse/evals/*.py src/fastbrowse/audit/*.py scripts tests
uvx zizmor==1.31.1 --offline .github/workflows
npx --yes markdownlint-cli2@0.23.3 README.md AGENTS.md CONTRIBUTING.md SECURITY.md 'docs/*.md'
npx --yes -p typescript@7.0.2 tsc --allowJs --checkJs --noEmit --target es2022 --lib es2022,dom,dom.iterable crates/fastbrowse-browser/js/capture.js crates/fastbrowse-browser/js/snapshot.js
gitleaks detect --source . --no-banner --redact
```

- Vulture reports unused context-manager exception parameters. Their protocol signatures are live.
- Clone detection reports shared test setup and client protocol shapes. Read both contracts before merging them.
- Zizmor remains advisory in CI: the Jev review executes base code and treats the PR as diff data;
  the release uses a local reusable site workflow. No ignore markers conceal those warnings.
- Markdownlint's initial scan reported 1,077 issues, including line length and table layout. Vale remains the prose gate.
- JavaScript type checking initially reported 156 issues, including DOM narrowing, implicit parameters and
  the injected window registry. It is evidence until those annotations can be added separately.
- Gitleaks is pinned to 8.28.0 in CI. No secret or detector baseline is installed.

## Live roots

- `fastbrowse.run_task`, package exports, public Pydantic models and callback protocols are consumed by embedders.
- `crates/fastbrowse` re-exports `run_task` and its types for crates.io, and the crates under it publish at the same version. `crates/fastbrowse-py` (pyo3) and `crates/fastbrowse-node` (the Node addon) are called only across the language boundary.
- `pyproject.toml` registers `fastbrowse.commands:fastbrowse` and `fastbrowse.commands:fastbrowse_mcp` console scripts, which run the Rust CLI in `fastbrowse._native`.
- `fastbrowse serve` is the Rust CLI's; `fastbrowse.protocol` models are the wire the npm SDK reads.
- `packages/sdk/src/index.ts` exports are the npm package's public API. `fastbrowse.scripted:run_task` is named
  at run time by `serve --run-task` in the smoke scripts, and the binary's `scripted` feature answers it.
- `scripts/` stand-alone programs are called from `ci.yml`, `binaries.yml` and `release.yml`.
- MCP registers its browse tool and uses its schema/docstrings as model-visible descriptions.
- `crates/fastbrowse-browser/src/session.rs` compiles in `crates/fastbrowse-browser/js/snapshot.js`, `capture.js` and the autoconsent bundle.
- CDP event registrations, context-manager methods, pytest fixtures and test discovery call symbols indirectly.
  `benchmarks/` is collected only by `codspeed.yml`, outside `testpaths`.
- Evals and audit modules have `python -m` entry points; scripts are also called from CI and the justfile.
- Approved benchmark rows, sanitized evidence projections, README and changelog content are consumed by
  fastbrowse.ai outside this repository.

## Model-read text

Text a model reads is behaviour, never cleanup:

- Prompts and Jev questions in `crates/fastbrowse-agent/src` (agent, planner, policy, retrieval, verification,
  shortcut) and `crates/fastbrowse-core/src/safety.rs`.
- LLM response schemas, including `.doc()` field descriptions and `constrained(...)` bounds.
- MCP tool descriptions in `crates/fastbrowse-cli/src/mcp`, and task text.
- What the observation renders: `ObservationLimits` and `TokenBudget` in `crates/fastbrowse-core/src/config.rs`,
  `crates/fastbrowse-browser/src/page/mod.rs`, and the two page scripts.
Validate behavior changes against dev evals; heldout runs are before and after a round and are not debugged.
See [the evaluation workflow](../../../docs/agents/evals.md) for paid-run prerequisites.

## Zones

| Paths | Zone |
|---|---|
| crates/*/src, crates/fastbrowse-browser/js/capture.js and snapshot.js | production |
| src/fastbrowse Python | production |
| crates/*/tests Rust | test |
| crates/*/tests/golden/, crates/fastbrowse-clients/tests/golden/ | fixture |
| packages/sdk/src/ except protocol.ts | production |
| packages/sdk/test/ | test |
| go/ Go except protocol.go and *_test.go | production |
| go/*_test.go | test |
| java/src/main except protocol/ | production |
| java/src/test/ | test |
| dotnet/src/ except Protocol.g.cs | production |
| dotnet/tests/ | test |
| packages/sdk/src/protocol.ts, protocol/schema.json, go/protocol.go, java/src/main/java/dev/agentlabs/fastbrowse/protocol/, dotnet/src/Fastbrowse/Protocol.g.cs | generated |
| scripts/ | script |
| tests/ Python, benchmarks/ Python | test |
| tests/browser/sites/, benchmarks/sites/, evals/fixtures/, audit/fixtures/, bench/cassettes/ | fixture |
| benchmarks/golden/ | generated |
| docs/results/, src/fastbrowse/evals/versions.json, packaging/aur/.SRCINFO | generated |
| crates/fastbrowse-browser/js/autoconsent/, .sift/gate.py, .sift/agents.py | vendor |
| .github/, packaging/, tool manifests and lockfiles | config |
| Markdown and .agents/skills/ | docs |

The golden fixtures under `crates/*/tests/golden/` hold the exact requests the removed Python core made, keyed byte
for byte, with the replies it got. A change to text a model reads changes those request strings. Rewrite only the
changed text in them and keep every reply.

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

- Context-manager exception arguments are protocol parameters, even when unused.
- Similar provider client shapes implement different wire protocols: crates/fastbrowse-clients/src/system_one.rs and
  vercel.rs.
- Browser scripts return CDP-serializable values and use DOM globals; they are not Node.js modules.
- snapshot.js stays a parenthesized function expression because the Rust page layer appends its mode arguments.
  capture.js is embedded inside parentheses and cannot have a leading or trailing semicolon.
  Their format markers preserve those contracts; JavaScript lint rules still apply, but the formatter
  cannot enforce the expression bodies without also rewriting their wrappers.

## Anti-patterns

- **arbitrary-cap**: a limit on what a model, grader or trace sees that no provider documents, such as a text
  prefix, a `take(N)`, a control quota or an output token ceiling (arbitrary-caps). Example:
  `crates/fastbrowse-core/src/decision.rs`, a profile holding limits its provider does not document. A documented
  limit is named as the provider's, as `DecisionProfile` does for Jev.
- **hidden-override**: code that rewrites, filters or replaces a model's output by a rule the model is never told
  (model-second-guessing). Example: `crates/fastbrowse-agent/src/agent/recovery.rs` carrying on past `give_up=true`.
- **python-port-residue**: a comment that explains Rust code by the removed Python core, a test file, class or function
  that no longer exists (comment-narration, session-residue). Example: `crates/fastbrowse-agent/src/agent/state.rs`
  `Flow`'s doc. `src/fastbrowse` is still live Python, and golden fixtures keep their Python names.

## Project rules and lenses

Existing Ruff rules cover Python style and correctness. Biome lists its JavaScript correctness rules explicitly.
Tests already enforce eval fingerprints, generated results, provider contracts, status semantics and safety behavior.
The changelog check, no_slop and Vale own release entries and prose. Do not duplicate those in sift rules.
The script rule `cited-path-exists` fails a code comment in a changed file that cites a repository path which does not
exist. No ast-grep rules have been added: whether a model reads a capped value needs judgment. The project lenses
`.sift/lenses/arbitrary-caps.md`, `.sift/lenses/model-second-guessing.md` and `.sift/lenses/eval-tuning.md` cover the
shapes above.
