# Evals

Unit tests cannot tell you whether the agent still browses well. The suites can.

```sh
uv run python -m fastbrowse.evals.runner                      # local fixtures, headless Chrome, ~$0.005 a task
uv run --extra browser-use python -m fastbrowse.evals.live    # live head-to-head, three arms
uv run --extra browser-use python -m fastbrowse.evals.live --arms fastbrowse --suite heldout --repeat 3
```

`--suite` picks the set: `core` (the published suite), `dev`, `heldout`, and the harder `stretch-dev` and
`stretch-heldout`. **Agent changes are iterated against `dev` only.** `heldout` is run before and after a
round of changes and never debugged, so its score says whether a round improved the agent or only its dev
score. A change made to fix a named held-out task spends that set's value; say so in the PR when it happens.

The fastbrowse arm uses `OPENROUTER_API_KEY` for the LLM when set, otherwise `AI_GATEWAY_API_KEY`.
Jev uses direct TypeSafe when keyed, otherwise OpenRouter or the gateway. The other live arms require
OpenRouter, and the live suite also needs `BROWSER_USE_API_KEY`. Upstream outages look
exactly like regressions, so re-read a red run before believing it.

The scheduled fixture job forwards `OPENROUTER_API_KEY`, `TYPESAFE_API_KEY` and `AI_GATEWAY_API_KEY`, then checks
them with `--check-providers` before it runs a task. A gateway key alone covers both Jev and the LLM.
Missing credentials fail that check without producing a task score.

The OpenRouter latency benchmark in `fastbrowse.evals.latency` still requires `OPENROUTER_API_KEY`.

