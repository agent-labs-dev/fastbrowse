# Design

fastbrowse splits a browser agent into three owners:

- **Jev decides, reads and finishes.** A batched request chooses an operation and its possible targets from indexed controls. On a page denser than the step's control budget, a batched relevance check keeps the controls the task can use rather than the first ones in document order. A page too dense even on screen offers the controls that fit and scrolls for the rest rather than ending the run. Large target sets use a second choice within the selected group. Jev also checks sign-in and bot walls on fresh pages, picks what a page says from its quoted spans, and ends the run by choosing DONE.
- **An LLM helps where nothing can be copied.** With no start page given, it proposes the first address from the task. When Jev says a field takes a value the task asks for without stating it word for word, such as a search query, it writes that value from the task and the field's label, never from the page. When Jev says an answer has to be put together from more than one fact, it writes that answer from the notes. A run given a start page whose values and answers Jev can quote calls no LLM.
- **Code owns the gates.** Freshness and hit-tests before every input, no automatic retry of a mutation, authorization for irreversible actions, secret resolution and redaction, and budgets. Code does not second-guess a DONE: the run ends `complete` with what Jev read, and a requested record that cannot be filled, a composed claim still unsupported after recovery, or a requested step the page shows the site does not allow ends `unverified`. Once the run has acted, Jev's finish check answers whether every step the task asks for was carried out: `done`, `undone` (the run recovers), or `refused` (the run reads, answers and ends `unverified`).

When a field value or key chord is copied, Jev selects its source and first word. If its end needs a separate choice,
each choice shows the complete text that would be typed from that first word to the candidate end, with one more
answer: the value does not begin at that word. Jev then picks the first word again without it. The beginning is checked
the same way against copies that end where the value ends, and an end Jev rejects there is picked again.
Code copies the selected span unchanged. A key chord is copied only from the task, never from page text.
Earlier typing is shown as history, without assuming
rejection or requiring a different value when the page changed the field.

That loop runs in Rust, in [crates/](../crates): `fastbrowse-agent` holds the run loop, `fastbrowse-browser`
the page and its indexing, `fastbrowse-clients` the Jev and LLM clients, and `fastbrowse-py` builds them as
`fastbrowse._native` into the wheel ([ADR 0004](adr/0004-rust-core-by-default.md)).

## Authorization

Jev judges every model-selected click, drag or key chord, including links, navigation to a caller-supplied address, every Enter press and acceptance of
confirm, prompt and before-unload dialogs. Code-selected pagination is exempt. One rule applies: the action
proceeds when Jev judges it not irreversible, or when the run is authorized and Jev judges the task asked
for it; otherwise the run stops at `needs_confirmation`. The irreversibility and task-authorization questions receive the action's control context and drag destination, the non-sensitive value submitted with Enter, and non-sensitive values from the submitted form. Sensitive values are excluded.

A submission of a form that holds more than one password field changes an account's credentials whatever
the page calls it, so Jev is not asked whether it is irreversible: an unauthorized run stops at
`needs_confirmation`, and an authorized one proceeds only when Jev judges the task asked for it. A refusal
is recorded as a failed step with a reason. The classifier can be wrong, so this gate is not a guarantee
that every externally visible change is detected.

These questions are asked with the task and the page's origin only, never the page's content, so a page
cannot argue an action through. An authorized run also asks, in the same request, whether the task asks for
the action only in some state of the page or site ("reply if nobody has answered yet"). When it does, or the run
has already carried out a commit, and the action commits or leaves something the site keeps, a second Jev request
asks, as a choice, whether in the state the page shows the task says to do it with these values, says not to, has
been done already by the run's earlier steps and is not asked for again, or cannot be told. It does not
say the action is conditional, so a step the task asks for outright before a condition on a later one is
judged as asked. The same request asks which of the task's values the action uses: those to use at first, or
those the task gives only once something happens ("if the site rejects anything, use SW1A 1AA"). For the later
values the action also needs a yes that the run has done what the task asks first and the page or a recorded
effect shows the site responded the way the task gives those values for, so a step that skips ahead to a
correction is refused. That request gets the masked page text, controls, media, dialog and tabs, with the notes and
recent actions, and can only refuse: the action proceeds only on a yes, and any other answer refuses it once
so the run can read the page or finish.
Repeated refusals spend recoveries and end the run `stuck`, never by committing.

