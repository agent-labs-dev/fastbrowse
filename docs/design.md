# Design

fastbrowse splits a browser agent into four owners:

- **The driver acts.** One LLM conversation, with the LLM purpose `action`, drives every run. Each turn it is told what its last actions did, then shown the page as `[id] role "name" = value` lines with states, headings and visible text in page order, with the ids new since its last look marked `*`. It returns a chain of tool actions: click, type, type_secret (a secret by name), select, key, scroll, drag, goto, back, wait, dialog, screenshot, done and give_up. A screenshot comes back as an image on the next turn, and the last two stay in the conversation; on a control marked drawn, a click names the thing to click inside it as the screenshot shows it, and Jev places the click there. They run in order while the page keeps the same controls; once one opens, closes or loads something, the rest are dropped and the model is told. A page too long for the model's context loses its earlier pages first, then the text outside the viewport.
- **Jev judges what code gates on.** It answers the closed questions the gates need: whether an action is irreversible and whether the task asked for it, whether every step the task asks for was carried out, which quoted span answers a read, and whether a written answer holds against its quotes (claim support, tallies and coverage). It never picks the next step.
- **An LLM helps where nothing can be copied.** With no start page given, it proposes the first address from the task. It writes an answer that has to be composed from the notes.
- **Code owns the gates.** Freshness and hit-tests before every input, no automatic retry of a mutation, authorization for irreversible actions, secret resolution and redaction, and budgets. A `done` quotes page text word for word: a quote no page shown to the model holds is refused back to it, and so is an answer whose claims are not supported or leave out what the task asks, up to three times, without spending the stall recovery. A requested record that cannot be filled, a composed claim still unsupported after recovery, an answer its composer says leaves a requested value unestablished, or a requested step the page shows the site does not allow ends `unverified`. Once the run has acted, Jev's finish check answers whether every step the task asks for was carried out: `done`, `undone` (the driver is told and the run recovers), or `refused` (the run reads, answers and ends `unverified`). It judges from the current page and the run's actions, the first of which is the run's own opening of its start page, so a task that says to open a page and go on from it is not held undone for the opening.

That loop runs in Rust, in [crates/](../crates): `fastbrowse-agent` holds the run loop, `fastbrowse-browser`
the page and its indexing, `fastbrowse-clients` the Jev and LLM clients, and `fastbrowse-py` builds them as
`fastbrowse._native` into the wheel ([ADR 0004](adr/0004-rust-core-by-default.md)).

## Authorization

Jev judges every click, drag, key chord or file attached to an input (a page can upload a file the
moment it is chosen), including links, navigation to a caller-supplied address, every Enter press and acceptance of
confirm, prompt and before-unload dialogs. Code-selected pagination is exempt. One rule applies: the action
proceeds when Jev judges it not irreversible, or when the run is authorized and Jev judges the task asked
for it; otherwise the run stops at `needs_confirmation`, unless it has already carried out a commit the task asked
for: then the action is refused as a failed step and the driver is told why, so a run that posted what it was asked to
is not reported as waiting for confirmation. The irreversibility and task-authorization questions receive the action's control context and drag destination, the non-sensitive value submitted with Enter, and non-sensitive values from the submitted form. Sensitive values are excluded.

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
already produced the requested result according to the page or recorded effects and is not asked for again, or cannot be told. A prior executed input is reported as an attempt, not proof that its result happened. It does not
say the action is conditional, so a step the task asks for outright before a condition on a later one is
judged as asked. The same request asks which of the task's values the action uses: those to use at first, or
those the task gives only once something happens ("if the site rejects anything, use SW1A 1AA"). For the later
values the action also needs a yes that the run has done what the task asks first and the page or a recorded
effect shows the site responded the way the task gives those values for, so a step that skips ahead to a
correction is refused. That request gets the masked page text, controls, media, dialog and tabs, with the notes and
recent actions, and can only refuse: the action proceeds only on a yes, and any other answer refuses it once
so the driver can read the page or finish.
Repeated refusals spend recoveries and end the run `stuck`, never by committing.

Rules also come from the task itself, judged once per run by Jev from the task text and kept for the run.
When the task rules out downloading, a control the page says downloads a file is refused. When the task
rules out changing anything, a fill, a select or a click on a setting control (a checkbox, radio, switch,
option, or any control that reports a checked state) is refused. Typing into a search box is exempt, since searching is how a read-only task finds things.
When the task tells the agent not to take some action ("do not click Upload"), every action the gate asks about
is also asked about in the same request, against the task and the action alone: one the task says not to take,
whatever the page shows, is refused once, and so is one the question goes unanswered for. When the task forbids it
only in some state of the page ("do not click Upload while the preview shows confidential data"), a request of its
own with the masked page asks whether the page shows that state, and the action goes ahead only on Jev's answer that
it does not. Whether the task forbids anything is judged first, so a task that forbids nothing adds no question to
its gates. Authorization does not lift any of these rules: it lets a run commit what
the task asks for, never what the task forbids.

The driver types a secret by the name it gives, so the task decides which secret goes in which password field;
code never infers from field count, order or a `new-password` marker which credential a field takes (a sign-in
page also marks its one field `new-password` to prevent autofill).

## Navigation

The driver opens an address with `goto`. Code accepts a plain HTTP(S) address with no resolved secret in it, revalidates
it before dispatch, runs it through the navigation gate, and uses the browser's origin grant and live access checks.
When a site failure that can clear stands on the current page (an HTTP 408, 419, 429 or 5xx status, or an address the
browser could not open), the driver reads the failed page like any other and can reload it, use a retry control or take
another route. A run whose stall budget runs out on such a failure ends `unavailable` with its address.
An editable search field can be filled directly, including when focusing it opens an editor. Opening or
focusing the field does not enter a query.

