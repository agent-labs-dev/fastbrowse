<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/wordmark-dark.svg">
  <img src="assets/wordmark.svg" alt="fastbrowse" width="360">
</picture>

**A browser agent that picks instead of generating.**

Jev chooses each action, an LLM plans and reads, and code owns verification, safety and secrets.

[![pypi](https://img.shields.io/pypi/v/fastbrowse?style=flat-square&color=6366F1)](https://pypi.org/project/fastbrowse/)
![python](https://img.shields.io/badge/python-3.13%20%7C%203.14-475569?style=flat-square)
![license](https://img.shields.io/badge/license-MIT-475569?style=flat-square)
![status](https://img.shields.io/badge/status-pre--alpha-6366F1?style=flat-square)

</div>

> **Warning**  
> This project is highly experimental and not recommended for production use yet.

---

## Why

fastbrowse indexes the page into candidates and has [Jev](https://typesafe.ai), a choice model, **pick one**, so it cannot click
something that was never on the page. Every claim in an answer cites a verbatim quote from the page.

![fastbrowse signing in to a shop, adding two products, filling the shipping form and placing the order](docs/assets/demo.gif)

13 steps in 20.9s on local Chrome, shown 1.4x faster with pauses cut.

### Against Browser Use

<!-- evals:headline -->
Measured on 2026-09-25 with the build released as 0.5.6: 46 tasks, 257 attempts across all arms, on cloud browsers.

Suite `core` `9b765b1a`: fastbrowse against Browser Use agent, on the same 14 tasks.

| | passed | cost per task | median time |
|:--|:--|:--|:--|
| fastbrowse | 36/37 | $0.0041 (median), $0.0069 mean | 20.7s |
| Browser Use agent | 37/42 | $0.4758 (median), $0.4899 mean | 35.8s |

Each arm made 42 attempts; left out as provider outages: fastbrowse 5.

Suite `core` `9b765b1a`: fastbrowse against jev-ultrafast, on the same 6 tasks.

| | passed | cost per task | median time |
|:--|:--|:--|:--|
| fastbrowse | 16/16 | $0.0019 (median), $0.0036 mean | 15.0s |
| jev-ultrafast | 11/15 | unknown (median), unknown mean | 13.0s |

Each arm made 18 attempts; left out as provider outages: fastbrowse 2, jev-ultrafast 3.

Suite `dev` `d562020d`: fastbrowse against Browser Use agent, on the same 8 tasks.

| | passed | cost per task | median time |
|:--|:--|:--|:--|
| fastbrowse | 21/21 | $0.0051 (median), $0.0068 mean | 18.2s |
| Browser Use agent | 22/24 | $0.1353 (median), $0.1884 mean | 12.9s |

Each arm made 24 attempts; left out as provider outages: fastbrowse 3.

Suite `heldout` `18b64a73`: fastbrowse against Browser Use agent, on the same 9 tasks.

| | passed | cost per task | median time |
|:--|:--|:--|:--|
| fastbrowse | 25/25 | $0.0072 (median), $0.0174 mean | 19.3s |
| Browser Use agent | 26/27 | $0.1970 (median), $0.2329 mean | 15.0s |

Each arm made 27 attempts; left out as provider outages: fastbrowse 2.

Suite `stretch-dev` `69abd819`: fastbrowse against Browser Use agent, on the same 5 tasks.

| | passed | cost per task | median time |
|:--|:--|:--|:--|
| fastbrowse | 12/15 | $0.0271 (median), $0.0366 mean | 44.9s |
| Browser Use agent | 15/15 | $0.3732 (median), $0.5790 mean | 59.9s |

Each arm made 15 attempts.

Suite `stretch-heldout` `f3f5c3f7`: fastbrowse against Browser Use agent, on the same 3 tasks.

| | passed | cost per task | median time |
|:--|:--|:--|:--|
| fastbrowse | 6/8 | $0.0283 (median), $0.0990 mean | 70.6s |
| Browser Use agent | 8/9 | $0.2579 (median), $0.4980 mean | 42.6s |

Each arm made 9 attempts; left out as provider outages: fastbrowse 1.

1 task graded on fastbrowse alone is in [docs/evals.md](docs/evals.md#results).
<!-- /evals:headline -->

Compare rows only at matching task versions. See [eval results and workflow](docs/evals.md).

### Why fastbrowse, against each kind of agent

- **LLM agents that generate actions** (Browser Use and similar): Jev picks each action from the controls
  that are on the page, so there is no invented selector to retry. Every claim in the answer links to the page text it came from.
- **Choice-model navigators** ([jev-ultrafast](https://github.com/browser-use/jev-ultrafast)): the same
  core technique, with page reading, cited answers, scoped secrets and an authorization gate. Navigation tasks compare the page each run ended on.
- **Scripts:** there are no selectors to maintain. The same agent handles a date picker, a checkout and
  a search box it has never seen.

## Try it

Needs [uv](https://docs.astral.sh/uv/); uv fetches Python itself (3.13 or newer). Runs use a
[Browser Use Cloud](https://cloud.browser-use.com) browser (`BROWSER_USE_API_KEY`) by default: it passes bot checks
a fresh local Chrome fails. Local Chrome is fully supported with `--local`. A browser already running anywhere,
from a container to a hosted browser with a CDP endpoint, is driven in place with `--cdp-url ws://…`: the run
opens one tab and leaves the browser as it was found.

```sh
export AI_GATEWAY_API_KEY=...   # or TYPESAFE_API_KEY, for Jev
export OPENROUTER_API_KEY=...   # for the LLM that plans and reads
export BROWSER_USE_API_KEY=...  # the cloud browser; or pass --local to use Chrome
uvx fastbrowse "What is the title of the top story right now?" --start https://news.ycombinator.com/
```

`uvx` runs the published package in an isolated cached environment. `uv tool install fastbrowse` keeps it on your
PATH, and `uv add fastbrowse` puts it in a project. Service keys can live in a `.env` file in the working directory;
[`.env.example`](.env.example) shows the settings. Values named by `--secret` must be in the process environment.

To work on fastbrowse itself:

```sh
git clone https://github.com/agent-labs-dev/fastbrowse.git && cd fastbrowse
uv sync --all-extras   # the extras carry mcp and browser_use_sdk, which the tests and evals import
cp .env.example .env   # then fill in the keys, BROWSER_USE_API_KEY included, or pass --local
uv run fastbrowse "What is the title of the top story right now?" --start https://news.ycombinator.com/
```

Steps go to stderr; the status, cost, step count and answer go to stdout. `--json` prints every step,
the quotes behind the answer, and cost by component.

| Flag | Effect |
|:--|:--|
| `--start URL` | the page to open first; worked out from the task when omitted |
| `--local` | use local Chrome instead of a Browser Use Cloud browser. Cloud is the default: it passes bot checks a fresh Chrome fails, and prints a URL to watch the run live |
| `--headed` | show the local Chrome window (implies `--local`) |
| `--profile DIR` | keep the local Chrome profile in `DIR`, so a site signed into there stays signed in (implies `--local`) |
| `--cloud-profile ID` | run on a Browser Use Cloud profile, signed in as whoever set it up |
| `--proxy-country CC` | browse from that country (Browser Use's codes: `uk`, `de`, ...; default `us`), so a shop shows its local delivery and prices |
| `--authorize` | allow submit, pay, delete and send; without it the run stops at `needs_confirmation` first |
| `--secret NAME=ENV_VAR[@ORIGIN]` | let the agent type `$ENV_VAR` on the declared origin, or the `--start` origin if omitted; models only see `NAME`. An explicit origin needs no `--start` |
| `--bitwarden ITEM` | match the vault login's saved URIs against `--start`, then allow its `username`, `password` and, when the item holds an authenticator key, `one_time_code` only on that start origin |
| `--max-steps N`, `--max-dollars N` | bound steps and model spend; defaults are 60 steps and no dollar cap. Cloud browser charges are added when it stops |
| `--downloads DIR` | keep downloaded files |
| `--json` | full result instead of the answer |
| `--record FILE` | save an MP4 of the tab, each step captioned, ending on the answer, time and cost (needs `ffmpeg`; the captions need its libass), e.g. `recordings/demo.mp4`, which git ignores; `demo.plain.mp4` beside it has no captions. It shows what the pages showed, so watch it before sharing |

```sh
export SAUCE_PASSWORD=secret_sauce
uv run fastbrowse "Log in as standard_user with the saved password and add the backpack to the cart." \
  --start https://www.saucedemo.com/ --secret password=SAUCE_PASSWORD --authorize
```

### Signed-in sites

Sign in once by hand in a profile of its own, then point runs at it:

```sh
google-chrome --user-data-dir="$HOME/.fastbrowse/amazon" https://www.amazon.com/   # sign in, then close Chrome
uv run fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." \
  --start https://www.amazon.com/ --profile ~/.fastbrowse/amazon --headed
```

On a cloud browser the profile lives on the [Browser Use Cloud](https://cloud.browser-use.com) account
rather than on disk, and `--cloud-profile ID` runs as it. Whoever signed that profile in did so once, in a
browser of their own; the run inherits the cookies and no model is shown a credential:

```sh
uv run fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." \
  --start https://www.amazon.com/ --cloud-profile prof_1234
```

Or from your vault, with the [Bitwarden CLI](https://bitwarden.com/help/cli/) unlocked. The item's saved URIs
must match `--start`; values are then typed only on that origin, and models see only the names `username` and
`password`, plus `one_time_code` when the item holds an authenticator key (the current code, computed as it is typed):

```sh
export BW_SESSION="$(bw unlock --raw)"
uv run fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." \
  --start https://www.amazon.com/ --bitwarden Amazon --profile ~/.fastbrowse/amazon --headed
```

### Models

The LLM defaults to `google/gemini-3.8-flash` at low reasoning effort, with
`google/gemini-3.5-flash-lite` for planning, proposing a direct address and typing field text.
Override with `FASTBROWSE_LLM_MODEL` (every purpose), `FASTBROWSE_LLM_MODEL_<PURPOSE>` (`PLAN`,
`READ`, `FIELD_TEXT`, `SHORTCUT`, `RECOVER`, `COMPOSE`, `VERIFY`) and `FASTBROWSE_LLM_REASONING`
(`low`, `medium`, `high`).

### Results

The exit code is 0 only for `complete`.

| Status | Meaning |
|:--|:--|
| `complete` | every information requirement is backed by a quote, and every action is confirmed on the page |
| `unverified` | it believes it finished but could not back every claim |
| `needs_confirmation` | stopped before an irreversible action; re-run with `--authorize` |
| `needs_login` | a sign-in wall that no `--secret` covers |
| `blocked` | a bot check (a CAPTCHA) that did not clear; not a sign-in, so no secret passes it |
| `needs_input` | a required value or file is missing, or an upload exceeds the configured size limit |
| `stuck` | recovery ran out without reaching a page state the run had not seen |
| `budget_exceeded` | a step, call, time or dollar limit was reached |
| `observation_limit` | the page or required evidence cannot fit the configured prompt budget |
| `unavailable` | a model or browser provider stayed unavailable through every retry; the same run later may pass |
| `error` | a model or browser failure |

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/architecture-dark.svg">
  <img src="assets/architecture.svg" alt="The task is planned and the start page opened in parallel; each step indexes the page, Jev picks an operation and target, code gates it and acts; reads keep verbatim quotes, and the answer cites every claim.">
</picture>

The LLM plans, reads and writes. Code owns the gates: irreversible actions stop without
`--authorize`, secrets reach models by name only, and supported cookie banners are refused through autoconsent.
More in [docs/design.md](docs/design.md).

## How it compares

| | Browser Use agent | [jev-ultrafast](https://github.com/browser-use/jev-ultrafast) | fastbrowse |
|:--|:--|:--|:--|
| Choosing an action | LLM generates from a screenshot | Jev picks from indexed controls | Jev picks from indexed controls |
| Returns | an answer | `DONE` or `BLOCKED` | an answer with quotes, or why it stopped |
| Reads pages | yes | no | yes, every claim cited |
| Signing in | yes | password fields excluded | `--secret` or a Bitwarden vault item; models see names only |
| Irreversible actions | not gated | not gated | stop unless `--authorize` |
| Browser | cloud | local Chrome, your profile | cloud by default, or local Chrome with `--local` |

jev-ultrafast is Browser Use's navigation agent; fastbrowse
shares its core techniques. Its column describes `main` as of 2026-09-18.

## Embed it

`uv add fastbrowse` first, then:

```python
import asyncio

from pydantic import BaseModel

from fastbrowse import run_task
from fastbrowse.models import Limits


class Release(BaseModel):
    package: str
    version: str


async def main() -> None:
    result = await run_task(
        "Find the httpx package and report its name and latest released version.",
        start="https://pypi.org/",
        output_schema=Release,
        limits=Limits(max_dollars=0.10),
    )
    print(result.status, result.data, f"${result.cost.known_dollars:.4f}")
    for evidence in result.evidence:
        print(f'  "{evidence.quote}" from {evidence.url}')


asyncio.run(main())
```

`run_task(cdp_url=...)` drives a browser that is already running, wherever it is, instead of starting one:
the run opens its own tab and closes the tabs it owns. It leaves the browser and pre-existing tabs open;
cookies and other changes made by the task can persist. Pass `browser_api_key=` to start a cloud browser;
with neither argument, it runs local Chrome. Passing both is an error.

`RunResult.citations` is a tuple of `Citation` objects, also importable from `fastbrowse`. Each has `id`
(the number in the answer), `text` (the Notes fact), `requirement_id` (or `None`), `url`, `quote` and
`deep_link`. Each claim in `result.answer` carries numbered Markdown links to its supporting facts.
Counts, totals and superlatives also cite the records they were derived from, including records read on earlier pages.
Only verified Notes facts supply citation URLs and quotes; an answer citing an unknown reference fails the
claim check, and the run falls back to an answer drafted from verified facts. Facts omitted from the answer have no citation, and citation numbers can have gaps.

Deep links follow the [WICG Text Fragments syntax](https://wicg.github.io/scroll-to-text-fragment/#syntax):
`url#existing-anchor:~:text=start`. Text is percent-encoded, including hyphens, ampersands and commas.
Whitespace is collapsed for the link; `quote` keeps the verbatim capture. Quotes over 120 characters with
more than ten words use the first and last five words as `text=start,end`. An existing anchor is preserved;
an old text directive is replaced. Pages that change or require a session may no longer show the quote.

To show a run as it happens, pass `on_event=`: a `BrowserEvent` arrives first with the live-view URL of a
cloud browser, then a `StepEvent` per step. `Config(step_frames=True)` adds a PNG of the page each step acted
on, for an interface that renders the run; a step whose page is showing a resolved secret sends no frame.
`StepEvent.step.facts` (also `StepResult.facts`) holds only the facts added by that step: text, requirement id,
quote, URL, deep link and reader (`jev_choice` or `llm`), with resolved secrets redacted before delivery.
`StepResult.note` carries read outcomes, dispatch details, gate refusals or recovery guidance when available;
it can be `None` for an ordinary successful action.

For continuous live images, pass an async `on_frame` handler accepting JPEG bytes. Frames follow the active
tab and are acknowledged after delivery, with no fixed frame rate. Only the latest pending frame is kept.
Handler failures are logged without stopping the run. Live frames and recordings are held back while a
resolved secret may show on the page, as PNG step frames are. No handler means no live capture.

Jev comes from Typesafe directly or through the Vercel AI Gateway, whichever key is set
(`FASTBROWSE_JEV_SOURCE=typesafe` or `gateway` picks the first provider when both are set). With both keys,
a retryable HTTP failure that exhausts retries switches the run to the other provider. A custom
`FASTBROWSE_JEV_BASE_URL` or nondefault `FASTBROWSE_JEV_MODEL` disables that failover.
Any other source
can be passed as `run_task(jev=...)`, implementing async `evaluate(state, questions)`; `run_task(llm=...)`
accepts an implementation of the `LLMClient.generate(...)` protocol in `fastbrowse.llm`.

## Use it from an MCP client

`fastbrowse-mcp` serves one `browse` tool over [MCP](https://modelcontextprotocol.io), so Claude Code, Claude
Desktop, Cursor or any other MCP client can hand it a task. It returns the answer, typed `fields` if asked for,
the quotes behind them, the status and what to do about it, and reports progress on every step.
The answer contains numbered Markdown links. MCP's `citations` list contains `quote` and `url` from the
run's evidence; it does not expose the Python `Citation` ids, requirement ids or deep-link fields.

```sh
claude mcp add fastbrowse -e OPENROUTER_API_KEY=... -e AI_GATEWAY_API_KEY=... -e BROWSER_USE_API_KEY=... \
  -- uvx --from 'fastbrowse[mcp]' fastbrowse-mcp --max-dollars 0.25
```

For a client configured by JSON, such as Claude Desktop:

```json
{
  "mcpServers": {
    "fastbrowse": {
      "command": "uvx",
      "args": ["--from", "fastbrowse[mcp]", "fastbrowse-mcp"],
      "env": { "OPENROUTER_API_KEY": "...", "AI_GATEWAY_API_KEY": "...", "BROWSER_USE_API_KEY": "..." }
    }
  }
}
```

The server's flags decide what a calling model may do; a call can ask for less, never more.

| Flag | Effect |
|:--|:--|
| `--local`, `--headed`, `--profile DIR`, `--downloads DIR` | as for the CLI, fixed for every call |
| `--cloud-profile ID` | every call runs signed in as that cloud profile; a calling model cannot choose it |
| `--allow-authorize` | let a call pass `authorize` to go through irreversible actions; without it they always stop at `needs_confirmation` |
| `--secret NAME=ENV_VAR@ORIGIN` | typed when a call's start page is on `ORIGIN` (`https://*.site.com` covers its hosts); the model sees `NAME` only |
| `--bitwarden ITEM` | a vault login a call may name in `bitwarden` |
| `--max-steps N`, `--max-dollars N`, `--max-seconds N` | ceilings per call (defaults 60, $1.00, 600s) |
| `--max-concurrent N` | runs at once, default 1; more calls wait their turn |
| `--transport http`, `--host`, `--port` | streamable HTTP at `/mcp` instead of stdio |

Over HTTP, set `FASTBROWSE_MCP_TOKEN` in the environment or `.env` and every request but `/healthz` needs
`Authorization: Bearer <token>`. The server refuses to bind anything but loopback without one. A run takes
seconds to minutes, so raise the client's tool timeout if it has one (`MCP_TOOL_TIMEOUT` in Claude Code).

## Safety model

- **Irreversible actions.** Jev judges clicks, including links, Enter presses and acceptance
  of confirm, prompt or before-unload dialogs. Code-selected pagination is exempt, as are authorized
  actions with sufficient confidence. A refusal appears as a failed step with a reason: an unauthorized,
  confident action stops at `needs_confirmation`; an uncertain action goes to recovery. This is a classifier,
  not a guarantee: a page can word a harmful control to look harmless.
- **Secrets.** Models see secret names only. A value is resolved at the moment of typing, only for
  its declared origin, and redacted from everything the run returns. A password field is typed only
  from a stored secret, never generated.
- **Page content is data.** Reader and verifier prompts say so, and completion is judged against quotes and page
  state rather than the model's say-so.

## Evals and development

The [eval workflow](docs/evals.md#workflow) has the run, publish and regeneration commands. Final page checks
require evidence read by the harness, and a pass also requires the task's expected ending. Published rows carry
build and task versions; the [site feed](docs/results/summary.json) is generated from them.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and checks, [docs/design.md](docs/design.md) for the browser
layer, and [docs/jev.md](docs/jev.md) for the Jev assumptions checked against Typesafe's documentation.

## Changelog

What changed in each release is in [CHANGELOG.md](CHANGELOG.md), and every
[GitHub release](https://github.com/agent-labs-dev/fastbrowse/releases) publishes that version's entry.

## License

MIT, copyright Agent Labs. Adapted third-party code is credited in
[NOTICE](NOTICE).