Two rules come from the task itself, judged once per run by Jev from the task text and kept for the run.
When the task rules out downloading, a control the page says downloads a file is refused. When the task
rules out changing anything, a fill, a select or a click on a setting control (a checkbox, radio, switch,
option, or any control that reports a checked state) is refused, and from then on those operations are no
longer offered. Typing into a search box is exempt, since searching is how a read-only task finds things.
Authorization does not lift either rule: it lets a run commit what the task asks for, never what the task
forbids.

Unmarked password fields leave the current-or-replacement role to Jev using the task and page context.
Field count and order do not establish that role. An explicit `current-password` marker identifies an
existing credential. A `new-password` marker identifies a replacement within a multi-password form;
a lone marker remains contextual because sign-in pages also use it to prevent autofill. Replacement
values come only from the scoped `new_password` secret.

## Navigation

The operation choice can open another literal HTTP(S) address from the caller's task. The address is a
closed choice, revalidated before dispatch, and uses the browser's origin grant and live access checks.
Page instructions cannot add addresses to that choice.
When a site failure that can clear stands on the current page (an HTTP 408, 419, 429 or 5xx status, an address
the browser could not open, or a page Jev judges to report that the site failed), the page's own address joins
that choice, so Jev can reload it. Code states the failure and how many steps it has stood in the step state's
`page_facts`, with the reminder that a failure is not the task's answer; Jev picks the reload, a retry control, or
another route. A run whose stall budget runs out on such a failure ends `unavailable` with its address.
On each page state it has not judged yet, Jev is also asked whether the page says an item the task requires
cannot be bought, such as one marked sold out, with no way on the site to get it. A yes ends the run
`unavailable` before the step: the run's LLM names the item and the reason from the task and the page text, and
the page's block holding the words it quotes is the stop's evidence.
An editable search field can be filled directly, including when focusing it opens an editor. Opening or
focusing the field does not enter a query.

Screenshot facts use the `llm_image` reader label; Jev-selected facts use `jev_choice`.
The LLM screenshot reader records `image_read` calls separately from answer composition. Jev image limits apply
to Jev questions; the LLM reader uses its own client, which must refuse unsupported image input explicitly.
Screenshot readings pass the same support and requested-coverage checks as composed text before completion.
When Jev is asked what output the task wants, it also says whether the task asks how the page looks, such as
its colours, styling, artwork or layout. For such a task the screenshot is read when the run answers, on any
page and beside what its text answered, and the reader describes what the image shows of the look as well as
quoting text. A page styled only by CSS states its colours nowhere in its text, so the image is the evidence.
A successful screenshot reading that finds no answer is reused for identical pixels within one run.
A changed screenshot is read again; failed model calls are not cached.
Model screenshots check fresh page text for resolved secrets before capture. Image readings mask resolved
secret values before they enter notes and evidence. A filled form that requires confirmation stops before
optional image reading.

Content shown only under the pointer is reached by a hover target, which moves the real pointer there. An
element is offered as one when a stylesheet hover rule reveals hidden content inside it, or when it is a card
of a list that shows a picture and holds no control of its own, because a script may render such a card's
controls only while a pointer is over it. The next observation offers what appeared. A control fading in under the
pointer counts as shown from the start of its fade, unless it ignores the pointer until the fade ends, and one fading
out counts as gone. A hover only shows content, so a task to carry out is not done by hovering alone.

An element with no role, tabindex or pointer cursor is offered as a button when a script gave it a click, mousedown,
pointerdown, mouseup or pointerup listener, as a Vue or React handler on a bare icon is. The listeners are read with
Chrome's own `getEventListeners`, which only an evaluate with DevTools' command line API has. That evaluate runs once
per document and keeps the function where the page cannot reach it, so the page sees no hook, and each observation asks
it afresh about the elements it weighs, so a removed listener takes its control away. A listened
element follows the rules a pointer cursor does: one inside a control, or holding one, is not offered, and neither is
a container whose listener hears its children's clicks while one of them is pressable. Listeners on the window, the
document, the root element and the body hear the whole page, so they mark nothing.

A browser observation reports each control's `focused` state. A custom page may leave it unknown.
The optional `invalid` flag reports `aria-invalid` or a failed native constraint on a field that participates
in form validation. It describes the current field, without claiming that a submission occurred. A field
that fails a native constraint also carries `validation_message`, the browser's own words for why, which
Jev sees with the field: a refused submit shows that reason only in a browser bubble, not in page text.
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

