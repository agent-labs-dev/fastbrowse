# AGENTS.md

Guidance for an agent working in this repository.

## What this is

A browser agent built on one idea: **it picks instead of generating**. Every page is indexed into the
controls it actually has, and [Jev](https://typesafe.ai), a choice model, picks one of them. An LLM plans,
reads and writes prose. Jev also decides when a page is read and when the run is done. Code owns authorization
gates, secret resolution and cost limits, and states what it knows to Jev instead of overruling it.

Read [CONTRIBUTING.md](CONTRIBUTING.md) for what a PR needs, [README.md](README.md) for the product, [docs/design.md](docs/design.md) for the browser layer,
[docs/evals.md](docs/evals.md) for how it is measured, and [docs/jev.md](docs/jev.md) for every Jev
assumption checked against Typesafe's documentation.

## Standing rules

These hold for every change, and a reviewer rejects a change that breaks one.

- **No eval tuning.** Behaviour must be general, because the evals exist to predict how the agent does on sites
  nobody has measured. Never special-case a benchmark task, site, page wording, selector or known answer in
  product code, prompts, Jev questions, defaults or the harness: "if Google Flights says reload, then wait" is
  the kind of fix that is forbidden. Fix the general cause: state the facts to Jev and let it decide. Tests and
  eval fixtures may name sites; product code and prompts may not. A transient site problem (an error page, a
  5xx, a 429, maintenance, endless loading, a network error the agent did not cause) is an environment
  interruption: the agent may retry it in general ways, and the harness judges it as not an agent failure.
- **Jev decides.** Steps, reads and when the task is done move toward Jev, never away from it; the LLM is the
  fallback for what Jev cannot be asked. When Jev picks wrong, give it the fact it lacked in its question. A code
  gate that overrules Jev is deleted, not stacked with another.
- **No heuristics.** A keyword list, a regex over page text or a length cutoff standing in for
  a judgement is a bug: ask Jev, or the LLM where Jev cannot answer.
- **No caps.** Nothing a model reads or writes is cut short, except by a limit its provider documents, named as
  that provider's in the code.
- **Measure before claiming.** A claim about the agent rests on a run made for it, compared with a baseline on
  the same harness, judge and task set. [docs/agents/evals.md](docs/agents/evals.md) has how.

## Commands

```sh
uv sync --all-extras          # the eval-data extra too, which ty checks; builds the Rust core (cargo)
uv run fastbrowse "..." --start https://example.com/
uv run fastbrowse-mcp         # the MCP server, stdio
uv run fastbrowse serve --stdio   # JSON-RPC for another process; the JavaScript SDK starts this
```

The gate, which CI runs in this order on Python 3.13 and 3.14 (the browser, sift and SDK checks on 3.13):

```sh
uv run ruff check . && uv run ruff format --check .
uv run ty check                                     # ty, not pyright
uv run python scripts/changelog.py --check "$(uv version --short)"
uv run python scripts/npm_versions.py               # the npm and Rust versions are the Python one
uv run python scripts/no_slop.py
uv run python scripts/docs_sync.py                  # the docs name flags, settings, statuses and links the code has
uv run vale sync && uv run vale README.md CHANGELOG.md AGENTS.md CONTRIBUTING.md docs skills src scripts tests
uv run python -m fastbrowse.evals.storage --manifest docs/results/evidence.manifest.json --check
uv run pytest -q
uv run python -m fastbrowse.evals.benchmarks --check-diff origin/main
uv run python -m fastbrowse.evals.publication --check-diff origin/main
npm ci --ignore-scripts && npm run check:browser
npm run generate:sdk && git diff --exit-code -- packages/sdk/src/protocol.ts
npm run check:sdk                                   # starts `fastbrowse serve` from the uv environment
go -C go generate ./... && git diff --exit-code -- go && go -C go vet ./...
FASTBROWSE_TEST_BINARY="$PWD/target/debug/fastbrowse" go -C go test -race ./...   # after cargo build -p fastbrowse-cli --features scripted
python3 scripts/java_types.py && git diff --exit-code -- java
FASTBROWSE_TEST_BINARY="$PWD/target/debug/fastbrowse" mvn -B -f java/pom.xml verify    # Java 17+, Maven
python3 scripts/dotnet_types.py && git diff --exit-code -- dotnet
cd dotnet && FASTBROWSE_TEST_BINARY="$PWD/../target/debug/fastbrowse" dotnet test --solution Fastbrowse.slnx; cd ..
uv run actionlint
uv run python .sift/agents.py check
uv run python .sift/gate.py --base origin/main
just rust                                           # the Rust workspace (docs/adr/0001, 0004); needs cargo and just
```

`uv run pre-commit install` runs ruff and ty before each commit; the hook versions and the pinned tools move
together.

One test, one file, one name:

```sh
uv run pytest tests/test_native.py -q
uv run pytest -k "cancelled" -q
uv run pytest tests/browser -q          # needs Chrome; skips without it
cargo test -p fastbrowse-agent --test agent   # the run loop's own suites are the crates'
```

A change to `crates/fastbrowse-browser/js/snapshot.js` is checked against `benchmarks/golden/` (`test_snapshot_matches_golden`
in `crates/fastbrowse-browser/tests/capture.rs`; `FASTBROWSE_UPDATE_GOLDEN=1` rewrites them, and the diff is the
behaviour change) and timed by CodSpeed
(`.github/workflows/codspeed.yml`, locally `uv run --group bench pytest benchmarks --codspeed`).

The [sift project skill](.agents/skills/sift-project/SKILL.md) records audit commands, live roots and generated files.

## Pull requests and branches

PR titles are conventional commits (`feat:`, `fix:`, `perf:`, `docs:`, `build:`, `ci:`, `chore:`) and become
the squash-merge subject. `main` requires the `check` status, resolved review threads and a branch up to date
with `main`. To update one, rebase locally on `origin/main` and `git push --force-with-lease`; `gh pr
update-branch` puts a merge commit on the remote branch that the local one lacks.

Pull requests also get an advisory Jev review (`.github/workflows/jev-review.yml`, `scripts/jev_review.py`):
Jev judges whether changed prose is true to the diff and whether the changelog misses a user-visible change.
Its doubts are warnings, never a block. It runs on `pull_request_target` from the base branch's code, so a
change to the script takes effect once it is on `main`.

Entire links each commit made in an agent session to that session, locally only;
[docs/agents/entire.md](docs/agents/entire.md) has the commands and why uploads are off.

## Evals

Unit tests cannot tell you whether the agent still browses well; the suites can. **Agent changes are iterated
against `dev` only**, and `heldout` is run before and after a round and never debugged. Read
[docs/agents/evals.md](docs/agents/evals.md) before changing agent behaviour or a task, or spending on a paid
run: it has the commands, keys, task versions, what to check before spending, the CI campaign gotchas and where
evidence is kept.
Read [docs/agents/publish-evals.md](docs/agents/publish-evals.md) before staging or publishing a result, or
changing the candidate schema that Parallax and the site share.

The harness (`src/fastbrowse/evals`, `tests/evals`) reaches fastbrowse only as an embedder does: the `fastbrowse`
package's exports, `fastbrowse.models`, `fastbrowse.config`, `fastbrowse.protocol`, `serve --stdio` and the trace.
That way it measures what the package runs; `tests/evals/test_public_only.py` fails on anything else.

## Architecture

The core is Rust, in [crates/](crates) ([ADR 0004](docs/adr/0004-rust-core-by-default.md)), and the `fastbrowse`
wheel carries it as `fastbrowse._native`; the Python package is the thin layer over it, so a behaviour change
lands in the crates. The run loop is [crates/fastbrowse-agent/src/agent](crates/fastbrowse-agent/src/agent),
and everything else is a seam it calls. Read [docs/agents/architecture.md](docs/agents/architecture.md) before
changing a crate, an entry point, a client SDK or a type that crosses from Rust to Python: a field serde skips
never reaches Python.

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
  refuses an irreversible one unless the run is authorized and Jev judges the task asked for it. Only
  code-selected pagination skips classification. The classifier is not a guarantee against missed changes.
- **One stall rule.** A step that changes nothing, a read that adds nothing, a refused DONE and a return to a
  page state already seen with nothing read since are each no progress. `unchanged_actions` of them in a row
  ask Jev again without the stalled action. A page state not seen before or new evidence renews that budget. Rewriting a field's current value cannot count as progress even if the page
  changes.


## Prompts and models

A prompt is code, and a model's output is untrusted until code accepts it.

- **One call does one job.** A request that states a task states everything the next step needs from it. Split a
  job across calls only when the two cannot be asked in the same breath, and give each half its own contract.
- **The LLM proposes.** A schema or a citation is checked by code against the quotes or the page
  state, and the run's next step and its end are Jev's. Trust an LLM's output only where code can prove it
  complete; where it cannot, repair it deterministically or ask again. A partial result is reported as
  partial.
- **Every call leaves a receipt.** The ledger records each call's purpose, tokens and dollars, and a budget
  is checked before the call that would break it. A change to the calls a run makes is measured by calls per
  purpose, not wall clock.
- **Nothing a model reads is cut short.** No text, control or output cap, except one a provider documents, named as
  that provider's (Jev's in `DecisionProfile`). A rule that overrides a model is stated to that model.
