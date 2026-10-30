# Design

fastbrowse splits a browser agent into three owners:

- **Jev picks.** A batched request chooses an operation and its possible targets from indexed controls. It also judges whether unread evidence should be preserved before interaction, and checks sign-in and bot walls on fresh pages. On a page denser than the step's control budget, a batched relevance check keeps the controls the task can use rather than the first ones in document order. A page too dense even on screen offers the controls that fit and scrolls for the rest rather than ending the run. On a page with more quotable spans than one choice can offer, the same kind of check narrows the passages before Jev picks a short fact. Large target sets use a second choice within the selected group. Code can follow a list's next-page link without an action-choice call.
- **An LLM reads and writes.** Proposing a direct address for the task while the start page loads, planning checkable requirements from the task alone, reading page content for answers when Jev cannot pick a short fact from quoted spans, writing non-secret field text, recovering when Jev stays unsure, composing the final answer.
- **Code owns the gates.** Freshness and hit-tests before every input, no automatic retry of a mutation, authorization for irreversible actions, secret resolution and redaction, budgets, and the definition of success: only `COMPLETE`, which requires Jev to judge every requirement met.

That loop runs in Rust, in [crates/](../crates): `fastbrowse-agent` holds the run loop, `fastbrowse-browser`
the page and its indexing, `fastbrowse-clients` the Jev and LLM clients, and `fastbrowse-py` builds them as
`fastbrowse._native` into the wheel ([ADR 0004](adr/0004-rust-core-by-default.md)).

## Authorization

Jev judges every model-selected click, including links, navigation to a caller-supplied address, every Enter press and acceptance of
confirm, prompt and before-unload dialogs. Code-selected pagination is exempt. One rule applies: the action
proceeds when Jev judges it not irreversible, or when the run is authorized and Jev judges the task asked
for it; otherwise the run stops at `needs_confirmation`. A pick under `Thresholds.recover_below` never
reaches this gate, since it goes to recovery before it acts.

A submission of a form that holds more than one password field, or one marked
`autocomplete=new-password`, changes an account's credentials whatever the page calls it, so Jev is not
asked whether it is irreversible: an unauthorized run stops at `needs_confirmation`, and an authorized one
proceeds only when Jev judges the task asked for it. A refusal is recorded as a failed step with a reason.
The classifier can be wrong, so this gate is not a guarantee that every externally visible change is
detected.

## Navigation

The operation choice can open another literal HTTP(S) address from the caller's task. The address is a
closed choice, revalidated before dispatch, and uses the browser's origin grant and live access checks.
Page instructions cannot add addresses to that choice. Recovery can select the same supplied addresses;
addresses inferred by the startup shortcut retain their separate verification rules.
When a site failure that can clear stands on the current page (an HTTP 408, 419, 429 or 5xx status, an address
the browser could not open, or a page Jev judges to report that the site failed), the page's own address joins
that choice, so Jev can reload it. Code states the failure and how many steps it has stood in the step state's
`page_facts` and to the done check, where an answer drawn from it is not the task's; Jev picks the reload, a retry
control, or another route. A run whose recovery runs out on such a failure ends `unavailable` with its address.
An editable search field can be filled directly, including when focusing it opens an editor. Opening or
focusing the field does not enter a query.

## Reading and citations

Jev's read assessment is the READ decision: when Jev judges the page evidence the notes do not yet preserve,
the step reads it, unless Jev picked DONE or ESCALATE. Code does not force a read. It tells Jev, in the step
state's `page_facts`, when a page changed after it was last read, when a list page has not been read, and when
an action that may have committed a change has not had its page read. Jev is asked the read assessment on
every page it can read, for evidence toward an unanswered requirement and for a value a remaining action needs,
such as a code shown on another page. Navigation notes write each source URL once and refer to it from later
claims. Every READ Jev picks goes to the reader, including a repeat of content already read; a read that adds no
evidence counts toward the stall rule like any other step that moved nothing. The reader is sent
the tally, ranking and paging guidance only when the capture holds records, the plan counts them, or a pager
is in play. Identical relevance questions share one score within a pass, while controls remain separate action
targets.