With `EXA_API_KEY`, Jev can choose a web search at the start or during a run. The LLM proposes queries from the
task, current page and evidence; Jev picks a query. Exa auto search returns up to ten results. Every returned result
field is kept in a citable record, and result addresses become links that use the existing navigation and origin
checks. The results view retains the current browser session. Repeating identical results counts as no progress.
A quote from result metadata cites the API response address; open the result to obtain a citation from its page.
The ledger reserves the documented search price before dispatch and records the provider receipt, or its bounded
price when the receipt is unavailable. No key leaves search unavailable.

Fetch reads the same way: by default a run reads pages over HTTP and follows links there, with the page's form controls
offered as what it has. A disabled control (`disabled`, `aria-disabled="true"` or `inert` on the element) is not
offered, as a browser capture leaves it out, so a disabled next link is no next page. A step only a browser can
take starts one on the same page, and the run carries on with
what it knows. A served page is not the page a browser shows: it holds text a stylesheet hides and lacks what scripts
build. So a run over HTTP that took no step cannot finish a task that asks for one, whatever it asks to be told: a
browser does it.
`fetch` forbids the browser, and `browser` starts one from the first step.
Fetched documents have no pixels, so optional screenshots and step frames are skipped.
JSON responses, including `application/*+json`, and other `text/*` responses are read as quoted text blocks.
JSON and delimited data keep their whitespace and literal values, without HTML parsing or numeric conversion.

Browser captures record an icon-only link's destination with nearby container text, passing through empty layout
wrappers. The search stops at a paragraph even when it is empty, and never uses the page root as nearby text.

Browser captures write each checked checkbox, radio or switch as `Label: checked`, a native one hidden behind a
visible label included, so text a page re-renders for the option chosen, such as an order total per shipping method,
is read with the option it belongs to.

Browser and fetch captures write a `time` or custom element's `datetime` in brackets after the text it shows, so a
relative date such as "3 weeks ago" keeps the exact date the page holds. `ins` and `del` are excluded, and nothing is
added when the shown text already states the value.

Browser and fetch captures leave a footnote reference out of the text it marks, so an infobox cell reads
`41,798,407 (37th)`, not `41,798,407[3] (37th)`. A reference is told by structure: a link marked `doc-noteref`, or
an in-page link whose target links back to that link, or to a wrapper showing only it, as Wikipedia, GitHub and
Pandoc footnotes do. A table of contents entry or a "back to top" link has no such back link and stays. The notes
themselves stay where the page lists them. A citation quoting across a removed marker can miss its text-fragment
highlight, since the page still shows the marker there.

Jev moves through a list. A `rel="next"` link is a stated fact on its element, and every other link keeps its href,
so choosing the next page is an ordinary step. A page number drawn as text rather than a link or a button is offered
when it is focusable and has visible text, including a label wrapped in child elements. Hidden menu links do not
exclude the trigger that reveals them. An element already inside a control, or holding visible controls, leaves
actions to those controls.

A field is named by its label, never by its value. A select or a combobox or listbox widget is not named from the
options, chips or buttons it shows. A widget wrapping exactly one field borrows that field's label, and its own label
comes first. A field with no label is offered by its role under its section, such as a fieldset legend, and the text it
shows never names that section. A control whose own clip or clip-path leaves it no area is not offered, as a tag
picker keeps the select it replaces on the page that way, and a hidden checkbox, radio or file input is reached through
its label.

Select on a select that allows more than one choice toggles the one option Jev picks, as a Ctrl-click does, and leaves the
others as they were. Jev is asked which option to toggle and told which are selected now. A change handler that puts the
selection back fails the step, as on a single select, and the action guard and page fingerprint read every selected
option, so choosing or removing any one of them is a change.

Press on an editable field sends the key or key chord Jev copied from the task. Its modifiers and DOM key are sent
as one browser input, and a chord that can submit or save goes through the same irreversibility and authorization gates
as Enter.

Scrollable containers are indexed separately for each overflowing axis, including open shadow roots and accessible
frames. Jev chooses a scroll area and an available direction. Code moves that container by its visible width or
height; horizontal right-to-left areas retain physical left and right directions. Container offsets contribute to
the page fingerprint, so a scroll that exposes content counts as progress even when its text is unchanged. A scroll
without a target retains viewport scrolling.

