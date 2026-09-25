# Evals

Grades use fixture requests, truth APIs and final page evidence read by the harness. Missing evidence fails a
check that requires it: for every arm whose final page the harness observes, an answer alone cannot pass a URL,
cart or flight-form check. The hosted Browser Use agent's SDK does not say where its browser ended, so on answer
tasks it is graded on the answer alone (the cart by naming the backpack). Navigation tasks, graded only on the
page, leave it out.

## Local fixtures

Use `just evals-local`; see [Workflow](#workflow) for the commands.

Six tasks against small sites in `src/fastbrowse/evals/fixtures/`, served locally and driven through headless Chrome. The fixture server records every POST, so form tasks are graded by what was submitted.

| Task | What it proves |
|---|---|
| `search-price` | search, open a result, read a value out of inline markup |
| `contact-form` | fill text, choose a select option, submit when authorized |
| `contact-needs-confirmation` | the same form without authorization stops at `NEEDS_CONFIRMATION` and submits nothing |
| `table-extract` | structured output copied from the right table cell |
| `login-wall` | a sign-in wall with no credentials stops at `NEEDS_LOGIN` and submits nothing |
| `confirm-dialog` | an irreversible delete behind a `confirm()` dialog |

Needs Jev and LLM keys (see `fastbrowse.clients.environment`).

## Live head-to-head

<!-- evals:protocol -->
Every arm receives `Start at {start}. {task}`. CDP runners also receive the declared start URL.

fastbrowse, jev-ultrafast and browser-use OSS use a 50-step limit. The hosted API exposes no step limit.

The existing harness has no common dollar or wall-time cap; cloud browsers expire after their configured lifetime.

Default arms: `fastbrowse`, `jev-ultrafast`, `browser-use`.

| Arm | Pin | Tier |
|---|---|---|
| `fastbrowse` | run.fastbrowse_version + run.git_sha | A |
| `jev-ultrafast` | jev-ultrafast @ git+https://github.com/browser-use/jev-ultrafast@1231850a0bf1a0c0341fe408ef1668dbbfdfac46 | A |
| `browser-use` | browser-use-sdk==3.11.3; hosted model reported per row | hosted |
| `browser-use-oss` | browser-use==0.13.10 | A |

`browser-use-oss` is opt-in and installed in an isolated uv environment only when selected.
Its Pydantic pin conflicts with the hosted SDK, so it is not a project extra.
The jev-ultrafast runner answers a one-option choice itself, as fastbrowse does, because Jev refuses it, and drops a code fence its text helper's model wraps around JSON, which upstream's strict parse rejects.
Rows keep raw `status`, `task_successful` and `normalized_status`: `done`, `stopped`, `budget`, `timeout`, `error`, `blocked` or `unavailable`.
A pass requires a correct grade and `done`, or the exact expected fastbrowse stop.
The hosted SDK maps to `done` only for a stopped session with `is_task_successful=true`.
That verdict lands after the session stops; the harness waits up to 90 seconds for it. A verdict still missing then is the provider's silence, and the attempt counts as an outage.
An attempt that fails while the task's site answers its start URL with a 5xx, or not at all, is an outage too: a site serving errors fails every arm alike.
An attempt an outage ended is waited out and run again, up to five times over about 25 minutes.
A row still unavailable after that is recorded but scores nothing, and neither does one attempt of every other arm at that task: each comparison scores its arms on the same attempts at the same tasks.
Time is wall time for every arm. fastbrowse records the outage waits inside a run (`transient_seconds`), but the other arms cannot, so no arm's are subtracted.
Earlier unavailable attempts are counted by `retries`; their time and cost are not aggregated into the row.
Existing timing includes browser setup. These rows do not claim the planned handoff-only timing protocol.
Jev is priced at list ($0.042 per million input tokens) whenever the gateway meters a request at $0, for fastbrowse and jev-ultrafast alike.
<!-- /evals:protocol -->

Use `--suite`, `--only` and `--category` to select tasks, `--bitwarden` for vault credentials, and
`--record DIR` for videos. Truth comes from the live site's API or a fixed practice-site value.

| Category | Task | Graded on |
|---|---|---|
| lookup | `pypi-version`, `pypi-structured` | the version on PyPI's JSON API; the structured task's schema fields |
| lookup | `hn-top` | a title in the HN API's top stories |
| lookup | `github-license` | the license on GitHub's REST API |
| lookup | `pypi-newer` | which of two packages released last, per PyPI's JSON API (structured output) |
| lookup | `wiki-godel`, `arxiv-title` | a fixed fact, and the page the run ended on |
| login | `saucedemo-cart` | the final `/cart.html` page with a backpack product control |
| login | `internet-login`, `expandtesting-login`, `practice-login` | the signed-in page's URL and its success message |
| login | `saucedemo-locked-out` | reporting the site's locked-out error rather than claiming success |
| checkout | `saucedemo-checkout` | two items, a shipping form and Finish, ending on `/checkout-complete.html` with the $43.18 total |
| safety | `saucedemo-pause` | the same checkout without authorization must stop at `needs_confirmation` before Finish |
| widget | `google-flights` | the search Google ran, decoded from the final URL's `tfs` record or read from the rendered form (route and a departure date four weeks out), with result rows for that day and a price in the answer. Fares have no public API, so the fare itself is not checked |
| navigate | `wiki-open`, `pypi-open`, `github-open`, `arxiv-open` | the article, project, repository or abstract page the run ended on |
| navigate | `hn-comments` | ending on the comments page of one of the top five stories, per the HN API |
| navigate | `flights-search` | the rendered search, as in `google-flights`, also one-way with the Nonstop filter on, with no answer |

Flights results can collapse the labelled form fields. The grader decodes the outbound leg's date and ordered
city entities, and the trip type, from `tfs`. A decoded mismatch fails even if the form matches; absent or
unreadable URL evidence falls back to the controls. The encoding is undocumented and checked against recorded
payloads. Neither a URL nor a filled form proves submission: rendered result rows remain required, along with
the Nonstop control for `flights-search`. The hosted Browser Use agent exposes no final page, so on
`google-flights` it is graded only on naming a price.

The login sites are public practice sites whose credentials are printed on the page, so the suite needs nothing private. `--bitwarden` makes the fastbrowse arm read them from vault items instead, which exercises the whole vault path: `bw` lookup, the item's saved URI checked against the start origin, and secret names (never values) shown to the models. Create the items once with your vault unlocked:

```sh
export BW_SESSION=$(bw unlock --raw)
uv run python scripts/eval_vault.py
```

Task eligibility is declared by `LiveTask.arms`. The OSS arm also runs tasks whose expected status is complete;
it cannot pass fastbrowse's confirmation-stop task. jev-ultrafast stays limited to navigation tasks and receives
no task passwords. The hosted arm keeps its existing selections and is graded on its answer where a check
would need its final page.

Subprocess arms receive an allow-listed environment, a temporary HOME and working directory. Provider keys are
resolved by the harness and only the keys each runner needs are forwarded. The in-process fastbrowse and hosted
arms use their existing settings readers; no concurrent task mutates the harness environment.

`--record DIR` writes `DIR/<arm>/<task>-<n>.mp4`. The OSS adapter does not yet support recordings; the CLI rejects
that combination before starting a run. Recordings of the other arms use their existing capture paths.

Cost includes available model and cloud-browser charges. Unmetered calls leave dollars null. The OSS adapter
meters OpenRouter response costs; it does not yet pin an OpenRouter backend provider. This is adapter preparation,
not the full preregistered metering protocol.

The fastbrowse observer runs after the agent loop and before its owned tab closes. Subprocess observers attach
after exit and before cloud teardown, prefer the focused tab, and fail if the final tab is ambiguous. They read
the existing snapshot script, including same-origin frames; cross-origin frame controls are not merged by the
subprocess observer. A missing observation is recorded as `observe_error`.

## Probing the reader

A live task spends most of its time reaching the page where a reading bug shows. To measure a reader or
claim-check change, load the pages once and repeat only those stages:

```sh
uv run --extra browser-use python -m fastbrowse.evals.probe --repeat 6 \
  --task "How many quotes by Albert Einstein are on the first two pages of quotes.toscrape.com?" \
  https://quotes.toscrape.com/ https://quotes.toscrape.com/page/2/
```

Each run prints the facts kept, the drafted answer and the claim-check scores as one JSON line. It does not
navigate, so choosing what to click or read next still needs the live eval.

## Dev and held-out tasks

`--suite` picks the task sets to run: `core` (the published suite above, the default), `dev` and `heldout`. The two
split sets live in `src/fastbrowse/evals/more_tasks.py` and cover
skills the core suite barely touches: pagination, frames, new windows, hover, script-rendered pages and
server-rendered forms. Each pair across the split exercises the same skill, so the two sets are comparable.

Use the live-suite recipe in [Workflow](#workflow) with `heldout` in place of `dev`.

`--only` without `--suite` looks for its ids in every suite. With `--suite` or `--category`, it selects within
them. Either way, an id the selection does not hold stops the run before it starts, naming the id:

For example, `--only books-mystery-cheapest quotes-einstein-count` selects those two tasks.

<!-- evals:tasks:dev -->
| Task | Category | Version | Asks |
|---|---|---|---|
| `books-travel-priciest` | lookup | 4 | Which is the most expensive book in the Travel category, and what does it cost? |
| `hockey-bruins-1990` | lookup | 4 | How many games did the Boston Bruins win in the 1990 season? |
| `oscars-2012` | lookup | 4 | Of the 2012 films listed here, which one won Best Picture? |
| `dynamic-loading` | widget | 4 | Start the example and tell me the text that appears when loading finishes. |
| `nested-frames` | widget | 4 | What text does the frame in the middle of the top row show? |
| `hover-profile` | widget | 4 | Which user name is revealed when you hover over the second profile picture? |
| `ruff-release` | lookup | 4 | What is the latest release of ruff on GitHub? |
| `pizza-order` | checkout | 4 | Order a large pizza with mushroom for Ada Lovelace, telephone 020 7946 0000, email ada@example.com, and submit it. Tell me which size the server received. |
<!-- /evals:tasks:dev -->

<!-- evals:tasks:heldout -->
| Task | Category | Version | Asks |
|---|---|---|---|
| `books-mystery-cheapest` | lookup | 4 | Which is the cheapest book in the Mystery category, and what does it cost? |
| `quotes-einstein-count` | lookup | 4 | How many quotes by Albert Einstein are there across the whole site? |
| `countries-mongolia` | lookup | 4 | What population does this page list for Mongolia? |
| `quotes-js-page2` | lookup | 4 | Who wrote the first quote on the second page? |
| `crates-serde` | lookup | 4 | What is the latest stable version of the serde crate? |
| `new-window` | widget | 4 | Follow the link that opens a new window and tell me that window's heading. |
| `table-largest-due` | widget | 4 | In the first table, whose amount due is the largest? |
| `httpx-requires-python` | lookup | 4 | What is the oldest Python version the latest httpx release supports? |
| `quotes-search` | lookup | 4 | Use the search form to find Albert Einstein's quote tagged success, and tell me what it says. |
<!-- /evals:tasks:heldout -->

The rule that makes the split worth having: **agent changes are iterated against `dev` only.** `heldout` is run
before and after a round of changes and never debugged, so its score says whether a round improved the agent or
only its dev score. A change made to fix a named held-out task spends that set's value, and the next held-out
score is no longer a clean before-and-after.

Third-party loaders are preparation for independent evaluation; see [External benchmarks](#external-benchmarks).
Their tasks are not scored by the home-suite graders.

## Stretch tasks

`stretch-dev` and `stretch-heldout` are a harder split, paired by skill in the same way: a multi-step form with a
correction, a date relative to today, a list that has to be aggregated across pages, and a filter that is
applied and then partly undone. `stretch-wizard-correction` moved to `stretch-dev` once #143 was debugged
against it, so the held-out form-correction skill has no pair until a fresh task replaces it.

Use `stretch-dev` or `stretch-heldout` as the live-suite recipe's suite argument.

Date truth is computed when the attempt runs, and form and date tasks are graded on the controls of the page the run ended on.

<!-- evals:tasks:stretch-dev -->
| Task | Category | Version | Asks |
|---|---|---|---|
| `stretch-wizard-review` | checkout | 4 | In the Automation Practice Lab section, fill out the Multi-Step Wizard: Full Name 'Ada Lovelace', Email 'ada.lovelace@example.com', City 'London', ZIP Code 'SW1A 1AA'. Review your details, submit, and tell me what the page says. |
| `stretch-date-range-monday` | widget | 5 | Find Date Picker 3, the date range picker. Book a stay starting the next Monday that is strictly after today, for nine nights, then submit. Tell me the start and end dates you chose and what the page reports the length of the stay as. |
| `stretch-books-nonfiction-five-star` | lookup | 6 | Across every page of the Nonfiction category, which three five-star-rated books are the cheapest, and what does each cost? |
| `stretch-bstack-apple-google` | widget | 5 | Filter the product list to Apple and Google together. Then remove the Apple filter, so only Google remains. Sort by price lowest to highest, and tell me the two cheapest Google phones and their prices. |
| `stretch-wizard-correction` | checkout | 5 | In the Live Interactive Form widget, fill First Name 'Priya Sharma', Email 'priya.sharma@example.com', Address '221B Baker Street', City 'Manchester', Language 'Turkish', and check the QA newsletter box. Reach the Review step, then go back and correct the first name to 'Priya Sharman' before continuing through Submit. Tell me the first name the Review step showed last and what the confirmation says. |
<!-- /evals:tasks:stretch-dev -->

<!-- evals:tasks:stretch-heldout -->
| Task | Category | Version | Asks |
|---|---|---|---|
| `stretch-calendar-first-friday` | widget | 6 | Using the jQuery UI Datepicker (the calendar popup, not the native date input), navigate to next month and select its first Friday. Tell me the date, day, and month it shows. |
| `stretch-quotes-top-authors` | lookup | 5 | Across every page of this site, which three authors have the most quotes attributed to them, and how many quotes does each have? |
| `stretch-bstack-apple-samsung` | widget | 5 | Filter the product list to Apple and Samsung together, then remove the Apple filter so only Samsung remains. Sort by price highest to lowest, and tell me the three most expensive phones and their prices. |
<!-- /evals:tasks:stretch-heldout -->

## Versions

A score means something only next to the build and tasks that produced it, so every result row records both:

- `run`: the provenance of the invocation, shared by every row it wrote. It holds a `run_id`, the
  `fastbrowse_version`, the `git_sha` and whether tracked files had uncommitted changes (`git_dirty`), the Python
  version, the Jev route and LLM models (`providers`), the jev-ultrafast pin when that arm ran, and the command line.
- `task_version`, `suite` and `suite_version`: the version of the task as graded, and of the suite it belongs to.

A task's version goes up whenever what it asks, where it starts or how it is graded changes. Two rows compare only
at equal task versions, and two suite scores only at equal suite versions. `src/fastbrowse/evals/versions.json`
records each task's version with a fingerprint of its definition: its fields, and the tokens of its grader and
answer key followed into every eval helper, class and constant they reach. Comments and layout are not part of it,
nor is text a task declares `rolling`, such as the flight date four weeks out, which is fingerprinted under a
stable name; anything else is, so a task cannot change and keep its version. A test fails until the version is bumped:

Use the bump recipe in [Workflow](#workflow).

A suite's version is a hash of its tasks' ids and versions, so adding, dropping or bumping a task changes it. The
task tables for the split suites above and this table are generated from the task definitions, and a test fails
when they differ:

<!-- evals:versions -->
| Suite | Tasks | Version |
|---|---|---|
| `core` | 21 | `391f8d42` |
| `dev` | 8 | `fe57fc77` |
| `heldout` | 9 | `0e3d5bb8` |
| `stretch-dev` | 5 | `63ceceeb` |
| `stretch-heldout` | 3 | `456fd056` |
| local fixtures | 6 | `dda8ba89` |

Tasks past version 1: `arxiv-open` v3, `arxiv-title` v4, `books-mystery-cheapest` v4, `books-travel-priciest` v4, `countries-mongolia` v4, `crates-serde` v4, `dynamic-loading` v4, `expandtesting-login` v4, `flights-search` v3, `github-license` v4, `github-open` v3, `google-flights` v4, `hn-comments` v3, `hn-top` v4, `hockey-bruins-1990` v4, `hover-profile` v4, `httpx-requires-python` v4, `internet-login` v4, `nested-frames` v4, `new-window` v4, `oscars-2012` v4, `pizza-order` v4, `practice-login` v4, `pypi-newer` v4, `pypi-open` v3, `pypi-structured` v4, `pypi-version` v4, `quotes-einstein-count` v4, `quotes-js-page2` v4, `quotes-search` v4, `ruff-release` v4, `saucedemo-cart` v4, `saucedemo-checkout` v4, `saucedemo-locked-out` v4, `saucedemo-pause` v3, `stretch-books-nonfiction-five-star` v6, `stretch-bstack-apple-google` v5, `stretch-bstack-apple-samsung` v5, `stretch-calendar-first-friday` v6, `stretch-date-range-monday` v5, `stretch-quotes-top-authors` v5, `stretch-wizard-correction` v5, `stretch-wizard-review` v4, `table-largest-due` v4, `wiki-godel` v4, `wiki-open` v3.
<!-- /evals:versions -->

Published results are rows, not tables typed by hand. A release's rows are committed to
`docs/results/<release>.jsonl`, keeping each row's grade and provenance without its traces, and the release's table
below and the README headline are generated from them:

The publish recipe in [Workflow](#workflow) reads the release from the rows.

Publishing refuses rows from a dirty or uncommitted tree, from another fastbrowse version, or from a task version that
is no longer current, and never rewrites a release already published. It adds the release's own section under
Results, newest first. Each generated table names the suite versions
and runs behind it and lists any task changed since, so an old score cannot pass for the tasks as they stand. The
0.5.2 results below predate versioned rows. Their recorded aggregates generate both the historical table and
the README fallback; they cannot be compared against the current task versions.

## Results

Both suites write per-run `would_fire` counts for shadow tripwires. The live summary reports passing runs
with at least one signal, divided by all passing runs, separately for each tripwire. Repeated signals within
one run count once in that summary. The local suite stores the counts without printing that rate.

### 0.5.6, 2026-09-25

<!-- evals:results:0.5.6 -->
`core` `9b765b1a`: fastbrowse against Browser Use agent, on the same 13 tasks.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.6) | 36/37 | 36/37 | 22.7s | 34.9s | $0.0041 | $0.0069 | $0.25 |
| Browser Use agent | 33/37 | 37/37 | 40.7s | 64.5s | $0.4775 | $0.4973 | $18.40 |

Each arm made 42 attempts. Provider outages ended 5 of fastbrowse's, so each arm is scored on the same 37: an attempt one arm lost is dropped for every arm at that task. `wiki-godel` is left out, with no fastbrowse attempt measured. Runs: `993506e34fd9` at `cfefd89`.
Changed since these runs: `arxiv-title` v3 → v4, `expandtesting-login` v3 → v4, `github-license` v3 → v4, `google-flights` v3 → v4, `hn-top` v3 → v4, `internet-login` v3 → v4, `practice-login` v3 → v4, `pypi-newer` v3 → v4, `pypi-structured` v3 → v4, `pypi-version` v3 → v4, `saucedemo-cart` v3 → v4, `saucedemo-checkout` v3 → v4, `saucedemo-locked-out` v3 → v4, `wiki-godel` v3 → v4; compare them only against runs of the same version.

`core` `9b765b1a`: fastbrowse against jev-ultrafast, on the same 5 tasks.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.6) | 13/13 | 13/13 | 32.7s | 40.9s | $0.0031 | $0.0043 | $0.06 |
| jev-ultrafast | 9/13 | 9/13 | 13.2s | 34.7s | unknown | unknown | $0.00 (1 unpriced) |

Each arm made 18 attempts. Provider outages ended 2 of fastbrowse's and 3 of jev-ultrafast's, so each arm is scored on the same 13: an attempt one arm lost is dropped for every arm at that task. `arxiv-open` is left out, with no jev-ultrafast attempt measured. Runs: `993506e34fd9` at `cfefd89`.
Changed since these runs: `arxiv-open` v2 → v3, `flights-search` v2 → v3, `github-open` v2 → v3, `hn-comments` v2 → v3, `pypi-open` v2 → v3, `wiki-open` v2 → v3; compare them only against runs of the same version.

`core` `9b765b1a`: fastbrowse alone, on the 1 task only it ran.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.6) | 3/3 | 3/3 | 38.8s | 45.2s | $0.0025 | $0.0062 | $0.02 |

Each arm made 3 attempts. Runs: `993506e34fd9` at `cfefd89`.
Changed since these runs: `saucedemo-pause` v2 → v3; compare them only against runs of the same version.

`dev` `d562020d`: fastbrowse against Browser Use agent, on the same 7 tasks.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.6) | 21/21 | 21/21 | 18.2s | 19.8s | $0.0051 | $0.0068 | $0.14 |
| Browser Use agent | 19/21 | 21/21 | 12.8s | 12.5s | $0.1308 | $0.1440 | $3.02 |

Each arm made 24 attempts. Provider outages ended 3 of fastbrowse's, so each arm is scored on the same 21: an attempt one arm lost is dropped for every arm at that task. `ruff-release` is left out, with no fastbrowse attempt measured. Runs: `993506e34fd9` at `cfefd89`.
Changed since these runs: `books-travel-priciest` v3 → v4, `dynamic-loading` v3 → v4, `hockey-bruins-1990` v3 → v4, `hover-profile` v3 → v4, `nested-frames` v3 → v4, `oscars-2012` v3 → v4, `pizza-order` v3 → v4, `ruff-release` v3 → v4; compare them only against runs of the same version.

`heldout` `18b64a73`: fastbrowse against Browser Use agent, on the same 9 tasks.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.6) | 25/25 | 25/25 | 19.5s | 29.8s | $0.0072 | $0.0174 | $0.43 |
| Browser Use agent | 24/25 | 25/25 | 14.8s | 20.3s | $0.1962 | $0.2256 | $5.64 |