For a bounded set of short quoted spans, Jev chooses a scalar fact, requests synthesis, or judges the
requirement absent from the page. Identical values from the same page and frame share one choice with all
their source contexts. A selected repeated value retains every span through `Fact.basis`. Uncertain choices,
comparisons, partial evidence and paginated lists reach the LLM reader. `FactReader` records `jev_choice` or
`llm`. Neither reader writes page text: Jev picks a value from spans code cut from the capture, and the LLM
reader cites the capture's source blocks by id (one block, or
consecutive blocks of one frame from the chunk it was shown). Code copies the quote from those blocks, so a
table's escaped pipe or a record read as two lines cannot drop a fact, and a claim citing a block it was
not shown is rejected. A prose claim can name a literal excerpt within its cited blocks. Code requires a
unique match in the offered chunk and copies the original span, retaining its source hash and offsets.
Missing or ambiguous excerpts are rejected. Complete record citations remain unchanged.

When quotes from the same address are already in notes, the short-fact batch can also ask whether the full
page adds relevant evidence. Literal diffs against matching source blocks help distinguish changed values
from cosmetic changes. A negative answer with at least 90% probability skips the read without marking any
requirement evidenced.
Missing or uncertain answers use the normal readers. This check is omitted when the full comparison does
not fit the input budget or the read needs pagination, continuation or incomplete-comparison context.

A filtered count or a comparison over a list that runs to further pages is read by the LLM on its first page,
which cites the matching records, and for a comparison the delimiters around each record's label and value.
The cited records name the list and say whether the count is grouped. When the reader says the list goes on, Jev
is asked, with the task, whether the requirement needs every page of the list or only pages the task names; the
reader does not decide this. From a yes at 0.70 or above, code pages the list itself and each later page of the
same shape is put to Jev in one request, a yes or no for each record of the one run of blocks shaped like those
records, matched against the requirement's own text. Code counts the matches, or ranks the kept records by the
comparison's delimiters, and ends the list on the page with no next-page control. A record Jev judges between 0.20 and 0.70
is sent to the LLM on its own, with only the records Jev was unsure of; a page whose title or record shape
differs goes to the LLM reader.

For a count, total or superlative, the reader cites every compared record on every page. Its conclusion
lists those facts in `draws_on`, using evidence ids from collected notes or `claim:N` for earlier claims in
the same response, indexed from zero. Code resolves these references and drops unknown ones with a debug
log. `Fact.basis` keeps the resolved ids, including when a span is reused. A conclusion the page does not
state (a count it never prints) cites no blocks: it is a derived fact with no evidence of its own, keyed
`derived:<hash>`, and it is judged and linked through its basis records.

Answer claims use numbered Markdown links built from those notes. `RunResult.citations` exposes the cited
facts and text-fragment deep links; unused facts have no citation. An answer citing an unknown reference
fails the claim check, and the run falls back to an answer drafted from verified facts. Jev checks the answer's claims against their quotes before completion; a derived fact contributes its basis records, never its own conclusion.
Both drafted and composed claims include the transitive basis of each cited fact, deduplicated in read order.
The claim check, numbered links and citation records all use those expanded ids.

## Prompt limits and progress

`Config.observation` names the group size for group-then-element choices. `Config.tokens` names the input
budgets and reader/composer output limits. A step request shows Jev the whole viewport, working notes and
history; one that does not fit Jev's limit loses its oldest actions first and then has its long texts cut, each
cut saying what it left out. Verdict prompts retain all requirement evidence or stop at `observation_limit`. The notes
budget keeps a retained fact's basis with it; requirement evidence includes its transitive basis. The Jev
completion check reduces page text first to make room for that evidence. Cut page text carries a marker
when there is room for one; an excerpt too small to carry the marker is empty.

A money or time limit keeps collected facts and their citations in a partial answer. The status stays
`budget_exceeded`, and producing the partial answer makes no additional model calls. A time budget stops
active work; the return also waits for owned requests and browser cleanup. Injected clients must honor
cancellation and finish their cleanup. Returning while a paid request still runs would lose its cost receipt.

