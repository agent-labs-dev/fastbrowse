# Design

fastbrowse splits a browser agent into three owners:

- **Jev decides, reads and finishes.** A batched request chooses an operation and its possible targets from indexed controls. On a page denser than the step's control budget, a batched relevance check keeps the controls the task can use rather than the first ones in document order. A page too dense even on screen offers the controls that fit and scrolls for the rest rather than ending the run. Large target sets use a second choice within the selected group. Jev also checks sign-in and bot walls on fresh pages, picks what a page says from its quoted spans, and ends the run by choosing DONE.
- **An LLM helps where nothing can be copied.** With no start page given, it proposes the first address from the task. When Jev says a field takes a value the task asks for without stating it word for word, such as a search query, it writes that value from the task and the field's label, never from the page. When Jev says an answer has to be put together from more than one fact, it writes that answer from the notes. A run given a start page whose values and answers Jev can quote calls no LLM.
- **Code owns the gates.** Freshness and hit-tests before every input, no automatic retry of a mutation, authorization for irreversible actions, secret resolution and redaction, and budgets. Code does not second-guess a DONE: the run ends `complete` with what Jev read, and a requested record that cannot be filled or a composed claim still unsupported after recovery ends `unverified`.

When a field value is copied, Jev selects its source and first word. If its end needs a separate choice,
each choice shows the complete text that would be typed from that first word to the candidate end.
Code copies the selected span unchanged. Earlier typing is shown as history, without assuming
rejection or requiring a different value when the page changed the field.

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

Screenshot facts use the `llm_image` reader label; Jev-selected facts use `jev_choice`.
The LLM screenshot reader records `image_read` calls separately from answer composition. Jev image limits apply
to Jev questions; the LLM reader uses its own client, which must refuse unsupported image input explicitly.
A successful screenshot reading that finds no answer is reused for identical pixels within one run.
A changed screenshot is read again; failed model calls are not cached.
Model screenshots check fresh page text for resolved secrets before capture. Image readings mask resolved
secret values before they enter notes and evidence. A filled form that requires confirmation stops before
optional image reading.

A browser observation reports each control's `focused` state. A custom page may leave it unknown.
The optional `invalid` flag reports `aria-invalid` or a failed native constraint on a field that participates
in form validation. It describes the current field, without claiming that a submission occurred.
Jev can choose `blur` for an editable field observed as focused, leaving that field without pressing
another control or submitting its form. The field must still be the same node and hold focus when the
action runs. A replacement field is not retried. Page change and blur handlers run as they do when the
field is left; their observed effects reach the next decision. A focus change counts as a changed page
state, but does not by itself prove a value was set or accepted.

## Reading and citations

READ is one of Jev's operations. On a page that has changed, Jev is also asked whether it shows something the task
asks to be told that the notes do not hold yet; when it does, and Jev picked another operation, the run reads first,
because an order total is gone once the order is placed. The step state's `page_facts` tell Jev when a page was read
and has not changed since, and when it changed after it was last read. Every READ Jev picks goes to the reader. A read
that adds no new source quote counts toward the stall rule like any other step that moved nothing.

Fetch reads the same way: by default a run reads pages over HTTP and follows links there, with the page's form controls
offered as what it has. A step only a browser can take starts one on the same page, and the run carries on with
what it knows. `fetch` forbids the browser, and `browser` starts one from the first step.
Fetched documents have no pixels, so optional screenshots and step frames are skipped.

Browser captures record an icon-only link's destination with nearby container text, passing through empty layout
wrappers. The search stops at a paragraph even when it is empty, and never uses the page root as nearby text.

Jev moves through a list. A `rel="next"` link is a stated fact on its element, and every other link keeps its href,
so choosing the next page is an ordinary step. A page number drawn as text rather than a link or a button is offered
when it is focusable and has visible text, including a label wrapped in child elements. Hidden menu links do not
exclude the trigger that reveals them. An element already inside a control, or holding visible controls, leaves
actions to those controls.

