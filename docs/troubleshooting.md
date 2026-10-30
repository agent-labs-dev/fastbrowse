# Troubleshooting

A run that does not end `complete` says why in its status, its exit code and its `error` text. This page takes
each status a user meets in practice: what it means, what causes it, and what to do next. The full table of
statuses and exit codes is in [the README](../README.md#results).

The CLI prints the status, cost and step count, then the error and anything the run read before it stopped.
`--json` prints the whole result, including every step and the quotes behind the answer. A run that stops
`stuck`, `unverified`, `budget_exceeded`, `observation_limit`, `unavailable` or `error` after it has read
something still returns those cited quotes, so check `evidence` before browsing again. A `stuck` or `unverified`
run also writes an answer from them, marked `Answer (unverified)`, when the task asks for one.

## needs_login (exit 4)

**What it means.** The run met a sign-in wall that no secret covers.

**Common causes.**

- Jev judged a fresh page to be a sign-in wall, and no secret is declared for that page's origin. The error is
  `sign-in required at <origin>`.
- The run reached a field that takes a secret, such as a password, and none is stored. The error is
  `'<field>' wants a secret and none is stored`.
- The site answered HTTP 401 and recovery found no way on. The error is
  `Could not complete the task after HTTP 401 at <url> in this browser session; the cause is unknown.`

`RunResult.wall` says what the wall asked for, so a caller can route it without parsing the error: `kind` is
`password` when a field takes a password, `one_time_code` when every credential field takes a code the site sent,
and `sign_in` when the page asks for an account name or shows only a control to sign in with. `origin` is the
page's origin, `fields` the visible labels of the fields it asked for (redacted as the error is) and `seen_at_ms`
when the run stopped, in UTC epoch milliseconds. A `blocked` run at a bot check carries a `wall` of kind
`bot_check`.

The CLI adds what to pass: `Pass --secret NAME=ENV_VAR@https://host with the password in that environment
variable; credentials in task text are not used.`

**Next step.** Give the run a way in, then run it again:

- `--secret NAME=ENV_VAR@https://host`, with the value in that environment variable. Without `@ORIGIN` the
  secret is scoped to the `--start` origin. Once a secret covers an origin, its sign-in wall is work for the
  run rather than a stop.
- `--bitwarden ITEM`, when the vault item's saved URIs match `--start`.
- `--profile DIR` for local Chrome you signed into once by hand, or `--cloud-profile ID` for a Browser Use
  Cloud profile. See [Signed-in sites](../README.md#signed-in-sites).

Over MCP, the server's operator adds the secret with `--secret` or `--bitwarden`, or signs in once in the
server's `--profile`.

A field is offered only the secrets declared for the origin of the frame it sits in. When none is, a sign-in
field ends `needs_login` with `'<label>' wants a secret and none is stored`, so a secret declared for the
wrong origin reads the same as no secret: check the origin in `--secret` against the address the form is on.
A secret that is declared for the origin but resolves to nothing, such as a password manager item that
changed during the run, ends `needs_login` too when the field takes a credential, with `sign-in required at
<origin>: secret <name> is not available there`, and `needs_input` otherwise.

## blocked (exit 6)

**What it means.** The site stopped the run with a bot check, or refused it.

**Common causes.**

- A bot check, such as a CAPTCHA, that did not clear. The run first waits for a check that clears itself once
  its script runs, and stops only when the page stays put. The error is `bot check at <origin>; it is not a
  sign-in and no credential passes it. Retry using an attached local browser or choose another source for
  this research.`
- The site answered HTTP 403 and recovery found no way on. The error is
  `Could not complete the task after HTTP 403 at <url> in this browser session; the cause is unknown.`

A bot check can block a cloud browser before any task step runs. Credentials do not resolve a CAPTCHA, so
`--secret` and `--bitwarden` do not help here.

**Next step.**

- Retry with an attached local browser (`--cdp-url` or `--cdp-port`, see
  [Open windows and Electron apps](../README.md#open-windows-and-electron-apps)), or choose another source for
  the research.
- Give `--start` a URL past the check.
- Run headed with a kept profile (`--headed --profile DIR`), solve the check once by hand in that profile,
  and point later runs at it.

## unavailable (exit 8)

**What it means.** A model or browser provider stayed unavailable through every retry. The task itself may
be fine, and the same run later may pass.

**Common causes.**

- Jev or the LLM failed in transport or ran out of retries.
- A Browser Use Cloud browser could not be started: the service could not be reached or answered with a
  retryable status.
- Before the first step, the start page never loaded, or Chrome reported a dropped, refused or empty
  connection. A name that does not resolve is not counted here, since it can be a mistyped address.
- The site answered HTTP 408, 419, 429 or a 5xx status, and recovery found no way on.
- Mid-run, the browser could not open an address the run chose, after its own retries:
  `Could not complete the task after opening <url> failed in this browser session: <error>.`
- Jev judged that the page reports the site failed, such as an error shown in place of search results, a
  maintenance notice or a load that never finished, and reloading and recovery found no way past it:
  `Could not complete the task after the page at <url> reported that the site failed to deliver what was asked.`
- A browser command went unanswered. One that stays unanswered for 120 seconds ends the run, even if the
  browser still answers health checks, and the error says the page did not answer or the browser stopped
  answering.
- The run's tab closed and no tab it owns remains.

**Next step.** Read `error` for the provider or address, then run the task again later. Step and dollar limits
do not bound elapsed time, so set `Limits(max_seconds=...)` in the embedding API, or `--max-seconds` on the
MCP server, for a deadline. When looking into a stalled run, write its trace with `--trace FILE` or `FASTBROWSE_TRACE=FILE` and keep the last event
with the result.

### An attached Chrome that waits for approval

With `--cdp-url` or `--cdp-port`, the run connects to a browser you started. If that browser never completes
the DevTools handshake, the run ends `unavailable` with:

```text
CDP.start failed (<cause>): the browser did not complete the DevTools handshake. If this is a Chrome you
started yourself, it may be waiting for you to approve remote debugging, or the address may be stale; check
the browser window and the address, then retry.
```

**Next step.** Look at the Chrome window for a prompt to allow remote debugging and accept it, check that the
address is the browser's current one, and run again.

### DevToolsActivePort and finding the DevTools address

`--cdp-port PORT` reads the browser's websocket address from `http://127.0.0.1:PORT/json/version`. When nothing
there answers with one, the run ends `error`, not `unavailable`, with `no DevTools endpoint at
http://127.0.0.1:PORT/json/version (<cause>)` or `... named no webSocketDebuggerUrl`. Pass the browser's `ws://`
address with `--cdp-url` instead.

A local Chrome that fastbrowse starts itself picks its own DevTools port and writes it to a file named
`DevToolsActivePort` in its profile directory, which the run reads. If Chrome exits first, or does not write
a port it then answers on within 90 seconds, the run fails before any status is reported, with
`Chrome exited with status <code> before DevTools started` or `Chrome did not start DevTools within 90s`.
Both messages end with Chrome's own last output, which names the cause; fix that and run again.

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
with `cannot be enforced: a call reported no cost`, because an unpriced call could have spent anything. This
does not mean the known charges reached the cap. A run without a dollar cap does not stop for this.

**Next step.** Run again with a higher limit for the resource named, or split the task. For an unknown cost,
running again lets the receipt lookup succeed, and supplied `inputs` give exact field text without a
field-writing generation.

## stuck (exit 9)

**What it means.** Recovery ran out without reaching a page state the run had not seen.

**Common causes.**

- Recovery found no way forward, and Jev, given its subgoal, found none before `max_recoveries` ran out.
  `error` holds recovery's diagnosis.
- The run chose to open an address that neither the task nor `--start` supplied:
  `navigation address was not supplied by the caller`.
- The site answered an HTTP error that maps to no other status:
  `Could not complete the task after HTTP <status> at <url> in this browser session; the cause is unknown.`

**Next step.** Try a `--start` URL closer to the goal, or a narrower task. Name any address the run needs in
the task or in `--start`. A run that ends `stuck` is a bug report worth having: open an issue with the task,
the start page, the status and the `--json` step trace, as [CONTRIBUTING.md](../CONTRIBUTING.md) describes.

## unverified (exit 10)

**What it means.** The run believes it finished, but could not back every requirement or claim with a quote
from the page. Treat the answer as unconfirmed.

**Common causes.**

- A claim in the answer, or a field of the requested data, was not backed by the page.
- The checks rejected the finish, and recovery could not gather the missing answer outputs.
- The task asked for a screenshot, and the run could not hand it over: a secret on screen, a dialog or a
  timeout can withhold it.
- The output schema has a field the run cannot fill. A run fills a flat object of required string, number,
  integer and boolean fields; with any other field it ends `unverified` with no data.

**Next step.** Read the answer and `evidence` in `--json` and check what is cited before relying on it.
Flatten an output schema to the field types above. Otherwise try a `--start` URL closer to the page that
holds the answer, or a narrower task.

## needs_confirmation (exit 3)

**What it means.** The run stopped before an action Jev judged irreversible, such as submit, pay, delete or
send, because the run was not authorized to take it, or was authorized but Jev judged the task did not ask
for it. Nothing was committed by that action.

**Common causes.** The task leads to an irreversible action, and the run had no authorization. The error is
`<operation> '<control>' needs confirmation`, and the refusal appears as a failed step. An uncertain pick of
such an action goes to recovery instead of stopping here.

**Next step.** Confirm the action is what you want, then run again with authorization:

- the CLI: `--authorize`;
- the embedding API: `authorization=Authorization(irreversible_actions=True)`;
- MCP: pass `authorize` on the call. The server must be started with `--allow-authorize`, or every such
  action stops here; ask the user to take the step, or to restart the server with that flag.

The irreversible check is a classifier, not a guarantee, so read the step the run stopped on before you
authorize it.

## observation_limit (exit 11)

**What it means.** The page, or the evidence the run must report, cannot fit the configured prompt budget.

**Common causes.**

- A page whose state exceeds Jev's input limits even with no controls offered:
  `the state on <url> exceeds Jev's input limits with no controls`.
- Requirement evidence that cannot fit the notes budget without losing facts:
  `Requirement evidence exceeds the <n> character notes budget`, or
  `The final page cannot fit every earlier record in its collected evidence`.

**Next step.** Try a `--start` URL closer to the goal, or a narrower task that needs fewer facts in one
answer. The embedding API takes the budgets in `Config` (`observation` and `tokens`) if you need to
raise them.

## needs_input (exit 5)

**What it means.** The run needs a value or a file that the task did not give, or an upload is over the size
limit. It stops rather than guess.

**Common causes.**

- The run opened no page: `the task names no page to start from, and none was given`.
- A field needs a value the task never states. The run first tries to go on without it, then stops with
  `'<field>' needs a value the task does not give`, or with recovery's diagnosis.
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

**Next step.** Read `error`. For a refused option or browser choice, fix the command and run again. For a
model or browser failure, run again, and if it repeats, open an issue with the task, the start page and the
`--json` step trace, as [CONTRIBUTING.md](../CONTRIBUTING.md) describes.