Only visible effects or added evidence count as progress. Rewriting the value already in the observed
field cannot count, even when it opens an autocomplete popup. `StallRules` has one stall rule: a step that
changes nothing, a read that adds nothing, a refused DONE, or a return to a page state already seen with
nothing read since is no progress, and `unchanged_actions` such steps in a row go to recovery. A page state
not seen before or a read that adds evidence resets the count and renews the `max_recoveries` budget.
Recovery asks Jev first: its state names what failed, and the stalled action is left out of its options,
which are the page's controls plus back, scroll, escape and the caller's start page. A pick at
`recover_below` or above is the next decision; only when Jev stays unsure does the LLM recover. Its
subgoal reaches Jev with the next step on that page, asked with the same problem and with the stalled action
still left out, so a refused DONE cannot undo it; the action recovery names is taken only when Jev is unsure
again or escalates. When the LLM gives up, that counts as one more recovery: its diagnosis becomes the run's status
once `max_recoveries` is spent, and a sign-in it names ends `needs_login` only when Jev agrees the page is a
wall. An HTTP error page goes to Jev like any other, with its status among the step's page facts, so Jev
reloads it when the status can clear, goes back, picks another link or escalates; code only waits out a 403,
429 or 503 once per document.

## Run events and images

`on_event` receives a `BrowserEvent` followed by `StepEvent` objects. Each step includes added facts,
their reader and source links in `StepResult.facts`, and any available explanation in `StepResult.note`.
`Config(step_frames=True)` adds a PNG after each step unless a fresh observation shows a resolved secret.

`on_frame` receives JPEG bytes from the active tab. Frames are acknowledged after the async handler returns;
there is no fixed frame rate, and only the latest pending frame is retained. Delivery runs separately from
the agent, and handler failures are logged. A handler must honour cancellation: it runs on the caller's event
loop, so one that swallows `CancelledError` keeps that loop alive and `asyncio.run` waits for it at shutdown. Live images and MP4 recordings are held back from the moment a secret is typed, and whenever the page is read showing one, until a reading shows none;
a recording holds its last clean frame meanwhile.

## Cursor feedback

`--cursor` (`run_task(cursor=True)`) shows where the agent is about to act, through the Cua Driver's synthetic
cursor. The click itself is still sent over CDP, so the overlay cannot change what an action did and is never
retried. The page hook is in `CdpPage.act`, where the hit-tested point is known; a secret's field is never
marked. `browser/cursor.py` owns one `cua-driver mcp` child per run, speaking JSON-RPC over stdio, and stops it
when the run ends, including on cancellation. The driver's `move_cursor` is sent in `window` scope only, so the
real pointer and focus never move.

The page point becomes a screen point from what the page reports: `screenX/Y`, `outerWidth/Height`,
`innerWidth/Height` and the zoom from `Page.getLayoutMetrics`. The top inset is `outerHeight - innerHeight * zoom`.
The cursor is drawn only when exactly one driver window matches the page's reported frame (which also shows the
two share a scale, so a scaled display is skipped), that window is in front of every other, the tab is visible and
the point is inside the viewport. Otherwise it is hidden. Any driver failure turns the feature off for the run.

Limits: Linux X11 only; native Wayland, macOS and Windows are not verified. It needs `cua-driver`
0.28.3 or newer with the session cursor tools. Cloud and headless browsers are skipped. The driver offers no click
pulse through `move_cursor`, so only the glide is shown. A docked DevTools panel or a pinch zoom hides the cursor,
and a window partly covered by another counts as covered. The overlay is a separate window, so CDP screenshots and
recordings do not show it.

## Browser capabilities over plain CDP

Verified over raw CDP against local headless Chrome and a Browser Use cloud browser:

| Capability | Mechanism | Local | Cloud |
|---|---|---|---|
| Own tab rendered | `Target.createTarget` + `Target.activateTarget` | pass | pass |
| Own tab rendered behind a visible window | `Target.createTarget(background)` + `Emulation.setFocusEmulationEnabled` + a 16px `Page.startScreencast` on a second session | pass | not used |
| Upload caller bytes | in-page `DataTransfer` + `File` on the input, `input`/`change` events (no host path needed) | pass | pass |
| Download bytes | `Fetch.enable` at Response stage for Document responses (download-attribute anchors included), `Fetch.getResponseBody` on `Content-Disposition: attachment` | pass | pass |
| Cross-origin iframe | `Target.setAutoAttach(flatten)` on the page session, evaluate in the iframe session | pass | pass |
| Popup ownership | `Target.targetCreated.openerId` equals our target | pass | pass |
| Dialogs | `Page.javascriptDialogOpening` + `Page.handleJavaScriptDialog` | pass | pass |

Downloads arrive whole over the DevTools socket, so a session keeps them up to `Config.max_download_bytes`
(512 MiB by default) and refuses a response whose stated `Content-Length` is larger without reading it.
A refusal is reported once in the observation (`[Download not kept: ...]`) rather than dropped, so a run
that clicked an export it could not hold says the file was not saved instead of reporting success. The
kept name is the response's `filename*` (RFC 5987), else the URL's path segment without its query, reduced
to one safe path segment.

