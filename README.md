<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/wordmark-dark.svg">
  <img src="assets/wordmark.svg" alt="fastbrowse" width="360">
</picture>

**A browser agent that picks instead of generating.**

Jev chooses each action, an LLM plans and reads, and code owns verification, safety and secrets.

![python](https://img.shields.io/badge/python-3.14-475569?style=flat-square)
![license](https://img.shields.io/badge/license-MIT-475569?style=flat-square)
![status](https://img.shields.io/badge/status-pre--alpha-6366F1?style=flat-square)

</div>

---

## Why

Most browser agents generate each action from a screenshot. fastbrowse indexes the page into
candidates and has [Jev](https://typesafe.ai), a choice model, **pick one**, so it cannot click
something that was never on the page. Every claim in an answer cites a verbatim quote from the page.

Six live tasks, two passes each, against hosted Browser Use on the same day, graded against each
site's own API ([tasks and method](docs/evals.md)):

| | passed | correct answer | median time | mean time | cost per task |
|:--|:--|:--|:--|:--|:--|
| **fastbrowse** (cloud browser) | **12/12** | 12/12 | **12.9s** | **15.4s** | **$0.0072** |
| hosted Browser Use | 11/12 | 11/12 | 14.7s | 25.8s | $0.3767 |

Faster on median and mean at about a fiftieth of the cost; hosted is still 1 to 2.6s ahead on three
of the six tasks ([per task](docs/evals.md#results)). Twelve runs is a smoke test, not a benchmark:
single runs swing by 4 to 5 seconds.

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/architecture-dark.svg">
  <img src="assets/architecture.svg" alt="The task is planned and the start page opened in parallel; each step indexes the page, Jev picks an operation and target, code gates it and acts; reads keep verbatim quotes, and the answer cites every claim.">
</picture>

The LLM plans, reads and writes. Code owns the gates: irreversible actions stop without
`--authorize`, secrets reach models by name only, and cookie banners are refused before they paint.
More in [docs/design.md](docs/design.md).

## How it compares

| | hosted Browser Use | [jev-ultrafast](https://github.com/browser-use/jev-ultrafast) | fastbrowse |
|:--|:--|:--|:--|
| Choosing an action | LLM generates from a screenshot | Jev picks from indexed controls | Jev picks from indexed controls |
| Returns | an answer | `DONE` or `BLOCKED` | an answer with quotes, or why it stopped |
| Reads pages | yes | no | yes, every claim cited |
| Signing in | yes | password fields excluded | `--secret` or a Bitwarden vault item; models see names only |
| Irreversible actions | not gated | not gated | stop unless `--authorize` |
| Browser | cloud | local Chrome, your profile | local Chrome or cloud |

jev-ultrafast is Browser Use's navigation agent (a measured 7.1s Google Flights run); fastbrowse
shares its core techniques. Its column describes `main` as of 2026-09-18.

## Try it

Needs Python 3.14, [uv](https://docs.astral.sh/uv/), and Chrome (not needed with `--cloud`).

```sh
git clone https://github.com/agent-labs-dev/fastbrowse.git && cd fastbrowse
uv sync
cp .env.example .env            # add AI_GATEWAY_API_KEY or TYPESAFE_API_KEY, and OPENROUTER_API_KEY
uv run fastbrowse "What is the title of the top story right now?" --start https://news.ycombinator.com/
```

```
   0 read  -> executed
complete ($0.0071, 1 steps)
The top story on Hacker News is titled "...".
```

Steps go to stderr and the answer to stdout. `--json` prints every step, the quotes behind the
answer, and cost by component.

| Flag | Effect |
|:--|:--|
| `--start URL` | required: the page to open first |
| `--cloud` | use a [Browser Use Cloud](https://cloud.browser-use.com) browser (`BROWSER_USE_API_KEY`); far less likely to be bot-challenged. Prints a URL to watch it live |
| `--headed` | show the local Chrome window |
| `--profile DIR` | keep the local Chrome profile in `DIR`, so a site signed into there stays signed in |
| `--authorize` | allow submit, pay, delete and send; without it the run stops at `needs_confirmation` first |
| `--secret NAME=ENV_VAR` | let the agent type `$ENV_VAR` on the start origin; models only see `NAME` |
| `--bitwarden ITEM` | let the agent type that vault login's `username` and `password`, only where the item's saved URIs and their match detection allow |
| `--max-steps N`, `--max-dollars N` | bound the run |
| `--downloads DIR` | keep downloaded files |
| `--json` | full result instead of the answer |
| `--record FILE` | save an MP4 of the tab ending on the answer, time and cost (needs `ffmpeg`), e.g. `recordings/demo.mp4`, which git ignores. It shows what the pages showed, so watch it before sharing |

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

Or from your vault, with the [Bitwarden CLI](https://bitwarden.com/help/cli/) unlocked. Values are typed
only on a site the item's saved URIs cover, and models see only `username` and `password`:

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
| `needs_input` | a field needs a value you did not give, which is never invented |
| `stuck` | recovery ran out without the page moving |
| `budget_exceeded` | a step, call, time or dollar limit was reached |
| `observation_limit` | the page has more controls than Jev can take in |
| `error` | a model or browser failure |

## Embed it

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

```
complete {'package': 'httpx', 'version': '0.28.1'} $0.0114
  "httpx 0.28.1" from https://pypi.org/project/httpx/
  "pip install httpx" from https://pypi.org/project/httpx/
```

Jev comes from Typesafe directly or through the Vercel AI Gateway, whichever key is set
(`FASTBROWSE_JEV_SOURCE` picks when both are, `FASTBROWSE_JEV_BASE_URL` adds a proxy). Any other source
can be passed as `run_task(jev=...)`, an object with one `evaluate(state, questions)` method; the LLM
works the same way.

## Safety model

- **Irreversible actions.** Before any button or submit, Jev is asked whether it commits something
  that cannot be undone. Without `--authorize`, a yes stops the run. This is a classifier, not a
  guarantee: a page can word a harmful control to look harmless.
- **Secrets.** Models see secret names only. A value is resolved at the moment of typing, only for
  its declared origin, and redacted from everything the run returns. A password field is typed only
  from a stored secret, never generated.
- **Page content is data.** Every prompt says so, and completion is judged against quotes and page
  state rather than the model's say-so.

## Evals and development

```sh
uv sync --all-extras                                         # the hosted-arm SDK too, which pyright checks
uv run python -m fastbrowse.evals.runner                     # local fixtures, under half a cent a task
uv run --extra browser-use python -m fastbrowse.evals.live   # live head-to-head; --arms fast skips hosted
uv run ruff format . && uv run ruff check . && uv run pyright && uv run pytest
uv run python scripts/no_slop.py && uv run vale sync && uv run vale README.md docs src scripts tests
```

Grades come only from things the agent cannot write: requests the fixture server recorded, truth
from a site's own API, or the URL the browser ended on. See [docs/evals.md](docs/evals.md),
[docs/design.md](docs/design.md), and [docs/jev.md](docs/jev.md) for every Jev assumption checked against
Typesafe's documentation.

## License

MIT, copyright Agent Labs. Adapted third-party code is credited in
[NOTICE](NOTICE).
