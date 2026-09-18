# Evals

Grades rest on things the agent cannot write where the arm allows it: a request the fixture server recorded, truth fetched from a site's own API, the URL the browser ended on, or a quote captured verbatim from the page. Where an arm exposes none of these (hosted Browser Use has no final URL or quotes), its answer text is checked against the truth.

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
uv run --extra browser-use python -m fastbrowse.evals.live [--arms fast hosted] [--category CATEGORY ...]
    [--only TASK_ID ...] [--bitwarden] [--repeat N]
```

The same prompts run through fastbrowse on a Browser Use Cloud browser and through hosted Browser Use. Tasks are defined in `src/fastbrowse/evals/live_tasks.py`. Truth is fetched at run time from PyPI's JSON API, the Hacker News API and GitHub's REST API, so grades follow the live site; the rest are fixed by the site (an arXiv title, a practice shop's prices). Cost is metered: provider-reported Jev/LLM cost plus the browser and proxy cost returned when the cloud browser stops, and `total_cost_usd` for the hosted session.

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

The login sites are public practice sites whose credentials are printed on the page, so the suite needs nothing private. `--bitwarden` makes the fast arm read them from vault items instead, which exercises the whole vault path: `bw` lookup, the item's saved URI checked against the start origin, and secret names (never values) shown to the models. Create the items once with your vault unlocked:

```sh
export BW_SESSION=$(bw unlock --raw)
uv run python scripts/eval_vault.py
```

Both arms are graded on their answer. The fast arm is also graded on the page it ended on and on its final status; the hosted SDK exposes neither, so hosted tasks rest on the answer alone, and `saucedemo-pause` runs on the fast arm only because hosted Browser Use has no confirmation stop to grade.

Needs `BROWSER_USE_API_KEY` as well as the Jev and LLM keys. Rows are appended to `artifacts/evals/live.jsonl` with category, status, error, step trace, `correct` (the task's check) and `passed` (the check plus the expected status: `complete` for fastbrowse unless the task expects a stop, a stopped session for hosted) and `seconds_by_call` (wall time per model call, by component and purpose). The summary prints both, per arm.

**Why not a public benchmark.** Online-Mind2Web (live sites) and BU Bench are graded by an LLM judge, WebVoyager's answers have drifted with the sites, and WebArena-Verified is deterministic but needs its self-hosted sites. None covers a password manager or a confirmation stop. This suite trades breadth for grades that cannot be argued with; see [External benchmarks](#external-benchmarks) to compare on the others.

## Results

Two passes of the original six live tasks (lookups and `saucedemo-cart`) on 2026-09-18, both arms the same day. The LLM is `google/gemini-3.8-flash` at low reasoning effort, with `google/gemini-3.5-flash-lite` for PLAN, SHORTCUT and FIELD_TEXT:

| | passed | correct answer | median time | mean time | cost per task |
|---|---|---|---|---|---|
| fastbrowse on a cloud browser | 12/12 | 12/12 | 12.9s | 15.4s | $0.0072 |
| fastbrowse, previous build | 11/12 | | 27.5s | 26.7s | $0.0160 |
| hosted Browser Use | 11/12 | 11/12 | 14.7s | 25.8s | $0.3767 |

Best successful run per task, fastbrowse against hosted Browser Use: pypi-version 9.5s against 18.1s, pypi-structured 11.8s against 15.7s, hn-top 12.1s against 9.5s, github-license 10.6s against 9.3s, wiki-godel 14.5s against 13.5s, saucedemo-cart 20.0s against 90.7s. Hosted Browser Use's mean is carried by Sauce Demo (79.9s, which failed, and 90.7s); ours by one wiki-godel run whose LLM read took 14.3s.

**The cost gap is structural.** Picking from indexed candidates spends a fraction of the tokens that generating actions from screenshots does, and most of what is left is the LLM rather than Jev or the browser.

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

**Where the time goes now.** Per task on the run above: READ 3.0s, Jev 2.6s, VERIFY 1.5s, COMPOSE 1.3s, PLAN 0.8s, SHORTCUT 0.7s, and no RECOVER. A lookup is now plan, one Jev step, one read and the done check; the LLM read is the next lever.

The local fixtures, simpler sites on a local Chrome, averaged 9.8s a task on the same build.

`passed` needs a successful status as well as the check, and `correct answer` is the check alone: a run can hold the right answer yet fail to confirm it on the page, which the previous build did once on `saucedemo-cart`.

**Twelve runs is a smoke test, and noise is 4 to 5 seconds a task:** two runs of an identical build came out 36.3s and 40.7s. Read the score as "both arms finish most of these tasks" and the cost column as the real finding.

**Model choice was measured per purpose.** `evals.latency` times candidate models on the request shapes a run is made of, and `google/gemini-3.5-flash-lite` was fastest on all of them. Flash-lite for every purpose scored 8/12 live, so it is used only where its output cannot become a conclusion unchecked: FIELD_TEXT, SHORTCUT, and PLAN, whose requirements the done check and VERIFY judge against the task text. PLAN on flash-lite went 12/12 live in an A/B against the default (12/12).

Both suites depend on upstream availability: one pass scored 0/6 during a Jev `model_unavailable` outage. Re-read a red run before believing it is a regression.

## External benchmarks

We don't vendor other people's benchmarks. To compare on them:

- [BrowserGym](https://github.com/ServiceNow/BrowserGym): a gym interface over MiniWoB, WebArena, WorkArena and others
- [WebArena](https://github.com/web-arena-x/webarena) and [WebArena-Verified](https://github.com/ServiceNow/webarena-verified): self-hosted sites with functional graders
- [Mind2Web](https://github.com/OSU-NLP-Group/Mind2Web) and [Online-Mind2Web](https://github.com/OSU-NLP-Group/Online-Mind2Web): real-site tasks, offline and live
- [WebVoyager](https://github.com/MinorJerry/WebVoyager): live tasks on 15 popular sites
- [WebBench](https://github.com/Halluminate/WebBench): large live read/write task set
- [AssistantBench](https://github.com/oriyor/assistantbench): time-consuming information-seeking tasks
