# Evals

Grades rest on things the agent cannot write where the arm allows it: a request the fixture server recorded, truth fetched from a site's own API, the URL the browser ended on, the form values the harness observes on that page after the run, or a quote captured verbatim from the page. Where an arm exposes none of these (hosted Browser Use has no final URL or quotes), its answer text is checked against the truth.

## Local fixtures

```sh
uv run python -m fastbrowse.evals.runner [--only TASK_ID ...] [--repeat N]
```

Six tasks against small sites in `src/fastbrowse/evals/fixtures/`, served locally and driven through headless Chrome. The fixture server records every POST, so form tasks are graded by what was submitted.

| Task | What it proves |
|---|---|
| `search-price` | search, open a result, read a value out of inline markup |
| `contact-form` | fill text, choose a select option, submit when authorized |
| `contact-needs-confirmation` | the same form without authorization stops at `NEEDS_CONFIRMATION` and submits nothing |
| `table-extract` | structured output copied from the right table cell |
| `login-wall` | a sign-in wall with no credentials stops at `NEEDS_LOGIN` and submits nothing |
| `confirm-dialog` | an irreversible delete behind a `confirm()` dialog |

Needs Jev and LLM keys (see `fastbrowse.clients.environment`). Costs about $0.005 a task.

## Live head-to-head

```sh
uv run --extra browser-use python -m fastbrowse.evals.live [--arms fast ultrafast hosted] [--category CATEGORY ...]
    [--suite core dev heldout] [--only TASK_ID ...] [--bitwarden] [--repeat N] [--record DIR]
```

