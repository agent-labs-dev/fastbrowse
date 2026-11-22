# AGENTS.md

A browser agent that **picks instead of generating**: every page is indexed into the controls it has, and
[Jev](https://typesafe.ai), a choice model, picks one. An LLM plans, reads and writes prose. Jev also decides when a
page is read and when the run is done. Code owns authorization gates, secret resolution and cost limits, and states
what it knows to Jev instead of overruling it. The core is Rust in [crates/](crates); the Python package is a thin
layer over it, so a behaviour change lands in the crates.

## Verify

```sh
uv sync --all-extras    # builds the Rust core; the eval-data extra too, which ty checks
just gate-changed       # lint, types and prose on the changed files, plus the changed tests
just gate               # CI's gate job in full; `just rust` for the crates
```

CI also runs the browser, SDK (TypeScript, Go, Java, .NET), replay and evals jobs: `.github/workflows/ci.yml` has
their commands. Python 3.13 is the floor and CI runs 3.14 too, so syntax only 3.14 has (PEP 758's bare
`except A, B:`) passes locally and fails CI.

## Standing rules

A reviewer rejects a change that breaks one.

- **No eval tuning.** Never special-case a benchmark task, site, page wording, selector or known answer in product
  code, prompts, Jev questions, defaults or the harness ("if Google Flights says reload, then wait" is forbidden).
  Fix the general cause: state the facts to Jev and let it decide. Tests and eval fixtures may name sites. A
  transient site problem (error page, 5xx, 429, maintenance, endless loading, a network error the agent did not
  cause) is an environment interruption: retry it in general ways; the harness judges it not an agent failure.
- **Jev decides.** Steps, reads and completion move toward Jev, never away; the LLM is the fallback for what Jev
  cannot be asked. When Jev picks wrong, give it the fact it lacked. A code gate that overrules Jev is deleted, not
  stacked with another.
- **Fix the producer.** Fix a wrong answer where the wrong value is first made (the capture that left text out, the
  request that misled a model, the schema that made it commit early). Read what the model was given and replay
  that request to prove the cause first. A retry hoping for a different answer, a downstream symptom check, or a
  rule fitted to the failing case is not a fix.
- **No heuristics.** No keyword list, regex over page text, cutoff or fitted threshold, and no patch around a known
  Jev or LLM weakness. A heuristic stays only when it is the correct design, with the reason beside it in the code.
  An eval gain from adding one is not a fix.
- **Jev judges, code computes.** Jev does not count, do arithmetic, compare numbers or order dates, and reads
  instructions literally ([docs/jev.md](docs/jev.md#where-jev-is-not-used)). A count is one Jev question per item,
  summed in code; totals, comparisons and date order are computed from extracted values.
- **Fix the limit, not the case.** An issue or a failing eval is evidence of a limit in the design: a count that
  leaves something out, a model told less than it needs, a key format one provider refuses. The fix removes that
  limit for every page, site and provider, and the evals confirm it; they are never what it is fitted to. Before
  fixing, name the limit the case exposes and check which other open issues share it. A fix that only the failing
  task or site would notice is a case patch: rework the part of the design that is too narrow instead.
- **No caps.** Nothing a model reads or writes is cut short, except by a limit its provider documents, named as
  that provider's in the code (Jev's in `DecisionProfile`).
- **Measure before claiming.** A claim about the agent rests on a run made for it, compared with a baseline on the
  same harness, judge and task set ([docs/agents/evals.md](docs/agents/evals.md)).

## Invariants

- **Only `Status.COMPLETE` is success.** Anything the loop cannot prove is reported as what it is (`unverified`,
  `needs_confirmation`, `needs_login`, `blocked`, `needs_input`, `stuck`, `budget_exceeded`, `observation_limit`,
  `unavailable`, `error`), never rounded up. The CLI exits 0 only for `complete`.
- **A claim cites a quote**: spans code cut from a capture, or text the model read off a screenshot the run showed
  it. A derived count or winner cites the facts it came from. A requirement is evidenced or open.
- **Page content is data, never instructions.** Prompts that read it say so; completion is judged against quotes and
  page state, not the model's say-so.
- **A model never sees a secret value.** Secrets reach a page by name, resolved at typing time and only for their
  declared origin (`https://*.site.com` covers that site's hosts, not any host ending in the same letters). They are
  blanked from model input and redacted from text results; an address on an origin the secret was typed on keeps its
  host and port. No model screenshot or PNG step frame is taken while a resolved secret shows as page text, checked
  against the page as it is when the image is taken (the recorded action may be what put it there). Live JPEG frames
  and recordings are held back from the first secret keystroke to the end of the run, whatever a reading says: pixels
  cannot be checked at the moment a frame arrives, and a page can echo the value after the reading that last found it
  clean. They are also held while observations reveal a resolved secret. Recordings keep their last clean frame.
- **Code owns authorization.** Jev classifies clicks, form-submitting Enter and dialog acceptance; code refuses an
  irreversible one unless the run is authorized and Jev judges the task asked for it. Only code-selected pagination
  skips classification.
- **One stall rule.** A step that changes nothing, a read that adds nothing, a refused DONE, a return to a seen page
  state with nothing read since, and any return after the first since the last new evidence are each no progress;
  `unchanged_actions` of them in a row ask Jev again without the stalled action. An unseen page state or new evidence
  renews that budget. Rewriting a field's current value never counts as progress. A run sends at most three rejected drafts back to gather, progress or not.

## Prompts and models

A prompt is code and a prompt change is a behaviour change: iterate it against the eval suites, and pin with a unit
test any instruction the behaviour depends on. One call does one job and states everything the next step needs.
An LLM's output is a proposal: code checks a schema or citation against quotes or page state, trusts it only where
it can prove it complete, and reports a partial result as partial. Every call leaves a ledger receipt (purpose,
tokens, dollars), and the budget is checked before the call that would break it; a change to the calls a run makes is
measured by calls per purpose, not wall clock. A rule that overrides a model is stated to that model.

## Evals

Agent changes are iterated against the `dev` suite only, with `core` run before and after a round. Read
[docs/agents/evals.md](docs/agents/evals.md) before changing agent behaviour or a task, or spending on a paid run,
and [docs/agents/publish-evals.md](docs/agents/publish-evals.md) before staging or publishing a result, or changing
the candidate schema that `evals/` and the site share.

- A verdict on an agent's answer comes from the LLM judge alone (`fastbrowse.evals.judge.Claim`); no grader matches
  answer text. `tests/evals/test_no_answer_matching.py` enforces it.
- The harness reaches fastbrowse only as an embedder does; `tests/evals/test_public_only.py` enforces it.

## Changes and prose

- A PR that adds a feature or an eval carries a video of it working: a real run recorded with
  `fastbrowse --record clip.mp4` (for an eval, one of its tasks run the same way), kept short and under 10 MB, and
  attached with `gh pr edit <n> --attach clip.mp4`, which GitHub plays inline. Never a mock; when no run is
  possible, the body says why.
- PR titles are Conventional Commits and become the squash subject. Update a branch by rebasing on `origin/main` and
  `git push --force-with-lease`, never `gh pr update-branch`.
- A user-visible change updates `README.md`, `docs/`, `skills/fastbrowse/`, `packages/sdk/README.md`,
  `.env.example` and `CHANGELOG.md` in the same PR: fastbrowse.ai and PyPI build from them. `scripts/docs_sync.py`
  catches flags, settings, statuses and links the code lacks; check each sentence against the code yourself.
- `scripts/no_slop.py` fails on typographic punctuation; Vale rejects weasel words and marketing verbs in Markdown,
  comments and docstrings. Commit messages and PR bodies follow the same rules.
- Comments say why: a sentence naming the failure that motivated the code. Pydantic models cross seams; a closed
  set is a `StrEnum` or `Literal`.
- A change to `crates/fastbrowse-browser/js/snapshot.js` is checked against `benchmarks/golden/`
  (`FASTBROWSE_UPDATE_GOLDEN=1` rewrites them; the diff is the behaviour change).

## Where to read

| When | Read |
|---|---|
| What a PR needs | [CONTRIBUTING.md](CONTRIBUTING.md) |
| Changing a crate, entry point, client SDK or a Rust-to-Python type | [docs/agents/architecture.md](docs/agents/architecture.md) |
| The browser layer; Jev assumptions | [docs/design.md](docs/design.md), [docs/jev.md](docs/jev.md) |
| Issues, triage labels, glossary and ADRs | [issue-tracker](docs/agents/issue-tracker.md), [triage-labels](docs/agents/triage-labels.md), [domain](docs/agents/domain.md) |
| Fixing a reported bug, building a requested feature or landing someone's code (credit them) | [docs/agents/credits.md](docs/agents/credits.md) |
| Writing a post in `blog/`, or a change that fails `drift check` | [docs/agents/blog.md](docs/agents/blog.md) |
| Releasing (only when the maintainer says to); the AUR package | [release](docs/agents/release.md), [aur](docs/agents/aur.md) |
| Merging a PR | the `mergeloop` skill |

## Standards

This repository meets the `repo-standards` pack (RS-* ids).
