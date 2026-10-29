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
FASTBROWSE_TEST_BINARY="$PWD/target/debug/fastbrowse" go -C go test -race ./...   # after cargo build -p fastbrowse-cli
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

## Agent session capture

[Entire](https://docs.entire.io) is enabled in this repository, so a commit made during an agent session is linked
to the session behind it: the prompt, the transcript, the tool calls and the files touched. The commit gets an
`Entire-Checkpoint` trailer, the context is stored in local Git refs under `entire/checkpoints`, and no extra commit
lands on the branch. `.entire/settings.json` and the hooks for Claude Code and Codex are committed here.

```sh
curl -fsSL https://entire.com/install.sh | bash   # or: brew install --cask entireio/tap/entire
entire enable                                     # hooks are already configured; this reinstalls them
entire status --detailed
entire checkpoint list --pending                  # what the next commit would carry
entire checkpoint explain <id|sha>
```

Automatic upload of checkpoints is off for this repository (`push_sessions` is false in `.entire/settings.json`),
because the repository is public and a checkpoint carries the prompt and transcript behind a commit. They stay
local until you send them somewhere private, with
`entire configure --local --checkpoint-remote github:<org>/<repo>`.

`uv run pre-commit install` rewrites the Git hooks, so run `entire enable` again after it. `entire agent add cursor`
installs hooks for another agent in the same way. OpenCode is left out: Entire's plugin uses the OpenCode 1 API,
which OpenCode 2 refuses to load ([entireio/cli#2588](https://github.com/entireio/cli/issues/2588)).

The committed hooks run through `sh`. On Windows, run `entire enable` in the checkout so Entire writes the hook form
for the local shell, and leave that rewrite uncommitted.

## Evals

Unit tests cannot tell you whether the agent still browses well; the suites can. **Agent changes are iterated
against `dev` only**, and `heldout` is run before and after a round and never debugged. The commands, keys,
task versions, Parallax benchmark ownership and publishing rules are in [docs/agents/evals.md](docs/agents/evals.md); read it before changing
agent behaviour or a task.

The harness (`src/fastbrowse/evals`, `tests/evals`) reaches fastbrowse only as an embedder does: the `fastbrowse`
package's exports, `fastbrowse.models`, `fastbrowse.config`, `fastbrowse.protocol`, `serve --stdio` and the trace.
That way it measures what the package runs; `tests/evals/test_public_only.py` fails on anything else.

Store detailed eval runs and validation in the dedicated Langfuse project. Keep local logs in an ignored
artifacts directory; Git keeps manifests, source hashes and compact publication baselines. Verify uploads by
reading them back before removing local evidence. Public results contain only the sanitized projection.

## Architecture

The core is Rust, in [crates/](crates) ([ADR 0004](docs/adr/0004-rust-core-by-default.md)), and the `fastbrowse`
wheel carries it as `fastbrowse._native`. The Python package is the thin layer over it:
[native.py](src/fastbrowse/native.py) hands `run_task`, `connect_cdp` and the commands to it, and the pydantic models
in `src/fastbrowse` are the shapes a caller passes and gets back. A behaviour change lands in the crates.

The run loop is [crates/fastbrowse-agent/src/agent](crates/fastbrowse-agent/src/agent), and everything else is a seam it calls.

- **[fastbrowse](crates/fastbrowse/src/lib.rs)** is the crate on crates.io a Rust caller depends on: `run_task` and the
  result's types, re-exported. The crates under it are published at the same version so it can be.
- **[fastbrowse-run](crates/fastbrowse-run/src/lib.rs)** opens the browser and builds the Jev and LLM clients from the
  environment, then hands control to the loop. Python's `run_task(...)` is this, in process. The browser is one of
  three, in this order: one the caller hands over (`cdp_url` or `cdp_port`, neither started nor stopped here; `attach`
  drives a window already open instead of a new tab, and `connect_cdp` yields that page without the agent), a Browser
  Use Cloud browser (`browser_api_key`), or local Chrome. `start` is optional; without one the first address is
  proposed from the task.
- **[fastbrowse-browser](crates/fastbrowse-browser/src)** indexes the page. [browser/snapshot.js](crates/fastbrowse-browser/js/snapshot.js) runs in the page and returns the controls
  with what tells them apart (role, label, the card or row that disambiguates twins, whether a field blocks
  its form); [browser/capture.js](crates/fastbrowse-browser/js/capture.js) returns the text as source blocks with stable spans, so a reader can cite them.
- **[policy.rs](crates/fastbrowse-agent/src/policy.rs)** batches operation and target choices, read assessment and applicable wall checks.
  Large target sets use a further choice inside a group. [decision.rs](crates/fastbrowse-core/src/decision.rs) is the client's shape;
  [system_one.rs](crates/fastbrowse-clients/src/system_one.rs) serves the System One API: direct TypeSafe when keyed, the recommended Jev route, otherwise OpenRouter, or Telnyx, Ollama or another host when chosen; [vercel.rs](crates/fastbrowse-clients/src/vercel.rs) serves the
  Vercel AI Gateway, which is also the Jev primary when no other Jev key is set. OpenRouter is supported for Jev but is not recommended. [environment.rs](crates/fastbrowse-clients/src/environment.rs) selects routes; [failover.rs](crates/fastbrowse-clients/src/failover.rs) switches after retries.
- **[retrieval](crates/fastbrowse-agent/src/retrieval)** routes short facts through Jev and other reads through the LLM. A read claim cites source
  blocks and code copies its quote from them; a count the page does not state rests on its basis facts; [memory.rs](crates/fastbrowse-agent/src/memory.rs) holds notes with citation ids, and [citations.rs](crates/fastbrowse-core/src/citations.rs) builds deep links.
- **[verification](crates/fastbrowse-agent/src/verification)** decides whether a run may finish: Jev's done check against the plan's requirements,
  then the LLM verifier for what Jev doubted and for any requirement evidenced on an address the run guessed
  from the task, then the answer: code checks each claim cites a current quote and each cited count or comparison
  rests on a list read to its end, and Jev checks each claim against its quotes and the page's other current quotes,
  and that the answer states each answer check and information requirement.
- **[safety.rs](crates/fastbrowse-core/src/safety.rs)** owns secrets and irreversible actions. **[effects.rs](crates/fastbrowse-core/src/effects.rs)** says what an action actually did,
  which is how a no-op is told from progress. **[telemetry.rs](crates/fastbrowse-core/src/telemetry.rs)** is the ledger: steps, calls, dollars.
- **[cli.rs](crates/fastbrowse-cli/src/cli.rs)**, **[mcp](crates/fastbrowse-cli/src/mcp)**, **[serve.rs](crates/fastbrowse-cli/src/serve.rs)** and **`run_task`** are the four entry points; [options.rs](crates/fastbrowse-cli/src/options.rs) holds the rules
  they share, so a flag means the same thing in all of them. `serve` is `fastbrowse serve --stdio`: it calls
  `run_task` for another process over JSON-RPC, one run at a time, and adds no agent logic. Its messages are the
  types in [protocol.rs](crates/fastbrowse-core/src/protocol.rs), written as JSON Schema to [protocol/schema.json](protocol/schema.json); [output_schema.rs](crates/fastbrowse-core/src/output_schema.rs) turns a caller's JSON Schema into an `output_schema`.
- **[go](go)**, **[java](java)** and **[dotnet](dotnet)** are the Go, Java (`dev.agentlabs:fastbrowse`) and .NET
  (`Fastbrowse`) clients. Each starts `serve` as the SDK does, with types generated from `protocol/schema.json`
  (`go generate`, [java_types.py](scripts/java_types.py), [dotnet_types.py](scripts/dotnet_types.py)). The release pins
  the Go module's binary archives by SHA-256 ([go_binaries.py](scripts/go_binaries.py)), ships Java's in
  `natives-<platform>` jars, and packs .NET's under `runtimes/<rid>/native`.
- **[packages/sdk](packages/sdk)** is the TypeScript SDK, `fastbrowse` on npm. It starts `serve` and knows the agent by
  the protocol alone; [its types](packages/sdk/src/protocol.ts) are generated from `protocol/schema.json` (`npm run generate:sdk`).
  What it starts is the Rust `fastbrowse` binary ([crates/fastbrowse-cli](crates/fastbrowse-cli)), with the Node addon beside it
  for `inProcess`, shipped as one `@fastbrowse/<os>-<arch>` npm package per platform.

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
  before a retry, and recovery asks Jev again without it; rewriting a field's current value cannot count as
  progress even if the page changes.

## Prompts and models

A prompt is code, and a model's output is untrusted until code accepts it.

- **One call does one job.** A request that states a task states everything the next step needs from it. The
  plan call writes its own answer checks instead of leaving a terse draft for a second call to repair. Split a
  job across calls only when the two cannot be asked in the same breath, and give each half its own contract.
- **Code decides, the model proposes.** A check, a count, a citation or a stop decision is settled by code
  against the schema, the quotes or the page state. Trust a model's output only where code can prove it
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

**The docs move with the code, and never fall behind it.** This is a standing rule for this repository and
for every public surface its prose reaches, not a task to schedule later. A user-visible change updates what
describes it in the same PR: `README.md`, `docs/`, `skills/fastbrowse/`, `packages/sdk/README.md`,
`.env.example` and the changelog entry. A doc that trails the code is a bug fixed on its own, not a follow-up
note on the next change. fastbrowse.ai builds its docs, skill and `llms` pages from these files and the PyPI
page renders `README.md`, so a stale sentence here is stale in both places; `fastbrowse-hosted` and the
marketing site hold to the same rule for the copy they own.

`uv run python scripts/docs_sync.py` is the mechanical half: it fails when a documented flag, setting, status,
exit code or relative link is not in the code, or when the version in `pyproject.toml` has no `CHANGELOG.md`
entry. CI runs it on every pull request. Write and review prose with the
[copywriting skill](https://github.com/cjber/skills/blob/main/copywriting/SKILL.md) and its
[docs and interface guidance](https://github.com/cjber/skills/blob/main/copywriting/references/docs-and-ui.md):
check each claim against the code, put the useful point first, and keep prerequisites, limits and recovery
steps. The checks above catch punctuation and wording; the skill covers whether the sentence is true.

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
   and the Rust workspace the same number. Open a `chore: <x.y.z>` PR. CI fails if the version has no entry or
   npm or Cargo disagrees with it.
3. `git tag v<x.y.z> <sha> && git push origin v<x.y.z>`. The tag's workflow runs the fixture suites on that
   commit itself, alongside the build, and publishes nothing until they pass.
4. Once the tag's workflow has published, `uv run python scripts/aur_package.py --write` moves the Arch package
   to the release. Open a `build: AUR <x.y.z>` PR with it, then push the same two files to the AUR:
   [docs/agents/aur.md](docs/agents/aur.md) has the commands and how the package is built and checked. The
   weekly AUR workflow fails while `packaging/aur` is older than the PyPI release, so a missed bump shows there.

Every release needs the fixture suites green on its exact commit: `.github/workflows/release.yml` calls
`.github/workflows/evals.yml` on the tag and holds the publish jobs behind that job, and the eval workflow fails
on any failed attempt or incomplete repeat coverage. `scripts/release_guard.py` is told the run happened
(`--evals-ran-here`); run by hand without that flag, it looks for a dispatched Evals run on the same commit
instead. A release that publishes a head-to-head figure also needs the comparison re-run on that same build.

One tag publishes one version to PyPI and npm (`.github/workflows/release.yml`). It first builds the wheel and the
five binaries and smoke-tests each; pull requests and `main` build only the Linux and Windows three, leaving the macOS two
to the release. The macOS two, and their Node addons, are signed ad hoc
(`codesign --sign -` in `.github/workflows/binaries.yml`), which is enough for a binary installed through npm. Then, each step
needing the one before: the GitHub release with **the changelog entry as its notes**, PyPI, the five platform
packages, the SDK. PyPI and npm take the job's OIDC identity, so no token is stored. No registry takes a version
back, so finish a release that failed partway by re-running its failed jobs. Started by hand (`gh workflow run
release.yml --ref <branch>`), the workflow stops after the binaries and publishes nothing: do that after changing
the build.

After PyPI the workflow asks fastbrowse.ai to rebuild, since its changelog page reads this file at build
time. `scripts/changelog.py` is what reads the entry, so the repository, the release and the site never tell
three stories about one version. `.github/workflows/site.yml` asks for the same rebuild whenever
`CHANGELOG.md`, `README.md`, `docs/` or `skills/` change on `main`, so the site never waits for a release to
catch up. It needs `SITE_DEPLOY_HOOK` (a Vercel deploy hook for `agent-labs-dev/fastbrowse-site`) in this
repository's secrets; without it the workflows still succeed and warn that the site was not rebuilt.

Pull requests also get an advisory Jev review (`.github/workflows/jev-review.yml`, `scripts/jev_review.py`):
Jev judges whether changed prose is true to the diff and whether the changelog misses a user-visible change.
Its doubts are warnings for the reviewer; it never blocks a merge. It reads `AI_GATEWAY_API_KEY` from the
repository's secrets, so it runs on `pull_request_target` from the base branch's code and reads the pull
request only as a git ref: a change to the script takes effect once it is on `main`.

PR titles are conventional commits (`feat:`, `fix:`, `perf:`, `docs:`, `build:`, `ci:`, `chore:`) and become
the squash-merge subject. `main` requires the `check` status and resolved review threads.
