# Design

fastbrowse splits a browser agent into three owners:

- **Jev decides, reads and finishes.** A batched request chooses an operation and its possible targets from indexed controls. On a page denser than the step's control budget, a batched relevance check keeps the controls the task can use rather than the first ones in document order. A page too dense even on screen offers the controls that fit and scrolls for the rest rather than ending the run. Large target sets use a second choice within the selected group. Jev also checks sign-in and bot walls on fresh pages, picks what a page says from its quoted spans, and ends the run by choosing DONE.
- **An LLM helps at the two ends.** With no start page given, it proposes the first address from the task. When Jev says an answer has to be put together from more than one fact, it writes that answer from the notes. A run given a start page whose answers Jev can quote calls no LLM.
- **Code owns the gates.** Freshness and hit-tests before every input, no automatic retry of a mutation, authorization for irreversible actions, secret resolution and redaction, and budgets. Code does not second-guess a DONE: the run ends `complete` with what Jev read, and a requested record that cannot be filled ends `unverified`.

That loop runs in Rust, in [crates/](../crates): `fastbrowse-agent` holds the run loop, `fastbrowse-browser`
the page and its indexing, `fastbrowse-clients` the Jev and LLM clients, and `fastbrowse-py` builds them as
`fastbrowse._native` into the wheel ([ADR 0004](adr/0004-rust-core-by-default.md)).

## Authorization

Jev judges every model-selected click, including links, navigation to a caller-supplied address, every Enter press and acceptance of
confirm, prompt and before-unload dialogs. Code-selected pagination is exempt. One rule applies: the action
proceeds when Jev judges it not irreversible, or when the run is authorized and Jev judges the task asked
for it; otherwise the run stops at `needs_confirmation`.

A submission of a form that holds more than one password field changes an account's credentials whatever
the page calls it, so Jev is not asked whether it is irreversible: an unauthorized run stops at
`needs_confirmation`, and an authorized one proceeds only when Jev judges the task asked for it. A refusal
is recorded as a failed step with a reason. The classifier can be wrong, so this gate is not a guarantee
that every externally visible change is detected.

## Navigation

The operation choice can open another literal HTTP(S) address from the caller's task. The address is a
closed choice, revalidated before dispatch, and uses the browser's origin grant and live access checks.
Page instructions cannot add addresses to that choice.
When a site failure that can clear stands on the current page (an HTTP 408, 419, 429 or 5xx status, an address
the browser could not open, or a page Jev judges to report that the site failed), the page's own address joins
that choice, so Jev can reload it. Code states the failure and how many steps it has stood in the step state's
`page_facts`, with the reminder that a failure is not the task's answer; Jev picks the reload, a retry control, or
another route. A run whose stall budget runs out on such a failure ends `unavailable` with its address.
An editable search field can be filled directly, including when focusing it opens an editor. Opening or
focusing the field does not enter a query.

## Reading and citations

READ is one of Jev's operations. On a page that has changed, Jev is also asked whether it shows something the task
asks to be told that the notes do not hold yet; when it does, and Jev picked another operation, the run reads first,
because an order total is gone once the order is placed. The step state's `page_facts` tell Jev when a page was read
and has not changed since, and when it changed after it was last read. Every READ Jev picks goes to the reader. A read
that adds no new source quote counts toward the stall rule like any other step that moved nothing.

Fetch reads the same way: by default a run reads pages over HTTP and follows links there, with the page's form controls
offered as what it has. A step only a browser can take starts one on the same page, and the run carries on with
what it knows. `fetch` forbids the browser, and `browser` starts one from the first step.

Jev moves through a list. A `rel="next"` link is a stated fact on its element, and every other link keeps its href,
so choosing the next page is an ordinary step.

A read offers Jev one bounded choice over the page's quoted spans, with two more answers: the page does not hold the
answer, or the answer must be composed from more than one fact. On a page with more spans than one choice can offer, a
relevance pass narrows them first. Jev's pick stands. Code cut the spans from the capture, so neither Jev nor the LLM
writes page text. A span Jev picks becomes a fact, and identical values from the same page and frame share one choice
with all their source contexts. When Jev says the answer must be composed, every candidate is kept as a fact and the
answer is written once, when the run ends.

Answer claims use numbered Markdown links built from those notes. `RunResult.citations` exposes the cited facts and
text-fragment deep links; unused facts have no citation. The LLM writes a composed answer from the notes, and a claim
citing nothing the notes hold is dropped, so every claim stands on its quotes. Otherwise the answer is drafted from
the facts Jev picked.

## Prompt limits and progress