A read offers Jev one bounded choice over the page's quoted spans, with two more answers: the page does not hold the
answer, or the answer must be composed from more than one fact. On a page with more spans than one choice can offer, a
relevance pass narrows them first. If no value choice fits, a page that fits Jev's input limit can still supply
facts for synthesis. Jev's pick stands. Code cut the spans from the capture, so neither Jev nor the LLM
writes page text. A span Jev picks becomes a fact, and identical values from the same page and frame share one choice
with all their source contexts. When Jev says the answer must be composed, every nonempty source block is kept as a fact and the
answer is composed when the run tries to finish, with recovery for a rejected draft. The recovery decision is told
which claim failed which check, and a draft rejected for leaving out something the task asks for is shown to it whole,
since no one claim is to blame and only the draft says what it lacks.

A fresh read, including source quotes kept for synthesis, retires earlier facts from the same address when their
quotes are no longer shown. The facts stay as history. Quotes still shown and evidence from other addresses remain.
The composer and the claim support check receive each source's known superseded state.
They judge the state the task asks about, retaining earlier quotes for historical answers and comparisons.

An answer that must be composed is always composed and checked. The composer is not skipped by asking Jev of each
note on its own whether it is the answer as written: such a question cannot tell which of two similar notes, such as the
headings of an opener and the window it opened, is the one the task asks about.
Before choosing DONE, Jev sees the selected answer with its citation links separately from retained evidence.
The selected answer must give every requested fact and value; a value kept only as context does not answer it.
When composition or the caller's schema extraction is pending, the same decision judges whether the retained
sources cover the request; the generated result is checked before delivery.

Answer claims use numbered Markdown links built from those notes. `RunResult.citations` exposes the cited facts and
text-fragment deep links; unused facts have no citation. The LLM writes a composed answer from the notes. Jev judges
each final claim against its original cited quotes, including record identity and values, after any packing rounds.
A count, total, minimum or maximum, or a place after one such as the second cheapest, is never written or judged as a number. The composer marks it as a tally with
the kind of record and the field, and Jev answers one question per note read: for a count, whether it holds exactly one
such record, and for a total or extreme, whether it holds one with its field, then which number in it is the field. A date, ISO or a day, month name and year, is offered whole with its year beside it, and its day and month are not offered alone, since neither orders anything; code orders dates for an extreme and never adds them. A
record's name, heading or a link to it holds no value, so it is not counted as a record without one. Code counts, adds
or compares, writes the value into the claim and cites every record it rests on. For a count per group, such as quotes
per author, the composer names only the group its claim counts: the run's LLM copies each record's group from its
quote, a read that is not text of that quote leaves the count unstated, and code groups the records, counts each group
and checks the claim's place among all of them. A minimum or maximum the claim
record's name, heading or a link to it holds no value, so it is not counted as a record without one. A record read
from a screenshot or an element's markup is in the reader's words, so Jev is also asked which record on the same page
it repeats, if any, and a repeat is not counted again. Code counts, adds
or compares, writes the value into the claim and cites every record it rests on. A minimum or maximum the claim
answers with its record's name instead keeps that name, and only the record check judges it. Such a claim gets no support question
over the value: Jev is asked only whether the claim, with `[computed]` in the value's place, states nothing besides the
tally. For a minimum or maximum both questions are choices: whether the claim states the value, the record holding it,
both, or something else as well, and which record it names, none, one of those code found at the extreme, or another.
A failed check tells the composer which it was, such as a claim that also lists each record's value, which then goes in
a claim of its own.
Packing continues only when fewer requests are needed or the rewritten claim text shrinks. Removing source-state
metadata alone does not count as progress.
For a table, each cell is judged with the whole row and its quotes as context; every cell must be supported.
The composer and support check interpret a requested field by its meaning: descriptive prose can supply its
value without a matching heading, and that value contradicts a claim that the field was not shown.
The support question distinguishes facts about a quoted subset from claims about a whole set, and what a counter
on the page says about its source, which supports a claim that restates it, from the number of records the answer
reports.
Initial and repaired compositions are checked
for both claim support and requested answer coverage in one decision batch. A claim with no cited quote or no
support judgment is withheld and ends `unverified`. When every check is answered and Jev rejects a composed claim, the run spends an existing recovery to correct the draft or gather
more evidence. Rejected prose is feedback, never a source fact. Code keeps each claim that passed every
check, with its citations, word for word in the corrected draft, and the composer writes only the rest, unless the
coverage or step check failed, which fault the claims together; a claim it
writes again with the same text is kept once. A rewrite that retyped a passed claim could change a copied name and
fail where the first draft had passed. A corrected draft
must pass citation checks and cover every requested fact or value. If the combined answer exceeds Jev's input limit for the coverage check, individually supported claims remain available as an unverified partial answer; the result identifies the coverage limit. The coverage check judges the written
facts and values, not the form the task asks for, such as JSON, since code writes every answer as claims; citation
support and browser actions have separate checks. A whole-answer check beside coverage
judges the claims against the steps the run carried out on the page: an answer that says something was not done, or
already existed, when one of those steps may have done it has every claim withheld, and a run still finishing spends a
recovery to write it again. Only that rewrite gives the composer the steps, as the run's own record and never as
citations; on every other draft a long run's clicks would crowd out the notes it writes from. Runs with a caller-supplied output schema
spend the same recovery, and when it ends without a checked answer they retain their extracted data and end `unverified`. Recovery is not
attempted when a claim lacks a retained source quote or packed composition cannot reduce to one request.
Exhausted composition recovery retains supported claims and ends `unverified`, unless a missing field value or
site failure establishes another status. Those claims can be reused without another composition call only while
their source evidence remains unchanged and the run has taken no further step. A later nonempty checked repair replaces the retained draft; an empty
repair preserves the earlier draft. Other stops also retain checked partial claims alongside their status,
wall or budget details.
A run that already stopped keeps its stop status. Otherwise the answer is drafted from the facts Jev picked, with
the same words picked on more than one page stated once and citing each. The request that checks the draft covers
the task also asks whether each picked text is only what the task asks, in the form it asks for. A pick is a span code
cut from the page, such as a table cell or a sentence, and can hold the value beside other text, or be a whole date
where the task asks for its year. When it holds more, the composer writes the answer from the picks and their context,
told that they held more than the task asks, and its claims pass the same checks as any composed answer.

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
A navigation retains its observed source page for decisions on the destination, including recovery and decisions after a redraw. The source's text, controls and Jev's sign-in reading are given only while a wall is suspected on the destination: the sign-in question is asked on that step, the destination's sign-in reading is above even odds, or it answered with a 4xx status. Otherwise the decision gets only the source's URL and title. The source is reported as a past observation whose current state is unknown. Credential availability uses each field's frame origin, as secret resolution does. When that origin has no stored secrets, the decision receives that fact before the historical source. The sign-in question distinguishes a restricted route from a task that requires an account; Jev still decides whether the wall blocks the task. Retained context is masked again before a model reads it. If the request exceeds Jev's documented input limit, retained page facts are shortened with an omission marker before the current page's text is shortened.

