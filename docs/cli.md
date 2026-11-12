# Command-line reference

Every flag, browser option, model setting and exit status of the `fastbrowse` command. The examples assume
`uv tool install fastbrowse`; in a checkout, prefix them with `uv run`. The
[README](../README.md#try-it) has the first run; [troubleshooting](troubleshooting.md) takes each status
from cause to next step.

A task must contain non-whitespace text. Empty tasks are refused before model or browser setup.

## Flags

Steps go to stderr; the status, cost, step count, why a run stopped short, and the answer go to stdout. `--json`
prints the full result: every step, the quotes behind the answer, and cost by component.

| Flag | Effect |
|:--|:--|
| `--start URL` | the first page; worked out from the task when omitted |
| `--context TEXT` | background the run may use but never treats as the task |
| `--extension` | run in your own Chrome, signed in as you are, through the fastbrowse extension; this is the default for a browser task, see [Your own Chrome](#your-own-chrome) |
| `--local` | an alias for `--extension` |
| `--extension-browser LABEL` | with the extension connected in more than one browser, use the one its popup labels `LABEL` |
| `--cloud` | run in a Browser Use Cloud browser instead, which needs `BROWSER_USE_API_KEY` |
| `--cloud-profile ID` | with `--cloud`, run on a signed-in Browser Use Cloud profile |
| `--fetch` | read pages over HTTP with no browser, for a task that only reads |
| `--desktop APP` | start a Linux desktop app and drive it instead of a browser, through [deskpilot](https://github.com/cjber/deskpilot); see [Desktop apps](#desktop-apps) |
| `--browser` | start a browser from the first step, rather than reading over HTTP until the task needs one |
| `--viewport WIDTHxHEIGHT` | with `--cloud`, the screen size requested from the cloud Chrome; does not emulate mobile, touch, device scale or user agent. Refused without `--cloud` |
| `--attach` | with the extension, drive the window you have open instead of a tab of its own |
| `--target-match TEXT` | with the extension, attach to the first window whose title or URL contains `TEXT` (implies `--attach`) |
| `--foreground` | bring the run's tab to the front instead of working behind the current one |
| `--proxy-country CC` | with `--cloud`, browse from that country (`uk`, `de`, ...; default `us`); starts the cloud browser before reading |
| `--authorize` | allow submit, pay, delete and send; otherwise the run stops at `needs_confirmation`. It does not override a task that says to change nothing or not to download |
| `--secret NAME=ENV_VAR[@ORIGIN]` | let the agent type `$ENV_VAR` on `ORIGIN` (`https://*.site.com` covers its hosts), or on the `--start` origin; models see `NAME` only |
| `--bitwarden ITEM` | use a vault login on the origin its saved URIs match, including its one-time code |
| `--max-steps N`, `--max-dollars N` | bound steps and model and web-search spend; unlimited by default |
| `--downloads DIR` | keep downloaded files |
| `--json` | print the full result |
| `--trace FILE` | append each model call with its full request and reply, read and check, and with `--extension` every message to and from the extension (image data left out), to `FILE` as JSON lines (`-` for stderr); `FASTBROWSE_TRACE` does the same for `serve` and MCP. The requests carry page text, so treat a trace as you would the pages |
| `--record FILE` | save a captioned MP4 of the run (needs `ffmpeg`); it shows what the pages showed, so watch it before sharing |
| `--recipe FILE` | replay a [recipe](design.md#recipes): offer Jev each saved action alone where its page and control match, and reuse saved values |
| `--input NAME=VALUE` | a named value the task refers to ("the person named in input person"), typed into a field by name; a recipe saved with it types the next run's value (repeatable) |
| `--file PATH` | a local file the run may attach to a file input on the page, offered under its file name, so a task can say "upload shrike_lid.stl"; the file is read when the run starts and sent to the page as bytes, also to a browser on another machine (repeatable) |
| `--save-recipe FILE` | write the run's steps as a recipe when it ends `complete`; naming the `--recipe` file repairs it after steps the agent had to decide |
| `--no-cursor` | don't draw the agent's cursor. By default it is drawn over a Chrome window that shows the run (`--foreground`, or an attached front tab) with fastbrowse's focus marker through [Cua Driver](https://github.com/trycua/cua) 0.45.4 or later on X11, or with [deskpilot](https://github.com/cjber/deskpilot) on Hyprland, and skipped anywhere else |
| `--cursor` | also bring the window to the front, including with `--extension`, so the live cursor can be drawn |

On X11, fastbrowse installs its own versioned Cua theme on first use and keeps the live marker visible between
steps. To keep it upright and still at rest, run these with Cua Driver's daemon running. They apply to every Cua
cursor:

```sh
cua-driver config set cursor.rotate off
cua-driver config set cursor.float off
```

```sh
export SAUCE_PASSWORD=secret_sauce
fastbrowse "Log in as standard_user with the saved password and add the backpack to the cart." \
  --start https://www.saucedemo.com/ --secret password=SAUCE_PASSWORD --authorize
```

The CLI refuses `--headed`, `--profile`, `--cdp-url` and `--cdp-port`. A profile
of its own, a headless Chrome or a DevTools endpoint is the Python, Node and Go SDKs' job: see
[A profile of its own](#a-profile-of-its-own) and [Open windows and Electron apps](#open-windows-and-electron-apps).

## Signed-in sites

### Your own Chrome

A browser task runs in the Chrome you already use, where you are already signed in. `--extension` is the default
and `--local` is an alias for it. Pass `--cloud` for a Browser Use Cloud browser, `--fetch` to read over HTTP, or
`--desktop` for a desktop app. Start tasks from your terminal; the extension connects fastbrowse to Chrome and
shows the runs you can stop.

You need Chrome, [uv](https://docs.astral.sh/uv/) and an [OpenRouter](https://openrouter.ai/) API key.
Model calls are billed by your provider. You do not need a Browser Use Cloud key for this mode.

1. Install fastbrowse and configure your model key:

   ```sh
   uv tool install fastbrowse
   export OPENROUTER_API_KEY=...  # replace with your key
   ```

2. Run `fastbrowse extension install`. It registers the installed program with the Chromium browsers it finds
   for your user. Chrome, Chromium, Edge and Brave are supported on Linux, macOS and Windows.
3. [Add fastbrowse to Chrome](https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop).
   Open it from Chrome's Extensions menu to see runs and their **Stop** buttons.
4. Run a task that reads a public page:

   ```sh
   fastbrowse "What is the title of the top story?" --start https://news.ycombinator.com/ --extension --browser
   ```

A run that reaches the page confirms the connection. If it ends `unavailable` with no browser connected,
restart Chrome or turn the extension off and on, then [check the connection](troubleshooting.md#the-extension-is-not-connected).
The terminal prints the answer and status. Only `complete` means the whole task was verified.
To read a signed-in site, use its address and describe what you need:

```sh
fastbrowse "Find my last order's delivery date. Do not change anything." \
  --start https://www.amazon.com/ --extension
```

`--attach` drives the window you have open instead of a tab of its own, and `--target-match TEXT` picks the window
whose title or URL contains `TEXT`.

Each run works in a tab group of its own, named after the task, and closes its tabs when it ends. A run that stops at
`needs_confirmation`, `needs_input` or `needs_login` leaves the tab it stopped in open, out of the group, so you can
finish the step in the page it filled. While a run is attached, Chrome shows a bar saying an extension started
debugging the browser. Each run has three ways to stop it, and each ends the run `cancelled` with an error that says
which one stopped it: **Stop** in the extension's popup, **Cancel** on that bar (which stops every run in that
browser), or closing all of the run's tabs.

The extension relays page-level commands, including reading page content and carrying out clicks and typing.
It cannot read cookies, saved passwords or other sites' storage through browser APIs,
and fastbrowse accepts connections only from the extension's own ID, over a socket only your user can open. The
extension sends that content to the local fastbrowse program, which can send it to the model providers you
configure. The run's [authorization](#flags) rules still apply: actions classified as irreversible wait for
`--authorize`. Classification can miss a change, so state read-only constraints in your task and check important
actions yourself.

A run with `--file` needs an extension that can intercept the browser's file chooser. If the run reports that
interception failed, update the extension. For an unpacked extension, run `fastbrowse extension path` with the
current CLI and click **Reload** on the extension's card in `chrome://extensions`.

With no browser connected, the run ends `unavailable` before it spends anything; it never falls back to another
browser. It also ends `unavailable` when the extension is older than fastbrowse and cannot relay a command fastbrowse
sends, naming the commands: update the extension from the Chrome Web Store. With the extension in more than one browser, label each one in its popup and pick one with
`--extension-browser LABEL`. `fastbrowse extension install --policy` (Linux) prints a Chrome policy for you to write
as root, which installs the extension for every profile with no debugging bar and marks Chrome as managed by your
organization;
`fastbrowse extension uninstall` removes what `install` wrote; remove the extension separately in Chrome.

For extension development, `fastbrowse extension path` writes an unpacked copy. Open `chrome://extensions`,
turn on Developer mode, choose **Load unpacked** and select that folder. Use the store version for ordinary runs.

**Other extensions can take the run's tab away.** Chrome lets no extension debug a tab while the page holds a frame
from another extension, and some extensions add one to every page: screen recorders, password managers and writing
assistants are common ones. When that happens the run ends `unavailable` and names the extension. Open
`chrome://extensions`, choose **Details** on that extension, and set **Site access** to **On click**: it then adds
its frame only where you click it, and runs work again. See
[Another extension's frame takes the tab away](troubleshooting.md#another-extensions-frame-takes-the-tab-away).

**Other extensions also see the run's pages.** A run's tabs are ordinary tabs, so every extension with site access
reads and can change them as it does any other tab, signed-in pages included: a coupon finder is sent the address,
an ad blocker or a translation overlay changes what the run reads. For sensitive work, or runs you want to
reproduce, use a separate Chrome profile with only fastbrowse installed.

### A profile of its own

The extension uses the profile of the Chrome it is installed in, so sign in there. For a kept profile apart from
your everyday Chrome, use an SDK: `chrome=LocalChrome(profile=..., headed=True)` in Python (see
[Embed it](../README.md#embed-it)) keeps the sign-in between runs; the CLI refuses `--profile` and `--headed`.

On a cloud browser, `--cloud --cloud-profile prof_1234` runs as a [Browser Use Cloud](https://cloud.browser-use.com)
profile someone signed in once. The run inherits its cookies and no model sees a credential.

`--bitwarden ITEM` types a Bitwarden vault login. The item's saved URIs must match `--start`. Models see only
`username`, `password` and, when the item holds an authenticator key, `one_time_code`, computed as it is typed.
With [rbw](https://github.com/doy/rbw) installed, fastbrowse reads the item through it, and a locked vault asks
for your password through rbw's pinentry. rbw reads its local copy of the vault, so run `rbw sync` after adding
an item, and pass the item's id when its name matches more than one entry:

```sh
fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." \
  --start https://www.amazon.com/ --bitwarden Amazon
```

Without rbw, fastbrowse reads the item through the [Bitwarden CLI](https://bitwarden.com/help/cli/), unlocked
first:

```sh
export BW_SESSION="$(bw unlock --raw)"
fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." \
  --start https://www.amazon.com/ --bitwarden Amazon
```

To change a password, add the replacement as an origin-scoped secret named `new_password`, refer to it in the
task, and pass `--authorize`. fastbrowse never generates a password or reuses the old one. A single field marked
`autocomplete=new-password` can also appear on a sign-in page. Jev uses the task and form context to choose its
role; when the role is unclear, the run stops at `needs_input`.

## Open windows and Electron apps

The CLI drives a window you have open through the extension: `--attach` takes over the first window and
`--target-match TEXT` picks one by title or URL, and neither opens a DevTools port. Without `--start` the run begins
on whatever the window shows, and the window stays open afterwards. Popups the window opens join the run.

An Electron app, or a browser you start with `--remote-debugging-port`, has no extension, so drive it from an SDK
with `cdp_url=` or `cdp_port=`, `attach=True` and `target_match=` (see [Embed it](../README.md#embed-it)). The CLI
refuses `--cdp-url` and `--cdp-port`. A DevTools port gives any program on the machine full control of the app and
its sessions: close the app, or restart it without the flag, when you are done.

## Desktop apps

`--desktop APP` starts a native Wayland app on a Hyprland desktop and runs the task in its window, through
[deskpilot](https://github.com/cjber/deskpilot). `APP` is the command line that starts the app, split into words as a
POSIX shell splits them, with no shell running it. The run starts it with deskpilot's `launch` as a deskpilot agent of
its own, so the window opens on that agent's hidden workspace, and the app closes when the run ends. A window that is
already open is not driven: deskpilot sends input in the background only to an app its agent launched. It needs a
deskpilot with `launch` ([cjber/deskpilot#86](https://github.com/cjber/deskpilot/pull/86)); with an older one the run
ends `unavailable`. Jev picks each action from the window's accessibility tree, and deskpilot clicks and types in the
background, so the pointer and keyboard focus stay where you left them:

```sh
fastbrowse --desktop gnome-text-editor "Write 'hello' in a new document."
```

An app that runs as one instance, such as one you already have open, may hand its new window to that copy, so
`launch` sees no window of its own and the run ends with deskpilot's error. Pass the app's flag for a separate
instance when it has one.

A password field is never read, and `--secret NAME=ENV_VAR@desktop://<class>` types a secret only into the window of
that app class. `--authorize` and the step and spend ceilings work as for a browser. An open combo box's list and a
popup menu's entries are controls like any other, and the run clicks the item it wants. Choosing a combo box's option
by name uses the keyboard: a list the click opened is closed again and the arrow keys move the choice. The run stays in the one
window: a dialog the app opens as a window of its own is out of its reach. Recipes, `--start` and every browser
option are refused. Set `FASTBROWSE_DESKPILOT` to run a `deskpilot` that is not on `PATH`.

## Models

The LLM defaults to `openai/gpt-6-luna` at low reasoning, for proposing a first address when no
`start` is given and for writing an answer that must be composed from more than one fact.

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
| `complete` | 0 | Jev judged the task done |
| `unverified` | 10 | Jev finished, but the requested data could not be filled from the page or the site refused a requested step |
| `needs_confirmation` | 3 | stopped before an irreversible action; re-run with `--authorize` |
| `needs_login` | 4 | a sign-in wall that no `--secret` covers |
| `blocked` | 6 | a bot check (a CAPTCHA) that did not clear; not a sign-in, so no secret passes it |
| `needs_input` | 5 | a required value or file is missing, or an upload exceeds the configured size limit |
| `stuck` | 9 | the stall budget ran out without reaching a page state the run had not seen |
| `budget_exceeded` | 7 | a resource limit was reached, or a missing cost prevents enforcing the dollar cap |
| `observation_limit` | 11 | the page or required evidence cannot fit the configured prompt budget |
| `unavailable` | 8 | a model or browser provider stayed unavailable through every retry, or the page says an item the task requires cannot be bought; the same run later may pass |
| `cancelled` | 12 | you stopped an `--extension` run in your browser: from its popup, the debugging bar or by closing its tabs |
| `error` | 1 | a model or browser failure |

The JSON result says why a run stopped where a caller can act on it:

- `budget` names the limit a `budget_exceeded` run hit (`steps`, `seconds`, `dollars`, `jev_calls` or
  `llm_calls`) and its value.
- `browser` says which browser the run drove: `via` is `cloud`, `launched`, `attached` or `extension`, with the
  `profile` and the `browser` product and version where known. A run that read every page over HTTP has none.
- `wall` describes a `needs_login` or `blocked` page: its `kind` (`password`, `one_time_code`, `sign_in` or
  `bot_check`), `origin`, the `fields` it asked for and `seen_at_ms`. A caller that can read the user's email or
  texts can answer a `one_time_code` wall with a secret scoped to that origin and run again.
- A run that stops before completion can retain a cited partial answer from checked claims while their
  source evidence remains unchanged. A `stuck` or `unverified` run can also compose an answer from evidence
  it read. Partial answers may start with `Partial evidence only.`, and one from a run that left a step of its
  task undone starts by saying so; the status determines whether the task completed.

A cost a provider omits is looked up from its receipt, or bounded from OpenRouter's listed prices. A cost that
cannot be bounded is unknown, and a run with a dollar cap then stops with `budget_exceeded`. A page that stops
answering (15 seconds for an input event, 120 for any other command) is reopened once during an action; a second
hang returns `unavailable`. Step and dollar limits do not bound elapsed time:
`Limits(max_seconds=...)` does.

A run's `proxy` cost line on a Browser Use Cloud browser has the basis `unsettled`: the service meters proxy data
about 30 seconds after a browser stops, and a run returns without waiting, so the figure can read low. An unsettled
line makes the total incomplete, even when it carries a provisional dollar figure. The
`browser_id` in the run's browser event reads the settled figure from `GET /browsers/{id}` once that time has passed.

## Updating

`fastbrowse self-update` prints how this copy was installed and the command or link that upgrades it, then exits 0.
It reads what the installer recorded and makes no network call, and it never replaces the running binary.

| Installed with | It prints |
| --- | --- |
| `uv tool install fastbrowse` | `uv tool upgrade fastbrowse` |
| `uvx fastbrowse` | `uvx fastbrowse@latest` |
| `uv add` or `uv pip install` | `uv lock --upgrade-package fastbrowse && uv sync`, or `uv pip install --upgrade fastbrowse` |
| `pip install fastbrowse` | `python -m pip install --upgrade fastbrowse` |
| `npm install fastbrowse` | `npm install fastbrowse@latest` |
| A release binary | the latest release link and this platform's `fastbrowse-rust-<target>.tar.gz` |
| A checkout | `git pull && uv sync --all-extras` |

Another installer, or one that left no record, gets the [PyPI page](https://pypi.org/project/fastbrowse/).

## Troubleshooting

[Troubleshooting](troubleshooting.md) covers each status: what it means, common causes and the next step.