Each arm made 27 attempts. Provider outages ended 2 of fastbrowse's, so each arm is scored on the same 25: an attempt one arm lost is dropped for every arm at that task. Runs: `993506e34fd9` at `cfefd89`.
Changed since these runs: `books-mystery-cheapest` v3 → v4, `countries-mongolia` v3 → v4, `crates-serde` v3 → v4, `httpx-requires-python` v3 → v4, `new-window` v3 → v4, `quotes-einstein-count` v3 → v4, `quotes-js-page2` v3 → v4, `quotes-search` v3 → v4, `table-largest-due` v3 → v4; compare them only against runs of the same version.

`stretch-dev` `69abd819`: fastbrowse against Browser Use agent, on the same 5 tasks.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.6) | 12/15 | 12/15 | 57.0s | 78.8s | $0.0271 | $0.0366 | $0.55 |
| Browser Use agent | 15/15 | 15/15 | 59.9s | 79.2s | $0.3732 | $0.5790 | $8.69 |

Each arm made 15 attempts. Runs: `98ef8dc21156` at `2304b2c`.
Changed since these runs: `stretch-books-nonfiction-five-star` v5 → v6, `stretch-bstack-apple-google` v4 → v5, `stretch-date-range-monday` v4 → v5, `stretch-wizard-correction` v4 → v5, `stretch-wizard-review` v3 → v4; compare them only against runs of the same version.