Screenshot facts use the `llm_image` reader label; Jev-selected facts use `jev_choice`; quotes the chat driver cites when it calls `done`, found verbatim in the capture by code, use `llm_quote`.
The LLM screenshot reader records `image_read` calls separately from answer composition. Jev image limits apply
to Jev questions; the LLM reader uses its own client, which must refuse unsupported image input explicitly.
Screenshot readings pass the same support and requested-coverage checks as composed text before completion.
When Jev is asked what output the task wants, it also says whether the task asks how the page looks, such as
its colours, styling, artwork or layout. For such a task the screenshot is read when the run answers, on any
page and beside what its text answered, and the reader describes what the image shows of the look as well as
quoting text. A page styled only by CSS states its colours nowhere in its text, so the image is the evidence.
A successful screenshot reading that finds no answer is reused for identical pixels within one run.
A changed screenshot is read again; failed model calls are not cached.
Model screenshots check fresh page text for resolved secrets before capture, and none is taken once the run knows an
address credential (see below). Image readings mask resolved secret values before they enter notes and evidence. A filled form that requires confirmation stops before
optional image reading.

Content shown only under the pointer is reached by a hover target, which moves the real pointer there. An
element is offered as one when a stylesheet hover rule reveals hidden content inside it, or when it is a card
of a list that shows a picture and holds no control of its own, or a card whose script listens for the pointer
entering it (`mouseenter`, `mouseover`, `pointerenter`, `pointerover`, read from the element or from React's `__reactProps$` record of the handlers its root calls) whatever else it holds, because a script may render such a card's
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

The browser visits each changed frame, then transfers unchanged control rows by their existing node
ids and sends only changed rows and guards. The receiver reconstructs the ordered controls against the exact
document and previous read named by the delta. Each request names the receiver's baseline; a missing or mismatched
baseline returns a full read directly. Each frame read releases saved snapshots of detached child frames, preserving live child snapshots across tab switches. Rust retains the raw rows with their validated snapshot, so an unchanged response reuses that parse. A delta that cannot be reconstructed requests a full read. Page text,
modal state and other frame metadata travel in full. A read that enters open shadow roots or same-origin child
documents is kept like any other: each root it walks gets its own mutation observer and event listeners, and the
saved read records every frame's document and address, so a frame that shows another document, or becomes readable,
renews it.
Captures and citation spans keep their own read path. Context construction indexes peer labels once per read,
preserving shared labels when excluding a control.

Within a changed frame, structural queries and control names reuse their previous results while their inputs
match. A mutation invalidates the affected subtree and its ancestors. A name records the nodes, ARIA references,
label associations, visibility and native values it used, and checks those dependencies before reuse. Queries
are also tied to their document or shadow root. Layout and event listeners are read afresh, and action guards
query current state. Each saved snapshot or glance keeps its own pending custom-element definitions, since an
upgrade can change label ownership without a DOM mutation. Before reusing a read, the browser checks the
elements of every tree it visited for newly attached shadow roots, which also produce no mutation, and compares
each tree's live values, stylesheets, address, scroll, size and running animations. It also
compares the native pointer-listener checks used by the saved read. Each atomic read asks for a node's listeners
once; later action guards ask again. A saved read also compares the rendering facts it consumed: computed
style properties, visibility, element and text-node boxes, rendered text, live selector matches, focus and
live control or media values. Scroll controls also record extents and positions, overflow propagated from the
body, and the writing-mode, direction and flex styles that determine their axes. Generated CSS content can
change those extents without a DOM mutation. Those facts stay in the installed script's private closure and
name the exact document and read they validate. A CSS state can then renew a read without writing a DOM node or attribute.
The frame keeps each document or shadow root's element order and static control membership. A child-list
change renews order through saved element-child edges; unchanged siblings keep those edges. Attribute changes
renew membership, while geometry, listeners and render facts are read fresh. Full reads rebuild the index.
Text spans still come from the text walker and capture path. Each control's fields and context decisions
are derived from the current page. Reusing the rows of untouched controls was measured and does not pay: a changed
read is dominated by the native style, visibility, box and text reads that would prove a row unchanged, and the
hover, scroll and guard passes read the same facts again when the walk no longer has them. Saved reads compare fresh boxes and selector results directly with their
snapshots, without allocating new comparison arrays. Repeated heading and scope text reuses its normalized
lines only when a fresh native read returns the exact same text.

A control also carries `role_description` (`aria-roledescription`) and `description` (the text its `aria-describedby` points at), each as the page states it and only when present. The driver reads `description` beside the control (`described "..."`); `role_description` stays on the control and is not shown to the driver. A control in a list also carries `item_position` and `item_count` (`aria-posinset` and `aria-setsize`, or its place among the items of its nearest list), which the driver reads as `item 2 of 5`, because "first" is a place the labels do not give. A browser observation reports each control's `focused` state. A custom page may leave it unknown.
The optional `invalid` flag reports `aria-invalid` or a failed native constraint on a field that participates
in form validation. It describes the current field, without claiming that a submission occurred. A field
that fails a native constraint also carries `validation_message`, the browser's own words for why, which
the driver reads beside the field as `refused: "..."`: a refused submit shows that reason only in a browser bubble, not in page text.
A control the page disables (`disabled`, or inside `aria-disabled="true"`) stays in the observation with
`disabled: true` and no operations. The driver sees it listed as `disabled`, so a Save that an edit did not enable
reads as a disabled Save rather than no Save, and an action on it is refused with that reason. A step that enables or
disables a control says so in its effect, and the page state counts it. Replay counts no disabled control as a
match for a recorded step.

## Reading and citations

Browser and HTTP captures keep the document title as a citable `document_title` block. It has its own source span
and participates in the capture hash and secret masking. Read candidates name their source kind, so the browser
tab title stays distinct from a heading rendered in the page body. A title alone does not establish that a
script-rendered body was read.

Browser and HTTP captures keep a text label's `title` advice beside it as `(title: ...)`, within the
same record or table cell. The advice supplements the label rather than replacing it, so a shortened listing
name can retain its full native tooltip text as citable evidence.

With `EXA_API_KEY` and no start page, the LLM proposes an entry address and web search queries from the task, and
Jev picks one. Exa auto search returns up to ten results. Every returned result
field is kept in a citable record, and result addresses become links that use the existing navigation and origin
checks. The results view retains the current browser session. Repeating identical results counts as no progress.
A quote from result metadata cites the API response address; open the result to obtain a citation from its page.
The ledger reserves the documented search price before dispatch and records the provider receipt, or its bounded
price when the receipt is unavailable. No key leaves search unavailable.

Fetch reads the same way: by default a run reads pages over HTTP and follows links there, with the page's form controls
offered as what it has. A disabled control (`disabled` or `aria-disabled="true"` on the element) is kept offering
nothing, as a browser capture keeps it, and an `inert` one is left out, so a disabled next link is no next page. A step only a browser can
take starts one on the same page, and the run carries on with
what it knows. A served page is not the page a browser shows: it holds text a stylesheet hides and lacks what scripts
build. So a run over HTTP that took no step cannot finish a task that asks for one, whatever it asks to be told: a
browser does it. A served page with no links or controls is stated to the driver as leading nowhere only when nothing a
browser would add is missing from the read: no `<script>` the HTML standard runs (a classic script with a
JavaScript type and no `nomodule`, or a module, with a `src` or text, outside `<template>` and `<noscript>`), no
non-empty event handler attribute, such as `onload` or `onanimationstart` (every one the web's specs add to
elements, and those Chrome adds), in the page or a frame read into it,
and no frame left unread. Otherwise the driver is told what was left out and whether a step can open a browser: by
default a wait or other step it takes there opens one on the page, and with `fetch` none opens.
`fetch` forbids the browser, and `browser` starts one from the first step.
Fetched documents have no pixels, so optional screenshots and step frames are skipped.
A JSON response, including `application/*+json`, is read by its structure: each object key is a heading, and each
string, number, `true`, `false` and `null` is its own block under the path of keys and array indices that leads to it.
A string reads as its decoded text, a number as written, and repeated keys keep every value. A response that is not
entirely valid JSON, and other `text/*` responses, are read as one quoted text block. Neither is parsed as HTML, and
delimited data keeps its whitespace. A browser capture of a JSON document is read the same way, by structure, when all of its served text parses.

Browser captures follow block structure inside inline containers, including custom elements and elements with
`display: contents`, so nested headings and repeated records keep their own source spans. Adjacent inline prose
stays in one block. A link enclosing multiple blocks keeps its destination on each block and its native title
advice beside the first label. CSS-generated text remains readable when an ancestor's
`aria-hidden` removes a dialog's background from accessibility APIs while it remains rendered. Generated text
on an element marked `aria-hidden` itself stays excluded, preserving decorative icon handling.

Browser and HTTP heading paths follow heading ranks. A heading inside a repeated record ends earlier headings
of the same or deeper rank, without naming later records. Headings hidden by the capture do not change that
context; HTTP sees hidden attributes and inline styles, but does not apply stylesheets.

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

The driver moves through a list. A `rel="next"` link is a stated fact on its element, and every other link keeps its href,
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

Select on a select that allows more than one choice toggles the one option the driver names, as a Ctrl-click does, and leaves the
others as they were. The driver is shown which options are selected now. A change handler that puts the
selection back fails the step, as on a single select, and the action guard and page fingerprint read every selected
option, so choosing or removing any one of them is a change.

Press sends the key or chord the driver names with `key`, checked against the page's key grammar; a key it does not know is refused back to the driver. The driver can name any key or chord the grammar accepts, such as the arrows, Home, End, Space, Enter, Escape, Backspace, Delete or Ctrl+Home. A `button`, `link`, `checkbox` or `switch` is offered a press only when it declares an `aria-roledescription`, so a sortable card can be picked up with Space, moved with the arrows and dropped with Space.
Sliders, spin buttons, tabs, lists, trees, menus and grids are offered a press when focus can reach them. An explicit `aria-orientation` on a widget or its owning composite is included in its context, so the driver knows its direction. Widget value changes count as progress. A composite container is indexed for keyboard use when it has a `tabindex`.
An active descendant keeps DOM focus on its owner while receiving the chosen key. Native selects keep their
Select operation. The chord is stated to the irreversible-action classifier before dispatch.
When the task forbids edits, Jev also judges whether that chord changes a value or only moves through the page.
Its modifiers and DOM key are sent as one browser input, and a chord that can submit or save goes through the
same irreversibility and authorization gates as Enter.

Scrollable containers are indexed separately for each overflowing axis, including open shadow roots and accessible
frames. The driver scrolls an area, or the page when it names none, up or down. Code moves that container by its visible width or
height; horizontal right-to-left areas retain physical left and right directions. Container offsets contribute to
the page fingerprint, so a scroll that exposes content counts as progress even when its text is unchanged. A scroll
without a target retains viewport scrolling.

The passages a `done` quotes are the answer's evidence: code finds each word for word in the capture of the page the
driver was shown, the page `done` was called on first and then the latest earlier page that holds it, and cuts the span
as a fact. A quote no page holds is refused back to the driver. When the driver quoted nothing and the notes hold nothing
that answers, a finishing read offers Jev one bounded choice over the page's quoted spans, with two more answers: the
page does not hold the answer, or the answer must be composed from more than one fact. A `done` on notes that answer
nothing is a step short of the answer. On a page with more spans than one choice can offer, a
relevance pass narrows them first. If no value choice fits, a page that fits Jev's input limit can still supply
facts for synthesis. Jev's pick stands. Code cut the spans from the capture, so neither Jev nor the LLM
writes page text. A span Jev picks becomes a fact, and identical values from the same page and frame share one choice
with all their source contexts. When Jev says the answer must be composed, every nonempty source block is kept as a fact and the
answer is composed when the run tries to finish, with recovery for a rejected draft. The recovery is told
which claim failed which check, and a draft rejected for leaving out something the task asks for is shown to it whole,
since no one claim is to blame and only the draft says what it lacks. A run that stops with a rejected draft, its
recoveries spent, returns the claims that passed while the notes behind them are unchanged; otherwise it writes its
answer once more as that draft's rewrite, told what was rejected.

A fresh read, including source quotes kept for synthesis, retires earlier facts from the same address when their
quotes are no longer shown. The facts stay as history. Quotes still shown and evidence from other addresses remain.
The composer and the claim support check receive each source's known superseded state.
They judge the state the task asks about, retaining earlier quotes for historical answers and comparisons.

An answer that must be composed is always composed and checked. The composer is not skipped by asking Jev of each
note on its own whether it is the answer as written: such a question cannot tell which of two similar notes, such as the
headings of an opener and the window it opened, is the one the task asks about.
The answer must give every requested fact and value; a value kept only as context does not answer it. When
composition or the caller's schema extraction is pending, the retained sources are checked against the request, and
the generated result is checked before delivery.

Answer claims use numbered Markdown links built from those notes. `RunResult.citations` exposes the cited facts and
text-fragment deep links; unused facts have no citation. The LLM writes a composed answer from the notes. Jev judges
each final claim against its original cited quotes, including record identity and values, after any packing rounds.
The composer selects source wording by note id in private text parts, either the whole note or a part of it, such as
one field of a record, that code finds in the note's text exactly as written. Code copies the selection and adds
quotation delimiters when requested; prose parts, objects like the others so the composer does not write connecting
wording as a source part, keep explanatory wording separate. A tally has a computed numeric part or
requested winner-field parts, which code fills without replacing matching text inside a source part. If the task asks for the calculated value, the composer places a numeric slot in the claim text or cells. The tally's format hint alone does not render the value. A numeric slot retains its exact position in its prose or table cell, so unit and sign checks see the surrounding text without confusing an identical source number with the slot. Packed compositions expose their original
sources separately, and kept claims retain these parts through a rewrite. A rewrite's feedback names a rejected
draft's notes by the labels each request shows, and leaves out the notes another request holds. When the check that the answer copies only what the task asks fails, the
feedback lists the copied texts it judged. Split notes cannot be selected as whole sources. The delivered claims keep the same text, cells, tally and evidence id fields.
For a comparison restricted to each group's minimum or maximum, the composer also names a private selection:
the grouping field, the inner field and its extreme. Group names must copy an exact span from the record's quote
or, for captured page text, its own captured heading path. Code selects each group's tied extremes before applying
the outer comparison. The claim cites every qualifying record used in the selection; record association checks see
only the outer winners. This selection survives packing and rewrites without adding fields to delivered claims.
An extreme can supply both its numeric result and requested fields from its winning records. If a draft uses a slot named for the compared field as its numeric result, that slot gives the shared value once, including when records tie. A prose claim or table cell uses one slot for the comparison. Separate cells can show the computed result and the value as each winner writes it. Other typed computed slots name the fields to copy from each winner. Code selects the rank and every tied winner first, then copies each requested field only from those records and their associated source notes. Literal-copy checks accept wording from a record's quote or captured heading, never its address or generated metadata. Field-support checks use those same sources and reject a missing, ambiguous or unrelated value. A name and price can therefore share a prose claim or occupy separate table cells without the composer choosing the winning record.
A count, total, minimum or maximum, or a place after one such as the second cheapest, is never written or judged as a number. The response schema requires a field for sums, minima and maxima; only counts may omit it. The composer marks it as a tally with
the kind of record, the scope the task gives those records in a field of its own (such as "by Albert Einstein" or
"httpx or requests"), and the field, and Jev answers one question per note read: for a count, whether it holds exactly one
such record, and for a total or extreme, whether it holds one with its field, then which number in it is the field. For a total or extreme, membership uses the task's scope as well as the draft's record kind, with the page's labels identifying the requested state. A date, ISO or a day, month name and year, is offered whole with its year beside it, and its day and month are not offered alone, since neither orders anything; code orders dates for an extreme and never adds them. A
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
Jev is also asked what the task asks to be told about the tally's records, offered every operation a tally does with
the tally's own among them, and the claim holds only when Jev chooses that one: the composer chooses the operation, and
code then works out exactly a count where the task asked for a total. An operation another tally of the same records
gives is not offered, so a task asking for the number of orders and their total is answered by a count and a sum, and
a table row's tally is asked the same.
Amounts retain their currency symbol or ISO 4217 code when extracted. Code adds or compares only matching units,
including within each group's minimum or maximum, and refuses to relabel a calculated value with another currency.
An amount without a currency cannot be mixed with one that states a currency. No conversion rate or equivalence
between a symbol and an ISO code is assumed.
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
Initial and repaired compositions are checked for claim support and requested answer coverage. Coverage and
copied-text checks receive the task and answer without the action history; claim and action checks retain that
history to judge what changed and when. A copied field may contain its complete value when that field is requested,
such as the full date when the task asks for the date, or only the year when the task asks for its year. A claim with no cited quote or no
support judgment is withheld and ends `unverified`. When every check is answered and Jev rejects a composed claim, the run spends an existing recovery to correct the draft or gather
more evidence. Rejected prose is feedback, never a source fact. Code keeps each claim that passed every
check, with its citations, word for word in the corrected draft, and the composer writes only the rest, unless the
coverage or step check failed, which fault the claims together; a claim it
writes again with the same text, tally meaning and computed layout is kept once. Claims with different ranks
or computed fields remain distinct even when their placeholder text is identical. An omitted rank and rank 1
mean the same first minimum or maximum; a grouped count without a rank makes no ranking assertion. A count
ignores its unused numeric field when detecting repeats. A field-only
winner claim ignores its unused initial number when detecting repeats. Computed layout compares slot positions
and field names, regardless of how identical surrounding text was split into source and prose parts. A rewrite that retyped a passed
claim could change a copied name and fail where the first draft had passed. A corrected draft
must pass citation checks and cover every requested fact or value. If the combined answer exceeds Jev's input limit for the coverage check, individually supported claims remain available as an unverified partial answer; the result identifies the coverage limit. The coverage check judges the written
facts and values, not the form the task asks for, such as JSON, since code writes every answer as claims; citation
support and browser actions have separate checks. A source statement that a value is absent can satisfy coverage
when supported. An answer that says the run did not establish the value leaves that request open. A quote of text
the run typed supports the typing itself. Claim and tally checks are told when a quote contains that text, including
when a typed page label is supplied as tally context. A whole-answer check beside coverage
judges the claims against the steps the run carried out on the page: an answer that says something was not done, or
already existed, when one of those steps may have done it has every claim withheld, and a run still finishing spends a
recovery to write it again. Every draft receives the steps as the run's own record; a claim about a page result still
needs evidence from the page. Runs with a caller-supplied output schema
spend the same recovery, and when it ends without a checked answer they retain their extracted data and end `unverified`.
Each schema field is chosen after the answer is checked, with its claims beside the candidates, or with the claims that
passed when a rewrite is pending: a field choice alone cannot compare records, and chose the other package where the
answer worked out which was newer. When a string value occupies part of a source passage, both the source
choice and the word-boundary questions retain the record's other field names and the selected source's
context. A whole table row carries its header rather than an unknown column label. Recovery is not
attempted when a claim lacks a retained source quote or packed composition cannot reduce to one request.
The composer only writes the answer; it never judges whether the answer is complete. Jev's coverage question judges whether the answer gives every requested fact, and code ends a run over a whole list, or the pages a task names, `unverified` while a page of it is unread, unless Jev's read found one passage that alone answers the task, such as a total the list states. A draft with no claims fails coverage and goes to recovery, so the driver can gather more evidence. Exhausted composition recovery retains supported claims and ends `unverified`, unless a missing field value or
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
A composed answer is judged for exactness on the text it copies through its source parts, since its own wording holds no
page text: a source part can insert its note's whole text, so a composed answer can copy more than the task asks too.
JSON record fields retain their exact captured spans. The composer can select a field by its local identifier
instead of retyping its contents, and code copies its whitespace unchanged. Selectors are unavailable for masked
fields, partial field quotes and context fragments. Other source parts must still match their source text exactly.
Each copied span retains its enclosing claim as typed prose, copied text and computed slots. Table claims retain
their cell boundaries. This gives the span its role without presenting the writer's words or worked values as page text.
The check rejects copied page information the task neither asks for nor names, such as another field's value, an
identifier or another record, not the task's own words or a sentence that states only an asked value.

## Prompt limits and progress

`Config.tokens` names the input budgets for Jev's questions. A question that does not fit Jev's limit loses its
oldest actions first and then has its long texts cut, each cut saying what it left out. Cut page text carries a marker
when there is room for one; an excerpt too small to carry the marker is empty.

The driver's conversation is kept within the context window its provider lists for the model. Earlier pages go first,
each left as its title and address, since the model acts on the page in front of it; then the controls and text outside
the viewport, which a scroll reaches, with a note saying they were left out.

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

Every cost line may carry the `model` that served it: the provider response's `model` for an LLM line (the requested
id when the response names none), and the Jev model for a Jev line. Lines merged from calls to different models omit it,
and older receipts remain readable.

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
A navigation or a step settles once no frame of the tab, embedded frames included, is still loading its document
up to DOMContentLoaded, within the same settle deadline. A frame that loads after its page is ready is part of what
the navigation opened, so its controls are in the first observation and the next step's effect does not claim them.
The step also tracks function timers and animation frames scheduled during its input task, including their
nested callbacks. Timers due before the settle deadline hold the wait; frame chains use the existing quiet
window so an endless animation can settle. Cancelled callbacks, callbacks from earlier actions, timers due
after the deadline, unrelated page timers and the cookie-banner helper's background timers do not hold it open.