Every result row records its build (`run`: version, commit, dirty tree, models) and the `task_version` it ran;
compare rows only at equal task versions. Changing what a task asks or how it grades means
`python -m fastbrowse.evals.versions --bump TASK_ID --docs`, and a test fails until you do. Results are
published as committed rows (`--publish`). The README headline, the results and task tables and the suite
versions in [docs/evals.md](../evals.md) are generated from the code and those rows, and a test fails when
they differ; regenerate them in the same PR ([versions](../evals.md#versions)).

A published comparison has one route: rows committed with `--publish` at the task versions they ran, every
physical attempt kept, and retries and their spend recorded with them. Never type a number into prose.
`.github/workflows/evals.yml` runs the fixture suites on a schedule and on demand; a release that publishes a
comparison needs that job green and, for a head-to-head figure, the comparison re-run on the same build.

`fastbrowse.evals.publication` is the gate that route runs, and it refuses before it advises. Deterministic
checks refuse a row that is not from a clean committed build at the release, a current task version and a
current suite version, that has no attempt ledger reconciling every physical attempt and retry, that claims a
number where cost is unknown, that repeats one `(arm, task, repeat)` key, or whose dataset pin does not match.
An interrupted live run retains each in-flight attempt as an unselected row before closing its ledger. Its
cost stays unknown unless the arm already returned its report; queued slots and truth-only preflight do not
claim a paid attempt. Interrupted comparisons cannot supply a published figure.
The regression check compares fastbrowse against fastbrowse only, per suite, task and task version, and only
against the latest published release that ran them at the same protocol and model route, so a comparison never
crosses arms, task versions, protocols or routes silently. Every live task needs three distinct measured
repeats before any baseline: fewer is a diagnostic and blocks even a new task, a bumped task version or a
re-routed task with no baseline to compare against. A per-task pass rate that declines, and a median wall time
or cost that rises by more than 20% and more than 1 second or $0.001, block. The newest matching release with at
least three attempts is the baseline, so a thin newest release does not hide an older full one; a task with no
matching baseline, a matching baseline under three attempts, and a baseline at another protocol or route are
reported and never block once the candidate has its three repeats. `versions --publish` enforces the gate with
the ledger required; the scheduled job runs the same module over its fixture rows:

```sh
uv run python -m fastbrowse.evals.publication --rows artifacts/evals/nightly.jsonl --baseline docs/results
```

`.github/workflows/ci.yml` runs the gate over the `docs/results` files a pull request adds or changes, against
the rows at the base branch, so a published row cannot be hand-edited past `--publish`; a new results file with
no attempt ledger fails there. The release workflow runs `scripts/release_guard.py` before the GitHub release
and PyPI, so every release needs a successful Evals run on its exact commit on `main`. A release that publishes
a comparison needs more: its measured build must be an ancestor that runs byte-identical executable code, the
same `src/`, tests, lockfiles, browser code and workflows, though a README, docs or changelog commit may follow
the measurement. Any code or config change may not. The Evals workflow itself is the protocol the green run
attests: a dispatch under three repeats fails before it spends and the fixture target is fixed at 100%, so a
red fixture gate refuses a release even when it publishes no comparison.


## Results browser and tracking

Detailed evals and validation live in the dedicated [fastbrowse-evals Langfuse project](https://us.cloud.langfuse.com/project/cmuwjxra401iyad0cymgswes5).
Keep local output under ignored `artifacts/evals/`. Git stores the campaign manifest, source hash receipts
and compact published score baselines. It does not store run logs, recordings or generated evidence bundles.
Eval traces remain private. The public results page withholds campaigns and comparison figures until all
remaining issues are resolved, complete matched runs are reviewed, and the maintainer explicitly approves publication.
Project credentials stay in ignored configuration and server-side build settings.

`fastbrowse.evals.evidence` projects task identifiers, source hashes, grades, completion, build identifiers,
time and cost. Answers, page content, credentials, local paths and error text stay out. Failed and incomplete
runs remain visible; their presence never approves a headline comparison. Unknown schedules, costs and historical
start times remain unknown. All physical runs and retries remain in the projection, including interrupted runs.

Upload and verify a projection, using Langfuse's optional SDK in the maintainer environment:

```sh
uv run --with 'langfuse>=4.17,<5' python -m fastbrowse.evals.storage \
  --push artifacts/evals/evidence.json --manifest docs/results/evidence.manifest.json \
  --out artifacts/evals/readback.json
```

The command requires `LANGFUSE_PUBLIC_KEY`, `LANGFUSE_SECRET_KEY` and the US Langfuse host. It verifies the
project id and name before writing. Campaign traces remain private and contain only the validated projection.
`--archive-root` preserves original source files as private, compressed chunks; the reader reconstructs their
bytes and verifies each original hash. Campaign storage times describe the upload, never an invented run time.
An identical campaign reuses its verified stored observation. Changed projections get different content identities.
Keep the local originals until the upload and read-back checks pass.

CI validates the compact manifest without credentials. Compact publication baselines preserve the original
scores, timing, cost and protocol used by the regression gate. A baseline migrated from a published file must
match that file's hash and score projection; an existing compact baseline cannot be edited or removed.
The publication and release gates still control headline figures. The latest diagnostic is not a published score.

Parallax's private tracking work remains the reference for dataset experiments and comparison dashboards.
Its code is not a Fastbrowse dependency. Eval uploads use `fastbrowse.evals.storage`, and the public reader
uses the same pinned campaign observations.

## Matched external corpus

`fastbrowse.evals.corpus_compare` runs the same live arm adaptors on one pinned, seeded draw. Every arm sees
one URL map, task text and independent grader. Reset the isolated capsule before every physical attempt,
including retries, with a reviewed program pinned by SHA-256. Hosted browsers need public endpoints; the
local preflight checks reachability but does not prove that a remote browser can load their assets.

```sh
uv run --all-extras python -m fastbrowse.evals.corpus_compare windtunnel \
  --site-urls local-sites.json --public-site-urls public-sites.json \
  --cache artifacts/corpus-cache --seed 75 --per-stratum 3 \
  --arms fastbrowse browser-use --repeat 3 --out artifacts/corpus-preflight
```

Execution adds `--execute --authorize`, `--reset-command 'reset.py {site} {run_id}'`, `--reset-code reset.py`,
`--reset-sha256 HASH`, and a pinned grader (`--grader-code`, `--grader-sha256`, then `--grader-command` last).
Obtain an approved total API and cloud-browser budget before execution. The default live adaptors keep their
normal models and step limits; record capsule boot, endpoint setup and preflight separately from agent time.

WindTunnel excludes its declared API-only task from browser comparisons. Its declared fixture logins become
named, origin-scoped secrets; unexpected login instructions fail before spending. Upstream task references
and digests stay intact. Native state probes determine correctness independently of each arm's completion
claim. A reset or grader error remains ungraded, and every physical attempt, retry and unknown cost stays in
the ledger. Report only task-repeat pairs that all named arms can grade, with at least three repeats per task;
never choose the best repeat or compare totals with different task sets. Keep the external report separate
from the canonical live-suite feed and disclose endpoint adapters and excluded tasks with it.

A stopped draw cannot supply a comparison headline. Truncated runs, interrupted runs and runs without a task
that has three paired repeats exit non-zero. One- and two-repeat runs retain diagnostic rows but cannot pass
the comparison gate.

### Endpoint adapter and strict state supplements

`fastbrowse.evals.endpoint_adapter` forwards one capsule to a public origin for hosted browsers:
`python -m fastbrowse.evals.endpoint_adapter SITE PORT --upstream-map up.json --public-map public.json`. It
validates both URL maps, forwards every method, and rewrites only the capsule's exact origin (plain and
JSON-escaped, with its `localhost` spelling) in text bodies, `Location` and similar headers. A cookie keeps every
attribute, `Secure` and `SameSite` included. Only a `Domain` naming the upstream host (any case, port stripped)
becomes the configured public host, and an unrelated domain stays. The public map is required, is re-read per
request, and fails closed: an unreadable map, or one without a valid origin for the site, is a 502 before anything
is forwarded, and the command refuses to start on it. The cookie domain never comes from a request header. Report
the adapter with the run, as an endpoint limit.

`fastbrowse.evals.native_state` is a declarative contract for judging an attempt from observed site state, never
from its answer text. A supplement file lists ordered witnesses (`before`, `during`, `after`), each naming the
observers that read it and the checks over the reading: `equals`, `minimum`, `maximum` (finite numbers), or
`records` (a count of records containing a partial record). A supplement is pinned to the dataset revision and
digest, the site, the SHA-256 of the task text and the SHA-256 of the upstream predicate, and is reported under its
own `strict-supplement` label beside the whole, untouched upstream grade; a pass needs both. `before` and `during`
are read only by a trusted observer the harness runs beside the attempt, delivered under the top-level request key
`native_observations` (never inside `outcome`, which the agent produced) as periodic samples
`{sequence, at, phase, data}` with the observer's code digest. `before` is the last sample before the attempt and
`during` the first sample whose checks hold, so a state the sampler missed is ungraded, never a pass. `after` is
read by the grader at grade time. A required witness that is absent leaves the attempt ungraded, and a contradicted
one fails it. A supplement that lists `unwitnessed` requirements never passes: it is reported limited and ungraded.

`scripts/native_grader.py` applies it behind the `external_grade` bridge, wrapping the upstream grader command
(`--upstream`, with required `--upstream-code` and `--upstream-code-sha256`) and accepting samples only from the
observer whose digest is `--observer-sha256`. Probe observers use an `--observe-arg` command template; HTTP
observers use `--base-urls` and `--auth-file`. The one supplement shipped,
`src/fastbrowse/evals/native-state-supplements.json`, strengthens the upstream booking-then-cancel task, whose
upstream check (two customers) is also met by an uncancelled or doubled booking. The capsule does not store the
cancellation reason and no action audit exists, so that task is limited and ungraded even when the sequence holds.
`fastbrowse.evals.corpus_compare` runs the observer as a watcher around each physical attempt. Options, which must
precede `--grader-command` because that option takes the rest of the line: `--observer-command` (a JSON argument
vector, never a shell string), `--observer-code` and `--observer-sha256` (the pinned file, which the command must
run), `--observer-interval` (default 0.5 seconds, 0.1 to 60) and `--observer-timeout` (default 10, at most 120).
They are validated before any arm is prepared, need `--grader-command`, and record only the program, code,
digest and bounds in the run protocol; the argument vector is redacted from the recorded argv. After the reset
the observer is invoked with `{"task": TaskRef, "phase": "before"}`, then with `"during"` every interval while the
arm runs, and prints a mapping of observer name to reading. Its code digest is rechecked before every invocation,
its environment is the grader's allowlist, and a timeout or cancellation kills its process group. The sampler is
stopped and joined before grading. The samples reach the grader command only as the top-level
`native_observations` key, through a context variable set for that one grade; nothing on the outcome can set it.
An observer that fails, times out or prints anything but a JSON object records an error and no sample, so a
required `before` or `during` witness leaves the attempt ungraded. Samples and errors are kept in
`observations.json` beside `arm-report.json`, capped at 600 samples per attempt. Use
`scripts/native_grader.py --sample-phase request` as the observer: it reads the matching runner witness with the
same probe and HTTP readers, never calls the upstream grader, and prints `{}` for a task with no supplement. A
trusted observer may read a credential from an `--auth-file` path the caller installed; no key is in the
environment or the recorded run.