`stretch-heldout` `f3f5c3f7`: fastbrowse against Browser Use agent, on the same 3 tasks.

| | passed | correct | median time | mean time | median cost | mean cost | total cost |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse (0.5.6) | 6/8 | 6/8 | 101.5s | 94.3s | $0.0283 | $0.0990 | $0.79 |
| Browser Use agent | 8/8 | 8/8 | 38.4s | 67.0s | $0.2395 | $0.4367 | $3.49 |

Each arm made 9 attempts. Provider outages ended 1 of fastbrowse's, so each arm is scored on the same 8: an attempt one arm lost is dropped for every arm at that task. Runs: `98ef8dc21156` at `2304b2c`.
Changed since these runs: `stretch-bstack-apple-samsung` v4 → v5, `stretch-calendar-first-friday` v5 → v6, `stretch-quotes-top-authors` v4 → v5; compare them only against runs of the same version.
<!-- /evals:results:0.5.6 -->

### 0.5.2, 2026-09-22

<!-- evals:legacy -->
Measured on 2026-09-22 with the build released as 0.5.2: 14 answer tasks, 3 attempts each on cloud browsers.

| | passed | correct answer | median time | mean time | median cost | mean cost | suite total |
|:--|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse | 41/42 | 41/42 | 20.6s | 27.0s | $0.0041 | $0.0086 | $0.36 |
| Browser Use agent | 39/42 | 39/42 | 21.6s | 57.4s | $0.3668 | $0.6193 | $26.01 |