A `wait` step lasts until the page changes by itself: a change to what the settle fingerprint compares, a player
starting, pausing, ending or muting (in the page, a granted child frame or a frame in a process of its own that an observation reads), a same-document navigation, a navigation to another document, or a dialog. It
has no fixed bound. A run with `max_seconds` waits at most until it must stop browsing. A run without one waits at
most one armed watch (`WAIT_ARM_SECONDS`, 60 s) and is then told the page did not change, so it can decide again and
the stall rule counts a page that never moves. The wait samples the fingerprint twice a second beside the events it hears, since a script writing
`checked` as a property raises none. A wait that ends with nothing changed is a step that changed nothing, under the one stall rule, unless
a player's clock moved meanwhile. A cancelled wait removes what it installed in the page. A direct `Page::act` Wait has no deadline of its own, so it lasts one armed watch at most; an embedder bounds it further through `Page::wait` or cancellation.

An empty observation waits on loading events before action history and progress are recorded.
The same blank state is waited out once until a readable page appears, so a later navigation gets its own wait.
Reloading remains restricted to an untouched opening page, since retrying a later page could repeat a submission.
`StallRules` has one stall rule: a step that
changes nothing, a read that adds nothing, a return to a page state already seen with nothing read since, or a
further return after one return has already counted for the same evidence is no progress, since one read buys one return. `unchanged_actions` such steps in a row tell the driver ("N actions in a row changed nothing") so it plans another way. A page state not seen before or a read that adds
evidence resets the count and renews the `max_recoveries` budget. A stalled action is not taken again until the page changes or the run
learns something: the driver is told it was already tried. Past `max_recoveries` the run ends
`stuck`, or `unverified` for a rejected composition, unless a site failure establishes another status or the
driver gives up with its own (`needs_input`, `needs_login`, `blocked`, `unavailable`). An HTTP error page goes to the driver like any other, which reloads it when the status can clear, goes back or takes
another link. Code does not wait out a status for it.

