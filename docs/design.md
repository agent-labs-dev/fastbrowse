# Design

fastbrowse splits a browser agent into three owners:

- **Jev picks.** A batched request chooses an operation and its possible targets from indexed controls. It also judges whether unread evidence should be preserved before interaction, and checks sign-in and bot walls on fresh pages. On a page denser than the step's control budget, a batched relevance check keeps the controls the task can use rather than the first ones in document order. A page too dense even on screen offers the controls that fit and scrolls for the rest rather than ending the run. On a page with more quotable spans than one choice can offer, the same kind of check narrows the passages before Jev picks a short fact. Large target sets use a second choice within the selected group. Code can follow a list's next-page link without an action-choice call.
- **An LLM reads and writes.** Proposing a direct address for the task while the start page loads, planning checkable requirements from the task alone, reading page content for answers when Jev cannot pick a short fact from quoted spans, writing non-secret field text, recovering when Jev is unsure, verifying completion in the uncertain band, composing the final answer.
- **Code owns the gates.** Freshness and hit-tests before every input, no automatic retry of a mutation, authorization for irreversible actions, secret resolution and redaction, budgets, and the definition of success: only `COMPLETE`, which requires every requirement evidenced.

## Authorization

Jev judges every model-selected click, including links, every Enter press and acceptance of
confirm, prompt and before-unload dialogs. Code-selected pagination is exempt. An authorized action whose
confidence reaches `Thresholds.sensitive_act_from` proceeds without that classification.

A refusal is recorded as a failed step with a reason. An unauthorized action with sufficient confidence
stops at `needs_confirmation`; an uncertain action goes to recovery. The classifier can be wrong, so this
gate is not a guarantee that every externally visible change is detected.

## Reading and citations

The policy's read assessment distinguishes useful evidence from editable query previews and irrelevant
content. Evidence is read before another interaction can remove it. Reads are deduplicated by document,
capture hash and unresolved information requirements, so changed content can be read again.

For a bounded set of short quoted spans, Jev chooses a scalar fact, requests synthesis, or judges the
requirement absent from the page. Identical values from the same page and frame share one choice with all
their source contexts. A selected repeated value retains every span through `Fact.basis`. Uncertain choices,
comparisons, partial evidence and paginated lists reach the LLM reader. `FactReader` records `jev_choice` or
`llm`. Neither reader writes page text: Jev picks a value from spans code cut from the capture, and the LLM
reader cites the capture's source blocks by id (one block, or
consecutive blocks of one frame from the chunk it was shown). Code copies the quote from those blocks, so a
table's escaped pipe or a record read as two lines cannot drop a fact, and a claim citing a block it was
not shown is rejected.

When quotes from the same address are already in notes, the short-fact batch can also ask whether the full
page adds relevant evidence. Literal diffs against matching source blocks help distinguish changed values
from cosmetic changes. A confident negative answer skips the read without marking any requirement evidenced.
Missing or uncertain answers use the normal readers. This check is omitted when the full comparison does
not fit the input budget or the read needs pagination, continuation or incomplete-comparison context.

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

`Config.observation` names the limits for viewport text, working notes and recent and earlier history.
`Config.tokens` names the input budgets and reader/composer output limits. Working notes can omit facts
with a count; verdict prompts retain all requirement evidence or stop at `observation_limit`. The notes
budget keeps a retained fact's basis with it; requirement evidence includes its transitive basis. The Jev
completion check reduces page text first to make room for that evidence. Cut page text carries a marker
when there is room for one; an excerpt too small to carry the marker is empty.

A money or time limit keeps collected facts and their citations in a partial answer. The status stays
`budget_exceeded`, and producing the partial answer makes no additional model calls.

Only visible effects or added evidence count as progress. Rewriting the value already in the observed
field cannot count, even when it opens an autocomplete popup. `StallRules` checks lack of progress,
repeated interactions and consecutive unproductive steps with the same unresolved requirements.
The latter two are recorded in `RunResult.would_fire` in `shadow` mode, the default; `armed` mode sends
them to recovery. A productive step
clears the plan-stagnation streak, and recovery resets the evidence used by all three checks.

## Run events and images

`on_event` receives a `BrowserEvent` followed by `StepEvent` objects. Each step includes added facts,
their reader and source links in `StepResult.facts`, and any available explanation in `StepResult.note`.
`Config(step_frames=True)` adds a PNG after each step unless a fresh observation shows a resolved secret.

`on_frame` receives JPEG bytes from the active tab. Frames are acknowledged after the async handler returns;
there is no fixed frame rate, and only the latest pending frame is retained. Delivery runs separately from
the agent, and handler failures are logged. Live images and MP4 recordings are held back from the moment a secret is typed, and whenever the page is read showing one, until a reading shows none;
a recording holds its last clean frame meanwhile.

## Browser capabilities over plain CDP

Verified with `cdp-use==1.4.5` against local headless Chrome and a Browser Use cloud browser:

| Capability | Mechanism | Local | Cloud |
|---|---|---|---|
| Own tab rendered | `Target.createTarget` + `Target.activateTarget` | pass | pass |
| Upload caller bytes | in-page `DataTransfer` + `File` on the input, `input`/`change` events (no host path needed) | pass | pass |
| Download bytes | `Fetch.enable` at Response stage for Document responses (download-attribute anchors included), `Fetch.getResponseBody` on `Content-Disposition: attachment` | pass | pass |
| Cross-origin iframe | `Target.setAutoAttach(flatten)` on the page session, evaluate in the iframe session | pass | pass |
| Popup ownership | `Target.targetCreated.openerId` equals our target | pass | pass |
| Dialogs | `Page.javascriptDialogOpening` + `Page.handleJavaScriptDialog` | pass | pass |

The cloud browser ignores `Browser.setDownloadBehavior(deny)`, so bytes come from response interception, never from the remote filesystem. Host-path `DOM.setFileInputFiles` is only valid for a browser on the same machine.

### Attached windows

A session normally opens its own tab and closes only the tabs it owns. With `BrowserConnection.attach` it opens
none: it takes the first page target whose title or URL contains `target_match` (or the first page, skipping
`devtools://` windows), and owns nothing, so `closeTarget` is never sent. That page loaded before the session's
new-document scripts were registered, so they are also evaluated once in it; otherwise freshness tracking would
start only at its next navigation. Turning target discovery on replays `targetCreated` for every window already
open, so an attached session turns it on only after recording those, and none of them joins the run. Popups of a
tracked window join without being owned. Windows with no
opener join only when `target_match` is set: an Electron main process opens its windows that way, and so does a
browser for each tab a person opens by hand.

Before a pointer press, the browser waits for a stable target and rechecks its guard and hit-test. A
replacement control must match the original semantics and receiving document; ambiguous matches are
refused. Focus and the receiving field are checked before typing. Settling waits for an interactive
document and a quiet DOM, with a bounded extra wait for visible loading indicators.
