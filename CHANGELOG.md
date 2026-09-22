# Changelog

What changed in each release, in the terms someone using fastbrowse would notice. Dates are UTC.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The entries are prose rather than bare
Added/Fixed lists: what matters about a browser agent's release is why a behaviour changed.

Released entries describe the corresponding package on PyPI; `Unreleased` describes changes awaiting a
release. Older entries are kept verbatim rather than rewritten as the product moves.

## [Unreleased]

- **Every step is credited to Jev or the LLM, never to "code".** A step code dispatches carries out a model's
  choice, and is now recorded as that model's: the next page of a list is the reader's, since the reader asked for
  the rest of the list, and the read taken before an interaction is Jev's, since Jev judged the page to be
  evidence. `decided_by` no longer takes the value `code`, and recording captions name only Jev or the LLM.
- **`fastbrowse --version`** prints the installed version, which bug reports now ask for.
- **A field the task has no value for is skipped before it ends the run.** Most such fields are optional: Google
  Flights opens a "Where else?" box beside the origin, and a run that picked it stopped `needs_input` with
  nothing searched. The first time, recovery is told the task gives no value and chooses another step; a field
  it sends the run back to is a required one, and the run still stops `needs_input` rather than invent a value.
- **A finished search is checked on the stronger model.** When Jev doubts a run is done, the verifier that has
  the last word now runs on `google/gemini-3.8-flash` rather than flash-lite. On a Google Flights search with
  every field right and no nonstop filter, flash-lite took the "Nonstop" rows for the filter and passed it ten
  times in ten; the default model refused all ten and passed the filtered search every time. A doubted finish
  costs about two seconds more.
- **The evals allow 50 steps.** A fare search on Google Flights took up to 29 steps, one short of the old limit,
  so a run that needed a recovery or two could run out of steps rather than fail on its merits.

## [0.5.2] - 2026-09-22

- **A page is read before it is scrolled.** A read takes in the whole page, so scrolling one nobody has read only
  spends steps: a lookup for Mongolia on a long country list scrolled until recovery ran out and stopped stuck
  with the answer on the page. Once a page is read, scrolling it goes ahead, as a list that draws more on scroll
  needs.

- **A reply the provider cut short is an outage, not a truncation.** JSON that ends mid-value short of the
  output cap is asked for again at the same cap; a second one ends the run `unavailable`, naming how many
  tokens came back. Only a reply that used the cap gets more room and, if cut again, is reported as truncated.
- **A browser that cannot be reached yet is an outage, not a failed task.** A dropped or refused connection
  when the run first connects, as a cloud browser still starting can give, ends the run `unavailable`, so the
  eval runs it again; any other connection failure is still an error.
- **An eval waits out a rate-limited answer key.** Fetching a task's expected answer from a public API now
  retries a 429, 5xx or dropped connection with the same backoff as a run, where it used to end the whole eval.

## [0.5.1] - 2026-09-21

- **A decision the page redraws under is dropped at once.** While Jev decides and gates a click, the run watches
  the controls on offer; if one changes, as Google Flights' date picker does when its prices arrive late, it
  decides again on the settled page instead of clicking into a stale refusal.
- **A guessed shortcut the site does not serve is skipped.** A direct address that answers 4xx/5xx sends the
  run back to its start page instead of reading an error page and spending a BACK to leave it.
- **Runs use a Browser Use Cloud browser by default** (`BROWSER_USE_API_KEY`). `--local` runs local Chrome;
  `--headed`, `--profile`, `FASTBROWSE_HEADED` and `FASTBROWSE_PROFILE` imply it. `--cloud` is removed.
  `--cloud-profile` with local Chrome is refused at startup.
- **A failure says what happened.** Each retry logs the call, the provider's status and error, and the
  backoff; a call that gives up names how many requests failed, over how long, and the last cause. Evals
  lead every failed run with how it ended, and print which providers (and whether a backup) they use.
- **`unavailable` is its own status.** A run that ends because a model or browser provider stayed unavailable
  through every retry reports `unavailable` rather than `error`: nothing about the task failed. The live evals
  run such a run again, so a published result is the agent's own.