A navigation or a step settles once no frame of the tab, embedded frames included, is still loading its document
up to DOMContentLoaded, within the same settle deadline. A frame that loads after its page is ready is part of what
the navigation opened, so its controls are in the first observation and the next step's effect does not claim them.

An empty observation waits on loading events before action history and progress are recorded.
The same blank state is waited out once until a readable page appears, so a later navigation gets its own wait.
Reloading remains restricted to an untouched opening page, since retrying a later page could repeat a submission.
`StallRules` has one stall rule: a step that
changes nothing, a read that adds nothing, a return to a page state already seen with nothing read since, or a
further return after one return has already counted for the same evidence is no progress, since one read buys one return. `unchanged_actions` such steps in a row ask Jev again. A page state not seen before or a read that adds
evidence resets the count and renews the `max_recoveries` budget. The new ask names what failed and leaves the stalled
action out of its options, and Jev's pick is the next decision on that page. Before a run would end
`stuck` past `max_recoveries`, the LLM proposes fresh known addresses using the current observation, evidence
and history. Code excludes the current page and routes already offered, opened or walled. Jev chooses whether
to take a proposed route; authorization and spending limits still apply. Without a fresh route the run ends
`stuck`, or `unverified` for a rejected composition, unless an unfilled field requires `needs_input` or a site
failure establishes another status. An HTTP error page goes to Jev like any other, with its status among the
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

