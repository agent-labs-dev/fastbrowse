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
![status](https://img.shields.io/badge/status-pre--alpha-6366F1?style=flat-square)

</div>

---

## Why

fastbrowse indexes the page into candidates and has [Jev](https://typesafe.ai), a choice model, **pick one**, so it cannot click
something that was never on the page. Every claim in an answer cites a verbatim quote from the page.

![fastbrowse signing in to a shop, adding two products, filling the shipping form and placing the order](docs/assets/demo.gif)

Recorded on local Chrome: 13 steps, 20.9s and $0.0034 in model calls. Playback is 1.4x faster with pauses cut.

### Against Browser Use agents

Benchmark methods and approved results belong on the [benchmark page](https://fastbrowse.ai/benchmarks).
Detailed runs remain private until reviewed and approved for publication.

Compare rows only at matching task versions. See [eval results and workflow](docs/evals.md).

### Why fastbrowse, against each kind of agent

- **LLM agents that generate actions** (the hosted Browser Use agent and similar): Jev picks each action from the controls
  that are on the page, so there is no invented selector to retry. Every claim in the answer links to the page text it came from.
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
export OPENROUTER_API_KEY=...   # for Jev and the LLM that plans and reads
# export AI_GATEWAY_API_KEY=... # Vercel AI Gateway: Jev backup, and the LLM when OPENROUTER_API_KEY is unset
export BROWSER_USE_API_KEY=...  # the cloud browser; or pass --local to use Chrome
uvx fastbrowse "What is the title of the top story right now?" --start https://news.ycombinator.com/
```

Jev uses direct TypeSafe when `TYPESAFE_API_KEY` is supplied; otherwise OpenRouter is primary.
Vercel AI Gateway is supported as a backup or an explicit primary. `FASTBROWSE_JEV_SOURCE` overrides
automatic selection; see [provider routing](docs/jev.md#provider-failover). The LLM uses OpenRouter when
`OPENROUTER_API_KEY` is set, otherwise the Vercel AI Gateway's OpenAI-compatible chat completions.

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
| `--cloud` | force cloud Chrome even when `FASTBROWSE_PROFILE` or `FASTBROWSE_HEADED` is set |
| `--local` | use local Chrome instead of a Browser Use Cloud browser. Cloud is the default: it passes bot checks a fresh Chrome fails, and prints a URL to watch the run live |
| `--headed` | show the local Chrome window (implies `--local`) |
| `--profile DIR` | keep the local Chrome profile in `DIR`, so a site signed into there stays signed in (implies `--local`) |
| `--cloud-profile ID` | run on a Browser Use Cloud profile, signed in as whoever set it up |
| `--cdp-url URL` | drive a browser already running at this `ws://` or `wss://` DevTools URL instead of starting one |
| `--cdp-port PORT` | the same, for a browser or Electron app listening on `127.0.0.1:PORT`; the URL is read from its `/json/version` |
| `--attach` | with `--cdp-url` or `--cdp-port`, drive a window already open instead of opening a tab, and leave it open after the run |
| `--target-match TEXT` | attach to the first window whose title or URL contains `TEXT` (implies `--attach`) |
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

A DevTools port gives any program on the machine full control of the app, including its signed-in sessions.
Close the app, or restart it without the flag, when you are done.

### Models

The LLM defaults to `google/gemini-3.8-flash` at low reasoning effort, with
`google/gemini-3.5-flash-lite` for planning, proposing a direct address and typing field text.
Override with `FASTBROWSE_LLM_MODEL` (every purpose), `FASTBROWSE_LLM_MODEL_<PURPOSE>` (`PLAN`,
`READ`, `FIELD_TEXT`, `SHORTCUT`, `RECOVER`, `COMPOSE`, `VERIFY`) and `FASTBROWSE_LLM_REASONING`
(`low`, `medium`, `high`).

### Results

The exit code identifies the run status. Configuration refusals exit 1; invalid command syntax exits 2.
Programs that only check zero versus nonzero continue to work.

| Status | Exit | Meaning |
|:--|--:|:--|
| `complete` | 0 | every information requirement is backed by a quote, and every action is confirmed on the page |
| `unverified` | 10 | it believes it finished but could not back every claim |
| `needs_confirmation` | 3 | stopped before an irreversible action; re-run with `--authorize` |
| `needs_login` | 4 | a sign-in wall that no `--secret` covers |
| `blocked` | 6 | a bot check (a CAPTCHA) that did not clear; not a sign-in, so no secret passes it |
| `needs_input` | 5 | a required value or file is missing, or an upload exceeds the configured size limit |
| `stuck` | 9 | recovery ran out without reaching a page state the run had not seen |
| `budget_exceeded` | 7 | a step, call, time or dollar limit was reached |
| `observation_limit` | 11 | the page or required evidence cannot fit the configured prompt budget |
| `unavailable` | 8 | a model or browser provider stayed unavailable through every retry; the same run later may pass |
| `error` | 1 | a model or browser failure |

A `budget_exceeded` JSON result includes `budget.resource` (`steps`, `seconds`, `dollars`, `jev_calls` or
`llm_calls`) and `budget.limit`. Other results have `budget: null`. The embedding and MCP APIs carry the same
optional object, so callers can identify the exhausted limit without parsing an error message.

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/architecture-dark.svg">
  <img src="assets/architecture.svg" alt="The task is planned and the start page opened in parallel; each step indexes the page, Jev picks an operation and target, code gates it and acts; reads keep verbatim quotes, and the answer cites every claim.">
</picture>

The LLM plans, reads and writes. Code owns the gates: irreversible actions stop without
`--authorize`, secrets reach models by name only, and supported cookie banners are refused through autoconsent.
More in [docs/design.md](docs/design.md).

## How it compares

| | Browser Use agent (hosted) | [Browser Use Ultrafast](https://github.com/browser-use/jev-ultrafast) (`jev-ultrafast`) | fastbrowse |
|:--|:--|:--|:--|
| Choosing an action | hosted model and tools | Jev picks from indexed controls | Jev picks from indexed controls |
| Returns | an answer | `DONE` or `BLOCKED` | an answer with quotes, or why it stopped |
| Cited answers | answer output; not graded for citations here | no cited-answer result API | answers backed by captured quotes |
| Signing in | yes | password fields excluded | `--secret` or a Bitwarden vault item; models see names only |
| Irreversible actions | explicit task instructions in our fixture tests | no authorization parameter in Agent | classifier plus a code-enforced authorization gate |
| Browser | cloud | Chrome through Browser Harness; cloud in these evals | cloud by default, or local Chrome with `--local` |

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
`--attach` and `--target-match` do. Pass `browser_api_key=` to start a cloud browser;
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

Jev uses direct TypeSafe when keyed, otherwise OpenRouter, with Vercel AI Gateway also supported.
See [Jev routing](docs/jev.md#provider-failover) for key precedence, overrides and failover.
Any other source
can be passed as `run_task(jev=...)`, implementing async `evaluate(state, questions)`; `run_task(llm=...)`
accepts an implementation of the `LLMClient.generate(...)` protocol in `fastbrowse.llm`.

## Use it from JavaScript

`npm install fastbrowse` gives a Node program the same agent, with no Python and no uv on the machine. The
package is a TypeScript SDK with no dependencies. With it npm installs one of five packages that hold the agent
as a native binary, the one for your platform: `@fastbrowse/darwin-arm64`, `darwin-x64`, `linux-arm64`,
`linux-x64` or `win32-x64`. No install script runs and nothing is downloaded on first use, so it installs under
pnpm and bun with scripts blocked, and it starts offline. It needs Node.js 20 or newer. The npm packages carry
the PyPI package's version and are published by the same tag, starting with the release after 0.5.18.

The binary brings no browser. It finds local Chrome, starts a Browser Use Cloud browser, or drives one already
running, as the command line does. It reads the same keys from the environment of your process
(`OPENROUTER_API_KEY`, and `BROWSER_USE_API_KEY` for a cloud browser); `env` in `Fastbrowse.start` adds to them.

```sh
npm install fastbrowse zod
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
Its fields keep their Python names, such as `final_url`, since the types are generated from the Python models;
the options are camelCase. The structured data is in `data`, as in Python. The SDK adds
`output`: with a Zod (4.2 or newer) or ArkType schema, or a Valibot schema wrapped by
`@valibot/to-json-schema`, `output` is `data` after the schema's own validation and transforms, and has the
schema's output type. Data the schema refuses rejects with `OutputValidationError`, which carries the result.
A JSON Schema object is accepted too; `output` is then `data`, typed `unknown`.

What a run can fill today is narrower than what a schema can say. The server accepts nested objects, arrays,
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
and anything but `true` keeps the run from `complete`. `onFrame` receives JPEG frames of the active tab;
without it no frame is sent. A callback that throws ends the run with status `error`. The other options are
the command line's: `inputs`, `attachments` as bytes, `downloads`, `record`, and the browser choices
`chrome`, `cloudProfile`, `cdpUrl`, `cdpPort`, `attach`, `targetMatch` and `proxyCountry`, which
`Fastbrowse.start` also takes as defaults for every run. A run passes `null` for one of them to go without
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
claude mcp add fastbrowse -e OPENROUTER_API_KEY=... -e BROWSER_USE_API_KEY=... \
  -- uvx --from 'fastbrowse[mcp]' fastbrowse-mcp --max-dollars 0.25
```

For a client configured by JSON, such as Claude Desktop:

```json
{
  "mcpServers": {
    "fastbrowse": {
      "command": "uvx",
      "args": ["--from", "fastbrowse[mcp]", "fastbrowse-mcp"],
      "env": { "OPENROUTER_API_KEY": "...", "BROWSER_USE_API_KEY": "..." }
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