A read offers Jev one bounded choice over the page's quoted spans, with two more answers: the page does not hold the
answer, or the answer must be composed from more than one fact. On a page with more spans than one choice can offer, a
relevance pass narrows them first. If no value choice fits, a page that fits Jev's input limit can still supply
facts for synthesis. Jev's pick stands. Code cut the spans from the capture, so neither Jev nor the LLM
writes page text. A span Jev picks becomes a fact, and identical values from the same page and frame share one choice
with all their source contexts. When Jev says the answer must be composed, every nonempty source block is kept as a fact and the
answer is composed when the run tries to finish, with recovery for a rejected draft.

A fresh read, including source quotes kept for synthesis, retires earlier facts from the same address when their
quotes are no longer shown. The facts stay as history. Quotes still shown and evidence from other addresses remain.
The composer, verbatim-answer decision and claim support check receive each source's known superseded state.
They judge the state the task asks about, retaining earlier quotes for historical answers and comparisons.

Jev selects verbatim answers only when every selected note can be repeated as written without unrelated text.
A value embedded in a broader note needs extraction through the composer and its support check.
Before choosing DONE, Jev sees the selected answer with its citation links separately from retained evidence.
The selected answer must give every requested fact and value; a value kept only as context does not answer it.
When composition or the caller's schema extraction is pending, the same decision judges whether the retained
sources cover the request; the generated result is checked before delivery.

Answer claims use numbered Markdown links built from those notes. `RunResult.citations` exposes the cited facts and
text-fragment deep links; unused facts have no citation. The LLM writes a composed answer from the notes. Jev judges
each final claim against its original cited quotes, including record identity and values, after any packing rounds.
Packing continues only when fewer requests are needed or the rewritten claim text shrinks. Removing source-state
metadata alone does not count as progress.
For a table, each cell is judged with the whole row and its quotes as context; every cell must be supported.
The composer and support check interpret a requested field by its meaning: descriptive prose can supply its
value without a matching heading, and that value contradicts a claim that the field was not shown.
The support question distinguishes facts about a quoted subset from claims about a whole set, and a source's
population counter from the number of records the answer reports.
A claim with no cited quote or no support judgment is withheld and ends `unverified`. When every check is
answered and Jev rejects a composed claim, the run spends an existing recovery to correct the draft or gather
more evidence. Rejected prose is feedback, never a source fact. A corrected draft must pass citation checks and
cover every requested fact or value. Runs with a caller-supplied output schema retain their extracted data and
end `unverified` when the composed prose fails its checks, without retrying prose composition. Exhausted recovery ends `unverified`, retaining supported claims; those
claims can be reused without another composition call only while their source evidence remains unchanged.
A run that already stopped keeps its stop status. Otherwise the answer is drafted from the facts Jev picked.

## Prompt limits and progress

`Config.observation` names the group size for group-then-element choices. `Config.tokens` names the input budgets. A
step request shows Jev the whole viewport, working notes and history; one that does not fit Jev's limit loses its
oldest actions first and then has its long texts cut, each cut saying what it left out. Cut page text carries a marker
when there is room for one; an excerpt too small to carry the marker is empty.

A money, time or step limit keeps collected facts and their citations in a partial answer. The status stays
`budget_exceeded`. A run whose answer must be composed can make reporting calls after its last browser step;
these calls share the run's receipts and still obey its dollar, time and model-call limits. The same deadline
wraps browsing and partial composition, so a stalled reporting call is cancelled when time runs out. A composing run
stops browsing a tenth of its time limit (at most 60 s) early to write that partial answer within the limit. A time budget stops
active work; the return also waits for owned requests and browser cleanup. Injected clients must honor
cancellation and finish their cleanup. Returning while a paid request still runs would lose its cost receipt.

Split batches collect provider receipts once. Custom decision clients that return partial answers without recording
receipts retain those answers' costs through the error.