## Run events and images

`on_event` receives a `BrowserEvent` followed by step and browser-warning events. `BrowserWarningEvent` carries
closed `operation` and `failure` codes without page content, addresses or transport error text. Browser sessions
without a warning consumer keep the stderr fallback. The run drains remaining warning events after closing the browser and
before returning. Each step includes added facts,
their reader and source links in `StepResult.facts`, and any available explanation in `StepResult.note`.
`StepResult.failure` carries an optional typed cause: `missing_target`, `target_gone`, `unauthorized_origin`,
`navigation_failed`, `dialog_interrupted` or `browser_error`. The browser records the cause where it detects
it, including a disconnected target reported as `stale`. A changed or covered control without a known cause
has no failure value. Policy refusals and an unmet caller condition also leave it unset, because the browser
has reported no fault. Older results without the field still parse; `note` remains diagnostic text.
`Config(step_frames=True)` attaches a PNG to each step unless a fresh observation shows a resolved secret or the run
knows an address credential.
This safety reading preserves the pending action's guards and leaves download notices for the next decision.

Jev identifies whether the task requests a final image, text, both, or an action without a report. A requested
final screenshot does not require step frames. Image-only output skips answer composition and text evidence
requirements; a request for both keeps the text evidence checks. An HTTP-first run opens a browser when
its requested output needs pixels. When the run has a browser page, the done check, the composer and the coverage
check are told that code attaches one screenshot, of the page the run ends on: a requested screenshot is given when
that page is the one the task asks to have pictured, and the answer need not say whether one is attached. A fresh secret check or a known address credential can withhold the image, and the
three checks are then told no screenshot is attached and why; a completed task without its requested image becomes
`unverified`. Python and Rust expose the PNG as `RunResult.final_frame`.
`Config(step_choices=True)` adds `StepResult.choices`, the controls the driver was shown for the action the step
carried out, with the ones it acted on marked `chosen` (for a drag, the destination too).
JSON clients receive a `screenshot` artifact with an `image/png` MIME type, SHA-256 digest and a base64 data URI.