`Config.observation` names the group size for group-then-element choices. `Config.tokens` names the input budgets. A
step request shows Jev the whole viewport, working notes and history; one that does not fit Jev's limit loses its
oldest actions first and then has its long texts cut, each cut saying what it left out. Cut page text carries a marker
when there is room for one; an excerpt too small to carry the marker is empty.

A money or time limit keeps collected facts and their citations in a partial answer. The status stays
`budget_exceeded`, and producing the partial answer makes no additional model calls. A time budget stops
active work; the return also waits for owned requests and browser cleanup. Injected clients must honor
cancellation and finish their cleanup. Returning while a paid request still runs would lose its cost receipt.

Only visible effects or added evidence count as progress. Rewriting the value already in the observed
field cannot count, even when it opens an autocomplete popup. `StallRules` has one stall rule: a step that
changes nothing, a read that adds nothing, or a return to a page state already seen with nothing read since is no
progress, and `unchanged_actions` such steps in a row ask Jev again. A page state not seen before or a read that adds
evidence resets the count and renews the `max_recoveries` budget. The new ask names what failed and leaves the stalled
action out of its options, and Jev's pick is the next decision on that page. Past `max_recoveries` the run ends `stuck`,
or as the site failure standing on the page. An HTTP error page goes to Jev like any other, with its status among the
step's page facts, so Jev reloads it when the status can clear, goes back or picks another link; code only waits out a
403, 429 or 503 once per document.

## Run events and images

`on_event` receives a `BrowserEvent` followed by `StepEvent` objects. Each step includes added facts,
their reader and source links in `StepResult.facts`, and any available explanation in `StepResult.note`.
`Config(step_frames=True)` adds a PNG after each step unless a fresh observation shows a resolved secret.
`Config(step_choices=True)` adds `StepResult.choices`, the controls Jev was offered for the decision the step carried
out, after any shortlist or cap, with Jev's pick marked `chosen` (for a drag, the destination too).

`on_frame` receives JPEG bytes from the active tab. Frames are acknowledged after the async handler returns;
there is no fixed frame rate, and only the latest pending frame is retained. Delivery runs separately from
the agent, and handler failures are logged. A handler must honour cancellation: it runs on the caller's event
loop, so one that swallows `CancelledError` keeps that loop alive and `asyncio.run` waits for it at shutdown. Live images and MP4 recordings are held back from the moment a secret is typed, and whenever the page is read showing one, until a reading shows none;
a recording holds its last clean frame meanwhile.

## Cursor feedback

The run shows where the agent is about to act, through the Cua Driver's synthetic cursor, whenever its tab is on
screen: by default it is drawn wherever it can be placed, `--cursor` (`run_task(cursor=True)`) also brings the
window to the front so it always can be, and `--no-cursor` (`cursor=False`) turns it off. A plain `--headed` run
works in a background tab behind `about:blank`, so the default draws nothing there; nothing of the run is on
screen to mark. The click itself is still sent over CDP, so the overlay cannot change what an action did and is never
retried. The page hook is in `CdpPage.act`, where the hit-tested point is known; a secret's field is never
marked. `browser/cursor.py` owns one `cua-driver mcp` child per run, speaking JSON-RPC over stdio, and stops it
when the run ends, including on cancellation. The driver's `move_cursor` is sent in `window` scope only, so the
real pointer and focus never move.

The page point becomes a screen point from what the page reports: `screenX/Y`, `outerWidth/Height`,
`innerWidth/Height` and the zoom from `Page.getLayoutMetrics`. The top inset is `outerHeight - innerHeight * zoom`.
The cursor is drawn only when exactly one driver window matches the page's reported frame (which also shows the
two share a scale, so a scaled display is skipped), that window is in front of every other, the tab is visible and
the point is inside the viewport. Otherwise it is hidden. Once drawn it stays for the whole run, with the driver's
idle fade off: a navigation, a scroll or a point outside the viewport leaves it at its last position, and a watcher
hides it as soon as the tab stops showing, the frame can no longer be placed, or another window comes in front. Any
driver failure turns the feature off for the run.

The driver answers `move_cursor` only once its glide has landed, and at its own pace a glide across the screen takes
one to three seconds, longer than the half second between quick clicks: each glide was cut off by the next, so the
arrow landed on nothing, and a glide past the two-second call bound turned the feature off. The run asks
`set_agent_cursor_motion` for a fixed 550 ms glide, which lands on each click about half a second after it, and
waits up to six seconds for a move. Shorter glides land sooner but cross the screen in a few frames.

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
`covered`. When focus is in a suggesting field, it leaves that field first, which presses nothing, and the control
is checked again, so a list the field left open does not hold up the next one. Focus and the receiving field are checked before typing. Settling waits for an interactive
document and a quiet DOM. It does not guess at loading indicators by class name: a page still loading is read as
it stands, and the reader and Jev judge from its text whether to look again.