Jev cost lines carry an optional `decision_purpose`: `action`, `authorization`, `relevance`, `read`,
`verify` or `locate`. The owning call supplies it before dispatch, so retries, provider failover and
cancellation keep the same attribution. LLM lines retain their separate `purpose` field. Calls made
directly through a decision client without an agent purpose omit `decision_purpose`; older receipts
remain readable. Purpose labels group calls without changing their charges.

Cancellation returns a receipt of reported steps, cost lines and finished recordings after cleanup. Protocol 2
keeps error code `-32002` and adds the optional `error.data` receipt; other errors omit it. Rust returns
`RunError::Cancelled(receipt)`. A cancelled cloud start keeps known charges as `unsettled`, or an `unknown`
browser charge when the provider has not answered. A failed start retains its final known charges.
An MCP browse error caused by runner cancellation puts the receipt in `_meta["fastbrowse.ai/cancellation"]`,
outside the browse result schema. By default MCP allows the runner its time limit plus a 60-second overrun grace before
forcing cancellation, then waits up to a further 60 seconds for cleanup and its receipt. A runner that does not
cooperate can therefore take its time limit plus 120 seconds to return. Client cancellation notifications
respond immediately without a receipt while cleanup continues for up to 60 seconds. The request ID is released
before that response, so cleanup cannot remove a later call's cancellation entry. Progress and log notifications
stop before the response, while cleanup continues. A run that does not finish
cleanup before the grace expires is abandoned without a receipt.

Only visible effects or added evidence count as progress. Rewriting the value already in the observed
field cannot count, even when it opens an autocomplete popup. Control changes are compared within one document;
a new document reports its controls instead of treating reused IDs as edits to the page left behind.
A navigation retains its observed source page, its controls and Jev's sign-in reading for decisions on the destination, including recovery and decisions after a redraw. The source is reported as a past observation whose current state is unknown. Credential availability uses each field's frame origin, as secret resolution does. When that origin has no stored secrets, the decision receives that fact before the historical source. The sign-in question distinguishes a restricted route from a task that requires an account; Jev still decides whether the wall blocks the task. Retained context is masked again before a model reads it. If the request exceeds Jev's documented input limit, retained page facts are shortened with an omission marker before the current page's text is shortened.

An empty observation waits on loading events before action history and progress are recorded.
The same blank state is waited out once until a readable page appears, so a later navigation gets its own wait.
Reloading remains restricted to an untouched opening page, since retrying a later page could repeat a submission.
`StallRules` has one stall rule: a step that
changes nothing, a read that adds nothing, or a return to a page state already seen with nothing read since is no
progress, and `unchanged_actions` such steps in a row ask Jev again. A page state not seen before or a read that adds
evidence resets the count and renews the `max_recoveries` budget. The new ask names what failed and leaves the stalled
action out of its options, and Jev's pick is the next decision on that page. Past `max_recoveries` the run ends `stuck`, `unverified` for a rejected composition,
or as the site failure standing on the page. An HTTP error page goes to Jev like any other, with its status among the
step's page facts, so Jev reloads it when the status can clear, goes back or picks another link; code only waits out a
403, 429 or 503 once per document.

## Run events and images

`on_event` receives a `BrowserEvent` followed by `StepEvent` objects. Each step includes added facts,
their reader and source links in `StepResult.facts`, and any available explanation in `StepResult.note`.
`Config(step_frames=True)` attaches a PNG to each step unless a fresh observation shows a resolved secret.
This safety reading preserves the pending action's guards and leaves download notices for the next decision.

Jev identifies whether the task requests a final image, text, both, or an action without a report. A requested
final screenshot does not require step frames. Image-only output skips answer composition and text evidence
requirements; a request for both keeps the text evidence checks. An HTTP-first run opens a browser when
its requested output needs pixels. A fresh secret check can withhold the image; a completed task without
its requested image becomes `unverified`. Python and Rust expose the PNG as `RunResult.final_frame`.
JSON clients receive a `screenshot` artifact with an `image/png` MIME type, SHA-256 digest and a base64 data URI.
`Config(step_choices=True)` adds `StepResult.choices`, the controls Jev was offered for the decision the step carried
out, after any shortlist or cap, with Jev's pick marked `chosen` (for a drag, the destination too).

