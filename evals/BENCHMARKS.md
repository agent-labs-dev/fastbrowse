# Browser benchmarks

The commands on this page run from `evals/` (`fastbrowse_evals`, a [Parallax](https://pypi.org/project/parallax-evals/)
plugin). Run `uv sync` there first.

Browser benchmarking compares exactly three things, each a matched head-to-head on the internal live-task suite
(`fastbrowse-internal`, the maintainer's suite, not an official Browser Use benchmark):

| | comparison | arms | publication group | matched tasks |
|:--|:--|:--|:--|:--|
| C1 | agent against agent | `fastbrowse` and `jev-ultrafast` live, `browser-use`, `browser-use-ultrafast` and `browser-harness` from the competitor reference | `internal-hosted` | 53 |
| C2 | backend against backend | `fastbrowse` on the Browser Use Cloud browser against `fastbrowse-pool` (Fastbrowse on its own hosted browser pool) | `internal-backend` | 53 |
| C3 | Fastbrowse and its backend against Jev Ultrafast | `fastbrowse-pool` against `jev-ultrafast` | `internal-ultrafast` | 6 |

Each comparison is its own invocation with exactly its two arms, over every suite, three repeats; the publication
candidate refuses a ledger whose recorded arms differ from its group's. Run one command per comparison:

```sh
SUITES="core dev stretch-dev"
uv run fastbrowse-evals browser internal --arms fastbrowse browser-use \
    --suite $SUITES --repeat 3 --budget 100 --out artifacts/evals/c1-agent.jsonl
uv run fastbrowse-evals browser internal --arms fastbrowse fastbrowse-pool \
    --suite $SUITES --repeat 3 --budget 100 --out artifacts/evals/c2-backend.jsonl
uv run fastbrowse-evals browser internal --arms fastbrowse-pool jev-ultrafast \
    --suite $SUITES --repeat 3 --budget 100 --out artifacts/evals/c3-ultrafast.jsonl
```

`--budget` is the approved total for that campaign. Without `--arms`, `fastbrowse-evals browser internal` runs `fastbrowse`,
`jev-ultrafast`, `browser-use` and `browser-use-ultrafast`; `fastbrowse-pool`, `fastbrowse-hosted`, `browser-use-oss`, `cua-codex` and `browser-harness` run only when named.
Each comparison is scored on the whole task set for every arm. A task an arm cannot run, as the catalog's `excluded`
says, counts as a failed attempt for that arm at every repeat, so a capability one agent lacks counts against it.
Time and cost are averaged over the attempts that ran, since an attempt that never ran has neither. Among the tasks
the arms ran, an attempt is compared only where every arm measured the same repeat, at one task version, outside an
outage.

Jev routes through OpenRouter by default, or TypeSafe directly when selected. The eval harness does not route through the Vercel AI Gateway or forward its key as a backup.

Credentials per arm (environment variable names; values never enter Git, rows or projections):

| arm | needs |
|:--|:--|
| `fastbrowse` | `BROWSER_USE_API_KEY` (cloud browser), `OPENROUTER_API_KEY` (its LLM and Jev) |
| `fastbrowse-pool` | `PARALLAX_POOL_URL`, `PARALLAX_POOL_TOKEN`, `OPENROUTER_API_KEY` |
| `browser-use` | `BROWSER_USE_API_KEY` |
| `browser-use-ultrafast` | `BROWSER_USE_API_KEY` |
| `jev-ultrafast` | `BROWSER_USE_API_KEY`, `OPENROUTER_API_KEY` |
| `browser-harness` | `BROWSER_USE_API_KEY` (cloud browser), `OPENROUTER_API_KEY` (Codex's model provider); the pinned Codex CLI on PATH |
| `fastbrowse-hosted` | `FASTBROWSE_HOSTED_API_KEY`; `FASTBROWSE_HOSTED_URL` overrides the public API |
| every run (the judge) | `PARALLAX_JUDGE_MODELS=anthropic/claude-opus-5.5`, `PARALLAX_JUDGE_BASE_URL=https://openrouter.ai/api/v1`, `PARALLAX_JUDGE_API_KEY`; `OPENROUTER_API_KEY` also with `--judge-policy browser-internal-jev-v4` |

Publish each comparison's ledger with `fastbrowse-evals browser publish-candidate`: `--hosted` (C1), `--backend` (C2) and
`--ultrafast` (C3); see [Publication candidate](#publication-candidate).

```sh
uv run fastbrowse-evals browser internal --help
uv run fastbrowse-evals browser official --help
```

Run internal agent iteration against `dev`. Task and grader changes require an explicit version update in the task lock
(`src/fastbrowse_evals/versions.json`). Export `fastbrowse-evals browser catalog PATH` to `src/fastbrowse/evals/benchmark-catalog.json`; `--check` verifies
the file byte for byte, and publication refuses a measured build that ships a different catalog. Shared status normalization is part of each task
fingerprint too: adding a distinct ending requires a matching lock and catalog update before running either
arm of a comparison. Historical rows keep their original task versions.

Each catalog task lists the head-to-head arms it leaves out under `excluded`, each with its reason
(`fastbrowse_evals.live.excluded_because`). Every arm runs the same tasks wherever its product allows:

- `browser-use` and `browser-use-ultrafast` are Browser Use's hosted agent on its V4 API: `browser-use` on V4's
  default model, `browser-use-ultrafast` on `bu-ultrafast`. Every other setting is the same: a US proxy, the task's
  output schema, its login as secret bindings typed only on the start host, `maxCostUsd` 1000, recording, and the
  cloud arms' screen size. Each row records the model the run reports. Both run every task but `saucedemo-pause`.
  The run's browser is kept alive for 900 seconds after the agent finishes, and the harness reads the final page
  through that browser's DevTools address, so they are graded on the six navigation tasks too. The harness then stops
  the browser and reads the session's cost once the stop has settled it. `saucedemo-pause` passes only on a
  confirmation stop, and V4 takes no authorization or pause setting.
- `jev-ultrafast` runs the six navigation tasks, which are graded on the page a run ends on. It returns no answer,
  only DONE or BLOCKED, so it skips every task that asks for one. It also takes no authorization setting and cannot
  be given credentials. It sends the Jev release the measured fastbrowse build sends (`fastbrowse._native.jev_models`),
  and its text helper is `google/gemini-3.1-flash-lite` through OpenRouter (`ULTRAFAST_TEXT_MODEL`).

A comparison scores both arms on the tasks both ran.

Source grades are imported evidence. They are not Parallax's semantic quality verdicts. Internal browser grading
uses policy `browser-internal-opus-v5` by default: every verdict comes from `anthropic/claude-opus-5.5` through
OpenRouter, with no Jev, and the run refuses to start unless `PARALLAX_JUDGE_MODELS` is exactly that model and
`PARALLAX_JUDGE_BASE_URL` is OpenRouter. Publication accepts only this policy. The judge reads, in one form for every
arm, each page the run saw, each action with its typed values, and the citations with their quotes, in the ending
vocabulary every arm shares; a run with no record at all is UNKNOWN without a judge. A state past the judge's
documented context window is shared across requests and the verdicts combined, so nothing is cut. Earlier `--judge-policy` values keep judging as they did.
`--judge-policy
browser-internal-jev-v4` grades Jev first with the LLM fallback, for unpublished runs. External predicate
and local-command outputs are retained as source evidence and interpreted by `browser-external-jev-v4`.
Mock fixture comparisons use `browser-mock-jev-v2` with the same semantic transport. Safety tasks, which pass by
pausing or refusing, go to the pinned fallback without Jev. The hosted Browser Use arm keeps its session's
messages as `step_log` and `trace`, the shape Fastbrowse's evidence has: each action and its arguments, the URLs
visited, the page text and values it read, and its answer, without ids, screenshot links or secret values. A large
observation repeated verbatim is stored once and referenced (`observation_same_as_step`); its screenshots reach the
fallback judge as images. A Fastbrowse run a site's
bot check stopped before it answered is UNKNOWN ("blocked by site bot check") without asking a judge.
A run whose judge finds its evidence unavailable (`evidence_available` false: an infrastructure interruption, missing
observer data or unavailable state) measured no agent, so on every arm it is an outage: waited out on the bounded
outage schedule and run again, every physical attempt kept in the ledger with its trace, spend and judge receipt.
One still unavailable past the bound is recorded as an outage, never published as an agent result. An agent that
gave up on a working site has its evidence available and stays a judged failure. A first page that never loaded,
before the agent took any step, is an outage when the harness finds the site down, whatever fastbrowse named the
network error: the cloud browser's proxy reports a dead site as ERR_TUNNEL_CONNECTION_FAILED, the pool's browser as
ERR_NAME_NOT_RESOLVED. A navigation that fails after the agent acted remains the agent's error.
Retain source grade, agent completion and infrastructure failures separately.
Save complete judge receipts and policy hashes. Agent spend and judge spend remain separate;
missing judge prices remain unknown, and bounded live campaigns include judge spend in settlement.
Never turn a weighted score into a binary pass rate or hide ungraded started runs.

Every paid run needs an approved total budget, current pinned code and datasets, recorded judge and provider
configuration, and a durable ledger across resumes. Official encrypted tasks and raw browser evidence stay
in private ignored output. Credentials never enter Git or public projections.

## Status and score

A row's outcome is its score, `passed`, which the judges set. Two other fields describe how the run ended and never
decide a result:

- `status` is the arm's own word, kept as it reported it: fastbrowse says `complete`, `needs_confirmation`, `stuck`
  and so on; the hosted Browser Use agent says `completed` for every run that finished, answered or not, and
  `failed` for one that broke;
  jev-ultrafast and Browser Use OSS say `done`. The same word means different things on different arms.
- `normalized_status` maps every arm's status onto one vocabulary (`done`, `stopped`, `budget`, `timeout`,
  `blocked`, `unavailable`, `error`; `fastbrowse.evals.status.normalize`). A hosted run that answered is `done`.
  It decides only what is not an agent result: `unavailable` rows are outages, left out of scores and latency.

So a `done` row can fail (the answer was wrong) and a `stopped` row can pass (a safety task's correct pause). Compare
arms on `passed`, and read `normalized_status` to see how a run ended, never `status`.

## No limits on any arm

Benchmarks never limit an agent. No arm (fastbrowse, fastbrowse-pool, jev-ultrafast, browser-use, browser-use-ultrafast,
browser-use-oss, cua-codex, browser-harness, or the official-suite agent) has a time, step, dollar, model-call or decision limit,
budgeted or not, in the internal and official runners alike. An attempt runs until its agent ends it or a provider
does. A cap ends hard tasks early and then scores the cap, not the agent. Each run and manifest records its limits
(`max_seconds` and `task_timeout` among them, all empty), and publication refuses any recorded limit.

## One clock

`seconds` runs from the browser session being ready to the agent's final answer, for every arm and both runners
(`attempt_clock`). Opening and closing the session are outside it and are recorded as `session_open_seconds` and
`session_close_seconds`, summed per public row. Every row records its `timing_basis`: `session-ready-to-answer`, or
`session-included` where an arm could not separate them. Publication refuses anything but the first, and comparisons
refuse mixed bases. Hosted Browser Use and Browser Harness connect to their browser where the harness cannot see, so
that connection stays in their seconds; each of their public rows says so in `timing_note`, and publication refuses a
slot whose tries disagree on it.

The harness bounds only the provider's two phases around the agent, so a hung provider is never an agent failure.
Opening is bounded by `attempt_clock.SESSION_OPEN_SECONDS` (recorded as `session_open_limit_seconds`, and checked by
publication), and reading the final page and closing by `SESSION_CLOSE_SECONDS`. Running past either is an outage:
unavailable and retried. The agent's phase between them has no bound.

Readiness is set where the harness can see it: once the cloud browser's DevTools address answers and the tab is
prepared for harness-owned browsers, when the pool lease is held, before the first navigation for cua-codex, and when
Browser Use lists the session's browser for the hosted agent, whose seconds are then read from the server's own
browser start and answer times. Two cases stay `session-included`: a local Chrome left to fastbrowse (headed or with a
profile, in the official runner or the corpus), and a hosted session that lists no browser, which happens when its
agent answers with its fetch tool alone and no browser ever starts.

The approved `--budget` only decides whether a new physical run may start (`start-gate-v1`). Every retry is a
physical run, so a slot waiting out an interruption holds nothing and frees its worker. Before each run starts,
the ledger holds `--reserve-dollars` plus `--browser-reserve-dollars` plus `--judge-reserve-dollars`; the run starts only while committed spend plus that hold fits the approved total, otherwise it waits for
runs in flight or stops the campaign when nothing is running. The hold is accounting, never a limit given to the agent.
At settlement the actual charge replaces the hold. A charge above its hold is recorded and stops nothing. Unknown or
interrupted charges keep their full hold, including after resume. A run whose provider refused the request that starts
it, with a status a repeat cannot clear (401 for a bad key, say), created and billed nothing: its row records
`started: false` and its hold settles at zero. A resume requires the identical limit and hold.
A budget stop leaves an incomplete report and cannot earn publication.

The internal `RequestWallet` primitive in `fastbrowse_evals.spend` uses the separate
`paid-call-reservations-v1` journal format. It persists exact decimal reservations before returning
authorization, retains interrupted or unknown charges, and refuses further admission after a journal
failure or a charge above its quoted bound. It does not establish provider prices or route requests.
Its journals cannot resume `start-gate-v1` campaigns or qualify for publication.

The separate `request_quotes` and `spend_proxy` modules provide offline-tested HTTP admission,
used by the opt-in request session below. An operator supplies a frozen profile with the exact HTTPS endpoint,
model, credential label, documentation URLs, source digest, check time, documented input and output
limits, decimal rates and request fees. Rates must cover every provider the endpoint may select.
The reservation uses the full documented input context, an explicit conservative input-pass
multiplier for each question in a System One batch, and the requested output limit or the provider's
documented maximum. The quote records the resulting input-pass count. These amounts
are conservative estimates, not guarantees about provider charges. Requests are never truncated
or given an application output cap.

The first supported shapes are nonstreaming text-only System One Noul/Choice and chat completions,
with structured JSON state and question text for System One and optional JSON-schema output for chat.
Chat profiles can enable `chat_images` with an explicit source-verified `image_usd_per_image` rate,
including zero where documented. They accept multipart text and inline PNG/JPEG data URLs only.
Reservations include the documented input context, output allowance and each image charge.
The original body is forwarded without System One headers; remote URLs and unknown parts are refused.
System One profiles can explicitly enable `images`, binding the image header into the quote.
They accept inline PNG/JPEG base64 assets and Box questions without criteria. Box requires an image.
[TypeSafe bills image tokens at the text rate and includes them in context](https://docs.typesafe.ai/images#image-tokens),
so the same full-context reserve covers them. The gate checks encoding, signatures and documented
file/body limits; TypeSafe validates decoding, dimensions and expanded references. It never resizes or drops images.
Provider routing, reasoning and usage options must equal the profile's `request_options_json` exactly;
its routing identity must be the same in a matched baseline and candidate. Remote image assets, tools,
paid plugins, unpinned routing overrides, model service suffixes and other unpriced fields are refused.
The transport replaces inherited credentials and headers with its owned key and priced headers,
reserves durably before each physical send, and never retries or follows a redirect upstream.
Every caller retry needs a new reservation.
Only a valid numeric `usage.cost` settles a charge; a missing or malformed receipt, interruption
or transport failure keeps the full hold. A charge above its estimate is retained and blocks further
admission. Gateway receipt formats, streaming and billed browser lifecycle accounting remain separate work. An admission stop is a `SpendStop`, never an agent quality verdict.

The separate `RequestBroker` serves these priced routes to Rust clients through fastbrowse's
existing loopback HTTP proxy. Its unguessable URL prefix stays outside recorded cassette paths.
Each exact path/model pair selects an owned credential and profile, sharing one request wallet.
Unknown routes or models never dispatch and return HTTP 422. Budget admission refusals return
HTTP 402. These responses remain broker stops, outside the provider cassette and agent quality
verdict. On exit the broker stops admission,
closes its listener and drains owned provider calls and recording before the caller can close the wallet.
A post-send overrun or redirect retains the provider body, status and receipt, stops further
admission, and appears in `provider_stops` separately from pre-send refusals. Redirect locations
are never forwarded.
The internal runner can opt into a shared `RequestSession` with `--request-profiles FILE`,
`--arms fastbrowse`, `--local-browser PATH` and an approved `--budget`. Other arms and cloud browsers
are refused in this mode. The file contains `decision`, `text` and `judge` price profiles.
The text profile selects the one model used for every LLM purpose. The decision profile must name
OpenRouter System One; the text and judge profiles must name OpenRouter chat completions. The judge
remains the pinned Opus model with the existing questions and policy. Every credential label is
resolved from the runner's environment. Client settings use placeholders; the priced transport adds
the owned key to provider requests. Recorded provenance contains credential labels without key values.

Before tasks start, the runner checks each key's remaining limit and provider budget limits against
the campaign total. Every key also requires a readable account-credit response. OpenRouter's
account-credit endpoint requires suitable permissions; a refused or unavailable check stops the run.
The check is a snapshot, not a reservation against other users of that provider account.
The same wallet covers agent requests and every physical judge retry. Unused per-run reservation
flags have null provenance in this mode. A judge admission refusal preserves the completed agent
answer and evidence, leaves the missing judgment unknown, preserves known partial judge charges and records `execution_admission_stopped`
with `execution_admission_reason`. These fields describe execution, not semantic failure.

The run records `paid-call-reservations-v1`, the complete role-keyed profile bundle and its
`request_profile_sha256`. `request_runtime` records the installed fastbrowse version and hashes its
Python and native package files. A missing source commit remains unknown. After owned requests drain, separate `.requests.evidence.json` evidence
hashes the wallet, actual agent and judge cassettes, and the credit receipt. Broker capability URLs
are replaced by canonical provider endpoints in result rows. These results remain private diagnostics:
the publication validator still requires `start-gate-v1`, and a matched comparison under the new
protocol requires its own publication validation. Historical results keep their original policy.

Every run is judged, so internal runs also hold `--judge-reserve-dollars` (default $1,
recorded in the run identity) beside the agent and browser holds, and settle at the agent's charge plus every priced
judge attempt; an unpriced or interrupted judge charge keeps the full hold. A grading interruption preserves the
completed agent answer, trace and known agent charges, with quality and unfinished judge charges unavailable.

Where a provider would otherwise impose a limit, the arm overrides it explicitly. Browser Use applies a default
cost limit of 25 dollars to a hosted run unless `maxCostUsd` is set, so every hosted run is sent `maxCostUsd=1000`
(`HOSTED_UNCAPPED_USD`), far above any run and any approved campaign. Fastbrowse asks for a 15-minute cloud browser
by default, so every Browser Use Cloud browser is requested for 240 minutes. jev-ultrafast ships a 60-action demo budget (120 model calls), so its runner raises that to the same
1,000,000 ceiling the open-source Browser Use arm uses. What no request can lift is recorded,
not hidden: Browser Use enforces a 4-hour maximum lifetime on hosted sessions and cloud browsers.
The removed flags `--campaign-budget-only`, `--max-dollars`, `--max-steps`, `--task-timeout`, `--max-llm-calls`,
`--max-jev-calls` and `--agent-budget` are refused by name.

Use `--local-browser /path/to/chrome` to run the `fastbrowse` arm on headless local Chrome. It sets that arm's
browser alone: in the same invocation `browser-use`, `browser-use-ultrafast`, `jev-ultrafast` and `browser-harness` keep their own hosted
browsers. It explicitly ignores configured cloud keys, creates a temporary profile, and records the browser version
and executable hash. `--browser-reserve-dollars 0` is accepted only when every arm runs on a local or pool browser.
Every row records each arm's browser under `run.browsers`.
Separate official output directories can run concurrently under separately approved campaign grants;
the caller must keep their combined grants and retained historical charges within its total approval.

## Fastbrowse on the browser pool

`fastbrowse-pool` runs Fastbrowse exactly as the `fastbrowse` arm does, with the same tasks, prompt, schema,
secrets, limits, evidence and recording. Only the browser differs: it is leased from fastbrowse-hosted's browser pool
instead of Browser Use Cloud, so the pair is a matched A/B of the browser backend. Both arms run their tab in front: a Browser Use Cloud browser does so by default, and the pool's is handed to fastbrowse with `foreground`, since a handed-over browser otherwise runs its tab behind another. Set `PARALLAX_POOL_URL` and
`PARALLAX_POOL_TOKEN`. The token is sent only to the pool and is never logged or recorded; provenance keeps the
pool hostname alone. Each attempt leases with `POST /leases`, resolves `/json/version` to its WebSocket address and
releases with `DELETE /leases/{id}` on every exit. A background task extends the lease by 15 minutes every
5 minutes, so the lease lifetime never becomes a time cap. Addresses the pool returns on its internal origin are
rebuilt on `PARALLAX_POOL_URL`'s scheme and host. A full pool (HTTP 503) is waited out with logged backoff.
Getting the browser (its lease and WebSocket address) is opening its session, so it stays out of `seconds` as a cloud
browser's start does, recorded as `session_open_seconds` and `lease_seconds`. Waiting out a full pool is outside both,
kept as `pool_full_seconds`: it measures the pool's size, not the backend. Navigation tasks get the same emulated viewport on both backends;
Browser Use Cloud's `allowResizing` switch applies only to its own browser. Dollars exclude pool infrastructure.

This pair is C2; its command is at the top of this page.

For judges with a higher context price, declare `--judge-context-threshold-tokens`,
`--judge-context-input-usd-per-mtok` and `--judge-context-output-usd-per-mtok` together.
Input above that threshold prices the whole request at the higher input and output rates. Reservations
use the conservative input bound; settled costs use recorded usage. These prices do not increase the
approved judge budget. Changing them invalidates resume, while absent flags preserve legacy pricing.
For o-series Mind2Web judges, the adapter preserves upstream messages and scoring while using the recorded
reasoning effort and completion ceiling. Empty or truncated responses remain ungraded, never task failures.

Detailed runs and validation are tracked in the private `fastbrowse-evals` Langfuse project. Tracking is
separate from publication. Public figures and sanitized evidence remain withheld until full runs are
reviewed, failures resolved and the maintainer explicitly approves publication. The public benchmark page
belongs to fastbrowse-site; it reads approved evidence rather than Markdown result tables.

Use `--concurrency` to run official tasks under one reservation ledger. Each output directory has one
exclusive process owner. `--task-id ID` (repeatable) runs only the named tasks, each checked against the whole loaded
official set; the manifest records the subset, so such a run is never published. `--sample N` is the smoke run: N tasks
spaced evenly across the suite's fixed order, with no randomness and no choice by name. The manifest records `sample`,
and `official-rows` and publication refuse the run. Runtime manifests record the actual browser mode and provider configuration.

Internal paid comparisons use `--budget`, `--reserve-dollars` and `--browser-reserve-dollars`. Both holds
are accounting only, not a limit on any agent or on provider traffic; reconcile browser receipts and monitor the
approved total across campaigns. Unknown charges keep their full reservation. Ultrafast spend is its
provider-metered cost; `--text-*` prices are recorded when given and are not required.

## Publication candidate

`fastbrowse-evals browser publish-candidate` writes the sanitized candidate that Fastbrowse's publication gate and the
public page read. Supply any complete groups: the three head-to-head ledgers, `--hosted` (C1, `internal-hosted`),
`--backend` (C2, `internal-backend`) and `--ultrafast` (C3, `internal-ultrafast`); the Fastbrowse-only internal
ledger with `--internal-fastbrowse`; or `--official-rows` with the public rows of each official run. Also supply the
exported catalog and the passing fixture run id. The exporter checks the original evidence.

Each internal option takes the ledgers of one or more runs of one campaign, in any order. A late fix reruns only the
tasks it affects, so the group takes each arm's rows at a task from the newest run, by its recorded start, that
measured that arm at every repeat of it. This is how the release gate scores a campaign, so the site shows the
results the release shipped on. The head-to-head also reads the `--internal-fastbrowse` ledgers, so its fastbrowse
rows come from a newer fastbrowse-only run where there is one, as the release gate pools them, while each competitor
keeps the run it was measured in. Every run supplying an arm must have driven it on the same browser and at the
catalog's pin for that arm, and every run is held to the one uncapped protocol and clock. A run's other arms may be on
older pins, so re-measuring one competitor on a new version leaves the earlier run's other arms publishable. A run that supplies nothing is not read further, and every run that does
is checked whole: its protocol, budget journal, judge receipts and Langfuse readback. Every task of each arm must come
from some run. The group records its runs, newest first, in `sources`, one entry per run and arm: the run's recorded
id, the arm, agent and runner commits, date and the tasks it supplied for that arm. The group's own `agent_sha` is its newest run's, and the candidate's is the newest
run's of all its groups. The fixture run must be on that build and an official group must be of it. An earlier
run stays valid whichever build it measured.

`--competitor-reference PATH` adds the `browser-use`, `browser-use-ultrafast` and `browser-harness` rows of this
repository's `docs/results/competitor-reference.json` to `internal-hosted`, unless its ledgers measured all three. Without a reference, or with
one under another judge policy or stale for any task, the exporter refuses the comparison and names the tasks.
`--write-reference PATH --measured-on DATE` writes that file from ledgers that measured all three.

An official run directory holds decrypted BU Bench goals and upstream records, so it never leaves its job in plaintext.
`fastbrowse-evals browser official-rows GROUP FOLDER --dataset --upstream-root --judge-python --out FILE` runs there, after
`track-official --push`: it holds the run to every rule below, reads the run back from Langfuse and writes only task
ids, scores, seconds, dollars and protocol facts (`publication.OfficialRows`). `publish-candidate` adds each file as its
suite's group; only `fastbrowse` is published, so the site shows Browser Use's own published figures for these suites.

`fastbrowse-evals browser track-ledger RESULTS --push` sends a campaign ledger to Langfuse for every arm it holds, unfinished
ones included. `publish-candidate` requires each group's experiments to be there.

The hosted group is the 52 tasks both `fastbrowse` and `browser-use` run. It measures whichever of `fastbrowse`,
`browser-use`, `browser-use-ultrafast`, `jev-ultrafast` and `browser-harness` its runs ran, each on the tasks of the 52 it supports
(`jev-ultrafast` skips tasks that need an answer, `browser-harness` those that need credentials or a safe stop); an arm that did not run, or none of whose runs started (its
provider refused every start, as with a bad key), is not measured rather than blocking the group. Each task of the
set a measured arm cannot run is listed in the group's `excluded`, with the arm, the task and the catalog's reason; it
stands for `repeats` failed attempts in every pass rate. The backend group requires the 53 tasks on
both fastbrowse arms, and the Ultrafast group the 6 tasks `fastbrowse-pool` and `jev-ultrafast` both run,
each with three repeats. The Fastbrowse-only group requires all 53 supported tasks; it needs no competitor reruns.
A candidate with the Ultrafast group is schema 5, the first in which that group pairs `fastbrowse-pool` (not
`fastbrowse`) with `jev-ultrafast`; with the backend group and no Ultrafast group it is schema 4.

Every group must have been run under the uncapped protocol: `start-gate-v1`, a positive budget, no recorded
step, time or dollar limit, every row's agent limits recorded with each field empty, and the hosted arm's
`max_cost_usd` override recorded. Any recorded cap refuses export. The budget journal (internal ledgers'
`.budget.jsonl`, official runs' `budget.jsonl`) is replayed to check that each run started only when the gate
allowed it, and physical spend over all attempts must not exceed the budget.
The public limits state that no arm was capped, the hosted cost override, the 4-hour provider session maximum,
the browser of each arm and the concurrency. The campaign workflow accepts a positive `concurrency` for every
group; leaving it empty keeps the harness default of 8. When a candidate keeps rows from earlier runs, pass
their recorded concurrency. Publication checks each physical attempt against its invocation and states the
concurrency of every contributing invocation: mixed settings publish as "Concurrency 4 or 8".

- Internal slots total seconds and dollars over every physical run, retries included. An attempt with unknown
  cost or missing timing blocks export.
- An internal slot whose agent ended `blocked` reached the site and was refused by its bot check. Another agent may
  get past it, so it is a failed row, with `failure` "blocked by the site's bot check" when the judge gave no verdict.
- An internal slot with no verdict on the current task for a reason of our own is left out of the rows and every
  score, counts as neither a pass nor a failure, and is listed in the group's `unjudged` slots with its reason:
  `evidence-unavailable` when the run ended in an `error` of the harness or its model provider and the judge found its
  evidence unavailable, `no-verdict` when the judge reached none, and `task-changed` when the run measured an earlier
  version of the task, which the release gate does not compare either. Evidence the judge found unavailable on a run
  with any other ending is a failure, since the ledger cannot tell a site's refusal from ours. A newer run of the task
  replaces an unjudged slot.
- The catalog must equal the canonical export byte for byte. Official task ids and the dataset hash are read
  from the actual source, not from the manifest. Judge model, reasoning, versions and pricing date must be recorded.
- Every run's agent build and runner are clean and recorded in every row, and the fixture ledger is read, not
  counted.
- Source grades stay as the upstream defines them (weighted rubric or binary); completion is the agent's own ending.
- Official dollars are agent spend, as internal ones are; judge spend counts toward the budget, and any unsettled
  judge reservation blocks export.
- An official run may have any number of repeats (`--repeat`, at least one); every task and repeat must be graded or
  blocked by the site. A slot the interruption check still finds interrupted after every retry is the agent's failed
  attempt, as an internal one is, since another agent may get past what stopped it: a row scoring 0 with its time and
  spend, whose `failure` names the bot check when the run ended `blocked`. One whose run ended in an error of the
  harness or its model provider measured nothing and is listed in `unjudged`. Publication confirms each from its saved
  checks; any other ungraded slot blocks export. Each repeat is its own row, and a task's figure is the mean of its
  repeat scores.
  The upstream score of each repeat is kept as upstream defines it. Only `fastbrowse` official runs are
  published, graded by `anthropic/claude-opus-5.5` through OpenRouter.
- Every campaign must already be uploaded with `fastbrowse-evals browser track-ledger --push` or `track-official --push`. The exporter reads each
  experiment back from `fastbrowse-evals` and requires stored items equal to the projection of the exact source rows.
- Limits record the protocol, models, judge, upstream versions, timing and cost inclusion as short phrases.
  Slashes in model ids appear as colons. Detailed receipts stay private.

The pinned Fastbrowse dependency supplies the public models, catalog and exact fixture checks.

Imported source grades remain source evidence and cannot enter semantic pass-rate comparisons. Publication requires
a complete semantic receipt for every physical attempt.

## Retired tasks

A task whose site is gone for good leaves the suite and its lock entry, and is listed under `retired` in the
exported catalog (`fastbrowse_evals.catalog.RETIRED`) with its suite, date and reason. Published results that
include it stay as they were. Fastbrowse's publication gate compares a new group with an approved one that ran the
task on the remaining tasks only, and only when the catalog changed by nothing but listed retirements.

- `stretch-wizard-correction` (`stretch-dev`), retired 2026-10-17: `lab.hakdogan.com` was removed from its DNS
  zone (Google and Cloudflare resolvers answer NXDOMAIN; zone SOA serial 2026101610), so every arm ended
  unavailable and no internal group that includes it could be published. It was the stretch split's only task
  that goes back in a multi-step form to correct an entry; that skill is uncovered until a replacement is chosen.

## Not part of the head-to-head

These remain available and documented, but none of them is one of the three comparisons.

### Upstream suites

BU Bench, Online-Mind2Web and WindTunnel ([data sources](../docs/data-sources.md), [licences](THIRD_PARTY_DATA.md)) are independent source benchmarks.
`fastbrowse-evals browser official --arm` runs BU Bench and Online-Mind2Web with `fastbrowse` (the default) or the hosted
Browser Use agent (`browser-use`), on the same
tasks and the same upstream judge request: the agent's answer, its action history from its transcript, its
screenshots and the final page captured through its browser's DevTools address. Neither hosted agent returns the
files it wrote, which the BU Bench judge is told. `jev-ultrafast` is not offered: it returns no answer and records
no step frames, and both upstream judges grade the answer and the action history. Only Fastbrowse's official runs
are published; the others are measured only.

The upstream judge is `anthropic/claude-opus-5.5` (`--judge-model`) through `--judge-base-url`
(`https://openrouter.ai/api/v1` unless set), with that endpoint's key as `OPENAI_API_KEY`. Publication accepts only
that model and route.
BU Bench keeps the upstream weighted rubric score; Online-Mind2Web keeps the upstream full-task grade.
Missing task, judge or protocol identity prevents a directly matched comparison.

The official BU judge accepts one screenshot timing for the whole run, and step captures are mixed. BU Bench sends
the step captures and the final page in order with no step mapping, which the upstream judge reads as chronological
evidence, and an evidence note states the mixed timing. If the final capture is absent, the note says so and no score
is assigned by the adapter. Online-Mind2Web receives the step captures and final page. Upstream asks Online-Mind2Web's
judge for 512 tokens; a reply that length cuts off is asked again at the approved ceiling. Record these differences
with every result.

Both upstream judges call OpenAI directly; the runner passes `--judge-base-url` to them as `OPENAI_BASE_URL`, so
their requests go to that OpenAI-compatible endpoint with its model id and prices. The judge meter admits only that
host, and the run records the URL in its manifest.
Online-Mind2Web's judge code asks a chat model for 512 tokens at temperature 0. A reasoning judge (o1, o3, o4, gpt-5
and gpt-6 families, with or without a provider prefix) is instead sent the recorded reasoning effort and the approved
completion ceiling, with the upstream messages and scoring unchanged.

### The Fastbrowse-only internal group

`internal-fastbrowse` (`--internal-fastbrowse`) is Fastbrowse alone on the 53 tasks. It is already published, and
Fastbrowse's release gate refuses a candidate that drops a published group, so it stays exportable.

### The hosted product

`fastbrowse-hosted` (`--arms fastbrowse-hosted`) sends each task to the hosted product's public API, as a customer
does, and grades what the API returns. Its dollars are the customer charge, not metered provider cost, and the API
returns no copy of the final page, so the page half of a task is ungraded for it. It is a diagnostic in no publication
group (`fastbrowse_evals.hosted_api`).

### Open-source Browser Use

`browser-use-oss` (browser-use 1.0 on its defaults) is opt-in: `--arms browser-use-oss`.

### Diagnostic suites

`--suite sweep` runs three store-page reads from a maintainer's weekly addon sweep (a CurseForge project page
behind a Cloudflare check, its comments, and a Wago Addons listing; `src/fastbrowse_evals/sweep_tasks.py`). It is
outside the catalog: never versioned, published or gated on, and a campaign runs it only as a smoke:
`suites: sweep`, `smoke: true`, `groups: head-to-head`, `head_to_head_arms: fastbrowse browser-use-oss`.

### Competitor versions

Every competitor arm runs its latest release. `src/fastbrowse_evals/adapters/arm-pins.json` pins browser-use, Browser
Harness, Cua Driver, the Codex CLI and jev-ultrafast, and `pyproject.toml` pins `browser-use-sdk`.
`python -m fastbrowse_evals.arm_pins check` names any pin behind its latest release, and a campaign group runs the
same check on its own arms before its first paid call. The daily Arm pins workflow opens the bump as a pull request
on `bot/arm-pins`.

### Codex with Cua Driver

`cua-codex` is an opt-in internal comparison arm using the pinned Cua Driver and Codex CLI.
It runs `gpt-6-astra` with xhigh reasoning and the default service tier, through Cua browser tools
on an owned Linux Xvfb/Openbox desktop. Install those tools and dbus-daemon, authenticate Codex, then run:

```sh
uv run fastbrowse-evals browser internal --arms cua-codex --suite dev --concurrency 1
```

`CUA_DRIVER` and `CUA_CODEX` select executable paths; versions are checked before running.
The arm has no tool-call budget. Credential-free completion tasks use
the source graders. Login and safe-stop tasks are excluded. Every attempt retains tool calls, model
events and cleanup evidence under ignored `artifacts/evals/cua/`. Video recording is unsupported.

Rows identify the local browser environment. Timing is not a controlled comparison with cloud arms,
and billed spend remains unknown. The arm uses Cua's supported coding-agent MCP route; its standalone
`cua-agent` package is deprecated. No new comparison figure is published by adding the adapter.

### Codex with Browser Harness

`browser-harness` is a head-to-head arm for [Browser Harness](https://github.com/browser-use/browser-harness)
run the way Browser Use measured it in its own README comparison: the Codex CLI given
`browser-harness` as its CLI and `SKILL.md` (not `browser-harness-mcp`), on `openai/gpt-6-luna` at medium reasoning
with web search off, attached over CDP to a fresh Browser Use Cloud browser, the browser the `fastbrowse` arm runs on.
It runs inside the hosted head-to-head (`HEAD_TO_HEAD_ARMS`), the invocation that also runs `fastbrowse`,
`browser-use`, `browser-use-ultrafast` and `jev-ultrafast`, and its rows publish in that `internal-hosted` group with `--hosted`, on the tasks of
the 52 it supports. Run alone beside `fastbrowse`:

```sh
uv run fastbrowse-evals browser internal --arms fastbrowse browser-harness \
    --suite $SUITES --repeat 3 --budget 50 --out artifacts/evals/head-to-head.jsonl
uv run fastbrowse-evals browser publish-candidate --hosted artifacts/evals/head-to-head.jsonl ...
```

Codex reaches the model through OpenRouter's Responses API with `OPENROUTER_API_KEY`; its default shell policy keeps
every `*KEY*`, `*SECRET*` and `*TOKEN*` variable out of the agent's commands. browser-harness runs in its own uv
environment (`HARNESS_COMMAND`), and each attempt owns its `CODEX_HOME`, `BH_HOME`, runtime directory and daemon,
which is shut down before the cloud browser stops. `BROWSER_HARNESS_CODEX` selects the Codex executable; both
versions are checked before the agent starts. The task set is `cua-codex`'s: completion tasks without credentials.
Dollars are the cloud browser's metered charge plus Codex's recorded token usage at OpenRouter's list prices
(`HARNESS_PRICES`), since Codex reports tokens, not charges. Commands, their output and Codex's event stream are kept
under ignored `artifacts/evals/browser-harness/`. Video recording is unsupported. Its publication
browser is `browser-use-cloud` (`BROWSER_MODES`).


LLM call counts on new Fastbrowse rows count physical provider requests, including retries, with
`llm_calls_basis` set to `physical_requests`. An absent receipt count leaves `llm_calls` unknown.
Historical rows without a basis retain their cost-line count. Retry totals and site summaries never
combine counts from different bases or present a partial count as a measured total.
