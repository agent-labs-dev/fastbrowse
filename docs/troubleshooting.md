# Troubleshooting

A run that does not end `complete` says why in three places: its status, its exit code and its `error` text.
This page covers each status you are likely to meet, in exit-code order: what it means, the common causes and
what to do next. The full table is in [Results](cli.md#results).

| Status | Exit | In short |
|:--|:--|:--|
| [error](#error-exit-1) | 1 | a failure retries do not cover, or a refused option |
| [needs_confirmation](#needs_confirmation-exit-3) | 3 | an irreversible action needs your authorization |
| [needs_login](#needs_login-exit-4) | 4 | a sign-in wall that no secret covers |
| [needs_input](#needs_input-exit-5) | 5 | a value or file the task did not give |
| [blocked](#blocked-exit-6) | 6 | a bot check or a refusal |
| [budget_exceeded](#budget_exceeded-exit-7) | 7 | a limit you set was reached |
| [unavailable](#unavailable-exit-8) | 8 | a model or browser provider stayed down |
| [stuck](#stuck-exit-9) | 9 | Jev found no way forward |
| [unverified](#unverified-exit-10) | 10 | the requested data could not be filled from the page, or the site refused a requested step |
| [observation_limit](#observation_limit-exit-11) | 11 | the page does not fit the prompt budget |
| [cancelled](#cancelled-exit-12) | 12 | you stopped an `--extension` run in your browser |

## Reading a stopped run

The CLI prints the status, cost and step count, then the error and anything the run read before it stopped.
`--json` prints the whole result, including every step and the quotes behind the answer.

A run that stops `stuck`, `unverified`, `budget_exceeded`, `observation_limit`, `unavailable` or `error` after
reading something still returns those cited quotes, so check `evidence` before you browse again. A `stuck` or
`unverified` run can also write an answer from them when the task asks for one. Other stops can retain an already
checked partial answer while its source evidence remains unchanged, including a sign-in wall or pending
authorization. Preserve any returned answer as partial. Some partial answers start with `Partial evidence only.`;
use the status to determine whether the task completed.

The causes below name Jev, the model that picks each action from the controls on the page; the LLM
plans, reads and writes. See [Models](cli.md#models).

## error (exit 1)

**What it means.** A model or browser failure that is not a provider outage, or a run that never started.
Configuration refusals also exit 1.

**Common causes.**

- A model or browser call failed in a way retries do not cover. `error` holds the whole failure, with any
  secret redacted.
- The browser could not be reached as configured, for example `no DevTools endpoint at
  http://127.0.0.1:<port>/json/version (<cause>)` from `--cdp-port`, or a pair of browser options that
  cannot go together, such as `cdp_url and cdp_port both name a browser to attach to; pass one`.
- A callback in the embedding API or the JavaScript SDK threw, or `check_access` raised.
- The CLI refused its options before starting, printing `fastbrowse: <reason>`, such as
  `--secret names unset variables: <names>` or
  `--bitwarden needs --start: the vault item is matched against its origin`. With `--json` it prints a result
  with status `error` and that reason.

**Next step.**

- Read `error`.
- For a refused option or browser choice, fix the command and run again.
- For a model or browser failure, run again. If it repeats, open an issue with the task, the start page and the
  `--json` step trace, as [CONTRIBUTING.md](../CONTRIBUTING.md) describes.

## needs_confirmation (exit 3)

**What it means.** The run stopped before an action Jev judged irreversible, such as submit, pay, delete or
send, because the run was not authorized to take it, or was authorized but Jev judged the task did not ask
for it. That action was not taken.

**Common causes.** The task leads to an irreversible action, and the run had no authorization. The error is
`<operation> '<control>' needs confirmation`, and the refusal appears as a failed step.

**Next step.** Read the step the run stopped on, since the irreversible check can be wrong. If the action is
what you want, run again with authorization:

- the CLI: `--authorize`;
- the embedding API: `authorization=Authorization(irreversible_actions=True)`;
- MCP: pass `authorize` on the call. The server must be started with `--allow-authorize`, or every such
  action stops here; ask the user to take the step, or to restart the server with that flag.

## needs_login (exit 4)

**What it means.** The run met a sign-in wall that no secret covers.

**Common causes.**

- Jev judged a fresh page to be a sign-in wall, and no secret is declared for that page's origin. The error is
  `sign-in required at <origin>`. It names the fields the page asks for, and `a security key or passkey`
  when the page marks a passkey field (`autocomplete="webauthn"`).
- The run reached a field that takes a secret, such as a password, and none is stored. The error is
  `'<field>' wants a secret and none is stored`.
- The site answered HTTP 401 and Jev found no way on. The error is
  `Could not complete the task after HTTP 401 at <url> in this browser session; the cause is unknown.`
- A secret is declared, but for a different origin. A field is offered only the secrets declared for the
  origin of the frame it sits in, so this reads the same as no secret.
- A secret is declared for the origin but resolves to nothing, such as a password manager item that changed
  during the run. The error is `sign-in required at <origin>: secret <name> is not available there`.

The CLI adds what to pass: `Pass --secret NAME=ENV_VAR@https://host with the password in that environment
variable; credentials in task text are not used.` The result's `wall` field says what the wall asked for, so a
caller can route it without parsing the error; see [Results](cli.md#results).

**Next step.** Give the run a way in, then run it again:

- `--secret NAME=ENV_VAR@https://host`, with the value in that environment variable. Without `@ORIGIN` the
  secret is scoped to the `--start` origin. Check that origin against the address the form is on.
- `--bitwarden ITEM`, when the vault item's saved URIs match `--start`.
- `--extension`, to run in your own Chrome where you are already signed in. See
  [Your own Chrome](cli.md#your-own-chrome).
- `--profile DIR` for local Chrome you signed into once by hand, or `--cloud-profile ID` for a Browser Use
  Cloud profile. See [Signed-in sites](cli.md#signed-in-sites).
- Over MCP, the server's operator adds the secret with `--secret` or `--bitwarden`, or signs in once in the
  server's `--profile`.

Once a secret covers an origin, its sign-in wall is work for the run and no longer stops it.

## needs_input (exit 5)

**What it means.** The run needs a value or a file that the task did not give, or an upload is over the size
limit.

**Common causes.**

- The run opened no page: `the task names no page to start from, and none was given`.
- A field needs a value the task never states. The run first tries to go on without it, then stops with
  `'<field>' needs a value the task does not give`, followed by the reason it gave up. A field the task asks
  nothing of is left as it is, and is named here only when the page marks it required.
- The page asks for a file and the run has none: `the page asks for a file and none was provided`.
- A file is larger than `Config.max_upload_bytes` (25 MiB by default): `<name> exceeds the upload size limit`.
- A password change without a replacement secret: `A replacement password requires --secret
  new_password=ENV_VAR@https://host; set ENV_VAR to the new value instead of putting it in the task.`
- A field that takes no credential chose a secret declared for the origin that resolves to nothing, such as a
  password manager item that changed during the run: `secret <name> is not available for <origin>`. A sign-in
  field ends `needs_login` instead, see [needs_login](#needs_login-exit-4).
- A drag with nowhere to drop: `the page offers no drop target for the drag`.
- An address the run would open contains a resolved secret: `navigation address contains a resolved secret`.

**Next step.** Supply what the error names, then run again:

- Give `--start`, or name the page in the task.
- Put missing text in the task. The embedding API and the JavaScript SDK also take `inputs` for exact field
  text.
- Pass files as `attachments` in the embedding API or the JavaScript SDK. The CLI and the MCP tool cannot
  supply files.
- Supply a new password as `--secret new_password=ENV_VAR@https://host`, and fix the origin of a secret
  scoped to the wrong one.

## blocked (exit 6)

**What it means.** The site stopped the run with a bot check, or refused it.

**Common causes.**

- A bot check, such as a CAPTCHA, that did not clear. The run waits while the page is still loading, or while
  a Browser Use Cloud browser's CAPTCHA solver works on the check, up to the run's time limit. It stops at once
  when nothing is working on the check or the solver gives up. The error is `bot check at <origin>; it is not a
  sign-in and no credential passes it, and no other route to the task was found. Retry after the check clears
  or choose another source for the task.`
- The site answered HTTP 403 and Jev found no way on. The error is
  `Could not complete the task after HTTP 403 at <url>: the site refused this browser session, which may be an access or bot check or a sign-in it requires.`

A bot check can block a cloud browser before any task step runs. Credentials do not pass a CAPTCHA, so
`--secret` and `--bitwarden` do not help here.

**Next step.**

- Retry with an attached local browser (`--cdp-url` or `--cdp-port`, see
  [Open windows and Electron apps](cli.md#open-windows-and-electron-apps)), or choose another source for
  the research.
- Give `--start` a URL past the check.
- Run headed with a kept profile (`--headed --profile DIR`), solve the check once by hand in that profile,
  and point later runs at it.

## budget_exceeded (exit 7)

**What it means.** The run reached a limit you set, or it cannot enforce its dollar cap.

**Common causes.** The JSON result's `budget.resource` names the limit and `budget.limit` its value:

| `budget.resource` | Error | Set by |
|:--|:--|:--|
| `steps` | `step limit <n> reached` | `--max-steps`, `Limits(max_steps=...)` |
| `dollars` | `spend limit $<n> reached` | `--max-dollars`, `Limits(max_dollars=...)` |
| `dollars` | `spend limit $<n> cannot be enforced: a call reported no cost` | the same dollar cap |
| `seconds` | `time limit <n>s reached` | `Limits(max_seconds=...)`, the MCP server's `--max-seconds` |
| `jev_calls` | `Jev call limit <n> reached` | `Limits(max_jev_calls=...)` |
| `llm_calls` | `LLM call limit <n> reached` | `Limits(max_llm_calls=...)` |

Other results have `budget: null`. The embedding and MCP APIs carry the same object.

A cloud browser bills when it stops, after the run, so its charge appears in the result's cost but cannot
stop the run that incurred it.

**Calls with an unknown cost.** If a model completion omits its cost, fastbrowse looks up the matching
OpenRouter or Vercel generation receipt. If that receipt is still unavailable, a run with a dollar cap stops
with `cannot be enforced: a call reported no cost`, because an unpriced call could have spent anything. The
known charges may still be under the cap. A run without a dollar cap does not stop for this.

**Next step.**

- Run again with a higher limit for the resource named, or split the task.
- For an unknown cost, run again so the receipt lookup can succeed. Supplied `inputs` give exact field text
  without a field-writing generation.

## unavailable (exit 8)

**What it means.** A model or browser provider stayed unavailable through every retry. The task itself may
be fine, and the same run later may pass. A run that had already taken a step that may have changed the site,
such as a click on Post, ends `error` instead: its error names those steps, so check the site before running
the task again.

**Common causes.**

- Jev or the LLM failed in transport or ran out of retries.
- A Browser Use Cloud browser could not be started: the service could not be reached or answered with a
  retryable status.
- Before the first step, the start page never loaded, or Chrome reported a dropped, refused or empty
  connection. A name that does not resolve is not counted here, since it can be a mistyped address.
- The site answered HTTP 408, 419, 429 or a 5xx status, and Jev found no way on.
- Mid-run, the browser could not open an address the run chose, after its own retries:
  `Could not complete the task after opening <url> failed in this browser session: <error>.`
- Jev judged that the page reports the site failed, such as an error shown in place of search results, a
  maintenance notice or a load that never finished, and reloading found no way past it:
  `Could not complete the task after the page at <url> reported that the site failed to deliver what was asked.`
- A browser command went unanswered while the browser still answered health checks. A command to a page that
  goes unanswered for 10 seconds has the page asked whether it still answers, and one that does not answer in 10
  more is hung; an input event gets 15 seconds and any other command 120 seconds. The first page that stops
  answering an action is reopened once at the address the tab shows; a second hang, or one outside an action, ends
  the run, and the error says the page did not answer or the browser stopped answering.
- The run's tab closed and no tab it owns remains.
- With `--extension`, Chrome took the run's tab away because the page loaded another extension's frame; see
  [Another extension's frame takes the tab away](#another-extensions-frame-takes-the-tab-away).
- With `--extension`, no browser has the fastbrowse extension connected. The error is `no browser has the
  fastbrowse extension connected`.

**Next step.**

- Read `error` for the provider or address, then run the task again later.
- For a deadline, set `Limits(max_seconds=...)` in the embedding API, or `--max-seconds` on the MCP server.
  Step and dollar limits do not bound elapsed time.
- For a stalled run, write its trace with `--trace FILE` or `FASTBROWSE_TRACE=FILE` and keep the last event
  with the result. Each `decide_choices` event lists the controls a step was chosen from, the chosen one marked.

### An attached Chrome that waits for approval

With `--cdp-url` or `--cdp-port`, the run connects to a browser you started. If that browser never completes
the DevTools handshake, the run ends `unavailable` with:

```text
CDP.start failed (<cause>): the browser did not complete the DevTools handshake. If this is a Chrome you
started yourself, it may be waiting for you to approve remote debugging, or the address may be stale; check
the browser window and the address, then retry.
```

**Next step.** Look at the Chrome window for a prompt to allow remote debugging and accept it, check that the
address is the browser's current one, and run again. Chrome asks again on every connection, so for your
everyday Chrome use `--extension` instead, which needs no approval; see [Your own Chrome](cli.md#your-own-chrome).

### The extension is not connected

`--extension` reaches Chrome through the fastbrowse extension, which Chrome connects to fastbrowse when the
browser starts. When the run ends `unavailable` with `no browser has the fastbrowse extension connected`:

- Check that `fastbrowse extension install` ran for this user, and run it again after moving or reinstalling
  fastbrowse, since it records where fastbrowse is.
- Check that the extension is on at `chrome://extensions`, and that Chrome is open.
- After installing, restart Chrome or turn the extension off and on, so it connects.
- The error names the directory the run looked in. A connected browser's host writes there too, and on Linux
  both use `$XDG_RUNTIME_DIR/fastbrowse-hosts`, or `/run/user/<uid>/fastbrowse-hosts` when the variable is
  unset. A run started with `FASTBROWSE_EXTENSION_DIR` set, or with a different `XDG_RUNTIME_DIR` from
  Chrome's, looks somewhere else.

### Another extension's frame takes the tab away

Chrome lets no extension debug a tab while the page holds a frame from another extension, and takes the tab
away from fastbrowse as soon as one appears. Screen recorders, password managers and writing assistants add
such frames to every page. The run ends `unavailable` with `Chrome detached fastbrowse from the run's tab when
the page loaded a frame from another extension (chrome-extension://<id>)`.

**Next step.** Open `chrome://extensions/?id=<id>` from the error, set **Site access** to **On click**, and run
again. The extension then adds its frame only on pages where you click it. With `--trace FILE`, every message
between the run and the extension is in the trace as an `extension` event, with screenshots and screencast
frames replaced by their length, and an `extension_tab_lost` event names the frame that took the tab.

### DevToolsActivePort and finding the DevTools address

`--cdp-port PORT` reads the browser's websocket address from `http://127.0.0.1:PORT/json/version`. When nothing
there answers with one, the run ends `error` with `no DevTools endpoint at
http://127.0.0.1:PORT/json/version (<cause>)` or `... named no webSocketDebuggerUrl`. Pass the browser's `ws://`
address with `--cdp-url` instead.

A local Chrome that fastbrowse starts itself picks its own DevTools port and writes it to a file named
`DevToolsActivePort` in its profile directory, which the run reads. If Chrome exits first, or does not answer
on the port it wrote within 90 seconds, the run fails before any status is reported, with
`Chrome exited with status <code> before DevTools started` or `Chrome did not start DevTools within 90s`.
Both messages end with Chrome's complete diagnostic output, which names the cause; fix that and run again.

## stuck (exit 9)

**What it means.** The stall budget ran out without reaching a page state the run had not seen.

**Common causes.**

- Jev, told each time what failed, found no way forward before `max_recoveries` ran out.
- The task says to change nothing, so the run cannot type into fields, pick options or tick boxes. A run
  that needs a value entered must not forbid edits. Typing a search is still allowed.
- The run chose to open an address that neither the task nor `--start` supplied:
  `navigation address was not supplied by the caller`.
- The page does not exist: `Could not complete the task after HTTP 404 at <url>: the page does not exist.`, or
  the same with HTTP 410. When the address the run started at answers this, a run that leaves it and stalls on
  another page still ends with this error. Check the address, or whether the page is public yet.
- The site answered an HTTP error that maps to no other status:
  `Could not complete the task after HTTP <status> at <url> in this browser session; the cause is unknown.`
- The run stopped with a canvas, a picture or a video in view, and reading its screenshot did not answer the
  task. The error then ends with `This page shows a canvas, picture or video beside its text, and its
  screenshot was not read as answering the task.` The page may still carry everything in its text: the
  stall can have another cause.

**Next step.**

- Try a `--start` URL closer to the goal, or a narrower task.
- Name any address the run needs in the task or in `--start`.
- On a drawn page, fastbrowse reads the DOM first, so it has two limits: a control that exists only in the
  pixels has no element to act on, and text that exists only in the pixels is not in the page text it quotes.
  The screenshot is the fallback for reading. When Jev's route accepts images, each step on a drawn page sends
  Jev the viewport, and before the run answers or gives up, the LLM reads the screenshot for text that answers
  the task, cited as read from the image. A drawn control still cannot be clicked, so start from a page of the
  site that offers the same action as an HTML control.
- Open an issue with the task, the start page, the status and the `--json` step trace, as
  [CONTRIBUTING.md](../CONTRIBUTING.md) describes.

## unverified (exit 10)

**What it means.** Jev finished, but the data the run was asked for could not be filled from the page, or a step the
task asks for was not carried out because the site does not allow it. Treat the result as unconfirmed.

**Common causes.**

- A field of the requested data was not backed by the page.
- The output schema has a field the run cannot fill. A run fills a flat object of required string, number,
  integer and boolean fields; with any other field it ends `unverified` with no data.
- The site refused a requested step, for example an option it marks unavailable or a control it shows disabled. The
  run ends with its answer, which says what was and was not done, and the error says a requested step was not carried
  out because the site does not allow it.

**Next step.**

- Read the answer and `evidence` in `--json`, and check what is cited before relying on it.
- Flatten an output schema to the field types above.
- Try a `--start` URL closer to the page that holds the answer, or a narrower task.

## observation_limit (exit 11)

**What it means.** The page cannot fit the configured prompt budget.

**Common causes.**

- A page whose state exceeds Jev's input limits even with no controls offered:
  `the state on <url> exceeds Jev's input limits with no controls`.

**Next step.**

- Try a `--start` URL closer to the goal, or a narrower task that needs fewer facts in one answer.
- To raise the budgets, set `observation` and `tokens` in `Config` in the embedding API.

## cancelled (exit 12)

**What it means.** You stopped an `--extension` run in your browser before it finished.

**Common causes.**

- **Stop** in the fastbrowse extension's popup.
- **Cancel** on Chrome's bar saying an extension started debugging the browser. That bar belongs to the whole
  browser, so it stops every run the extension is driving.
- Closing every tab in the run's tab group.

The result keeps the steps the run took and what it spent.

**Next step.** Run it again if you still want it done. To watch a run without stopping it, leave the bar and the
run's tabs alone; the run closes its own tabs when it ends.