Jev judges every page for answers, errors and sign-in walls, and chooses every action. When the next saved step is
on the page it was saved on, the original run read nothing before it, exactly one live control is the one it names,
and a fill has a saved value the field may take, that control is the only one Jev is offered. Jev can take it, read,
or stop, and it is not sent a screenshot or asked to shortlist the page's controls. A step Jev declines, or one that
fails, is decided with the whole page in view from then on. Any other step is decided as in a fresh run, and when
Jev's action agrees with a matching remaining recipe step, even if an earlier action is still offered, the fill can
reuse the saved value. A step carried out from the recipe reports `decided_by: "recipe"`. Freshness checks, hit testing, authorization and secret handling still apply. Password
fields always use the live credential checks. A stored secret is kept only by name, caller inputs by name, and
text copied word for word from the task as a literal. Values taken from page evidence, values the LLM composed and select options are decided live on every run. Recipes can
contain personal information from ordinary fields, so keep them private like the task that produced them. A step whose page address or control metadata reveals a resolved secret is omitted and decided live on replay.
A recipe serves any run of the same task, so a value that changes between runs belongs in a named input rather than
in the task: `fastbrowse "Search Wikipedia for the person named in input person" --input person="Ada Lovelace"
--save-recipe r.json`, then the same task with `--input person="Grace Hopper" --recipe r.json`. Jev reads the task
with each value it names shown beside the name ("input person (Ada Lovelace)"), and a fill that types an input's
value is saved as the input's name, so the replay types the new value. The recipe keeps the task as written. Pages after it, such as the article, have a different address
and are decided live.
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

The run shows where the agent is about to act, through the Cua Driver's synthetic cursor on X11 or deskpilot's
agent cursor on Hyprland, whenever its tab is on screen: by default it is drawn wherever it can be placed, `--cursor` (`run_task(cursor=True)`) also brings the
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

On a Wayland session the cursor is drawn only under Hyprland, through `deskpilot mcp` (`FASTBROWSE_DESKPILOT` or
`deskpilot` on `PATH`), since `cua-driver` lists no windows there. Chrome on native Wayland cannot know where its
window is, so the window is the one listed by deskpilot's `windows` with the browser's pid and the page's
`outerWidth x outerHeight`, and the point is sent to `move` in that window's pixels. Nothing is drawn when no window
or more than one matches, the window is on a workspace no monitor shows, or any other window on screen overlaps it:
Hyprland gives no stacking order, so an overlap cannot be shown to be behind. `move` reports the global point it
drew at, and a point other than the window's position plus the one sent turns the feature off, since deskpilot
scales a point by its latest shot of that window when it holds one. deskpilot has no session or hide: its arrow
fades after eight idle seconds, so an arrow drawn just before another window covers the browser can stay that long.
`take_pointer`, `show` and `focus` are never sent, so the user's pointer and focus stay where they are.

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

Limits: Linux only, on X11 or Hyprland; other Wayland compositors, macOS and Windows are not verified. X11
needs `cua-driver` 0.28.3 or newer with the session cursor tools, and Hyprland needs deskpilot with its plugin
loaded. Cloud and headless browsers are skipped. The desktop cursor has no
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

Downloaded XLSX workbooks, DOCX documents and ZIP archives use the same killable helper. Workbooks retain each
worksheet's name and each stored cell's row, column and value. Empty coordinates are omitted, so sparse sheets
are read without allocating the rectangle between their cells. Formulas are not evaluated. Word
text retains paragraph, table-cell, tab and line-break boundaries, with its package part named. Archive members
are read in memory, never extracted onto the host. Text, CSV, TSV, JSON, XML, Markdown and HTML members are read
as UTF-8 text; XLSX, DOCX and PDF members use their document readers. Other or invalid members remain named with
an explicit notice that their contents were not read. Nested archives are not expanded. Generic or missing MIME types use the download filename to select these readers.
The configured `max_download_bytes` also bounds aggregate decompressed container bytes and extracted output; a
document exceeding it is reported unread, rather than returning truncated text.

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
none: it takes the first page target whose title or URL contains `target_match`, or without one the first web
page, skipping browser-internal pages such as extension options, `chrome://`, `chrome-untrusted://` and
`devtools://` windows unless `target_match` names one, and putting `http`/`https` pages before Electron and app
windows. It owns nothing, so `closeTarget` is never sent. An attach with no usable window, scoped or not, as in a
fresh browser showing only `about:blank`, takes over an empty window (`about:blank` that no page opened, still
holding the empty document) when there is one, and otherwise opens one blank tab, keeping either open on exit
like the window it stands in for; it closes an opened tab only if setup fails. A new tab page is never taken
over: it can list the person's most visited sites. That page loaded before the session's
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
