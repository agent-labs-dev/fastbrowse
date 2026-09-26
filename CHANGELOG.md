# Changelog

What changed in each release, in the terms someone using fastbrowse would notice. Dates are UTC.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The entries are prose rather than bare
Added/Fixed lists: what matters about a browser agent's release is why a behaviour changed.

Released entries describe the corresponding package on PyPI; `Unreleased` describes changes awaiting a
release. Older entries are kept verbatim rather than rewritten as the product moves.

## [Unreleased]

- **Evals never count a provider's outage against an agent.** Browser Use is now scored on its own agent's
  answer (its `done`, or its final reply when it never calls `done`), not on Browser Use's later
  `is_task_successful` verdict, which failed six correct 0.5.7 answers. It is timed to that answer, not to the
  session reporting stopped, which came up to two minutes later. Outage waits
  fastbrowse measures inside a run are left out of every published time. A fastbrowse attempt in which any Jev call
  took over 2 seconds (healthy calls take about half a second at any page size) is a Jev outage, run again rather
  than scored. The 0.5.7 results are republished under these rules.
- **Fewer model hand-offs on the way to an answer.** A lookup whose every requirement cites evidence finishes
  without the screenshot verifier, which never found an ungrounded claim in 76 0.5.7 verifications. Only an
  answer read on a search the run built itself (an address with a query) is still verified. A verifier naming a
  requirement as `req_1` or `1` now names `req-1` rather than nothing. A read that answers a lookup finishes
  without another decision or another look at the page it read, and a page with text but no controls is no longer
  waited on for 12 seconds.

## [0.5.7] - 2026-09-25