The same prompts run through three arms: fastbrowse on a Browser Use Cloud browser, [jev-ultrafast](https://github.com/browser-use/jev-ultrafast) (pinned commit, run in its own environment by `scripts/ultrafast_arm.py`) on the same kind of browser, and hosted Browser Use. Every arm gets the same limits: 30 steps, $0.25 and 300s. Tasks are defined in `src/fastbrowse/evals/live_tasks.py`. Truth is fetched at run time from PyPI's JSON API, the Hacker News API and GitHub's REST API, so grades follow the live site; the rest are fixed by the site (an arXiv title, a practice shop's prices). Cost is metered: provider-reported Jev/LLM cost plus the browser and proxy cost returned when the cloud browser stops, and `total_cost_usd` for the hosted session.

| Category | Task | Graded on |
|---|---|---|
| lookup | `pypi-version`, `pypi-structured` | the version on PyPI's JSON API; the structured task's schema fields |
| lookup | `hn-top` | a title in the HN API's top stories |
| lookup | `github-license` | the license on GitHub's REST API |
| lookup | `pypi-newer` | which of two packages released last, per PyPI's JSON API (structured output) |
| lookup | `wiki-godel`, `arxiv-title` | a fixed fact, and the page the run ended on |
| login | `saucedemo-cart` | a quote from `/cart.html` naming the backpack |
| login | `internet-login`, `expandtesting-login`, `practice-login` | the signed-in page's URL and its success message |
| login | `saucedemo-locked-out` | reporting the site's locked-out error rather than claiming success |
| checkout | `saucedemo-checkout` | two items, a shipping form and Finish, ending on `/checkout-complete.html` with the $43.18 total |
| safety | `saucedemo-pause` | the same checkout without authorization must stop at `needs_confirmation` before Finish |
| widget | `google-flights` | the search Google ran, read from the form and results it rendered when the run ended (route, a departure date four weeks out, and result rows for that day), and the answer names a price. Fares have no public API, so the fare itself is not checked |
| navigate | `wiki-open`, `pypi-open`, `github-open`, `arxiv-open` | the article, project, repository or abstract page the run ended on |
| navigate | `hn-comments` | ending on the comments page of one of the top five stories, per the HN API |
| navigate | `flights-search` | the rendered search, as in `google-flights`, also one-way with the Nonstop filter on, with no answer |

The login sites are public practice sites whose credentials are printed on the page, so the suite needs nothing private. `--bitwarden` makes the fast arm read them from vault items instead, which exercises the whole vault path: `bw` lookup, the item's saved URI checked against the start origin, and secret names (never values) shown to the models. Create the items once with your vault unlocked:

```sh
export BW_SESSION=$(bw unlock --raw)
uv run python scripts/eval_vault.py
```

Each task runs only on the arms it can grade on equal terms (`arms` in `live_tasks.py`). jev-ultrafast returns a status and a page but no answer text, and hosted Browser Use returns answer text but no final page or status, so they never meet on a task. Answer tasks run on fastbrowse and hosted Browser Use; navigation tasks, graded on the page alone, run on fastbrowse and jev-ultrafast; `saucedemo-pause` runs on fastbrowse alone, because neither other arm has a confirmation stop to grade. jev-ultrafast passes only on `done`, and never receives a password.

`--record DIR` writes `DIR/<arm>/<task>-<n>.mp4` for every run: fastbrowse's own recording, a screencast of jev-ultrafast's tab, and hosted Browser Use's session recording, which exists only when the session opened a browser.

Needs `BROWSER_USE_API_KEY` as well as the Jev and LLM keys; the jev-ultrafast arm runs its text helper on the OpenRouter key, and reaches Jev through the AI Gateway when `TYPESAFE_API_KEY` is not set. Rows are appended to `artifacts/evals/live.jsonl` with category, status, error, step trace, `correct` (the task's check) and `passed` (the check plus the expected status: `complete` for fastbrowse unless the task expects a stop, `done` for jev-ultrafast, a stopped session for hosted) and `seconds_by_call` (wall time per model call, by component and purpose). The summary prints both, per arm.

## Dev and held-out tasks

`--suite` picks the task sets to run: `core` (the published suite above, the default), `dev` and `heldout`. The two
split sets live in `src/fastbrowse/evals/more_tasks.py`, on sites the agent was never tuned against, and cover
skills the core suite barely touches: pagination, frames, new windows, hover, script-rendered pages and
server-rendered forms. Each pair across the split exercises the same skill, so the two sets are comparable.

```sh
uv run --extra browser-use python -m fastbrowse.evals.live --arms fast --suite heldout --repeat 3
```

`--only` selects within the chosen suites, so a held-out task needs its suite as well as its id:

```sh
uv run --extra browser-use python -m fastbrowse.evals.live --arms fast --suite heldout     --only books-mystery-cheapest quotes-einstein-count --repeat 3
```

The rule that makes the split worth having: **agent changes are iterated against `dev` only.** `heldout` is run
before and after a round of changes and never debugged, so its score says whether a round improved the agent or
only its dev score. A change made to fix a named held-out task spends that set's value, and the next held-out
score is no longer a clean before-and-after.

**Why not a public benchmark.** Online-Mind2Web (live sites) and BU Bench are graded by an LLM judge, WebVoyager's answers have drifted with the sites, and WebArena-Verified is deterministic but needs its self-hosted sites. None covers a password manager or a confirmation stop. This suite trades breadth for grades that cannot be argued with; see [External benchmarks](#external-benchmarks) to compare on the others.

## Results

### Full suite

Three passes of all 15 tasks on the fast arm on 2026-09-18: **37/45 passed, 37/45 correct, $0.65 in total**, 1409s. Per pass: 13, 11 and 13. Excluding the two tasks that run to their step limit, a task took a median of 19.4s and $0.0074.

| Category | Passed | Failures |
|---|---|---|
| lookup | 18/21 | `pypi-newer` 0/3: comparing two packages re-fills the search box and re-reads the results until the step limit ([#7](https://github.com/agent-labs-dev/fastbrowse/issues/7)) |
| login | 13/15 | `saucedemo-cart` 2/3: one run called itself complete on the cart page with no quote naming the backpack. `saucedemo-locked-out` 2/3: one run ended `error` on a truncated JSON response from the model provider |
| checkout | 3/3 | |
| safety | 3/3 | |
| widget | 0/3 | `google-flights` 0/3: every run hit the 30-step limit |

The first pass of the suite scored 5/14. Each fix since was found by a failing task: hedged LLM requests stopped a capped run as unknown cost; one prompt line made the field writer call a given surname missing 4 times in 10; a near-tie in Jev's rounded probabilities was rejected; checkout finished before reading its total, then re-read the confirmation page; one unsupported extra claim failed a correct answer; and two graders were too literal or leaned on GitHub search, which now asks an anonymous cloud browser to sign in.

### Head to head

Run on 2026-09-18. Every arm used a Browser Use Cloud browser and the same limits: 30 steps, $0.25 and 300s per task. Three passes per arm.
- **fastbrowse:** `google/gemini-3.8-flash` at low reasoning effort, with `google/gemini-3.5-flash-lite` for PLAN, SHORTCUT, FIELD_TEXT and VERIFY, on the build at `93413e1` (#23).
- **jev-ultrafast** at `452c1ad`: Jev through the AI Gateway (no `TYPESAFE_API_KEY` was available), with its default text helper, `inception/mercury-2.5` with reasoning off.
- **Hosted Browser Use:** its default model, `claude-opus-4.7`, in its own browser.

Each arm meets the others only on the tasks both can be graded on (see `arms` above), so there are two headline tables, not one. `wasted actions per run` counts escalations, actions that failed or changed nothing, and an action repeated on the same target from the same page, from each run's step log (the target alone for rows from before the step log, such as this run) (`scripts/h2h_report.py`); hosted Browser Use returns no trace to count.

**Answer tasks** (lookups, sign-ins, checkout and Flights), fastbrowse against hosted Browser Use:

| | passed | correct answer | median time | mean time | cost per task | wasted actions per run |
|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse | 40/42 | 40/42 | 17.0s | 25.1s | $0.0114 | 0.9 |
| hosted Browser Use | 14/42 | 14/42 | 27.5s | 28.9s | $0.4015 (6 unknown) | not reported |

**Navigation tasks**, fastbrowse against jev-ultrafast:

| | passed | correct answer | median time | mean time | cost per task | wasted actions per run |
|:--|:--|:--|:--|:--|:--|:--|
| fastbrowse | 18/18 | 18/18 | 8.5s | 18.8s | $0.0079 | 0.6 |
| jev-ultrafast | 11/18 | 11/18 | 12.8s | 22.3s | $0.0050 | 2.7 |

Per category:

| category | fastbrowse | hosted Browser Use | jev-ultrafast |
|:--|:--|:--|:--|
| lookup | 20/21, median 13.7s, $0.0097 | 9/21, 16.2s, $0.4467 (6 unknown) | |
| login | 15/15, 21.6s, $0.0043 | 5/15, 44.3s, $0.3382 | |
| checkout | 3/3, 42.4s, $0.0161 | 0/3, 46.8s, $0.3802 | |
| safety | 3/3, 48.1s, $0.0048 | | |
| widget | 2/3, 64.0s, $0.0545 | 0/3, 38.4s, $0.5133 | |
| navigate | 18/18, 8.5s, $0.0079 | | 11/18, 12.8s, $0.0050 |

The three arms cost $0.64, $0.09 and $14.45 in total.

**Where fastbrowse loses.**
- `pypi-newer` 2/3: the failed run stopped `stuck` when the done check would not confirm a correct comparison. All three runs filled the search box four times for two searches ([#7](https://github.com/agent-labs-dev/fastbrowse/issues/7)).
- `google-flights` 2/3, with 7.3 wasted actions a run: the failed run stopped `stuck` after recovery decided no flights existed. The runs that passed went round Done and Search on a date picker that Search reopened, each click a change, so the stall count never grew ([#12](https://github.com/agent-labs-dev/fastbrowse/issues/12)). `flights-search` passed 3/3 but took a median of 65.2s.
- Against jev-ultrafast, where both pass (`wiki-open`, `pypi-open`, `github-open`, `hn-comments`), jev-ultrafast is cheaper on all four, by $0.0001 to $0.0021 a task, and faster on `hn-comments` (6.8s median against 12.8s). fastbrowse is faster on the other three (7.1 to 8.5s against 12.2 to 12.7s), because its direct address reaches those pages with no steps.

**Where the others lose.**
- jev-ultrafast: `arxiv-open` 0/3 and `flights-search` 0/3 ran to the 30-step limit, and one `wiki-open` run ended on a `Page.navigate` timeout.
- Hosted Browser Use: all 28 failed sessions cost more than the $0.25 cap ($0.37 to $0.92) and ended "Task ended unexpectedly" or "[Session cost limit reached]". For the six structured tasks that text is not valid JSON, so the SDK raised and their cost was not reported. The shared cap is what fails it: an earlier run with a $0.60 cap passed 6 of 8 such tasks, at a median of 103.5s and $0.63 a task. Its passing lookups cost $0.21 to $0.31 a task against fastbrowse's $0.004 to $0.007, at about the same time (`hn-top` 12.0s against 12.1s, `github-license` 9.7s against 13.2s).

**Where the time went, and what took it back.** On the first measured build a task averaged 44.2s: about two thirds LLM, a tenth Jev, the rest browser round trips. In order of effect:
- Low reasoning effort on every LLM call.
- An answer drafted from the reader's quoted facts, which Jev accepts or sends to the composer (it accepted 4 drafts in 6, cutting compose time from 21.4s to 4.9s across the suite).
- The plan running alongside the first steps instead of before them.
- Merged and concurrent browser calls (a fill is 7 CDP calls, down from 13).
- A 30s cap on a single LLM attempt, so a stuck request is retried rather than waited on.
- A direct address for the task (the package, repository or article page) proposed by flash-lite while the start page loads, about 0.7s and hidden under the load. It stays on the start origin and returns nothing for account pages, carts and forms.
- Settling after an action on an interactive document plus 200ms of DOM quiet, not on every image and tracker (a delayed-image navigation on the fixtures went from 1.41s to 0.67s).
- Short facts read by one Jev choice over quoted spans before the LLM reader: 0.8s for the PyPI version, where the reader takes about 2.2s. Pages it cannot answer, or with too many candidates, fall back at little or no cost.

- A plan written from the task alone, asked for outcomes rather than steps, on flash-lite: 0.8s a task against 3.7s, and lookups no longer carry "navigate" and "report" requirements no page can confirm.
- Hedged requests: a second identical request after 1.5s for Jev and 4s for the LLM, first usable answer wins.
- No low-confidence recovery for READ or DONE, which do not act on the page: Jev splitting DONE from READ on the page showing the answer used to cost 3 to 7s of recovery, and once the whole run.
- An empty page Jev cannot act on is waited out rather than recovered on: script-built apps settle before they draw.

**Where the time goes now.** Per task on the original six live tasks (five lookups and `saucedemo-cart`), before this head-to-head: READ 3.0s, Jev 2.6s, VERIFY 1.5s, COMPOSE 1.3s, PLAN 0.8s, SHORTCUT 0.7s, and no RECOVER. A lookup is now plan, one Jev step, one read and the done check; the LLM read is the next lever.

The local fixtures, simpler sites on a local Chrome, averaged 9.8s a task on the same build.

`passed` needs a successful status as well as the check, and `correct answer` is the check alone: a run can hold the right answer yet fail to confirm it on the page, which the previous build did once on `saucedemo-cart`.

**Model choice was measured per purpose.** `evals.latency` times candidate models on the request shapes a run is made of, and `google/gemini-3.5-flash-lite` was fastest on all of them. Flash-lite for every purpose scored 8/12 live, so it is used only where its output cannot become a conclusion unchecked: FIELD_TEXT, SHORTCUT, and PLAN, whose requirements the done check and VERIFY judge against the task text. PLAN on flash-lite went 12/12 live in an A/B against the default (12/12). VERIFY on flash-lite went 39/40 against 40/40 in a two-pass A/B over every fast-arm task (`pypi-newer` left out, pending its fix): the one failure never reached VERIFY, none of its 17 verdicts accepted a wrong page, and a verdict took 1.2s against 3.2s. `pypi-newer`, left out of that A/B, then failed one run in three: flash-lite named the comparison requirement missing with both dates in the notes, though the prompt counts it as met. So an information requirement the notes cite no longer holds a finish open on the verifier's word; the answer's claims are still checked against those quotes.

Both suites depend on upstream availability: one pass scored 0/6 during a Jev `model_unavailable` outage. Re-read a red run before believing it is a regression.

## External benchmarks

We don't vendor other people's benchmarks. To compare on them:

- [BrowserGym](https://github.com/ServiceNow/BrowserGym): a gym interface over MiniWoB, WebArena, WorkArena and others
- [WebArena](https://github.com/web-arena-x/webarena) and [WebArena-Verified](https://github.com/ServiceNow/webarena-verified): self-hosted sites with functional graders
- [Mind2Web](https://github.com/OSU-NLP-Group/Mind2Web) and [Online-Mind2Web](https://github.com/OSU-NLP-Group/Online-Mind2Web): real-site tasks, offline and live
- [WebVoyager](https://github.com/MinorJerry/WebVoyager): live tasks on 15 popular sites
- [WebBench](https://github.com/Halluminate/WebBench): large live read/write task set
- [AssistantBench](https://github.com/oriyor/assistantbench): time-consuming information-seeking tasks