- **A prompt change is a behaviour change.** It is iterated against the eval suites like any other agent
  change, and a unit test pins any instruction the behaviour depends on.


## Style

Python 3.13 is the floor while CI also runs 3.14, so syntax the floor lacks, such as PEP 758's
parenthesis-free `except A, B:`, passes locally on 3.14 and fails CI. Pydantic models for anything crossing a
seam; `StrEnum` or `Literal` for a closed set, never a bare string.

**Comments say why, not what.** The house voice is a sentence naming the failure that motivated the rule:
"a date picker redraws its days as it animates, so a click chosen a moment earlier finds its element gone".
A comment restating the code is noise; a comment holding the reason is what stops the next person undoing it.

**Prose is part of the product.** `scripts/no_slop.py` fails on typographic punctuation in any tracked text
file (write a hyphen, a comma, or two sentences), and Vale rejects weasel words, cliches and marketing verbs
in Markdown and in Python comments and docstrings. Commit messages and PR descriptions follow the same rules.

**The docs move with the code.** A user-visible change updates what describes it in the same PR: `README.md`,
`docs/`, `skills/fastbrowse/`, `packages/sdk/README.md`, `.env.example` and the changelog entry. fastbrowse.ai
builds its docs, skill and `llms` pages from these files and PyPI renders `README.md`, so a stale sentence here
is stale there too. A doc found trailing the code is fixed on its own, not left for the next change.
`scripts/docs_sync.py` catches a documented flag, setting, status, exit code or relative link the code lacks;
whether a sentence is true is checked against the code with the
[copywriting skill](https://github.com/cjber/skills/blob/main/copywriting/SKILL.md).

**Agent instructions stay short.** `.sift/agents.py check` holds this file to 300 lines. Detail only some tasks
need goes in `docs/agents/` behind a line here that says when to read it.

## Agent skills

- Issues and specs live in GitHub Issues on `agent-labs-dev/fastbrowse`, worked through `gh`:
  [docs/agents/issue-tracker.md](docs/agents/issue-tracker.md).
- Triage labels are the five default roles, each named after its role:
  [docs/agents/triage-labels.md](docs/agents/triage-labels.md).
- Domain docs are single-context, one glossary and one ADR directory at the root:
  [docs/agents/domain.md](docs/agents/domain.md).

## Credit contributors

A fix for a reported bug, a feature someone asked for, or someone's merged code credits them in its changelog bullet
and in [CREDITS.md](CREDITS.md). The rule is in [docs/agents/credits.md](docs/agents/credits.md).

## Releasing

Release only when the maintainer says to. [docs/agents/release.md](docs/agents/release.md) has the steps and
what the tag's workflow publishes; [docs/agents/aur.md](docs/agents/aur.md) has the Arch package.