`on_frame` receives JPEG bytes from the active tab. Frames are acknowledged after the async handler returns;
there is no fixed frame rate, and only the latest pending frame is retained. Delivery runs separately from
the agent, and handler failures are logged. A handler must honour cancellation: it runs on the caller's event
loop, so one that swallows `CancelledError` keeps that loop alive and `asyncio.run` waits for it at shutdown. Live images and MP4 recordings are held back from the moment a secret is typed until the run ends, and whenever the page is read showing one;
a recording holds its last clean frame meanwhile. A run whose start, task, context or inputs (names or values) write an
address with a user name or password holds them from before the browser opens to the end of the run: Chrome keeps the
credentials in `document.URL` and in the tab's address, so a page can draw them from its first paint, and pixels cannot
be checked as a frame arrives. A credential first seen mid-run, in a redirect, a tab or history address or any text a
page shows, starts the hold then, to the end of the run. That is the one case no hold covers: a frame sent before the
run first saw a credential that arrives mid-run may already show it. The same hold stops every other image of the
page: a model screenshot for Jev or the LLM, a PNG step frame, a screenshot artifact and `RunResult.final_frame`. A
page can paint `document.URL` on a canvas, where no text check sees it, so every image path asks one gate, the one live
frames and recordings ask, and the boundary every Jev and LLM request passes refuses a request carrying an image while
the run holds. The page state tells Jev that screenshots are withheld because the run holds an address credential, and
the done check, the composer and the coverage check are told no screenshot is attached. Without a credential, PNG step
frames and the final screenshot are checked against the page as it is when each is taken, and withheld while it shows
a resolved secret.

