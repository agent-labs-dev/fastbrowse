# Contributing

Thanks for looking at fastbrowse. Bug reports, eval failures and pull requests are all welcome. This page
covers what a change needs to be merged; [AGENTS.md](AGENTS.md) has the architecture, invariants and house
style in full.

fastbrowse is in beta. Interfaces change without deprecation, and a change that makes the agent simpler or
more accurate beats one that keeps an old behaviour working.

## Before you start

- **Bugs:** open an issue with the task, the start page, the status the run ended with and, if you can, the
  step trace (`--json` prints it). A run that ends `stuck` or answers wrongly is a bug report worth having.
- **Features and larger changes:** open an issue first, so we can agree on the approach before you spend
  time on it.
- **Security issues:** report them privately, as [SECURITY.md](SECURITY.md) describes, never in a public issue.

## Making a change

```sh
uv sync --all-extras
uv run pre-commit install
```

`uv run pre-commit install` rewrites `.git/hooks`, including the hooks Entire installs when you capture agent
sessions: run `entire enable` after it if you use Entire ([AGENTS.md](AGENTS.md#agent-session-capture) has the
commands).

`uv sync` builds the package's Rust core, `fastbrowse._native`, so it needs a Rust toolchain: install
[rustup](https://rustup.rs), and it reads the pinned version from `rust-toolchain.toml`.

Run the gate before you push. CI runs the same checks, and `main` requires them:

```sh
uv run ruff check . && uv run ruff format --check .
uv run ty check
uv run python scripts/changelog.py --check "$(uv version --short)"
uv run python scripts/npm_versions.py
uv run python scripts/no_slop.py
uv run python scripts/docs_sync.py
uv run vale sync && uv run vale README.md CHANGELOG.md AGENTS.md CONTRIBUTING.md docs skills src scripts tests
uv run python -m fastbrowse.evals.storage --manifest docs/results/evidence.manifest.json --check
uv run pytest -q
uv run python -m fastbrowse.evals.benchmarks --check-diff origin/main
uv run python -m fastbrowse.evals.publication --check-diff origin/main
npm ci --ignore-scripts && npm run check:browser
npm run generate:sdk && git diff --exit-code -- packages/sdk/src/protocol.ts
npm run check:sdk
uv run actionlint
uv run python .sift/agents.py check
uv run python .sift/gate.py --base origin/main
just rust
```

The JavaScript checks need Node.js 22 or newer. `npm run check:sdk` lints, type-checks and tests the SDK in
`packages/sdk`, and its tests start `fastbrowse serve` from the uv environment. Audit evidence commands and tool limits are in the
[sift project skill](.agents/skills/sift-project/SKILL.md).

The protocol's messages are the types in `crates/fastbrowse-core/src/protocol.rs` and the models those use, and
`protocol/schema.json` is their JSON Schema. After changing one, run
`FASTBROWSE_UPDATE_GOLDEN=1 cargo test -p fastbrowse-core --test protocol_schema`, then `npm run generate:sdk`, which
writes the JavaScript SDK's types in `packages/sdk/src/protocol.ts` from the schema, and `go -C go generate ./...`,
which writes the Go client's in `go/protocol.go`, and commit all three. CI fails when
either committed file differs from what the types produce, and `tests/test_protocol_schema.py` fails when the Python
models in `src/fastbrowse/protocol.py` no longer describe the same schema.

The npm packages and the crates on crates.io are published at the Python package's version, and the Rust binary
reports it too. `scripts/npm_versions.py` fails when `packages/sdk/package.json`, the `[workspace.package]` version in
`Cargo.toml` or a crate's pin in `[workspace.dependencies]` holds another number, and with `--write` sets them all from
`pyproject.toml`, along with the workspace's own entries in `Cargo.lock`. A crate that is published reaches no file
outside its own directory: `cargo package` in CI builds each one from what would be uploaded.

Add a line under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice, saying
what changed and why. The docs move with the code in the same PR: update the README, `docs/`,
`skills/fastbrowse/` or `packages/sdk/README.md` wherever they describe what you changed.
`scripts/docs_sync.py` fails the gate on a documented flag, setting, status, exit code or link the code does
not have.

## Changes to how the agent behaves

Unit tests cannot tell whether the agent still browses well, so a change to prompts, choices, recovery or
verification needs eval results in the PR, in this order:

1. Run `heldout` on `main` before changing anything. That is the baseline.
2. Make and iterate on the change against `dev` only.
3. Run `heldout` again on the finished change, and never tune against it.

Run these commands in the maintainer's Parallax checkout:

```sh
uv run --extra browser parallax browser internal --arms fastbrowse --suite heldout --repeat 3
uv run --extra browser parallax browser internal --arms fastbrowse --suite dev --repeat 3
```

Paste the pass counts, time and cost from the final `dev` run and both `heldout` runs. A handful of runs on one site shows the change can
help; the suites show it does not hurt elsewhere.

The agent must work on any site, so a rule written for one site, such as matching a label only Google uses,
will not be merged. Fix the general cause in the prompt, the choice or the check that failed.

The local and mock fixture suites drive a real browser, Jev and the LLM, so they run on a schedule and on demand
in `.github/workflows/evals.yml` rather than on every pull request. Run them yourself before a change to browsing
behaviour or to the fixtures:

```sh
just evals-local --suite local mock --repeat 3
```

A published comparison figure has its own rule: it may only come from committed rows (`--publish`) at the task
versions they ran, with every physical attempt and its spend retained. See [docs/evals.md](docs/evals.md).

## Pull requests

- Keep one change per PR. The title is a conventional commit (`fix:`, `feat:`, `docs:`, `perf:`, `chore:`)
  and becomes the commit subject, since every PR is squash-merged.
- Sign every commit (`git commit -S`, or set `commit.gpgsign`) with a GPG or SSH key added to your GitHub
  account. `main` accepts only commits GitHub shows as Verified, so a PR with an unsigned commit cannot merge.
  [GitHub's guide](https://docs.github.com/en/authentication/managing-commit-signature-verification) covers
  the setup.
- CI on a first-time contributor's PR waits for a maintainer to approve it, so expect a short delay before
  checks appear.
- We aim to reply to every issue and PR within a few days: to merge, to ask for specific changes, or to close
  it with the reason.

By contributing you agree your work is released under the [MIT License](LICENSE).