`on_frame` receives JPEG bytes from the active tab. Frames are acknowledged after the async handler returns;
there is no fixed frame rate, and only the latest pending frame is retained. Delivery runs separately from
the agent, and handler failures are logged. A handler must honour cancellation: it runs on the caller's event
loop, so one that swallows `CancelledError` keeps that loop alive and `asyncio.run` waits for it at shutdown. Live images and MP4 recordings are held back from the moment a secret is typed, and whenever the page is read showing one, until a reading shows none;
a recording holds its last clean frame meanwhile.

## Recipes

A recipe is the steps a `complete` run took, stored as JSON. `--save-recipe FILE` writes one and `--recipe FILE`
loads it. The task must match the recorded task. When `--recipe` is supplied without `--start`, its recorded start
address is used for a new tab. An attached tab keeps its current page unless an explicit `--start` overrides it. A recipe start must be a credential-free HTTP or HTTPS URL with a usable host and port. Naming the same file for both flags replaces it atomically after
another completed run, including actions the agent chose when a saved step no longer matched.

A step identifies its full page address and a control's role, label, context, link and input attributes. Node ids
and selectors are not stored. Query strings, fragments and generated path segments must match too: two orders can
have identical controls without being the same order. A missing or ambiguous control is left to Jev.

Jev decides each action and judges the current page for answers, errors and sign-in walls. When its action agrees
with a matching remaining recipe step, even if an earlier action is still offered, the fill can reuse the saved value and the step reports
`decided_by: "recipe"`. Freshness checks, hit testing, authorization and secret handling still apply. Password
fields always use the live credential checks. A stored secret is kept only by name, caller inputs by name, and
ordinary task text as a literal. Values taken from page evidence, values written by the LLM and select options are decided live on every run. Recipes can
contain personal information from ordinary fields, so keep them private like the task that produced them. A step whose page address or control metadata reveals a resolved secret is omitted and decided live on replay.
If the task or start URL contains a resolved secret, no recipe is saved: a redacted task would fail the exact task match required for replay, and a redacted start URL would not open the original page.

Reading, answering and the check for `complete` always run live. Replay searches the remaining saved steps without
an arbitrary lookahead or miss cutoff, so a non-replayable step does not discard later reusable actions. A recipe is not evidence that the present task
has succeeded. After a mismatch, actions decided live can advance the saved sequence. A recipe save failure is written to server stderr without losing the completed run's result. SDK responses do not carry that warning; callers that require the file should check it exists on the server.

The public Python `fastbrowse.recipes.validate_recipe()` accepts a JSON string, UTF-8 bytes or a dictionary
and returns the normalized recipe, including defaults. Unsupported versions and invalid fields raise
`ValueError` before a browser opens. `recipe_schema()` exports the Rust-owned file schema, also kept in
`protocol/recipe.schema.json`. The schema checks field shapes; use `validate_recipe()` for URL semantics, including credentials and ports. Validation does not prove that a control exists or authorize an action.
To regenerate the schema, run `FASTBROWSE_UPDATE_GOLDEN=1 cargo test -p fastbrowse-core --test recipe_schema`.

## Cursor feedback

The run shows where the agent is about to act, through the Cua Driver's synthetic cursor, whenever its tab is on
screen: by default it is drawn wherever it can be placed, `--cursor` (`run_task(cursor=True)`) also brings the
window to the front so it always can be, and `--no-cursor` (`cursor=False`) turns it off. A plain `--headed` run
works in a background tab behind `about:blank`, so the default draws nothing there; nothing of the run is on
screen to mark. The click itself is still sent over CDP, so the overlay cannot change what an action did and is never
retried. The page hook is in `CdpPage.act`, where the hit-tested point is known; a secret's field is never
marked. MP4 recordings draw a timed cursor at those points, with a glide between actions and a pulse on clicks.
The recording uses ffprobe to read its frame dimensions. If that fails, it omits the cursor with a warning.
`browser/cursor.py` owns one `cua-driver mcp` child per run, speaking JSON-RPC over stdio, and stops it
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
`set_agent_cursor_motion` for fixed timing with a glide paced between 550 and 2,200 ms, and waits up to six
seconds for the move itself. The outer deadline also allows two seconds each for refreshing the session,
finding its window, restoring visibility and updating the pace. Shorter glides land sooner but cross the
screen in a few frames.