- **Every eval comparison sets its arms on the same attempts, and an outage scores no arm.** Each suite is split
  into comparisons of the arms that ran the same tasks. At each task every arm keeps as many attempts as the arm
  with fewest measured, all timed in wall time. A missing hosted verdict, a site answering 5xx and an error sent
  with HTTP 200 now count as outages, rerun up to five times. The gateway's $0 Jev metering is priced at list for
  both Jev-backed arms. The feed moves to `schema_version` 2.
  ([#153](https://github.com/agent-labs-dev/fastbrowse/pull/153), [#154](https://github.com/agent-labs-dev/fastbrowse/pull/154))
- **A dense page's step is asked again smaller when Jev sheds it.** The gateway answers large Jev requests with
  503s far more often than small ones, and a step on a Wikipedia article (160 controls, about 27k tokens) ran out
  of retries in six runs of six, ending each one `unavailable`. When that happens to a large step request, it is
  now asked again with the on-screen controls only, then with half of those left; a small request that is still
  refused ends the run `unavailable` as before. The error for a request that split into single questions now
  counts every request the call sent, not only the last question's.
- **A secret that is also part of a site's hostname no longer breaks the addresses a run cites.** 0.5.6 kept such
  a host readable in reported addresses, but the model's own view of the page was blanked first, so steps, facts
  and citations after signing in as `practice` still linked to `https://••••••••.expandtesting.com/secure`.
  Masking and redaction now share one rule: a value that sits wholly inside the host of an address on an origin
  it was typed on is left, wherever the address appears, and every other appearance of it, a password's
  included, is still blanked or redacted.
- **An answer states the value, not the requirement ahead of it.** A value read straight off the page was answered
  as the requirement it met followed by the value ("Find the latest released version of httpx." then
  "httpx 0.28.1"); the answer is now the value alone.
- **The page a run began on counts as visited.** A task saying "Start at" an address could be planned as a
  requirement to go there, and a run that opened a deeper page from it was sent back because nothing it was checked
  against showed it had been there. The done check and verifier now see every address the run has been on, first
  the one it began on, and the planner no longer makes a start address a requirement of its own.

## [0.5.6] - 2026-09-25

- **Correcting a wizard step is not a loop.** Going Back through a multi-step form to fix an earlier step reached
  pages the run had seen before, and it was stopped as stuck. A return now counts as a loop only when it repeats a
  move the run already made. ([#132](https://github.com/agent-labs-dev/fastbrowse/issues/132))
- **What a form holds can be quoted.** A filled field, date or select now reads as `Label: value` in the page text,
  so an answer about the dates or options chosen can cite them. Password and secret values are never read.
  ([#136](https://github.com/agent-labs-dev/fastbrowse/issues/136))
- **Finishing is judged on what was typed, in order.** The completion checks now see today's date and each action's
  typed value and effect, so "enter one name, then go back and correct it" or "the next Monday" can be confirmed or
  refused. A field asked to change later now gets its first value first. A control is named by the nearest title
  before it, so a date picker's Submit is no longer named after the post around it.
  ([#143](https://github.com/agent-labs-dev/fastbrowse/issues/143),
  [#144](https://github.com/agent-labs-dev/fastbrowse/issues/144))
- **Live evals require the evidence they grade.** A missing final page fails any arm whose page the harness can
  observe (the hosted agent, which cannot report one, is graded on its answer), unfinished hosted sessions no
  longer pass, subprocess arms get only allowed environment values, and all arms share one prompt. An opt-in
  browser-use OSS adapter and pinned external task loaders support preparing independent comparisons.
- **Eval results have a site feed.** Regenerating docs also writes `docs/results/summary.json` from published
  rows, with release and suite versions, per-arm time and cost, and changed task versions. Just recipes run,
  publish and regenerate without editing totals.

## [0.5.5] - 2026-09-25

- **A Jev outage costs a batch, not the run.** After two 5xx answers in a row, a batch of questions is asked one
  at a time within the retries it had left, and the answers that came back are kept.
  ([#131](https://github.com/agent-labs-dev/fastbrowse/issues/131))
- **Back stays on the task's site.** Back is offered only when the page before is on the same site, so a wizard
  that shares one address no longer steps back to a blank page. ([#132](https://github.com/agent-labs-dev/fastbrowse/issues/132))
- **Counts over long lists fit in the run's notes.** Records are tallied compactly with their citations, code
  computes the totals and rankings, and a run that still fills its notes ends `observation_limit` with what it
  had grounded rather than an error. ([#133](https://github.com/agent-labs-dev/fastbrowse/issues/133))
- **Star ratings drawn as icons are read.** A rating shown only as a class such as `star-rating Three`, or an
  accessible label, now appears in the page text. ([#134](https://github.com/agent-labs-dev/fastbrowse/issues/134))
- **Date pickers and date fields can be used.** Links that act as buttons, such as a calendar's days, are
  offered as controls, and native date, time, month and week fields are filled in ISO form and read back.
  ([#135](https://github.com/agent-labs-dev/fastbrowse/issues/135), [#136](https://github.com/agent-labs-dev/fastbrowse/issues/136))
- **A browser that never loads the first page reports `unavailable`.** Such a timeout is retried once; a
  timeout on the agent's own later navigation remains an `error`. ([#137](https://github.com/agent-labs-dev/fastbrowse/issues/137))
- **Evals record what they measured.** Every task has a version that a test forces up when its grader changes,
  every result row records the build and task version it ran, published results are committed rows that are
  never rewritten, and the tables in the docs are generated from them. `--only` searches every suite and rejects
  a task it cannot find. ([#139](https://github.com/agent-labs-dev/fastbrowse/issues/139))
- **A comparison missing one of its records no longer names a winner.** When a page of a list named a record the
  reader could not tie to the page's text, or more records than one page holds, the record was dropped and a later
  page could still settle "the cheapest" without it. That requirement now stays open for the rest of the run, so
  the run reports it could not settle the comparison instead of answering from part of the list. A page that
  states its own order still settles it on the leading record.
  ([#129](https://github.com/agent-labs-dev/fastbrowse/issues/129))
- **An answer about what the run bought is checked against the page it bought on.** An Amazon run bought one pen
  and reported another, citing the search listing; the checkout page it had read named the right one. In a run authorized to commit, Jev
  now judges which of its clicks, Enters and accepted dialogs committed something, once the run answers and only
  for those whose pages it read; the answer is checked against those pages, and the composer is told which notes
  come from them. Other runs make no extra call. ([#117](https://github.com/agent-labs-dev/fastbrowse/issues/117))
- **A click that found its element redrawn no longer spends a step.** A date picker that redraws under a click
  dispatches nothing, but the stale step counted toward `max_steps`, and Google Flights runs spent two to four of
  them. It still counts toward the stall budget, which bounds a page that never stops redrawing.
  ([#101](https://github.com/agent-labs-dev/fastbrowse/issues/101), [#94](https://github.com/agent-labs-dev/fastbrowse/issues/94))
- **Fewer wasted steps on forms and lists.** The target question now sets a form's mode (a trip type) before its
  submit, and knows that a broad "Explore" control is not the search asked for; a calendar's fare per day is an
  input, not evidence to stop and read; a plan no longer turns "report the total" into "the total once the order
  is finished"; and a shortcut for a count over records goes to the page listing them, not a page about one of
  them. ([#99](https://github.com/agent-labs-dev/fastbrowse/issues/99), [#101](https://github.com/agent-labs-dev/fastbrowse/issues/101))

## [0.5.4] - 2026-09-25

- **A cheapest or highest from part of a list needs the page to state its order.** A reader that cited the
  page's sort order to settle a superlative on the leading row still settled it when that citation named no block
  on the page; the row alone no longer answers it.
- **A step with one possible target no longer fails.** Jev now refuses a choice of only one option, and the
  typesafe-ai route reports that refusal as a 503, so a page with one field to fill ended the run, or in the evals
  retried it for hours as an outage: `wiki-godel` never finished. A choice of one option is answered without
  asking, and a request left with no open question is not sent.
- **A run that has not acted cannot be talked past an action Jev holds undone.** Asked to open a project page,
  a run chose DONE on the start page, Jev's done check held the requirement unmet, and the verifier called it
  complete, so the run reported `complete` on the wrong page. With no action taken, an action requirement Jev
  holds unmet now keeps the run going. Opening a shortcut address counts as acting, so a run the shortcut
  already took to the page is not held back.
- **The live evals send `GITHUB_TOKEN` to the GitHub API when it is set.** A row's answer key is fetched again
  on every retry, so a long provider outage spent the anonymous 60 requests an hour and failed `github-license`
  on a 403 that said nothing about the agent.

- **A secret that is also part of a site's hostname no longer breaks the reported address.** A username of
  `practice` signed in at `https://practice.expandtesting.com/secure`, and redaction rewrote the host as well,
  so the run reported `https://[secret:username].expandtesting.com/secure`, which is not an address. Final
  addresses, trace addresses, fact and citation links now keep their host and port on an origin the secret was
  typed on, since a value there was published by that site; on any other host, and in the path, query, fragment
  or sign-in part of an address, it is still redacted, as is every other appearance of it in the run's text.

- **Evidence from the wrong page no longer counts as an answer.** A proposed address opened a flights summary
  rather than the search the task described, the reader quoted a price from it, and the requirement counted as
  evidenced, so the verifier could not hold it open however plainly it was the wrong page. The verifier is now
  told where each requirement's facts were read and which addresses the run built from the task rather than
  reached by clicking, and it can name a requirement whose evidence came from the wrong page. A requirement
  evidenced on such an address goes to the verifier even when Jev's own check accepts. When that evidence was read on a guessed address, the requirement is not excused by
  having it: the requirement reopens, and the refusal names the page it was read off, so the run goes to find
  the right one rather than finishing again from the same notes.

- **A reader can settle a superlative the site has already ordered, or name the control that shows the rest.**
  Three reads of a filtered results page returned nothing while the cheapest row was on screen: the reader saw
  the list go on and never assigned the requirement, so the done check refused and the run stuck. A page that
  states it is ordered or filtered by the quantity being compared now settles the superlative on its leading
  record, citing that statement so the claim rests on it. A count or total over a list that goes on is not
  settled this way. Where the list really does go on, the reader names
  the control that shows the rest, and the run opens it when the page offers that label.

- **A page that cannot settle a list now has to say what it compared.** The reader's prompt asked a continuing
  page to quote every record it compared, and about half the time it quoted only the leading one, so the
  winner on a later page could not show the values it beat and the claim check scored it unsupported. The
  records are now a required part of the reader's answer rather than a request in prose, and code copies each
  quote from the blocks named, so a record is the page's own text. A read that settles its list pays nothing
  for this.

- **A run reads what its last interaction changed before it calls itself finished.** A run clicked a filter
  and declared itself done against the results as they were before the filter applied, so the check read a
  list the run never saw. A run that owes an answer now reads the page its last interaction drew before the
  done check judges it, asking the reader again for what it had already found. A run that only acts
  finishes without reading or waiting.

- **A filter put back to a state its page already held is not progress.** On a results page, turning a filter
  on and off redraws the rows underneath it, so every click reached a page state the run had never seen and
  nothing counted it: runs toggled one control until the step limit. A setting returned to committed values
  its document has already held no longer counts as progress, however the results redraw, so three of them
  reach the no-progress check and the run recovers. A setting given a value its page has not held is
  untouched.

- **A live eval survives a grader that raises.** The agent chooses where a run ends, so a grader is handed any
  address a page can navigate to, and one that could not be parsed took down the whole suite: 59 of 63 runs
  were discarded after four had finished. A grader that raises now fails that row and nobody else's, and so
  does an answer key that cannot be fetched for a reason a retry would not cure.

- **A page that rewrites its own text cannot be read for ever.** Reads were remembered by the page's exact
  content, so a ticker, a rotating advert or a live counter minted a key the run had never seen on every
  observation, and the agent could read one page until its step budget ran out instead of acting. Reads are
  now also budgeted by the page's address and what it lets you do rather than by its text, and two reads of
  one page state that add nothing the notes did not already hold make the run act instead. A read that adds a
  new fact restores the budget. Each page of a list paged in place keeps a budget of its own, and the same
  records read again off a ticking page are not new facts.

- **A Cloudflare edge error is retried like a 503.** A provider behind Cloudflare answered a 503, which was
  retried, and then a 520 seconds later in the same outage, which ended the run in error and failed its eval
  row for good, although a re-run of the task passed. Statuses 520 to 524 are now retried with backoff, and
  once the retries run out the run ends `unavailable`, so the live eval retries the row rather than scoring it.

- **A value is still the page's when the model retypes its punctuation.** A field the page writes with a curly
  apostrophe, an en dash or an ellipsis was dropped when the model quoted it with a straight apostrophe, a
  hyphen or three dots, so the fact never landed, the requirement stayed open, and the run read the same page
  until it stalled. A value now matches across a punctuation family, and across the backslash a capture puts
  before a table cell's own pipe. The value returned and the quote kept as evidence are both the page's own
  text, not the model's retyping, and the words, their order and their spacing all still have to be there.
- **A table filtered on the page is read as filtered.** Rows a filter hid, with `hidden`, `display:none` or
  `visibility`, and rows in a hidden header, body or footer still entered the capture, so a reader could answer
  from a row the page no longer showed. They are left out now, and so are cells a column toggle hid.

## [0.5.3] - 2026-09-22

- **Eval times leave out provider outages.** A run's `seconds` in the live and local evals no longer counts time
  spent retrying a provider's 503s and dropped requests, which says nothing about the agent; the row's
  `transient_seconds` holds what was left out.
- **Relevance passes send small requests in parallel.** Packing a page's Noul questions into as few requests as
  Jev's 64k limit allows made each one about 26k tokens, and the gateway answered most of those with 503 until
  the retries ran out, adding up to 25s a pass or ending the run `unavailable`. Questions now go in requests of
  about 8k tokens, sent together.
- **`--proxy-country CC` picks where the cloud browser browses from.** It always browsed from the US, so
  amazon.co.uk opened on "Deliver to United States" and a dispatch-country prompt. `--proxy-country uk` shows the
  UK storefront as a UK visitor sees it. A country on a local or attached browser is refused, since that
  browses from the machine's own IP. The Python API's `proxy_country` gains its command-line surface.
- **A link's name no longer includes a nested stylesheet.** Amazon puts a `<style>` block inside a result's link,
  and the element's name was built from every child's text, so Jev was offered a control named by a page of CSS
  and tried to click it. Style, script, noscript and template children no longer contribute to a name.
- **A dense results page is compacted rather than refused.** Amazon's signed-in search results offered Jev 112
  products with ~200-character titles and ~480-character tracking links, and the run stopped at
  `observation_limit` before it could pick one. When a page does not fit even on-screen only, its elements are now
  sent with labels and links shortened before fastbrowse gives up.
- **The CLI attaches to a browser already running.** `--cdp-url ws://…` drives any browser exposing CDP: a
  container, a VM, a hosted browser. The run opens one tab and closes only the tabs it owns, so the browser is
  left as it was found. The Python API's `cdp_url` gains its command-line surface; flags that shape a browser
  fastbrowse starts (`--local`, `--headed`, `--profile`, `--cloud-profile`, and their FASTBROWSE_* environment
  counterparts) are refused alongside it.
- **`--bitwarden` signs in past an authenticator-app code.** A vault item that holds an authenticator key now
  also offers `one_time_code`, the current code computed from the key at the moment it is typed, scoped to the
  same origin as the username and password. Amazon's two-step sign-in had stopped a run at "Enter OTP" with the
  key in the vault. Base32 keys and `otpauth://totp` URIs are read locally, so Bitwarden Premium is not needed; a
  code with a few seconds left waits for the next one, and a key fastbrowse cannot use (`steam://`, HOTP) is
  refused when the item is read.
- **A stored username is typed into an email or phone sign-in field.** Amazon's sign-in field is labelled
  "Enter mobile number or email", and Jev, seeing only a secret named `username`, chose to write new text,
  so the run stopped `needs_input` before signing in. The field question now says that stored secrets are the
  site's sign-in credentials, named by role.
- **A dense page keeps the controls the task needs, not the first ones in the document.** The browser now
  indexes up to 320 controls, and when a page offers more than 160, Jev is asked in one batched pass whether each
  could serve the task's next actions; the step sees the most relevant 160. Before, a page was cut at its first
  160 controls, so a result or filter drawn after a long header and sidebar never reached the choice. Pagers,
  blocking fields and controls holding a value or selection are always kept, and a control Jev did not answer
  for is kept rather than dropped. A dense page costs one more Jev round trip; a page under the limit costs
  nothing more.
  A page whose on-screen controls alone outgrow Jev's input no longer ends the run at `observation_limit`: the
  controls that fit are offered, pagers and set fields first, and the rest are reported as omitted so a scroll
  can reach them. The run stops there only when the page's state does not fit with no controls at all.
- **Jev reads short facts on long pages too.** Jev's quick read of a fact, such as a version or a date, gave
  up on any page with more than 253 quotable spans or more text than its input allows, which was six of ten real
  pages measured, from Wikipedia articles to GitHub releases, and the LLM reader then read the page a chunk at a
  time. Jev now first judges which passages bear on the requirements, one batched pass, and picks the fact from
  those. A requirement Jev calls absent from a narrowed page still goes to the LLM reader, since the evidence may
  sit in a passage it set aside.
  A field Jev picks from a listing record, such as a release date, now quotes the record up to it, so the
  answer's claim that version 0.1.0 shipped that day still has the version in its citation.
  A batched Jev pass cut short by the time limit now keeps the cost of the requests that had already answered,
  and one the call limit cannot cover counts none of its calls.
- **A setting switched back to where it was is caught as a loop.** A run on a results page could turn a filter
  on and off until it ran out of steps: each click changed the page, so no two steps looked alike. When every
  control on a page returns to the values it held before an earlier action, the run now stops and asks the
  recovery model for a different approach, naming the control and the actions in between.
- **Recovery remembers its earlier attempts.** The recovery model and Jev now see the last few diagnoses and
  subgoals of the current stall, and which attempt this is, so a second recovery does not propose the plan the
  first one already tried.
- **A form's Next button no longer ends the run.** A click on a "Next" that was not a link to another address
  was treated as paging to the next part of a list, and the run stopped on a multi-step form's first step. Only
  a link to another page counts as a pager now.
- **`stretch-dev` and `stretch-heldout` eval suites.** `dev` and `heldout` now pass almost every run, so these
  harder tasks (a multi-step form with a correction, a date relative to today, a list aggregated across pages, a
  filter applied and partly undone) are what show whether an agent change helps.
- **Every step is credited to Jev or the LLM, never to "code".** A step code dispatches carries out a model's
  choice, and is now recorded as that model's: the next page of a list is the reader's, since the reader asked for
  the rest of the list, and the read taken before an interaction is Jev's, since Jev judged the page to be
  evidence. A refused finish is the verifier's when it ran, and Jev's otherwise. `decided_by` no longer takes the value `code`, and recording captions name only Jev or the LLM.
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

[unreleased]: https://github.com/agent-labs-dev/fastbrowse/compare/v0.5.7...HEAD
[0.5.7]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.5.7
[0.5.6]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.5.6
[0.5.5]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.5.5
[0.5.4]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.5.4
[0.5.3]: https://github.com/agent-labs-dev/fastbrowse/releases/tag/v0.5.3
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
