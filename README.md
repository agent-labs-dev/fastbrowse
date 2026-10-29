<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/wordmark-dark.svg">
  <img src="assets/wordmark.svg" alt="fastbrowse" width="360">
</picture>

**Give your agent a browser.**

Run browser tasks locally or in a cloud browser. Jev chooses from indexed page controls; fastbrowse returns
cited answers or schema-validated data. Code controls credentials, authorization and spending.

[![pypi](https://img.shields.io/pypi/v/fastbrowse?style=flat-square&color=6366F1)](https://pypi.org/project/fastbrowse/)
![python](https://img.shields.io/badge/python-3.13%20%7C%203.14-475569?style=flat-square)
![license](https://img.shields.io/badge/license-MIT-475569?style=flat-square)
![status](https://img.shields.io/badge/status-beta-6366F1?style=flat-square)

</div>

---

## Why

fastbrowse indexes the page into candidates and has [Jev](https://typesafe.ai), a choice model, **pick one**, so it cannot click
something that was never on the page. Factual claims in a complete answer cite verbatim quotes from the page.

![fastbrowse signing in to a shop, adding two products, filling the shipping form, placing the order and reporting the total with its quotes](docs/assets/demo.gif)

Recorded on local Chrome: 13 steps, 20.9s and $0.0034 in model calls. Playback is 1.4x faster with pauses cut.

### Against Browser Use agents

Benchmarking compares exactly three things:

| | Comparison | Arms | Recorded |
|:--|:--|:--|:--|
| C1 | agent against agent | fastbrowse against the hosted Browser Use agent | 46 tasks, three repeats: 99.2% against 96.3%, execution time 18.9s against 15.3s, cost per run $0.0296 against $0.3180 |
| C2 | backend against backend | fastbrowse on its own browser pool against fastbrowse on a Browser Use Cloud browser | earlier campaign record (release-0.5.25), not on the published page; the pool passed fewer tasks than the Browser Use Cloud browser - see [Benchmark results](docs/benchmarks.md) |
| C3 | fastbrowse and its backend against [Browser Use Ultrafast](https://github.com/browser-use/jev-ultrafast) | fastbrowse on its own browser pool against `jev-ultrafast`, 6 tasks | not yet measured |

C1's figures are the recorded head-to-head on 46 tasks, three repeats each, on Browser Use cloud browsers under
identical limits, with the hosted agent at its defaults; the same release also publishes fastbrowse alone on the
53-task internal catalog, at 99.3%, an execution time of 17.0s and $0.0268 a run. C2 compares browsers, not
agents: both arms run the same fastbrowse build, tasks and repeats, and only the browser differs. That comparison
is an earlier record and is not on the published page; on it fastbrowse passed fewer tasks on its own pool than on
the Browser Use Cloud browser. [Benchmark results](docs/benchmarks.md)
gives each comparison's sources, the campaigns behind C1, the browser cost basis and the limitations. The
[benchmark page](https://fastbrowse.ai/benchmarks) publishes the release-0.7.1 campaign (agent commit `7b8b0e8f`,
data sha256 `7d6ac26f23fa6d354128fa7e4bcc8190125c441f4f136dc39d0e3c35cec5d4e2`).
Rows are comparable only at matching task versions; the upstream suites and the Codex with Cua Driver arm are
diagnostics, described in [eval results and workflow](docs/evals.md).

### Why fastbrowse, against each kind of agent

- **LLM agents that generate actions** (the hosted Browser Use agent and similar): Jev picks each action from the controls
  that are on the page, so there is no invented selector to retry. Factual claims in a complete answer link to the page text they came from.
- **Choice-model navigators** ([Browser Use Ultrafast](https://github.com/browser-use/jev-ultrafast), the
  `jev-ultrafast` package and eval arm): both choose actions with Jev. fastbrowse adds cited answers, schema-validated
  data, scoped credentials and an authorization gate. Navigation tasks compare the page each run ended on.
  This navigation result does not establish a winner for every workflow.
- **Scripts:** there are no selectors to maintain. The same agent handles a date picker, a checkout and
  a search box it has never seen.

## Try it

Needs [uv](https://docs.astral.sh/uv/); uv fetches Python itself (3.13 or newer). Runs use a
[Browser Use Cloud](https://cloud.browser-use.com) browser (`BROWSER_USE_API_KEY`) by default: it passes bot checks
a fresh local Chrome fails. Local Chrome is fully supported with `--local`. A browser already running anywhere,
from a container to a hosted browser with a CDP endpoint, is driven in place with `--cdp-url ws://…` or
`--cdp-port 9222`: the run opens one tab and leaves the browser as it was found. To drive a window that is
already open, such as an Electron app, see [Open windows and Electron apps](#open-windows-and-electron-apps).
A Node program needs neither uv nor Python: see [Use it from JavaScript](#use-it-from-javascript).

```sh
export TYPESAFE_API_KEY=...     # Jev, direct TypeSafe (recommended)
export OPENROUTER_API_KEY=...   # the LLM that plans and reads; also a Jev text route
# export AI_GATEWAY_API_KEY=... # Vercel AI Gateway: the LLM when OPENROUTER_API_KEY is unset; Jev last resort
export BROWSER_USE_API_KEY=...  # the cloud browser; or pass --local to use Chrome
uvx fastbrowse "What is the title of the top story right now?" --start https://news.ycombinator.com/
```

Jev uses direct TypeSafe when `TYPESAFE_API_KEY` is supplied. That is the recommended route. OpenRouter
is supported for Jev text when `OPENROUTER_API_KEY` is set and no direct key is supplied, but it is not
recommended: it puts another service between fastbrowse and TypeSafe, and it never receives Jev image
input. A gateway key on its own also serves Jev, as a last resort rather than the recommended route; with
`AI_GATEWAY_API_KEY` beside another key, the gateway is the backup. `FASTBROWSE_DECISION_SOURCE` overrides
automatic selection, and also picks another decision model's host: `telnyx`, OpenAI's `openai`, `perplexity`,
`aimlapi`, a local `ollama`, or any `custom` System One server ([other hosts](docs/jev.md#other-decision-hosts)); see [provider routing](docs/jev.md#provider-failover). The LLM uses OpenRouter when
`OPENROUTER_API_KEY` is set, otherwise the Vercel AI Gateway's OpenAI-compatible chat completions.

`uvx` runs the published package in an isolated cached environment. `uv tool install fastbrowse` keeps it on your
PATH, and `uv add fastbrowse` puts it in a project. Service keys can live in a `.env` file in the working directory;
[`.env.example`](.env.example) shows the settings. Values named by `--secret` must be in the process environment.

On Arch Linux, the [`fastbrowse` AUR package](https://aur.archlinux.org/packages/fastbrowse) puts `fastbrowse` and
`fastbrowse-mcp` on your PATH from the same PyPI release: `paru -S fastbrowse` with an AUR helper, or `makepkg -si`
in [`packaging/aur`](packaging/aur).

To work on fastbrowse itself:

```sh
git clone https://github.com/agent-labs-dev/fastbrowse.git && cd fastbrowse
uv sync --all-extras   # the eval-data extra carries pyyaml, which the tests and evals import;
                       # this also builds the Rust core, so it needs cargo, which rustup installs at the version rust-toolchain.toml pins
cp .env.example .env   # then fill in the keys, BROWSER_USE_API_KEY included, or pass --local
uv run fastbrowse "What is the title of the top story right now?" --start https://news.ycombinator.com/
```

Steps go to stderr; the status, cost, step count, the reason a run stopped short and the answer go to stdout.
`--json` prints every step, the quotes behind the answer, and cost by component.

| Flag | Effect |
|:--|:--|
| `--start URL` | the page to open first; worked out from the task when omitted |
| `--context TEXT` | background the run may use but never treats as the task, such as where the request came from; `context=` in `run_task` and `serve` |
| `--cloud` | force cloud Chrome even when `FASTBROWSE_PROFILE` or `FASTBROWSE_HEADED` is set |
| `--local` | use local Chrome instead of a Browser Use Cloud browser. Cloud is the default: it passes bot checks a fresh Chrome fails, and prints a URL to watch the run live |
| `--fetch` | read pages over HTTP and start no browser at all, for a task that only has to read one and needs no clicking, forms or screenshots |
| `--fetch-first` | read over HTTP first and start the browser only if the run needs one: a step that clicks, fills or looks at the page, or a page the static read cannot see. The browser run starts again from the task inside what is left of the limits, and its cost includes the attempt |
| `--headed` | show the local Chrome window (implies `--local`) |
| `--profile DIR` | keep the local Chrome profile in `DIR`, so a site signed into there stays signed in (implies `--local`) |
| `--cloud-profile ID` | run on a Browser Use Cloud profile, signed in as whoever set it up |
| `--cdp-url URL` | drive a browser already running at this `ws://` or `wss://` DevTools URL instead of starting one |
| `--cdp-port PORT` | the same, for a browser or Electron app listening on `127.0.0.1:PORT`; the URL is read from its `/json/version` |
| `--attach` | with `--cdp-url` or `--cdp-port`, drive a window already open instead of opening a tab, and leave it open after the run |
| `--target-match TEXT` | attach to the first window whose title or URL contains `TEXT` (implies `--attach`) |
| `--foreground` | bring the run's tab and its browser window to the front, to watch it. Without it a run in a visible window works behind the window's current tab; see [working in the background](#working-in-the-background) for what Chrome still brings forward |
| `--proxy-country CC` | browse from that country (Browser Use's codes: `uk`, `de`, ...; default `us`), so a shop shows its local delivery and prices |
| `--authorize` | allow submit, pay, delete and send; without it the run stops at `needs_confirmation` first |
| `--secret NAME=ENV_VAR[@ORIGIN]` | let the agent type `$ENV_VAR` on the declared origin (`https://*.site.com` covers its hosts), or the `--start` origin if omitted; models only see `NAME`. An explicit origin needs no `--start` |
| `--bitwarden ITEM` | match the vault login's saved URIs against `--start`, then allow its `username`, `password` and, when the item holds an authenticator key, `one_time_code` only on that start origin |
| `--max-steps N`, `--max-dollars N` | optionally bound steps and model spend; defaults are unlimited. Cloud browser charges are added when it stops |
| `--downloads DIR` | keep downloaded files |
| `--json` | full result instead of the answer |
| `--trace FILE` | append the run's trace to `FILE` as JSON lines, or `-` for stderr: each model call with its purpose and timing, which reader answered each read and why, every check's scores and each recovery's reason. It holds ids, scores, redacted addresses, requirement texts and control labels, not page captures. `FASTBROWSE_TRACE` does the same for `serve` and the MCP server |
| `--record FILE` | save an MP4 of the tab, each step captioned, ending on the answer, time and cost (needs `ffmpeg`; the captions need its libass), e.g. `recordings/demo.mp4`, which git ignores; `demo.plain.mp4` beside it has no captions. It shows what the pages showed, so watch it before sharing |
| `--cursor` | draw the agent's cursor over a visible Chrome (`--headed`, or one you attach to) with [Cua Driver](https://github.com/trycua/cua), so you can watch where it acts. Off by default. It needs `cua-driver` on `PATH` and an X11 display on Linux, and does nothing without them. It brings the run's tab to the front as `--foreground` does, since the cursor lies over the window's front tab |

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

`--headed` is not needed once the profile is signed in: without it the run is headless and opens no window. A
headed run opens one window when Chrome starts and then works in a tab behind it.

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

To change a password, supply its replacement as an origin-scoped secret named `new_password`, alongside
the existing `password`. Refer to `new_password` in the task and use `--authorize` to allow submission.
Without the replacement secret, the run asks for input; it never generates a password or reuses the old one.

### Open windows and Electron apps

An Electron app is Chromium underneath, so a run can drive it once the app exposes a DevTools port. Quit the app,
start it again with `--remote-debugging-port` (an app that is already running ignores the flag), then attach to
its window by title or URL. VS Code titles each window after the folder it has open:

```sh
open -a "Visual Studio Code" --args --remote-debugging-port=9222 ~/code/my-project   # elsewhere, pass the flag to the binary
uv run fastbrowse "Open the Extensions view and report how many extensions are installed." \
  --cdp-port 9222 --target-match my-project
```

The same works for a Chrome you started with `--remote-debugging-port`. With `--attach`, the run takes over an
existing window instead of opening a tab of its own. Without `--start` it begins on whatever the window shows,
and the window stays open when the run ends. `--target-match` picks the first window whose title or URL contains
the text; `--attach` alone takes the first window, skipping DevTools. Popups the window opens join the run.
Windows that nothing opened, as an Electron app's main process opens them, join only with `--target-match`,
because in a browser such a window could equally be a tab you opened yourself.

### Working in the background

A run in a visible window, whether `--headed` or a browser you handed over, opens its tab behind the window's
current tab and renders it there under focus emulation. Nothing the run sends raises or focuses the window. `--foreground` brings the tab and its window to the front instead, to watch the run. Three things
are Chrome's own and still bring a window forward: a headed Chrome showing its window as it starts, a popup a
page opens, and, with `--attach`, a click in a window's front tab.

For a signed-in browser that stays open and asks for nothing, start a Chrome of its own once, sign in, and leave
it running. Each run then opens and closes a background tab in it:

```sh
google-chrome --remote-debugging-port=9222 --user-data-dir="$HOME/.fastbrowse/chrome"
uv run fastbrowse "Report my open orders." --start https://www.amazon.com/ --cdp-port 9222
```

Chrome ignores `--remote-debugging-port` on its default data directory, so the directory of its own is required.
Your everyday Chrome can instead offer an endpoint from `chrome://inspect/#remote-debugging`, but it then stops
every connection on an "Allow remote debugging?" prompt that takes focus and waits for a click, once per run.
Any program on the machine can drive a browser that listens on a debugging port.

A DevTools port gives any program on the machine full control of the app, including its signed-in sessions.
Close the app, or restart it without the flag, when you are done.

### Models

The LLM defaults to `google/gemini-3.8-flash` at low reasoning effort, with
`google/gemini-3.5-flash-lite` for proposing a direct address and typing field text.
Override with `FASTBROWSE_LLM_MODEL` (every purpose), `FASTBROWSE_LLM_MODEL_<PURPOSE>` (`PLAN`,
`READ`, `FIELD_TEXT`, `SHORTCUT`, `RECOVER`, `COMPOSE`, `VERIFY`) and `FASTBROWSE_LLM_REASONING`
(`none`, `low`, `medium`, `high`), and `FASTBROWSE_LLM_REASONING_<PURPOSE>` sets one purpose's effort over it.
`none` turns reasoning off, for a model such as DeepSeek V4.1 Flash that reasons at every effort and can spend a
short output cap before it answers: `FASTBROWSE_LLM_REASONING_VERIFY=none` turns it off for the verdict alone.

`FASTBROWSE_LLM_BASE_URL` sends every LLM call to another OpenAI-compatible chat completions endpoint instead
of OpenRouter and the gateway, such as a local vLLM server at `http://127.0.0.1:8000/v1`. Name the model it
serves with `FASTBROWSE_LLM_MODEL`; a run refuses to start without one, and a blank value counts as unset.
OpenRouter's `reasoning` field is sent to that endpoint only when `FASTBROWSE_LLM_REASONING` is set, and for one
purpose when `FASTBROWSE_LLM_REASONING_<PURPOSE>` is, since a strict server refuses a field it does not know. `FASTBROWSE_LLM_API_KEY` is its bearer
token, sent only when set, and that endpoint never receives `OPENROUTER_API_KEY` or `AI_GATEWAY_API_KEY`. Two
settings shape its requests: `FASTBROWSE_LLM_STRICT=0` sends each schema as the model defines it rather than
in strict form, for a server whose structured output breaks under strict mode, and `FASTBROWSE_LLM_EXTRA_BODY`
is a JSON object added to every request, such as `{"chat_template_kwargs":{"enable_thinking":false}}`. It may
not set a field fastbrowse sends itself (`model`, `messages`, `max_tokens`, `response_format`, `provider`,
`reasoning`). Jev still needs its own key. An endpoint that reports no price leaves those calls' cost unknown
rather than zero, so `--max-dollars` stops the run after the first such call.

`FASTBROWSE_LLM_PROVIDER` names the provider instead of leaving it to which keys are set: `openrouter`,
`gateway`, `openai`, `anthropic`, `gemini`, `groq`, `together`, `mistral`, `deepseek`, `xai`, `fireworks`,
`azure`, `ollama`, `lmstudio`, `vllm` or `custom`. Each reads its own key, such as `OPENAI_API_KEY` or
`ANTHROPIC_API_KEY`, and sends it to that provider's host alone. [LLM providers](docs/llm-providers.md) lists
what each preset sends, the models it defaults to, and the provider documentation behind it. Unset, the LLM is
chosen as above.

### Results

The exit code identifies the run status. Configuration refusals exit 1; invalid command syntax exits 2.
Only `complete` exits 0, so a program can also check zero versus nonzero.

| Status | Exit | Meaning |
|:--|--:|:--|
| `complete` | 0 | every information requirement is backed by a quote, and every action that changes the site is confirmed on the page |
| `unverified` | 10 | it believes it finished but could not back every claim |
| `needs_confirmation` | 3 | stopped before an irreversible action; re-run with `--authorize` |
| `needs_login` | 4 | a sign-in wall that no `--secret` covers |
| `blocked` | 6 | a bot check (a CAPTCHA) that did not clear; not a sign-in, so no secret passes it |
| `needs_input` | 5 | a required value or file is missing, or an upload exceeds the configured size limit |
| `stuck` | 9 | recovery ran out without reaching a page state the run had not seen |
| `budget_exceeded` | 7 | a resource limit was reached, or a missing cost prevents enforcing the dollar cap |
| `observation_limit` | 11 | the page or required evidence cannot fit the configured prompt budget |
| `unavailable` | 8 | a model or browser provider stayed unavailable through every retry; the same run later may pass |
| `error` | 1 | a model or browser failure |

A `budget_exceeded` JSON result includes `budget.resource` (`steps`, `seconds`, `dollars`, `jev_calls` or
`llm_calls`) and `budget.limit`. Other results have `budget: null`. The embedding and MCP APIs carry the same
optional object, so callers can identify the affected limit without parsing an error message.

A `needs_login` result, and a `blocked` one at a bot check, carries `wall`: `kind` (`password`, `one_time_code`,
`sign_in` or `bot_check`), the page's `origin`, the visible labels of the `fields` it asked for and `seen_at_ms`.
A `one_time_code` wall wants a code the site sent, which a caller that can read the user's email or texts can
supply as a secret scoped to that origin and run again; a `password` or `sign_in` wall wants the user. Other
results have `wall: null`.

A run that stops `stuck` or `unverified` before writing its answer, with evidence read for a task that asks for
one, returns a cited answer that starts `Answer (unverified)`, written by one more composer call. If that call
fails, the answer is the list of quotes the run collected, as it is for the other statuses that stop with evidence.

A bot check can block a cloud browser before any task steps execute. Retry with an attached local browser
or choose another source for the research. Credentials do not resolve a CAPTCHA.
The `blocked` result's `evidence` quotes the opening text of the challenge page, with its address.

If a model completion omits its cost, fastbrowse looks up the matching OpenRouter or Vercel generation
receipt. If that receipt is still unavailable, an OpenRouter call is charged at an upper bound from the model's
listed prices: its whole context window at the highest price. A call that cannot be bounded, through the Vercel AI
Gateway or a custom endpoint, leaves the cost unknown, and a run with a dollar cap stops with `budget_exceeded`;
this does not mean the known charges reached the cap. Supplied `inputs` can provide exact field text
without a field-writing generation.

A browser command that remains unanswered for 120 seconds returns `unavailable`, even if the browser
still answers health checks. Step and dollar limits do not bound total elapsed time. Set `Limits(max_seconds=...)` for a run deadline.
When investigating a stalled run, write its trace with `--trace FILE` (or `FASTBROWSE_TRACE=FILE` for `serve` and the MCP server) and keep the last event alongside the result.

[Troubleshooting](docs/troubleshooting.md) takes each status a run can stop with: what it means, its common
causes and the next step.

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/architecture-dark.svg">
  <img src="assets/architecture.svg" alt="The task is planned and the start page opened in parallel; each step indexes the page, Jev picks an operation and target, code gates it and acts; reads keep verbatim quotes, and the answer cites every claim.">
</picture>

The LLM plans, reads and writes. Code owns the gates: irreversible actions stop without
`--authorize`, secrets reach models by name only, and supported cookie banners are refused through autoconsent.
More in [docs/design.md](docs/design.md).

The core that runs the loop is Rust, in [crates/](crates): `run_task`, `fastbrowse`, `fastbrowse serve` and
`fastbrowse-mcp` hand their work to it, and the `fastbrowse` wheel carries it as `fastbrowse._native`
([ADR 0004](docs/adr/0004-rust-core-by-default.md)). It reaches the result the earlier Python core reached on all 36
fixture tasks, with 18x less agent CPU, 54x faster startup and 5.4x less peak memory
([measurements](bench/results/rust-vs-python.md)).

## How it compares

| | Browser Use agent (hosted) | [Browser Use Ultrafast](https://github.com/browser-use/jev-ultrafast) (`jev-ultrafast`) | fastbrowse |
|:--|:--|:--|:--|
| Choosing an action | hosted model and tools | Jev picks from indexed controls | Jev picks from indexed controls |
| Returns | an answer | `DONE` or `BLOCKED` | an answer with quotes, or why it stopped |
| Cited answers | answer output; not graded for citations here | no cited-answer result API | answers backed by captured quotes |
| Signing in | yes | password fields excluded | `--secret` or a Bitwarden vault item; models see names only |
| Irreversible actions | explicit task instructions in our fixture tests | no authorization parameter in Agent | classifier plus a code-enforced authorization gate |
| Browser | cloud | Chrome through Browser Harness; cloud in these evals | cloud by default, or local Chrome with `--local` |
| Benchmarked in | C1 | C3 | C1 on a cloud browser; C2 on its own pool and on a Browser Use Cloud browser; C3 on its own pool |

Browser Use Ultrafast (`jev-ultrafast`) is Browser Use's navigation agent, separate from its hosted
LLM agent; fastbrowse shares its core techniques. Its column describes
[commit 1231850a](https://github.com/browser-use/jev-ultrafast/tree/1231850a0bf1a0c0341fe408ef1668dbbfdfac46),
the version pinned by the eval runner.

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
cookies and other changes made by the task can persist. `cdp_port=` finds the same browser from its DevTools
port on `127.0.0.1`. `attach=True` and `target_match=` drive a window already open and leave it open, as
`--attach` and `--target-match` do, and `foreground=True` brings the run's tab to the front as `--foreground`
does. Pass `browser_api_key=` to start a cloud browser;
with neither argument, it runs local Chrome. Passing both is an error. `cloud_extensions=[...]` loads up to
three of your account's ready Browser Use Cloud extensions, by ID, into that cloud browser; it is an error with
local or attached browsers.

`connect_cdp()` hands a script of your own the attached page, without the agent. Downloads go to `downloads=`, or
to a scratch directory removed on exit:

```python
from fastbrowse import connect_cdp


async def main() -> None:
    async with connect_cdp(9222, target_match="my-project") as page:
        print(await page.observe())
```

`resolve_cdp_port(port)` returns the `ws://` URL behind a DevTools port, for a caller that passes `cdp_url=`.

`run_task` and `connect_cdp` take two access controls that the command line, the MCP server and the JavaScript
SDK do not offer. `allowed_origins=["https://app.example.com"]` limits the documents a run may read and control
to those exact origins: scheme, host and optional port, with no wildcard or path. A navigation, redirect, frame
or popup outside the list is refused before it is sent, and a window already open outside it is never read.
A sign-in that redirects through another origin needs that origin listed too. The list scopes documents, not
the network: images, scripts and requests a listed page makes to other hosts still load. A scoped run delivers
no live frames or screenshots, and combining it with `record=` is an error. `check_access=` is an async callback
awaited before the browser starts and before every read, navigation and action. When it raises, `connect_cdp`
raises a `BrowserError` and `run_task` returns a `RunResult` with status `error`; either way the error names only
the exception's type, never its text.

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
Handler failures are logged without stopping the run. A handler must honour cancellation: it runs on the
caller's event loop, so one that catches `CancelledError` without re-raising keeps that loop alive, and
`asyncio.run` then waits for it at shutdown. Live frames and recordings are held back while a
resolved secret may show on the page, as PNG step frames are. No handler means no live capture.

To keep a run's trace in this process, as `--trace` writes it for the command, make the run inside
`with traced() as events:` (also importable from `fastbrowse`). Each event is a dict naming its `event`, and runs
that overlap keep separate lists. A run given neither `jev=` nor `llm=` begins with `route`, whose `providers`
names the Jev and LLM providers it calls, and each model request that failed transiently is a `request_transient`
with its `began` and `ended` on `time.monotonic()`, beside the model requests' other events.

Jev uses direct TypeSafe when keyed, which is the recommended route, otherwise OpenRouter. A gateway key
on its own also serves Jev, as a last resort rather than the recommended route, and OpenRouter never
receives Jev image input. See [Jev routing](docs/jev.md#provider-failover) for key precedence, overrides and failover.
Any other source
can be passed as `run_task(jev=...)`, implementing async `evaluate(state, questions)`; `run_task(llm=...)`
accepts an implementation of the `LLMClient.generate(...)` protocol in `fastbrowse.llm`.

## Use it from JavaScript

`npm install fastbrowse` gives a Node program the same agent, with no Python and no uv on the machine. The
package is a TypeScript SDK with no dependencies. With it npm installs one of five packages that hold the agent
as a native binary, the one for your platform: `@fastbrowse/darwin-arm64`, `darwin-x64`, `linux-arm64`,
`linux-x64` or `win32-x64`. No install script runs and nothing is downloaded on first use, so it installs under
pnpm and bun with scripts blocked, and it starts offline. It needs Node.js 20 or newer. The npm packages carry
the PyPI package's version and are published by the same tag.

The binary brings no browser. It finds local Chrome, starts a Browser Use Cloud browser, or drives one already
running, as the command line does. It reads the same keys from the environment of your process
(`TYPESAFE_API_KEY` for Jev, `OPENROUTER_API_KEY` for the LLM, and `BROWSER_USE_API_KEY` for a cloud
browser); `env` in `Fastbrowse.start` adds to them.

```sh
npm install fastbrowse zod
export TYPESAFE_API_KEY=...
export OPENROUTER_API_KEY=...
```

```ts
import { Fastbrowse } from 'fastbrowse';
import { z } from 'zod';

const Release = z.object({ package: z.string(), version: z.string() });

const fb = await Fastbrowse.start({ local: true });
try {
  const result = await fb.run('Find the httpx package and report its name and latest released version.', {
    start: 'https://pypi.org/',
    output: Release,
    limits: { maxDollars: 0.1 },
    onEvent: event => {
      if (event.type === 'step') console.error(event.step.operation, event.step.target);
    },
    signal: AbortSignal.timeout(120_000),
  });
  console.log(result.status, result.answer);
  if (result.status === 'complete') console.log(result.output.package, result.output.version);
  for (const evidence of result.evidence) console.log(`  "${evidence.quote}" from ${evidence.url}`);
} finally {
  await fb.close();
}
```

`Fastbrowse.start` starts one fastbrowse process and `run` sends it a task. The process serves one run at a
time: a second `run` while one is active rejects with the busy error, and a program that wants runs side by
side starts more instances. `close()` shuts the process down and waits for it. The process also exits when
yours does, however yours ended.

`run` resolves with the result whatever status the run ended in, so `needs_login` or `stuck` is read from
`status` and is not an exception. It rejects when no run took place or none finished: with `RpcError` when the
server refuses the request before a browser opens (a bad option, a missing key, a busy server), with
`AbortError` when `signal` stopped the run, and with `ProcessExitedError` when the process is gone. A run that
fails after it has started resolves with status `error`.

The result is the `RunResult` the Python library returns, with the same statuses, citations and cost lines.
Its fields keep their Python names, such as `final_url`, since the types are generated from the protocol's schema;
the options are camelCase. The structured data is in `data`, as in Python. The SDK adds
`output`: with a Zod (4.2 or newer) or ArkType schema, or a Valibot schema wrapped by
`@valibot/to-json-schema`, `output` is `data` after the schema's own validation and transforms, and has the
schema's output type. Data the schema refuses rejects with `OutputValidationError`, which carries the result.
A JSON Schema object is accepted too; `output` is then `data`, typed `unknown`.

What a run can fill is narrower than what a schema can say. The server accepts nested objects, arrays,
`enum`, `const` and optional values, and refuses any other keyword by name before a browser opens. A run
fills a flat object of required string, number, integer and boolean fields, as the example has. With any
other field the run ends `unverified` with no data.

The callbacks that keep credentials and the last word in your code cross the process boundary:

```ts
await fb.run('Log in as standard_user with the saved password and add the backpack to the cart.', {
  start: 'https://www.saucedemo.com/',
  authorization: { irreversibleActions: true },
  secrets: {
    refs: [{ name: 'password', origins: ['https://www.saucedemo.com'] }],
    resolve: name => vault.get(name),
  },
  until: url => url.endsWith('/cart.html'),
});
```

`secrets.resolve` is called each time a value is typed and never for an origin its ref does not cover, so the
value leaves your vault at that moment and a one-time code is fresh. `until` gets the address the run ended on,
and anything but `true` keeps the run from `complete`. `onFrame` receives JPEG frames of the active tab; without
it no frame is sent. A callback that throws ends the run with status `error`. The other options are `run_task`'s:
`context`, `inputs`, `attachments` as bytes, `downloads`, `record`, and the browser choices `chrome`, `cloudProfile`,
`cdpUrl`, `cdpPort`, `attach`, `targetMatch`, `foreground`, `proxyCountry`, `viewport` and `cloudAllowResizing`,
which `Fastbrowse.start` also takes as defaults for every run. A run passes `null` for one of them to go without
that default.

There is no binary for Alpine or another musl system, and none for a platform outside the five. There
`Fastbrowse.start` rejects with an error that names the platform, and a binary of your own is named with
`binaryPath` or `FASTBROWSE_BINARY`. The macOS binaries carry an ad-hoc signature and are not notarized, which a Mac
that allows programs only by the team that signed them refuses. The Windows binary is not signed. Bun and Deno are
untested.

## Use fastbrowse with your agent

Install the [fastbrowse skill](docs/skill.md) to delegate website tasks from an agent that supports Agent Skills. It uses the MCP
tool when connected, or the CLI, and preserves the task's citations, status, authorization, and budget.
The guide covers agent selection, configuration, and example prompts for Codex and Claude Code.
Agents without skill support can use the CLI or MCP server directly.

## Run fastbrowse through Nebula

[Nebula](https://www.nebula.gg/) embeds fastbrowse in its agent harness, with browser steps, results and
permitted vault logins. Use Bitwarden or 1Password through Nebula with credentials scoped to authorized websites.

## Use it from an MCP client

`fastbrowse-mcp` serves one `browse` tool over [MCP](https://modelcontextprotocol.io), so Claude Code, Claude
Desktop, Cursor or any other MCP client can hand it a task. It returns the answer, typed `fields` if asked for,
the quotes behind them, the status and what to do about it, and reports progress on every step.
The answer contains numbered Markdown links. MCP's `citations` list contains `quote` and `url` from the
run's evidence; it does not expose the Python `Citation` ids, requirement ids or deep-link fields.

```sh
claude mcp add fastbrowse -e TYPESAFE_API_KEY=... -e OPENROUTER_API_KEY=... -e BROWSER_USE_API_KEY=... \
  -- uvx --from fastbrowse fastbrowse-mcp --max-dollars 0.25
```

For a client configured by JSON, such as Claude Desktop:

```json
{
  "mcpServers": {
    "fastbrowse": {
      "command": "uvx",
      "args": ["--from", "fastbrowse", "fastbrowse-mcp"],
      "env": { "TYPESAFE_API_KEY": "...", "OPENROUTER_API_KEY": "...", "BROWSER_USE_API_KEY": "..." }
    }
  }
}
```

The server's flags decide what a calling model may do; a call can ask for less, never more.

| Flag | Effect |
|:--|:--|
| `--local`, `--headed`, `--profile DIR`, `--foreground`, `--downloads DIR`, `--cursor` | as for the CLI, fixed for every call |
| `--cloud-profile ID` | every call runs signed in as that cloud profile; a calling model cannot choose it |
| `--allow-authorize` | let a call pass `authorize` to go through irreversible actions; without it they always stop at `needs_confirmation` |
| `--secret NAME=ENV_VAR@ORIGIN` | typed when a call's start page is on `ORIGIN` (`https://*.site.com` covers its hosts); the model sees `NAME` only |
| `--bitwarden ITEM` | a vault login a call may name in `bitwarden` |
| `--max-steps N`, `--max-dollars N`, `--max-seconds N` | optional ceilings per call; defaults are unlimited |
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

[Eval commands](docs/evals.md) cover running suites, publishing results and regenerating evidence. Final page checks
require evidence read by the harness, and a pass also requires the task's expected ending. Published rows carry
build and task versions; the [compact results feed](docs/results/summary.json) is generated from them.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and checks, [docs/design.md](docs/design.md) for the browser
layer, and [docs/jev.md](docs/jev.md) for the Jev assumptions checked against Typesafe's documentation.

## Changelog

What changed in each release is in [CHANGELOG.md](CHANGELOG.md), and every
[GitHub release](https://github.com/agent-labs-dev/fastbrowse/releases) publishes that version's entry.

## License

MIT, copyright Agent Labs. Adapted third-party code is credited in
[NOTICE](NOTICE).