The hold covers the images the run sends, and these are outside it. A cloud browser's `live_url` shows the page's
pixels, so a run whose images are held from its start sends no `live_url` in its browser event: one predicate decides
both, a credential known when the browser opens or a start holding a declared secret. A credential learnt later cannot
recall a view already given. A headed Chrome window shows the page on screen. An embedder that
drives the browser itself, through `connect_cdp`, `Page.screenshot` or a `FrameTap`, takes images the run never sees.

The credential the run recognises by its structure is an address's userinfo, in any string a caller writes, in its
literal, percent-encoded, JSON-escaped and `Authorization: Basic` base64 forms. A query parameter, a path segment, a
bare header value or any other string is not recognisable as a credential by its structure, so the run does not mask
it unless the caller declares the value as a secret. A declared value the run holds (`--secret NAME=ENV_VAR@ORIGIN`, a
secret an MCP call names, a Rust `ScopedSecrets` plain value) is masked from the run's first request, before anything
types it, in the start, the task, a refused `cdp_url`, events, the result, the trace and every model request; frames
are held, and no `live_url` is sent, from before the browser opens when the start holds it, and frames from the first
reading on whenever the page shows it. A declared value is matched as text with no minimum length, so a short one masks
every occurrence of its letters from the first request, inside other words too. Declaring a value masks it and nothing else: where it may be typed is unchanged, and an address the caller wrote
with it stays theirs to open. A value a resolver gives only when asked (a serve client's `secrets/resolve`, a Python
resolver, a computed value) is masked from when the run types it. Not covered: a token in a query, path or header that
is not declared, a serve `run_id`, the base64 of a password alone, and a provider error that echoes a transformed
`FASTBROWSE_LLM_API_KEY`.