- **An error never quotes a key.** Error text built from a provider's response omits the values validation
  rejected and scrubs the key wherever it appears, including a model's own JSON keys.
- **A browser that stops answering is an outage; a slow one is not.** A CDP command with no reply after 60s asks
  the browser whether it is still there, and keeps waiting if it answers. Only a browser that does not ends the
  run `unavailable`, where before a lost cloud browser could leave it waiting forever.
- **Eval arms are named for their agent:** `--arms fastbrowse jev-ultrafast browser-use`.
- **Recordings are captioned and kept plain beside them** (`<name>.plain.mp4`). Captions are timed from the
  video's first frame; an ffmpeg without libass still writes both videos, uncaptioned, and a failed write
  leaves an earlier recording at the same path intact.
- **Clicks are safer on covered controls.** A covered control is clicked at an exposed edge only when no
  other control (a row's own Delete button) sits under that point; a hover-only region inside a button does not
  count as one. A select the page reverts is a failed step.
- **A recording ends on a readable answer.** The closing card shows each citation as a number beside the
  claim and lists the cited site and quote underneath, instead of the raw text-fragment link.
- **A run reports where its recording went.** `RunResult.recordings` lists the videos written; the CLI prints
  `recorded: <paths>` or `not recorded`, and prints warnings to stderr.
- **Local Chrome has no password popups.** Every launch switches off Chrome's password manager and leak
  detection, in a kept `--profile` too; a kept profile whose preferences cannot be read is a `BrowserError`.
- **Finishing is judged more carefully.** A lookup that has its answer ends rather than clicking on; a search
  the task only asks to run is a requirement to act on; the verifier confirms requirements one by one, sees the
  controls that are set and the ones the done check doubted, and a narrowing requirement needs its filter
  applied rather than matching rows.
- **Pages are read when they have settled.** Navigation waits for the loaded page's DOM to go quiet; a
  transparent checkbox styled by its ancestors is offered; an `aria-disabled="false"` added during hydration is
  not a new control.

## [0.5.0] - 2026-09-21

- **Measured on 2026-09-21: 42/42 answer-task runs passed at a $0.0042 median cost**, against 41/42 and
  $0.0057 for 0.4.1; hosted Browser Use scored 40/42 at $0.4163 the same day. Navigation tasks passed 18/18.
  Google Flights is slower (109.3s against 71.7s). See `docs/evals.md`.
- **`RunResult.would_fire` lists the shadow tripwires that crossed**, one entry per crossing, so a caller or
  eval can count them without reading logs.
- **The reader cites page blocks instead of retyping quotes.** A claim names the capture's source blocks
  and fastbrowse copies the quote from them, so a fact is no longer lost when the model's copy differs from
  the page (a table cell's `|`, a record quoted as two lines). A count or winner the page never states
  cites no text: it is kept as a derived fact, and the claim check judges it from the records it counts.
  `StepFact.quote`, `url` and `deep_link` are `None` for such a fact.
- **A card or table row is one block.** A repeated card (a quote with its author and tags, a product) and each
  table row with its header are captured as single blocks, so a record is read and cited whole.
- **Model responses use strict structured output, and every prompt was audited.** Schemas are sent strict,
  with field docs included. Page text is marked untrusted in the same words everywhere. Context comes
  before the question. Rules added for one incident became general rules or were removed. Text fields no longer ask the model to retype a quote: the value must appear in the
  block it names.
- **`python -m fastbrowse.evals.probe` measures the reader and claim check on live pages.** It loads the
  given pages once and runs the agent's own read, draft and claim check over them N times at once.
- **Counts, totals and superlatives cite their underlying records.** The reader preserves every compared
  record across pages and records which facts a conclusion draws on. Drafted and composed answers carry
  those records through claim checks, citation links and `RunResult.citations`. Required evidence keeps
  its full basis within the notes budget or stops at `observation_limit`.
- **Long lists are read to the end before they are answered.** A single block longer than the reader's
  input, such as a flight results list, is split at line breaks rather than stopping the run at
  `observation_limit`, and the last chunk of a page decides whether its list goes on. When the list does
  go on, recovery and the next-step hint say to load the rest or narrow it with the page's own filter or
  sort, instead of finishing early with the best record seen so far. A load-more button at the foot of a
  long list ("View more flights", "Show 20 more") stays on offer past the off-screen control limit, as a
  pager link already did.
- **The completion check keeps the page state that matters on long pages.** When the page and the notes
  do not both fit, the check drops page text and controls without state before it drops the evidence, so
  a checked filter such as "Nonstop only" still counts.
- **Recovery can direct a read or a finish.** A recovery subgoal that names READ or DONE is followed; a
  finish is still judged by the completion check.
- **Page-script output is validated.** What the browser scripts return is parsed into typed models, so a
  mismatch raises `BrowserError` at once instead of failing later in the run.
- **Concurrent live evals keep separate traces.** Each run records only its own events, including events
  from its child tasks. Finishing one run no longer disables trace collection for the others.
- **Answer claims link to the words that support them.** `RunResult.citations` exposes each cited
  fact, its requirement, source URL, verbatim quote and text-fragment deep link. An answer citing an
  unknown reference fails the claim check and falls back to one drafted from verified facts.
  `RunResult.answer` contains numbered Markdown links; integrations should render them as Markdown. MCP answers carry the links too, while its citation records
  retain their `quote` and `url` shape.
- **See what each step learned and why it stopped.** `StepResult.facts` carries that step's added facts on
  its `StepEvent`, with quotes, source links and the reader (`jev_choice` or `llm`). `StepResult.note`
  reports read outcomes, dispatch details, refusal reasons and recovery guidance when available.
  Resolved secrets are redacted before delivery.
- **Watch the active tab as the agent works.** `run_task(on_frame=...)` sends JPEG bytes, follows tab
  switches and works with browsers reached over CDP. Delivery paces capture by acknowledging each frame
  after the handler returns, with no fixed frame rate; a later pending frame replaces an earlier one.
  Slow or failing handlers do not hold up the run. Capture is off unless a handler is supplied. Live
  images and recordings are held back while a resolved secret may show on the page, as PNG step frames are.
- **Links pass the same authorization gate as buttons.** Jev judges whether a click, an Enter press or
  dialog acceptance commits an irreversible change. Code-selected pagination is exempt, and
  authorized actions with sufficient confidence go straight through. Refusals appear as failed steps
  with reasons before confirmation or recovery.
- **Read evidence before a click can hide it.** Jev judges whether the page holds information the task
  needs. Reading preserves that evidence before another interaction, skips unchanged content for the same
  open requirements, and sends comparisons and partial evidence to the LLM reader. Short facts can be
  copied from quoted spans by Jev; each fact records which reader supplied it.
- **Moving controls are rechecked before input.** A click waits for its target to stop moving and checks
  its meaning and position again. A replaced field must still match and hold focus before it receives
  text. The browser also gives visible loading indicators time to clear before declaring a page settled.
- **Repeated work does not count as progress.** Filling or selecting a value already present in the
  observed field cannot reset the stall count. Repeated actions and unresolved requirements are also
  monitored: by default they log possible stalls without changing the run. `StallRules.tripwires` can
  enable recovery for them. Productive steps break the unresolved-requirement streak, and recovery resets
  the evidence used by all three stall checks. Local and live evals record these signals per run; the live
  summary counts passing runs affected, so repeated signals in one run do not inflate the rate.
- **Prompt limits preserve required evidence.** `ObservationLimits` names the page-text, working-notes
  and history limits; `TokenBudget` names input and reader/composer output limits. Shortened page excerpts
  carry a cut marker when it fits. Verdicts retain requirement evidence or stop at `observation_limit`;
  the Jev completion check makes room by reducing page text before refusing the evidence.
- **A Jev provider outage can use the other configured provider.** With both keys set, a retryable HTTP
  failure that exhausts retries switches the run to the backup provider and keeps it there. Authentication
  errors do not switch providers. A custom endpoint or model disables automatic failover.
- **CLI secrets can declare their own origin.** `--secret NAME=ENV_VAR@ORIGIN` works without `--start`,
  including wildcard site origins. The shorter form still takes its scope from `--start`; Bitwarden
  still needs a start page to match the vault item.
- **The MCP HTTP token can be read from `.env`.** `FASTBROWSE_MCP_TOKEN` follows the same settings rules
  as model keys, with the process environment taking precedence.
- **Flights grades use the submitted search even when its fields collapse.** The grader decodes the
  route, date and trip type from the URL, falls back to visible fields when decoding is unavailable, and
  still requires matching result rows and any requested Nonstop filter. A decoded mismatch fails.

## [0.4.2] - 2026-09-20

- **A form is set up in the order that works.** Its mode - which tab of a search, which kind of account or
  ticket, which category - decides which fields it has and empties what they hold, so it is chosen before
  any value is typed rather than after, which used to mean typing the values twice. The filters a task asks
  for are set before submitting where the form offers them, because setting one afterwards submits twice,
  and a filter the page only reveals once there are results is set there. On a flight search this removed
  five steps of rework; on a two-package comparison it removed the repeated writes to the search box that
  had made it the most expensive lookup in the suite.

## [0.4.1] - 2026-09-20

- **A secret can be declared for a site rather than for one of its hosts.** `https://*.example.com` covers
  `www.example.com`, `accounts.example.com` and `example.com` itself, which is how one sign-in spans a site:
  the login typed on the account host is the login the shop host asks for. The wildcard
  stands for whole labels only, so it does not cover `example.com.evil.test`, and neither the scheme nor the
  port is ever wildcarded. An exact origin behaves exactly as before. This reaches the MCP server too
  (`--secret NAME=ENV_VAR@https://*.example.com`), where each secret keeps the scope it was declared with
  rather than the start page's.
- **`ScopedSecrets.per_secret({name: (value, origins)})`** holds a person's credentials each scoped to the
  sites it belongs to, for an application that stores them that way. The single-origin constructor is
  unchanged.
- **An IPv6 origin survives being read back.** `origin_of` returned `https://::1`, which is not a URL any
  parser reads again, so a check against an IPv6 origin could raise rather than answer. The literal keeps its
  brackets.

## [0.4.0] - 2026-09-20

Everything an application needs to run fastbrowse as its browser engine rather than as a command someone
types. Each of these came from wiring it into a product that already had one.

- **Drive a browser you already have.** `cdp_url` attaches to any browser over the DevTools protocol,
  wherever it runs: a container, a VM, a machine you own. The run opens one tab and closes that tab, so a
  browser handed over is left exactly as it was found, and nothing is billed to a cloud account. This is the
  option to reach for when the browser should live next to the user rather than in someone else's cloud.
- **Start from the task alone.** `start` is optional now. A caller whose own interface takes a goal and no
  URL had nowhere to get one; the first address is worked out from the task, as a person would. `--start`
  is optional in the CLI and `start` is optional on the MCP server's `browse` tool, where `task` is now the
  only thing a call must carry, and it holds for an attached browser too, which the run opens its own tab on.
  A secret is only ever typed on the start origin, so asking for one without a start page is refused rather
  than quietly dropped.
- **Stop a cloud browser you did not start.** The browser event carries the cloud browser's id, so an
  application that has to end a run out of band (a user pressing cancel, a subscription ending) can.
- **`proxy_country` and `viewport`** reach a cloud browser the run starts, instead of being fixed at what
  the library guessed.

Fixed in the same release, from tasks that failed in the field:

- **A list longer than one page is answered from the whole of it.** A task over a paginated catalogue read the
  first page, answered from it and called that done. The run now follows the pager until what was asked for is
  evidenced or the page cap is reached, the reader is told when the page it is reading continues, and a claim
  about a whole list is not accepted from one page of it.
- **A bot check is reported as one.** `Status.BLOCKED` is new: a CAPTCHA is not a sign-in and no credential
  passes it, so a run that meets one says so rather than ending as `stuck`. A challenge that clears itself once
  its script runs is still waited out first, and the check is made whether or not a secret is held for the site.
- **A reply cut short is asked for again.** A read whose answer hit the output limit was parsed as though it
  were whole, so facts after the cut were lost without a word.
- **`--json` keeps its contract on a bad limit.** `--max-steps 0` printed a traceback and nothing parseable; it
  is now refused like any other bad flag, with the error on stdout as JSON.
- **A limit reads as what it is** in the message that reports it: a dollar limit as money, a duration as a
  duration.

## [0.3.4] - 2026-09-20

- **A step frame can no longer carry a secret the step itself revealed.** `Config(step_frames=True)` checked
  whether a resolved secret was on screen using the reading of the page the step was decided from, which is
  the page *before* the action ran. A fill that a page mirrors into ordinary text put the secret on the page
  after that check, so the frame sent to the caller could contain it as pixels. The check now reads the page
  as it is when the image is taken. Affects 0.3.2 and 0.3.3 with step frames enabled; no other surface sent
  an image.

## [0.3.3] - 2026-09-20

- **Runs on Python 3.13.** The floor was 3.14, which an application pinned below that could not work around:
  `uv add fastbrowse` simply would not resolve. Nothing in the package needed 3.14. CI now runs the whole gate
  on 3.13 and 3.14, so the floor is exercised rather than claimed.

## [0.3.2] - 2026-09-20

- **A picture of each step, for an interface that shows a run as it happens.** `Config(step_frames=True)` puts
  a PNG of the page a step acted on onto every step event. It is off by default, because it costs a screenshot
  round trip per step. A step whose page is showing a resolved secret sends no frame: pixels cannot be masked
  the way text is.

## [0.3.1] - 2026-09-20

- **A cloud browser can run as a profile someone already signed in.** `--cloud-profile ID`,
  `run_task(cloud_profile=...)` and the MCP server's `--cloud-profile` start a Browser Use Cloud browser from
  one of that account's profiles, so a run acts as whoever set the profile up. No credential is shown to a
  model, and a local run's `--profile DIR` keeps working as before. Passing a cloud profile id to local Chrome
  is refused rather than ignored.
- **Install from PyPI.** The README opened with `git clone`, which was the only way to run fastbrowse before it
  was published and is now the contributor path. It opens with `uvx fastbrowse`.

## [0.3.0] - 2026-09-20

- **A list split across pages is read whole.** Counting or ranking over a paginated list had no path to the
  answer: a run either stopped at the step limit or finished early on page one. The reader can now say a list
  continues past the page it read, a claim from part of a list cannot close the question, and when a page has a
  single next-page link the agent follows it and reads what it opened without asking the choice model each time.
- **A click that changed nothing is not repeated.** An action that left the page as it was goes to recovery
  instead of being taken again from the same page, and fields a form will not submit without (required and
  empty, or marked invalid) are named in what the action reports.
- **An MCP server.** `fastbrowse-mcp` serves one `browse` tool over MCP, so Claude Code, Claude Desktop, Cursor
  or any other client can hand it a task. What a calling model may do is fixed by the operator's flags: a call
  can ask for less, never more.
- **Runs on Windows.** A checkout failed before any test ran: files were read in the locale's code page, a date
  used a flag only glibc has, and Chrome was never found where its Windows installer puts it.
- Fixes found by the live suite: a run outlasts a brief provider outage, a redrawn control's twin is acted on
  rather than decided again, an empty page is drawn before it is read, a start page that never loads is tried
  again, and a finish stands when the verifier doubts only what the notes already cite.

## [0.2.0] - 2026-09-18

- **`--record FILE`** saves an MP4 of the tab, ending on the answer, its time and its cost.
- **Sign in with a stored login the task never mentions.** `--secret NAME=ENV_VAR` and `--bitwarden ITEM` type a
  credential on the start origin without the value entering a model's context.

## [0.1.0] - 2026-09-18

- First release: a browser agent that picks its next action from the controls the page actually has, with an
  LLM to plan and read, and code owning verification, safety and secrets.

[unreleased]: https://github.com/agent-labs-dev/fastbrowse/compare/v0.5.2...HEAD
[0.5.2]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.5.2
[0.5.1]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.5.1
[0.5.0]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.5.0
[0.4.2]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.4.2
[0.4.1]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.4.1
[0.4.0]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.4.0
[0.3.4]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.3.4
[0.3.3]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.3.3
[0.3.2]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.3.2
[0.3.1]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.3.1
[0.3.0]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.3.0
[0.2.0]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.2.0
[0.1.0]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.1.0