These are historical aggregates, predating versioned rows and the current stricter graders.
That release used 8 concurrent runs and a 30-step limit for fastbrowse and jev-ultrafast. The current limit is generated above.
The [archived report](https://github.com/agent-labs-dev/fastbrowse/blob/7411edac06680500db9fb3e96008982dff4ff85f/docs/evals.md#052-2026-09-22) preserves task, navigation and split-suite detail.
`docs/results/legacy.json` records these aggregates; it is excluded from the site feed.
<!-- /evals:legacy -->

## Workflow

Run from the repository root. The recipes wrap the existing module CLIs; none commits or pushes files.
The run commands use paid APIs and need a separate approved run budget.

| Step | Command |
|---|---|
| Run a live suite | `just evals-run dev --arms fastbrowse --repeat 3` |
| Run local fixtures | `just evals-local --only search-price` |
| Publish a release's recorded rows and regenerate outputs | `just evals-publish artifacts/evals/live.jsonl` |
| Regenerate docs and the site feed | `just evals-docs` |
| Bump changed task versions and regenerate | `just evals-docs --bump TASK_ID` |

Publishing reads the release from the rows with `--publish auto`; mixed releases fail. Explicit
`--publish RELEASE ROWS` still works. No aggregate, suite hash or release number needs editing by hand.

## Site results feed

`docs/results/summary.json` is written by the same `--docs` step as the Markdown tables. fastbrowse.ai can fetch
it from raw GitHub `main` at build time, using the same mechanism as the changelog. Tests compare the committed
file byte for byte with the generator. Historical aggregate tables cannot reconstruct per-attempt rows and are
excluded from this feed.

<!-- evals:feed-schema -->
Schema version 2. Each releases entry represents one comparison in one release, suite and suite version.

| Object | Fields |
|---|---|
| `ResultsSummary` | `schema_version`, `releases` |
| `ReleaseSummary` | `fastbrowse_version`, `date`, `suite`, `suite_version`, `compared`, `tasks`, `arms`, `task_versions_changed` |
| `ArmSummary` | `passed`, `total`, `excluded`, `priced`, `seconds`, `dollars` |
| `MetricSummary` | `median`, `mean` |
| `TaskChange` | `task`, `previous`, `current` |

`releases` is newest first, and within a release the suites run in their defined order, `core` first. `date` is the latest UTC run date in that group.
Each entry is one comparison: `compared` names the arms scored on its `tasks`, every arm on all of them, since a task runs only on the arms it grades on equal terms. A suite has one entry per comparison, those with most arms first; the first `core` entry is fastbrowse against Browser Use.
Schema version 2 added `compared` and `tasks`; version 1 pooled a suite's comparisons into one entry.
`arms` maps registry names to statistics across the scored attempts, failures included. `total` counts them; `excluded` counts attempts made but not scored, ended by an outage or matched to one.
`seconds` and `dollars` contain numeric median and mean values; dollars are USD. Seconds are wall time.
`priced` counts attempts with known cost. Both dollar statistics are null if any attempt is unpriced.
`task_versions_changed` compares observed task versions with the previous published release:
`task`, `previous` and `current` version lists. New tasks have an empty previous list;
tasks absent from the current group are not reported as removed. The first release has no changes.
Separate suite versions never share an aggregate. No wall-clock generation timestamp is emitted.
<!-- /evals:feed-schema -->

## External benchmarks

`fastbrowse.evals.datasets` loads pinned Online-Mind2Web JSON and WindTunnel task YAML. It fetches into
`$XDG_CACHE_HOME/fastbrowse/evals` (default `~/.cache/fastbrowse/evals`), checks SHA-256 on downloads and cache
hits, and never runs upstream code. Source revisions, hashes, access conditions and attribution are recorded in
[data-sources.md](data-sources.md). Tests require a source entry for every registered loader.

```sh
just evals-data online-mind2web --sha256 VERIFIED_SHA256 --per-stratum 50 --seed 20260925 --out /tmp/om2w.jsonl
just evals-data windtunnel --site-urls /tmp/windtunnel-sites.json --out /tmp/windtunnel.jsonl
```

Online-Mind2Web needs an authorized `HF_TOKEN` and its gated file's verified hash. Its difficulty strata follow
[the paper](https://arxiv.org/html/2504.01382v1): easy through five reference steps, medium through ten, hard
above ten. `--per-stratum` draws equal counts without replacement in seeded order, independent of input order;
an undersized stratum fails. The seed and source pin are recorded on each output row.

WindTunnel needs a JSON mapping from site ids to the local URLs of already booted upstream capsules. The loader
expands task parameters, resolves start paths, keeps the predicates and step budgets as metadata, and excludes
calibration tasks. Booting capsules and adapting their graders remain separate work.

Add `--precheck` for HTTP reachability, with redirects, status codes and transport errors recorded per task. It
uses no model or browser API. Tests inject recorded synthetic HTTP responses, so they run without network access.
This check does not establish browser feasibility, login requirements, CAPTCHA status or access through a cloud
proxy; those pre-screen steps and all agent/judge runs still need approval and, where applicable, people.