Every request to Jev or an LLM passes one boundary that masks every registered secret and address credential in it,
whatever built the request, and an observation and a capture are masked field by field before any request is built;
a field added to either does not compile until it is masked or named as one the browser layer makes (an id, a key, a
kind). The userinfo of every address is masked in every text a model is shown or the run writes out, registered or
not: an address with a scheme, a scheme-relative `//user:pass@host`, and an address percent-encoded inside another's
query. Everything else that leaves a run is redacted once, at its exit, by its schema: the result (its `data` keys
and values, `browser.profile`, recordings, artifacts and `error`), each event, the `until` check, which receives the
address masked rather than as the page has it, the trace and the HTTP trace, every stderr warning and log line, and
the errors a run raises, whatever caller text they repeat. Serve's JSON-RPC and MCP over stdio and HTTP learn every
string of every message a client sends, keys included, each as one whole address, before anything settles or quotes
it, whatever the message is (a request, a notification, a reply to a callback, an error reply, an answer to
`secrets/resolve` or `run/until`), and keep it for the life of the connection or session; MCP over HTTP learns the
header values it may quote too. Every binary entry (the CLI, `fastbrowse serve`, `fastbrowse-mcp` and the
subcommands) learns its arguments and the environment and `.env` values it may quote through one path before it
reports anything. Python and the Rust `Agent` learn the run's arguments, and the address a host's `open_browser`
answers is learnt as it is handed over, like `cdp_url`. Every reply, log line and printed request is masked by what
was learnt, so a refusal (an unknown method, a value serde could not read, an unsupported `MCP-Protocol-Version`, the
run's preflight check) quotes no credential a message carried. Only the members of a reply that can quote the caller
are masked (a result, an error's message and data, an event, a progress or log note); the envelope (`jsonrpc`, `id`,
`method`, an error's `code`) is the protocol's and stays as written, or a password `0` turned `"2.0"` into `"2.•"`.
Input that is not JSON has no strings to learn from, so it is refused with a fixed message that quotes none of it. A
JSON-RPC id is echoed whole to be matched, so a request whose id is itself an address with a credential is refused
with a fixed message and a null id; a short password learnt elsewhere does not refuse an ordinary id that holds its
letters. A closed
set's value and a model's field name are code's and stay whole; a key the caller chose is redacted like a value, in a
trace record as in the result. A task, browser connection, recipe, settings or HTTP request printed for a log prints
its addresses masked; `FASTBROWSE_LLM_EXTRA_BODY` is printed, and written to the HTTP trace, with its keys kept and its
values withheld.

A string the caller writes in a field that must come back as written (a path to record or save to, an output schema,
a profile, a selector, an attachment's name, a serve `run_id`, a secret's name or origin) cannot be
masked without breaking what the caller matches it against, so a run whose such field holds a credential is refused
before it starts, by a message that does not repeat it. Only the start, task, context, inputs and `cdp_url` may carry
an address's credentials. Before constructing clients, a run checks text settings against credentials learned from
its arguments, recipe and settings. The check includes nested keys and values in `FASTBROWSE_LLM_EXTRA_BODY`. The two
base URLs may carry endpoint credentials. A short credential refuses any checked field containing it, so a one-letter
password refuses every checked path holding that letter.

The userinfo of an address runs to its last `@`, as the WHATWG URL parser reads it, so a password holding `@`, `'`,
`(`, `)`, `,`, `;`, `!`, `.`, `:` or `%20` is masked whole; prose punctuation ends only the host. A `"`, `<`, `>` or
`` ` `` cannot stand in a URL as written (RFC 3986), so in text it ends the address: `"https://a.test","mail":"b@c.test"`
is two strings, not one userinfo. A whole address the caller gives, such as a start, is parsed whole, so a password
holding one of those is still learnt and masked wherever it appears. An email address or a `mailto:` link has no
userinfo, so a page showing one teaches the run nothing and holds no image. A literal quote, angle bracket or backtick
inside userinfo is masked when the address arrives as its own caller string or is a page URL the browser reports, and
not when it appears only inside free text the run never saw as a discrete value. The
run learns each credential it sees, from every string the caller gives, read by the run's schema with its keys, a
recipe, the settings' text and every string a page, tab or history address shows, and masks it wherever it appears in each form a page can echo it: as
written, percent-decoded, as the browser encodes it, and as the `Authorization: Basic` value the browser sends for it,
in standard and URL-safe base64. The secret of an address is its password when one is written, else its user name,
which is how an access token is written (`https://TOKEN@host/`); a user name beside a password is an identifier and
stays readable outside an address, and masked inside one. A lone user name is a credential by structure, since nothing
in the address tells a token from a login: `ssh://git@github.com/o/r` blanks `git` for the run, as
`https://ghp_TOKEN@github.com/o/r` blanks the token. A learned value is matched as text with no minimum length,
as a resolved secret is, so a short one also blanks the same letters inside other words. A whole address with a tab or
newline inside it (a start) registers its credential both as written and as the browser reads it, with
the tab or newline removed.

Masking covers these forms and no others, and these are its limits: a tab or newline inside an address in free prose
(a task, a context) ends the address there, as it separates words; an address written with no scheme
(`user:pass@host/path`) is not an address, and no entry point adds a scheme to one; `,` and `)` are characters a
userinfo may hold (RFC 3986 sub-delims), so in free prose `https://a.test,b@c.test` and `[x](https://a.test)b@c.test`
read as one address whose userinfo is `a.test,b` or `a.test)b`, which is masked and learnt; a literal escape in free prose
(`\u0040`, `&#64;` typed as characters) is not decoded. A caller's JSON is decoded before the run reads it, and a page's
character references are decoded by the browser or the HTTP reader before the run sees its text, so neither is a form
to register. A password written bare inside an attachment's contents is not known to be one: an attachment's bytes go
only to the page's file input, and its name reaches Jev only through the masked boundary. Base64 wrapped across lines,
a value percent-encoded twice, hex and a credential split across a page's text nodes are not forms the run registers
or matches.

The browser opens an address as written, credentials included, and a path the task names is resolved on the start
with them. A value Jev picks from the task to type is cut from the task as written, at the same offsets: masking keeps
the text's length in code points. A model's proposed address with a user name or password in it is discarded, except
an address the task writes, given back as the model was shown it, which opens as written. The known pages and start
page requests are told what the bullets are.

## Recipes

A recipe is the steps a `complete` run took, stored as JSON. `--save-recipe FILE` writes one and `--recipe FILE`
loads it. The task must match the recorded task. When `--recipe` is supplied without `--start`, its recorded start
address is used for a new tab. An attached tab keeps its current page unless an explicit `--start` overrides it. A recipe start must be a credential-free HTTP or HTTPS URL with a usable host and port. Naming the same file for both flags replaces it atomically after
another completed run, including actions the agent chose when a saved step no longer matched.

A step identifies its full page address and a control's role, label, context, link and input attributes. Node ids
and selectors are not stored. Query strings, fragments and generated path segments must match too: two orders can
have identical controls without being the same order. A missing or ambiguous control is left to the driver.

Saved steps run first, ahead of the driver's turns, through the same gates as the driver's own actions. A step runs only
while it is certain: the same page, exactly one live control it names, no password field, and its text written down as a
task literal, a caller input or a secret by name. At the first step that is not certain, or that is refused or does not take
effect on the page (the browser reports it was not carried out), the rest are dropped and the driver is told which step stopped and why, so it carries on from the page in front
of it. A step carried out from the recipe reports `decided_by: "recipe"`, and one the driver took `"llm"`. Freshness checks, hit testing, authorization and secret handling still apply. Password
fields always use the live credential checks. A stored secret is kept only by name, caller inputs by name, and
text copied word for word from the task as a literal. Values taken from page evidence, values the LLM composed and select options are decided live on every run. Recipes can
contain personal information from ordinary fields, so keep them private like the task that produced them. A step whose page address or control metadata reveals a resolved secret is omitted and decided live on replay.
A recipe serves any run of the same task, so a value that changes between runs belongs in a named input rather than
in the task: `fastbrowse "Search Wikipedia for the person named in input person" --input person="Ada Lovelace"
--save-recipe r.json`, then the same task with `--input person="Grace Hopper" --recipe r.json`. A fill that types an input's
value is saved as the input's name, so the replay types the new value. The recipe keeps the task as written. Pages after it, such as the article, have a different address
and are decided live.
If the task or start URL contains a resolved secret, or writes an address with a user name or password, no recipe is saved: a redacted task would fail the exact task match required for replay, and a redacted start URL would not open the original page.

Reading, answering and the check for `complete` always run live. Saved steps stop at the first one that is not certain, so a later step is never taken on a page the run has not
confirmed. A recipe is not evidence that the present task
has succeeded. A recipe save failure is written to server stderr without losing the completed run's result. SDK responses do not carry that warning; callers that require the file should check it exists on the server.

The public Python `fastbrowse.recipes.validate_recipe()` accepts a JSON string, UTF-8 bytes or a dictionary
and returns the normalized recipe, including defaults. Unsupported versions and invalid fields raise
`ValueError` before a browser opens. `recipe_schema()` exports the Rust-owned file schema, also kept in
`protocol/recipe.schema.json`. The schema checks field shapes; use `validate_recipe()` for URL semantics, including credentials and ports. Validation does not prove that a control exists or authorize an action.
To regenerate the schema, run `FASTBROWSE_UPDATE_GOLDEN=1 cargo test -p fastbrowse-core --test recipe_schema`.

## Cursor feedback

The run shows where the agent acts with a cursor the page draws itself (`js/cursor.js`): ember focus brackets
around a dot, with a fastbrowse tag. It glides along a shallow arc to each action point, the brackets close in as it
lands, a ring goes out on each press, and a drag carries it along the drag's own path while the button is held. It
never moves the user's pointer or changes focus, and the input is still sent over CDP, so the drawing cannot change
what an action did.

It is drawn wherever someone can see the page: the user's own Chrome through the extension, a headed or attached
local browser, a cloud browser's live view, a host's live frames, or a `--record` video, which films it with the
page. A headless run nobody watches draws nothing. `--cursor` (`cursor=True`) draws it in every run and
`--no-cursor` (`cursor=False`) never.

`cursor.rs` evaluates the script in an isolated world of the tab's top document (`Page.createIsolatedWorld`), so page
scripts can neither see nor call it, and the extension relays the same three commands, so both browser routes share
it. The first call in a document installs it; a navigation destroys the world, and the next action draws it again
from the last point the run sent. Every call is bounded and quiet on failure: a document that refuses the script only
misses its drawing. The glide is paced to a travel speed, not a duration: as long as its distance needs at
`GLIDE_PX_PER_S`, between `GLIDE_MS` and `GLIDE_MAX_MS`, so a sweep across the page and a hop between neighbours
cross at the same pace. The action waits for the glide to land before its input is sent.

The cursor draws into a closed shadow root on one host element marked `data-fastbrowse-cursor`, `aria-hidden` and
`pointer-events: none`, in the top layer as a manual popover, so it shows above dialogs and fullscreen elements and
is never hit by the run's own hit tests. It never reaches what the agent reads: the host holds no text, the snapshot
skips it, and a screenshot for the model takes it out of the page first. While it is out, the session withholds
frames as it does for a secret, so the live view and the recording hold their last frame rather than show the page
without it.

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
are read in memory, never extracted onto the host, and each readable member is its own downloaded text named
`archive.zip/member`, so a CSV member is read row by row as a downloaded CSV is. Text, CSV, TSV, JSON, XML, Markdown and HTML members are read
as UTF-8 text; XLSX, DOCX and PDF members use their document readers. Other or invalid members remain named with
an explicit notice that their contents were not read. Nested archives are not expanded. Generic or missing MIME types use the download filename to select these readers.
The configured `max_download_bytes` also bounds aggregate decompressed container bytes and extracted output; a
document exceeding it is reported unread, rather than returning truncated text.

Every capture taken after a download carries the file's text, at offsets the page around it moves. A quote from the
file is one note however many reads take it: a read that quotes the same place in the same file with the same text
joins the note an earlier read made. Equal text at two places in one file stays two notes.

The cloud browser ignores `Browser.setDownloadBehavior(deny)`, so bytes come from response interception, never from the remote filesystem. Host-path `DOM.setFileInputFiles` is only valid for a browser on the same machine.

### Foreground and background

Chrome raises and focuses the whole window of a tab that is activated (`Target.activateTarget`,
`Page.bringToFront`) or created in front, so a session does either only when `BrowserConnection.foreground` is
set: on a cloud browser, whose live view shows the front tab, on headless Chrome, which has no window, and when
the caller asks to watch. Otherwise the tab is created with `background: true` and nothing activates it. A tab
behind another is hidden, so two things stand in for being in front. Focus emulation makes the document visible
and focused, which runs animation frames and lets it take typing. A screencast keeps the tab's compositor
awake, without which a screenshot of an idle page waits seconds for a frame; it is 16 pixels, every thousandth
frame. Direct CDP uses a separate session so live frames and recordings cannot replace it. The extension has
one session per tab, so setup starts the keeper on that session unless live frames or a recording already
keep it painting. Measured on Chrome 155 in
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
`covered`. The screenshot fallback clicks only while a fresh viewport still matches the pixels the model used to choose
its coordinates. When focus is in a suggesting field, it leaves that field first, which presses nothing, and the control
is checked again, so a list the field left open does not hold up the next one. Focus and the receiving field are checked before typing. Settling waits for an interactive
document and a quiet DOM. It does not guess at loading indicators by class name: a page still loading is read as
it stands, and the driver judges from its text whether to look again.
