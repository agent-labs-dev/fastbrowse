# Command-line reference

Every flag, browser option, model setting and exit status of the `fastbrowse` command. The examples assume
`uv tool install fastbrowse`; in a checkout, prefix them with `uv run`. The
[README](../README.md#try-it) has the first run; [troubleshooting](troubleshooting.md) takes each status
from cause to next step.

## Flags

Steps go to stderr; the status, cost, step count, why a run stopped short, and the answer go to stdout. `--json`
prints the full result: every step, the quotes behind the answer, and cost by component.

| Flag | Effect |
|:--|:--|
| `--start URL` | the first page; worked out from the task when omitted |
| `--context TEXT` | background the run may use but never treats as the task |
| `--local` | local Chrome instead of a cloud browser |
| `--cloud` | force the cloud browser even when `FASTBROWSE_PROFILE` or `FASTBROWSE_HEADED` is set |
| `--headed` | show the local Chrome window (implies `--local`) |
| `--profile DIR` | keep the local Chrome profile in `DIR`, so sign-ins persist (implies `--local`) |
| `--cloud-profile ID` | run on a signed-in Browser Use Cloud profile |
| `--fetch` | read pages over HTTP with no browser, for a task that only reads |
| `--fetch-first` | read over HTTP, and start a browser only if the task needs one |
| `--cdp-url URL`, `--cdp-port PORT` | drive a browser already running, at a `ws://` URL or on `127.0.0.1:PORT` |
| `--attach` | with `--cdp-url` or `--cdp-port`, drive a window already open instead of opening a tab |
| `--target-match TEXT` | attach to the first window whose title or URL contains `TEXT` (implies `--attach`) |
| `--foreground` | bring the run's tab to the front instead of working behind the current one |
| `--proxy-country CC` | browse from that country (`uk`, `de`, ...; default `us`) |
| `--authorize` | allow submit, pay, delete and send; otherwise the run stops at `needs_confirmation` |
| `--secret NAME=ENV_VAR[@ORIGIN]` | let the agent type `$ENV_VAR` on `ORIGIN` (`https://*.site.com` covers its hosts), or on the `--start` origin; models see `NAME` only |
| `--bitwarden ITEM` | use a vault login on the origin its saved URIs match, including its one-time code |
| `--max-steps N`, `--max-dollars N` | bound steps and model spend; unlimited by default |
| `--downloads DIR` | keep downloaded files |
| `--json` | print the full result |
| `--trace FILE` | append each model call, read and check to `FILE` as JSON lines (`-` for stderr); `FASTBROWSE_TRACE` does the same for `serve` and MCP |
| `--record FILE` | save a captioned MP4 of the run (needs `ffmpeg`); it shows what the pages showed, so watch it before sharing |
| `--no-cursor` | don't draw the agent's cursor. By default it is drawn with [Cua Driver](https://github.com/trycua/cua) over a Chrome window that shows the run (`--foreground`, or an attached front tab; needs `cua-driver`, and X11 on Linux), and skipped anywhere else |
| `--cursor` | also bring the window to the front, so the cursor is always drawn |

```sh
export SAUCE_PASSWORD=secret_sauce
fastbrowse "Log in as standard_user with the saved password and add the backpack to the cart." \
  --start https://www.saucedemo.com/ --secret password=SAUCE_PASSWORD --authorize
```

## Signed-in sites

Sign in once by hand in a profile of its own, then point runs at it. `--headed` is only needed for the first run:

```sh
google-chrome --user-data-dir="$HOME/.fastbrowse/amazon" https://www.amazon.com/   # sign in, then close Chrome
fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." \
  --start https://www.amazon.com/ --profile ~/.fastbrowse/amazon --headed
```

On a cloud browser, `--cloud-profile prof_1234` runs as a [Browser Use Cloud](https://cloud.browser-use.com)
profile someone signed in once. The run inherits its cookies and no model sees a credential.

With the [Bitwarden CLI](https://bitwarden.com/help/cli/) unlocked, `--bitwarden ITEM` types the vault login. The
item's saved URIs must match `--start`. Models see only `username`, `password` and, when the item holds an
authenticator key, `one_time_code`, computed as it is typed:

```sh
export BW_SESSION="$(bw unlock --raw)"
fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." \
  --start https://www.amazon.com/ --bitwarden Amazon --profile ~/.fastbrowse/amazon
```

To change a password, add the replacement as an origin-scoped secret named `new_password`, refer to it in the
task, and pass `--authorize`. fastbrowse never generates a password or reuses the old one.

## Open windows and Electron apps

Any Chromium app that exposes a DevTools port can be driven. Restart the app with `--remote-debugging-port` (a
running app ignores the flag), then attach to its window by title or URL:

```sh
open -a "Visual Studio Code" --args --remote-debugging-port=9222 ~/code/my-project   # elsewhere, pass the flag to the binary
fastbrowse "Open the Extensions view and report how many extensions are installed." \
  --cdp-port 9222 --target-match my-project
```

`--attach` takes over the first window, skipping DevTools, and `--target-match` picks one by title or URL.
Without `--start` the run begins on whatever the window shows, and the window stays open afterwards. Popups the
window opens join the run.

For a signed-in browser that stays open, start a Chrome with a data directory of its own (Chrome ignores the port
on its default one), sign in, and leave it running. Each run opens and closes a background tab in it:

```sh
google-chrome --remote-debugging-port=9222 --user-data-dir="$HOME/.fastbrowse/chrome"
fastbrowse "Report my open orders." --start https://www.amazon.com/ --cdp-port 9222
```

A run in a visible window works in a tab behind the current one and never raises the window itself. Chrome still
brings a window forward when it starts headed, when a page opens a popup, and, with `--attach`, on a click in the
window's front tab.

A DevTools port gives any program on the machine full control of the app and its sessions. Close the app, or
restart it without the flag, when you are done.

## Models

The LLM defaults to `google/gemini-3.8-flash` at low reasoning effort, with `google/gemini-3.5-flash-lite` for
proposing a direct address and typing field text.

| Setting | Effect |
|:--|:--|
| `FASTBROWSE_LLM_MODEL` | the model for every purpose |
| `FASTBROWSE_LLM_REASONING` | `none`, `low`, `medium` or `high`; `none` turns reasoning off for a model such as DeepSeek V4.1 Flash that otherwise reasons at every effort |
| `FASTBROWSE_LLM_PROVIDER` | `openrouter`, `gateway`, `openai`, `anthropic`, `gemini`, `groq`, `together`, `mistral`, `deepseek`, `xai`, `fireworks`, `azure`, `ollama`, `lmstudio`, `vllm` or `custom`; each reads its own key, such as `OPENAI_API_KEY`, and sends it to that provider alone |
| `FASTBROWSE_LLM_BASE_URL`, `FASTBROWSE_LLM_API_KEY` | any OpenAI-compatible endpoint, such as a local vLLM at `http://127.0.0.1:8000/v1`; it needs `FASTBROWSE_LLM_MODEL`, and never receives the OpenRouter or gateway key |
| `FASTBROWSE_LLM_STRICT=0`, `FASTBROWSE_LLM_EXTRA_BODY` | for that endpoint: send schemas in non-strict form, and add a JSON object to every request |

[LLM providers](llm-providers.md) lists what each preset sends and its default models. Jev still needs its
own key. An endpoint that reports no price leaves those calls' cost unknown, so `--max-dollars` stops the run
after the first one.

Jev, the decision model, has settings of its own:

| Setting | Effect |
|:--|:--|
| `FASTBROWSE_DECISION_SOURCE` | `typesafe`, `openrouter` or `gateway` picks Jev's first provider, or another decision host by name |
| `FASTBROWSE_DECISION_BASE_URL` | retargets the selected source, and turns off automatic failover |
| `FASTBROWSE_DECISION_MODEL` | pins the decision model, and turns off automatic failover |
| `FASTBROWSE_DECISION_API_KEY` | the key for a decision host that takes one |

[Jev routing](jev.md#provider-failover) gives the provider order and failover, and
[other decision hosts](jev.md#other-decision-hosts) lists each host with its key and models.

## Results

The exit code is the status. Only `complete` exits 0; configuration refusals exit 1 and bad syntax exits 2.

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

The JSON result says why a run stopped where a caller can act on it:

- `budget` names the limit a `budget_exceeded` run hit (`steps`, `seconds`, `dollars`, `jev_calls` or
  `llm_calls`) and its value.
- `wall` describes a `needs_login` or `blocked` page: its `kind` (`password`, `one_time_code`, `sign_in` or
  `bot_check`), `origin`, the `fields` it asked for and `seen_at_ms`. A caller that can read the user's email or
  texts can answer a `one_time_code` wall with a secret scoped to that origin and run again.
- A `stuck` or `unverified` run that read evidence still returns a cited answer, starting `Answer (unverified)`.

A cost a provider omits is looked up from its receipt, or bounded from OpenRouter's listed prices. A cost that
cannot be bounded is unknown, and a run with a dollar cap then stops with `budget_exceeded`. A browser command
unanswered for 120 seconds returns `unavailable`. Step and dollar limits do not bound elapsed time:
`Limits(max_seconds=...)` does.

## Troubleshooting

[Troubleshooting](troubleshooting.md) covers each status: what it means, common causes and the next step.