The cloud browser ignores `Browser.setDownloadBehavior(deny)`, so bytes come from response interception, never from the remote filesystem. Host-path `DOM.setFileInputFiles` is only valid for a browser on the same machine.

### Foreground and background

Chrome raises and focuses the whole window of a tab that is activated (`Target.activateTarget`,
`Page.bringToFront`) or created in front, so a session does either only when `BrowserConnection.foreground` is
set: on a cloud browser, whose live view shows the front tab, on headless Chrome, which has no window, and when
the caller asks to watch. Otherwise the tab is created with `background: true` and nothing activates it. A tab
behind another is hidden, so two things stand in for being in front. Focus emulation makes the document visible
and focused, which runs animation frames and lets it take typing. A screencast keeps the tab's compositor
awake, without which a screenshot of an idle page waits seconds for a frame; it is 16 pixels, every thousandth
frame, on a session of its own so that live frames and recordings cannot replace it. Measured on Chrome 155 in
a headed window beside a focused window of another program: none of the session's commands moved focus.

What still moves it is Chrome's own doing: a popup a page opens is created in front, and a pointer press
dispatched to the front tab of an inactive window activates that window, as an attached window often is.

### Attached windows

A session normally opens its own tab and closes only the tabs it owns. With `BrowserConnection.attach` it opens
none: it takes the first page target whose title or URL contains `target_match` (or the first page, skipping
`devtools://` windows), and owns nothing, so `closeTarget` is never sent. A scoped session with no `target_match`
that finds no page inside its grant, as in a fresh browser showing only `about:blank`, takes over an empty window
(`about:blank` that no page opened, still holding the empty document) when there is one, and otherwise opens one blank tab, keeping either open on exit
like the window it stands in for; it closes an opened tab only if setup fails. A new tab page is never taken over: it
can list the person's most visited sites. That page loaded before the session's
new-document scripts were registered, so they are also evaluated once in it; otherwise freshness tracking would
start only at its next navigation. Turning target discovery on replays `targetCreated` for every window already
open, so an attached session turns it on only after recording those, and none of them joins the run. Popups of a
tracked window join without being owned. Windows with no
opener join only when `target_match` is set: an Electron main process opens its windows that way, and so does a
browser for each tab a person opens by hand.

`allowed_origins` scopes documents with the same Fetch hook downloads use. A scoped session also pauses every
`Document` request before it is sent and fails one outside the grant with `ERR_ABORTED`, which commits no page that
could name the address. Redirect hops, iframes and popups all pause as documents, so one check covers each. Pages and
child targets attach paused (`waitForDebuggerOnStart`) and resume only once their Fetch is on, so a popup or
out-of-process frame cannot send its first request unchecked. Reads check the address each text was read from, the
committed address of the active tab, and nested document addresses, so an opaque document (`about:blank`,
`srcdoc`, `blob:` or `data:`) or a foreign page restored from the back-forward cache is refused rather than read.
Nested blank documents remain refused even when `document.write` gives them their parent's address. A window already open outside the grant is never matched, and no window is navigated to enforce
anything. Service workers are bypassed on every scoped session (`Network.setBypassServiceWorker`, undone on an
attached window at close), so no navigation is answered without meeting the gate. A scoped run delivers no live
frames, screenshots or recordings, because pixels cannot be matched to a document. The grant covers documents, not
network egress: subresources and requests a granted page makes are not scoped, and neither are workers.
`check_access`, when given, is awaited before browser startup and every browser read and action, scoped or not.

Before a pointer press, the browser waits for a stable target and rechecks its guard and hit-test. A
replacement control must match the original semantics and receiving document; ambiguous matches are
refused. A target covered by a band fixed to the window (an anchored ad, a sticky footer) is scrolled clear of
it, and one the band still covers is focused and pressed or typed into from the keyboard, so the band is never
clicked. Any other cover, such as a dialog, an open menu or popover, or a full-window backdrop, is reported as
`covered`. Focus and the receiving field are checked before typing. Settling waits for an interactive
document and a quiet DOM. It does not guess at loading indicators by class name: a page still loading is read as
it stands, and the reader and Jev judge from its text whether to look again.