The glide a move is drawn with is paced to a travel speed, not to a duration: it takes as long as its distance
needs at `GLIDE_PX_PER_S`, between `GLIDE_MS` and `GLIDE_MAX_MS`. One fixed duration made a sweep across the page
the same length as a hop between neighbours, so the long move crossed four or five times faster and could not be
followed. The floor is the value tuned to land the arrow on every click, and the cap keeps one sweep from holding
up the step behind it. On the measured replay a sweep peaked at 1,350-2,400px/s and now peaks near 750px/s.
`glide_duration_ms` is only the glide length under `timing: fixed`; every other timing keeps the style's own curve
and ignores the number, so the run asks for `fixed` when it sets a glide and again when it re-paces one. The paced
value is sent with `set_agent_cursor_motion` only when it changes, and a driver without that tool keeps the glide
it was started with.

Limits: Linux X11 only; native Wayland, macOS and Windows are not verified. It needs `cua-driver`
0.28.3 or newer with the session cursor tools. Cloud and headless browsers are skipped. The desktop cursor has no
click pulse through `move_cursor`; recordings add one in the video. A docked DevTools panel or a pinch zoom hides
the desktop cursor, and a window partly covered by another counts as covered. CDP screenshots do not show the
desktop cursor.

## Browser capabilities over plain CDP

Verified over raw CDP against local headless Chrome and a Browser Use cloud browser:

| Capability | Mechanism | Local | Cloud |
|---|---|---|---|
| Own tab rendered | `Target.createTarget` + `Target.activateTarget` + `Emulation.setFocusEmulationEnabled` | pass | pass |
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

A PDF is kept the same way, inline or downloaded, and its text is read in a helper process: the host program run
again with `__fastbrowse-helper pdf-text 1` as its arguments, the PDF on stdin and a framed reply on stdout. The
run kills and reaps it after 10 seconds, when the run is cancelled, and when its runtime shuts down, and a parser
crash ends only the helper. Only that one process is killed, so each host names a program that is the helper
itself. The `fastbrowse` binary runs itself. The Python package runs its interpreter's own executable as
`-I -S fastbrowse/_helper.py`, never a venv's `python.exe`, which on Windows starts the interpreter as a child. It
does so when Python parsed the process arguments, detected by `sys.argv` differing from `sys.orig_argv`;
a frozen application or an embedding host that keeps its own arguments registers nothing. The Node addon runs the binary
selected by `binaryPath`, `FASTBROWSE_BINARY` or its platform package, and a Rust program calls `fastbrowse::helper::run_if_requested()`
first in `main`. A program that registers no helper still keeps the PDF, and the observation says its text was not
read (`[Downloaded file: ...] The file was kept, but its text was not read: ...`), naming why: no reader, a read
past 10 seconds, a file that is no PDF, or a reader that did not start or ended without an answer.

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

New popup pages pause until Fetch can capture their first response, including an inline PDF, and scoped sessions
install service-worker bypass before that first request. Renderer setup
follows resuming the page because a paused popup may have no renderer yet. A page outside the run's ownership
is resumed and detached.

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
`covered`. The screenshot fallback clicks only while a fresh viewport still matches the pixels Jev used to choose
its coordinates. When focus is in a suggesting field, it leaves that field first, which presses nothing, and the control
is checked again, so a list the field left open does not hold up the next one. Focus and the receiving field are checked before typing. Settling waits for an interactive
document and a quiet DOM. It does not guess at loading indicators by class name: a page still loading is read as
it stands, and the reader and Jev judge from its text whether to look again.
