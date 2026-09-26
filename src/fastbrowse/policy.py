"""Jev's per-step decision: one batched request picks the operation and its target.

Question wording is adapted from browser-use/jev-ultrafast (MIT) `questions.py` and `model.py`, where it was
live-bench proven. The batch asks for an operation, its possible targets, and an optional login check.
Choices beyond Jev's option limit use a group choice followed by a separate element request.
A relevance filter shortlists dense pages before the action choice so late controls can still be offered.
"""

import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from enum import StrEnum
from typing import assert_never

from pydantic import JsonValue

from fastbrowse.batches import evaluate_batches
from fastbrowse.config import Config
from fastbrowse.jev import (
    ChoiceAnswer,
    ChoiceQuestion,
    Evaluation,
    JevClient,
    JevInputTooLarge,
    JevRetriesExhausted,
    NoulAnswer,
    NoulQuestion,
    Question,
)
from fastbrowse.models import TARGETED, UNTRUSTED, CostComponent, CostLine, Frozen, Operation, StepOutcome
from fastbrowse.page import Control, Observation, loads_more, pager_link
from fastbrowse.telemetry import Ledger, trace

NEXT_ACTION = f"""{UNTRUSTED}
Advance the user's task from the current page using one operation.
When a subgoal is supplied, take its next action first; it describes the current obstacle.
Use current field values and the recent history.
Do not repeat satisfied steps. Fill required fields before submitting. An element marked blocking is a field
its form will not submit without: fill it, or change the form's mode, before submitting again. An action whose
effect is "nothing visible changed" did nothing: take another way, not the same action.
When a control offers choices or a confirmation, use its observed behaviour to determine whether entering
a value commits it or whether selecting or confirming is needed to reach the requested state.
If changing a form's mode changes its fields or clears their values, set the requested mode before filling
the affected fields.
If a form asks for extra values the task does not need, change its mode before inventing those values.
Set the filters the task asks for that this form already offers before submitting; setting one afterwards
submits twice. A filter the page only shows once there are results is set there, after submitting.
Once the task's fields, mode and the filters offered here are set, submit if the form requires submission
to apply them.
A matching result alone does not prove a filter was set.
Do not toggle a checkbox, switch or radio already in the requested state.
Elements marked offscreen can be targeted directly; do not scroll just to reach them.
A link's href shows where it leads; use it to tell site navigation from content links.
A matching link is not an opened result.
A wizard or multi-step form can keep one URL for every step, so browser history has no entry for the step before
this one: when the page itself offers a Back or Previous control, prefer it over the back operation to move
within the form."""

TARGET = f"""{UNTRUSTED}
Choose the best observed target if the next operation is the one this question names.
Use the task, field values, nearby text and recent actions. Another question decides which operation runs.
Do not choose a field that already contains the requested value. Choose only an offered element.
Do not toggle a checkbox, switch or radio already in the requested state.
While the form's mode (a trip or ticket type, a tab) is not the one the task needs, the control that sets
the mode is the target, not the form's submit. A control that explores or browses broadly does not run the
search the task asked for.
Elements marked offscreen can be targeted directly. A link's href shows where it leads.
Elements that read alike carry a `context`: the card, row or section each one belongs to. When the task
or subgoal names one of those, choose the element whose context matches it."""

GROUP = """Too many elements to list at once. Choose the group that contains the best target if the next
operation is the one this question names. A later question picks the element inside the group."""

RELEVANCE = f"""{UNTRUSTED}
Could the task's next few actions, or reading what it needs, act on or rely on the element in each question?
Judge by its label, role, context and href. Site chrome, footers, ads, social links and unrelated navigation
are not relevant."""

OPERATION_LABELS: Mapping[Operation, str] = {
    Operation.CLICK: "Click an element, button, link, menu option, autocomplete suggestion or calendar day.",
    Operation.HOVER: "Hover over an element to reveal content the page shows only under the pointer.",
    Operation.FILL: "Enter or replace text in an editable field.",
    Operation.SELECT: "Select a value in an observed dropdown.",
    Operation.ENTER: "Press Enter in a text field to submit or search for what it already contains.",
    Operation.UPLOAD: "Attach one of the task's files to a file input.",
    Operation.ESCAPE: "Press Escape to close a menu, popup or overlay.",
    Operation.SCROLL: "Scroll the page to reveal content that is not observed yet.",
    Operation.BACK: "Go back to the previous page.",
    Operation.SWITCH_TAB: "Switch to another open tab.",
    Operation.DIALOG: "Respond to the open browser dialog.",
    Operation.READ: "Read this page's content to extract information the task needs.",
    Operation.DONE: "Every requirement is visibly satisfied.",
    Operation.ESCALATE: "No offered operation can make progress.",
}


class Reduction(StrEnum):
    NONE = "none"
    RELEVANCE = "relevance"
    ONSCREEN_ONLY = "onscreen_only"
    COMPACT = "compact"
    """On-screen elements with long labels shortened and links cut to their path. Amazon's signed-in results
    page offered 112 products with ~200-character titles and ~480-character tracking links, twice over (state
    and target question), and the run stopped at `observation_limit` before it could pick one."""
    CAPPED = "capped"
    """Compacted, and only the elements that fit offered; the rest are counted as omitted for a scroll to reach."""


COMPACT_CHARS = 80
COMPACT_HREF_CHARS = 120
"""Longer than a label: a link's identity can sit in its query (`/item?id=123`), so it is trimmed, not cut to the
path. Amazon's product id sits in the path's first 70 characters, ahead of the tracking query."""


class ReadAssessment(StrEnum):
    ABSENT = "absent"
    EDITING = "editing"
    EVIDENCE = "evidence"


class ObservationTooLarge(RuntimeError):
    """The page's state cannot be represented within Jev's limits even with no controls offered."""


class HistoryEntry(Frozen):
    operation: Operation | None
    """None for a move code made before Jev's first step, such as opening a shortcut address."""
    target: str | None
    outcome: StepOutcome
    page_changed: bool
    note: str | None = None
    text: str | None = None
    """Entered value, redacted before storage; secrets are represented only by a marker."""
    effect: str | None = None
    """What the action visibly did: the address, controls shown or removed, and values before and after."""
    setting: bool | None = None
    """True for a click that chose an option or ticked a box, a value set and kept in the record as a typed one is;
    unset otherwise, so the actions a model is shown do not each carry it."""


class StepContext(Frozen):
    task: str
    subgoal: str | None
    requirements: tuple[str, ...]
    notes: str
    history: tuple[HistoryEntry, ...]
    check_login: bool
    check_bot: bool
    """Asked apart from `check_login`: credentials mean a sign-in wall is work to do, but nothing a caller can
    supply passes a CAPTCHA, so a page is worth checking for one whether or not a secret is held for it."""
    has_attachments: bool
    secrets: tuple[str, ...]
    """Names of stored secrets the current origin may receive; a fill can type one without Jev seeing it."""
    unread_requirements: tuple[str, ...] | None = None
    """None while planning; an empty tuple means no information remains to collect."""
    recovery_memory: str = ""
    """Recent reasons, diagnoses and subgoals, separate from actions actually taken."""


class Decision(Frozen):
    operation: Operation
    target: Control | None
    tab_id: str | None
    operation_confidence: float
    target_confidence: float | None
    login_required: float | None
    offered_controls: int
    reduction: Reduction
    cost: tuple[CostLine, ...]
    input_tokens: int
    bot_check: float | None = None
    read_assessment: ReadAssessment = ReadAssessment.ABSENT
    directed: bool = False
    """Recovery's action on this decision's page. Its confidence scores the action Jev chose instead, so it
    says nothing about this one."""

    @property
    def confidence(self) -> float:
        """Weakest link: the action is only as certain as its least certain choice."""
        if self.target_confidence is None:
            return self.operation_confidence
        return min(self.operation_confidence, self.target_confidence)


@dataclass(frozen=True, slots=True)
class _Request:
    state: JsonValue
    questions: Mapping[str, Question]
    targets: Mapping[Operation, tuple[Control, ...]]
    groups: Mapping[Operation, tuple[tuple[Control, ...], ...]]


async def decide(
    jev: JevClient, observation: Observation, context: StepContext, config: Config, *, ledger: Ledger | None = None
) -> Decision:
    """Ask Jev for the next action, reducing the observation when it will not fit."""
    controls = observation.controls
    reduction = Reduction.NONE
    cost: list[CostLine] = []
    tokens = 0
    shortlist_tried = len(controls) > config.observation.max_offered_controls
    if shortlist_tried:
        shortlist = await _shortlist(jev, observation, controls, context, config, ledger)
        if shortlist is not None:
            controls, cost, tokens = shortlist
            reduction = Reduction.RELEVANCE
        else:
            keep = {i for i, control in enumerate(controls) if _protected(control)}
            room = max(0, config.observation.max_offered_controls - len(keep))
            keep.update([i for i in range(len(controls)) if i not in keep][:room])
            controls = tuple(control for i, control in enumerate(controls) if i in keep)
    # Shortening text loses nothing a choice needs, so it is tried before any control is dropped, and kept for every
    # rung after it.
    compact = False
    while True:
        request = build_request(observation, controls, context, config, compact=compact)
        if fits(request, config):
            try:
                decision = await _evaluate(jev, request, controls, reduction, ledger, compact=compact)
                return decision.model_copy(
                    update={"cost": (*cost, *decision.cost), "input_tokens": tokens + decision.input_tokens}
                )
            except JevInputTooLarge:
                pass
            except JevRetriesExhausted as error:
                smaller = _shed(request, observation, controls, context, config, reduction)
                if smaller is None:
                    raise
                trace("decide_shed", offered=len(controls), retry_with=len(smaller[0]), reason=str(error)[:300])
                controls, reduction = smaller
                continue
        if not compact:
            compact = True
            if reduction is Reduction.NONE:
                reduction = Reduction.COMPACT
            continue
        if not shortlist_tried:
            shortlist_tried = True
            shortlist = await _shortlist(jev, observation, controls, context, config, ledger, compact=True)
            if shortlist is not None:
                controls, cost, tokens = shortlist
                reduction = Reduction.RELEVANCE
                continue
        if reduction not in (Reduction.ONSCREEN_ONLY, Reduction.CAPPED) and any(c.offscreen for c in controls):
            controls = tuple(c for c in controls if not c.offscreen)
            reduction = Reduction.ONSCREEN_ONLY
            continue
        # A page too dense even on screen is still worked rather than ending the run: the controls that fit are
        # offered, the rest counted as omitted, and a scroll brings them into the next step's view.
        capped = _cap(observation, controls, context, config, below=len(controls))
        if capped is None:
            raise ObservationTooLarge(f"the state on {observation.url} exceeds Jev's input limits with no controls")
        controls, reduction = capped, Reduction.CAPPED


def _shed(
    request: _Request,
    observation: Observation,
    controls: Sequence[Control],
    context: StepContext,
    config: Config,
    reduction: Reduction,
) -> tuple[tuple[Control, ...], Reduction] | None:
    """Fewer controls to offer after the provider answered a large request with 503s until its retries ran out.

    The gateway sheds large Jev requests. A Wikipedia article's step offered 160 controls, twice over (state and
    target question), in 27k input tokens, and it failed six runs in six as unavailable; replayed, that same request
    was answered four times in ten while every request under 10k tokens but two in 29 was. So a large request
    is asked again smaller: off-screen controls first, as the fitting ladder drops them, then half of those left.
    A request already as small as a batch is not the reason, so its failure stays an outage.
    """
    ratio = config.tokens.chars_per_token
    state = len(json.dumps(request.state)) / ratio
    largest = max(len(question.model_dump_json()) for question in request.questions.values()) / ratio
    if state + largest <= config.tokens.batch_tokens:
        return None
    if reduction not in (Reduction.ONSCREEN_ONLY, Reduction.CAPPED) and any(c.offscreen for c in controls):
        return tuple(c for c in controls if not c.offscreen), Reduction.ONSCREEN_ONLY
    capped = _cap(observation, controls, context, config, below=len(controls) // 2)
    return None if capped is None else (capped, Reduction.CAPPED)


def _cap(
    observation: Observation, controls: Sequence[Control], context: StepContext, config: Config, *, below: int
) -> tuple[Control, ...] | None:
    """The largest prefix, protected controls first, shorter than `below` whose request fits; None if none does."""
    order = sorted(range(len(controls)), key=lambda i: not _protected(controls[i]))

    def kept(prefix: int) -> tuple[Control, ...]:
        indices = set(order[:prefix])
        return tuple(control for i, control in enumerate(controls) if i in indices)

    if below <= 0 or not fits(build_request(observation, kept(0), context, config, compact=True), config):
        return None
    low, high = 0, below - 1
    while low < high:
        middle = (low + high + 1) // 2
        if fits(build_request(observation, kept(middle), context, config, compact=True), config):
            low = middle
        else:
            high = middle - 1
    return kept(low)


def _protected(control: Control) -> bool:
    # Only state the task has already set is kept unasked: counting every unchecked option of a long filter list
    # would fill the shortlist and push out the button that applies it.
    return bool(
        pager_link(control)
        or loads_more(control)
        or control.blocking
        or control.value
        or control.checked
        or control.selected
        or control.expanded
    )


async def _shortlist(
    jev: JevClient,
    observation: Observation,
    controls: Sequence[Control],
    context: StepContext,
    config: Config,
    ledger: Ledger | None,
    *,
    compact: bool = False,
) -> tuple[tuple[Control, ...], list[CostLine], int] | None:
    protected = {i for i, control in enumerate(controls) if _protected(control)}
    candidates = [i for i in range(len(controls)) if i not in protected]
    state: JsonValue = {
        "rules": RELEVANCE,
        "task": context.task,
        "subgoal": context.subgoal,
        "requirements": list(context.requirements),
        "unread_requirements": (list(context.unread_requirements) if context.unread_requirements is not None else None),
        "recent_actions": [entry.model_dump(mode="json", exclude_none=True) for entry in context.history],
        "page": {"url": observation.url, "title": observation.title},
    }
    # The rubric sits once in the state; repeating it in every question would pack a third as many per request.
    questions: dict[str, Question] = {
        f"r{i}": NoulQuestion(instructions=f"Is this element relevant? {json.dumps(_relevance_element(controls[i]))}")
        for i in candidates
    }
    # An element too large to score is left unscored rather than dropped.
    answered = await evaluate_batches(jev, state, questions, tokens=config.tokens, ledger=ledger)
    if answered is None:
        return None
    scores = {
        int(key[1:]): answer.probability for key, answer in answered.answers.items() if isinstance(answer, NoulAnswer)
    }
    # A failed answer says nothing about relevance, so keep unscored controls ahead of equally scored ones.
    ranked = sorted(candidates, key=lambda i: (-scores.get(i, 1.0), i in scores, i))

    def kept(prefix: int) -> tuple[Control, ...]:
        indices = protected | set(ranked[:prefix])
        return tuple(control for i, control in enumerate(controls) if i in indices)

    low, high = 0, min(len(ranked), max(0, config.observation.max_offered_controls - len(protected)))
    while low < high:
        middle = (low + high + 1) // 2
        if fits(build_request(observation, kept(middle), context, config, compact=compact), config):
            low = middle
        else:
            high = middle - 1
    trace("shortlist", pool=len(controls), offered=len(protected) + low, scored=len(scores), requests=answered.requests)
    return kept(low), list(answered.cost), answered.input_tokens


def _index_controls(controls: Sequence[Control]) -> dict[Operation, tuple[Control, ...]]:
    indexed: dict[Operation, list[Control]] = {}
    for control in controls:
        for operation in control.operations:
            indexed.setdefault(operation, []).append(control)
    return {operation: tuple(candidates) for operation, candidates in indexed.items()}


def _offered_operations(
    observation: Observation, indexed: Mapping[Operation, tuple[Control, ...]], context: StepContext
) -> tuple[Operation, ...]:
    if observation.dialog is not None:
        return (Operation.DIALOG, Operation.ESCALATE)
    available: list[Operation] = []
    for operation in Operation:
        match operation:
            case Operation.CLICK | Operation.HOVER | Operation.FILL | Operation.SELECT | Operation.ENTER:
                if operation in indexed:
                    available.append(operation)
            case Operation.UPLOAD:
                if context.has_attachments and operation in indexed:
                    available.append(operation)
            case Operation.SWITCH_TAB:
                if len(observation.tabs) > 1:
                    available.append(operation)
            case Operation.BACK:
                if observation.can_go_back:
                    available.append(operation)
            case Operation.ESCAPE | Operation.SCROLL | Operation.READ | Operation.DONE | Operation.ESCALATE:
                available.append(operation)
            case Operation.DIALOG:
                pass
            case _:
                assert_never(operation)
    return tuple(available)


def build_request(
    observation: Observation,
    controls: Sequence[Control],
    context: StepContext,
    config: Config,
    *,
    compact: bool = False,
) -> _Request:
    indexed = _index_controls(controls)
    offered = _offered_operations(observation, indexed, context)
    questions: dict[str, Question] = {
        "operation": ChoiceQuestion(
            instructions=NEXT_ACTION,
            criteria={op.value: OPERATION_LABELS[op] for op in offered},
        )
    }
    if context.unread_requirements is None or context.unread_requirements:
        questions["read_assessment"] = ChoiceQuestion(
            instructions=(
                f"{UNTRUSTED}\nDoes the current page contain evidence for an unanswered information "
                "requirement that should be read before further interaction? Use unread_requirements, the "
                "collected notes and recent actions; while planning, judge from the task. Evidence can answer "
                "part of a comparison or explain a failed action. A relevant error, refusal, result or total "
                "must be preserved even when the page also has an editable form. Field values, suggestions "
                "and previews are inputs, not results, and so are the prices or availability a picker shows beside "
                "its options (a calendar's fare per day) while a value is still being chosen. A review page before "
                "a final "
                "submit is evidence: the totals it shows may not appear again once the submit commits. A "
                "rewritten URL alone proves nothing. Judge the content regardless of control labels or roles."
            ),
            criteria={
                ReadAssessment.ABSENT.value: "The page adds no evidence for the unanswered requirements.",
                ReadAssessment.EDITING.value: (
                    "Only an editable form or query preview is relevant; it still needs interaction, not reading."
                ),
                ReadAssessment.EVIDENCE.value: (
                    "The page contains relevant evidence, including partial results or a failure message, "
                    "that the notes do not yet preserve. Read it before interacting again."
                ),
            },
        )
    targets: dict[Operation, tuple[Control, ...]] = {}
    groups: dict[Operation, tuple[tuple[Control, ...], ...]] = {}
    limit = config.observation.max_choice_options
    for operation in offered:
        if operation not in TARGETED:
            continue
        candidates = indexed[operation]
        head = f"{operation.value}_target"
        if len(candidates) <= limit:
            targets[operation] = candidates
            questions[head] = _target_question(operation, candidates, compact)
            continue
        size = config.observation.group_size
        chunks = tuple(candidates[i : i + size] for i in range(0, len(candidates), size))
        groups[operation] = chunks
        questions[f"{operation.value}_group"] = ChoiceQuestion(
            instructions=json.dumps({"rules": [TARGET, GROUP], "operation": operation.value}),
            criteria={
                str(i): " | ".join(_shortened(c.label) if compact else c.label for c in chunk)
                for i, chunk in enumerate(chunks)
            },
        )
    if Operation.SWITCH_TAB in offered:
        questions["switch_tab_target"] = ChoiceQuestion(
            instructions=json.dumps({"rules": TARGET, "operation": Operation.SWITCH_TAB.value}),
            criteria={t.id: {"title": t.title, "url": t.url, "active": t.active} for t in observation.tabs},
        )
    if context.check_login:
        questions["login_required"] = _noul(
            "Does a sign-in or verification wall block the task in state, with no credentials given in the task to "
            "pass it?",
            "A sign-in, verification or access wall blocks the task and the task gives no way through it.",
            "The task can progress without signing in, or the task supplies the credentials to sign in.",
        )
    if context.check_bot:
        questions["bot_check"] = _noul(
            "Does this page ask the visitor to pass an automated-traffic check?",
            "The page is a CAPTCHA, a browser verification or a similar bot check.",
            "The page is a sign-in form or an ordinary page.",
        )
    return _Request(_state(observation, controls, context, compact), questions, targets, groups)


def fits(request: _Request, config: Config) -> bool:
    ratio = config.tokens.chars_per_token
    state = len(json.dumps(request.state)) / ratio
    sizes = [len(q.model_dump_json()) / ratio for q in request.questions.values()]
    return (
        state + max(sizes) <= config.tokens.state_plus_largest_question
        and state + sum(sizes) <= config.tokens.state_plus_all_questions
    )


async def _evaluate(
    jev: JevClient,
    request: _Request,
    controls: Sequence[Control],
    reduction: Reduction,
    ledger: Ledger | None,
    *,
    compact: bool = False,
) -> Decision:
    if ledger is not None:
        ledger.reserve(CostComponent.JEV)
    evaluation = await jev.evaluate(request.state, request.questions)
    if ledger is not None:
        ledger.record(evaluation.cost)
    cost = [evaluation.cost]
    tokens = evaluation.input_tokens
    operation_answer = _choice(evaluation, "operation")
    operation = Operation(operation_answer.choice)
    target: Control | None = None
    tab_id: str | None = None
    target_confidence: float | None = None
    if operation in request.targets:
        target_answer = _choice(evaluation, f"{operation.value}_target")
        target = _control(request.targets[operation], target_answer.choice)
        target_confidence = target_answer.confidence
    elif operation in request.groups:
        group_answer = _choice(evaluation, f"{operation.value}_group")
        group = request.groups[operation][int(group_answer.choice)]
        if ledger is not None:
            ledger.reserve(CostComponent.JEV)
        inner = await jev.evaluate(
            request.state,
            {f"{operation.value}_target": _target_question(operation, group, compact)},
        )
        if ledger is not None:
            ledger.record(inner.cost)
        cost.append(inner.cost)
        tokens += inner.input_tokens
        target_answer = _choice(inner, f"{operation.value}_target")
        target = _control(group, target_answer.choice)
        # The element was only ever chosen from inside the group, so a doubtful group is a doubtful target.
        target_confidence = group_answer.confidence * target_answer.confidence
    elif operation is Operation.SWITCH_TAB:
        target_answer = _choice(evaluation, "switch_tab_target")
        tab_id = target_answer.choice
        target_confidence = target_answer.confidence
    return Decision(
        operation=operation,
        target=target,
        tab_id=tab_id,
        operation_confidence=operation_answer.confidence,
        target_confidence=target_confidence,
        login_required=_noul_probability(evaluation, "login_required"),
        bot_check=_noul_probability(evaluation, "bot_check"),
        read_assessment=(
            ReadAssessment(_choice(evaluation, "read_assessment").choice)
            if "read_assessment" in request.questions
            else ReadAssessment.ABSENT
        ),
        offered_controls=len(controls),
        reduction=reduction,
        cost=tuple(cost),
        input_tokens=tokens,
    )


def _state(observation: Observation, controls: Sequence[Control], context: StepContext, compact: bool) -> JsonValue:
    state: dict[str, JsonValue] = {
        "task": context.task,
        # "Next month" and "the next Monday" are relative to today, which only the done check was told: shown
        # which month a date picker was on, Jev clicked Next through two years of months looking for next month.
        "date": observation.captured_at.date().isoformat(),
        "subgoal": context.subgoal,
        "page": {"url": observation.url, "title": observation.title, "text": observation.viewport_text},
        "requirements": list(context.requirements),
        "unread_requirements": (list(context.unread_requirements) if context.unread_requirements is not None else None),
        "notes": context.notes,
        "recent_actions": [entry.model_dump(mode="json", exclude_none=True) for entry in context.history],
        "elements": [_element(c, compact=compact) for c in controls],
    }
    if context.recovery_memory:
        state["recovery_memory"] = context.recovery_memory
    if context.secrets:
        state["stored_secrets"] = list(context.secrets)
    omitted = observation.omitted_controls + len(observation.controls) - len(controls)
    if omitted > 0:
        state["omitted_elements"] = omitted
    if observation.dialog is not None:
        state["dialog"] = observation.dialog.model_dump(mode="json", exclude_none=True)
    if len(observation.tabs) > 1:
        state["tabs"] = [t.model_dump(mode="json", exclude_none=True) for t in observation.tabs]
    return state


def _relevance_element(control: Control) -> dict[str, JsonValue]:
    element: dict[str, JsonValue] = {
        "label": control.label,
        "role": control.role,
        "operations": [op.value for op in sorted(control.operations)],
    }
    if control.context is not None:
        element["context"] = control.context
    if control.href is not None:
        element["href"] = control.href
    if control.offscreen:
        element["offscreen"] = True
    return element


def _shortened(text: str, limit: int = COMPACT_CHARS) -> str:
    return text if len(text) <= limit else text[:limit] + "..."


def _element(control: Control, *, compact: bool = False) -> dict[str, JsonValue]:
    label, context, href = control.label, control.context, control.href
    if compact:
        label = _shortened(label)
        context = None if context is None else _shortened(context)
        href = None if href is None else _shortened(href, COMPACT_HREF_CHARS)
    element: dict[str, JsonValue] = {
        "id": control.id,
        "label": label,
        "role": control.role,
        "operations": [op.value for op in sorted(control.operations)],
    }
    optional: dict[str, JsonValue] = {
        "context": context,
        "value": control.value,
        "href": href,
        "checked": control.checked,
        "selected": control.selected,
        "expanded": control.expanded,
        "input_type": control.input_type,
    }
    element.update({key: value for key, value in optional.items() if value is not None})
    if control.options:
        element["options"] = list(control.options)
    if control.offscreen:
        element["offscreen"] = True
    if control.blocking:
        element["blocking"] = True
    return element


def _target_question(operation: Operation, candidates: Sequence[Control], compact: bool) -> ChoiceQuestion:
    return ChoiceQuestion(
        instructions=json.dumps(
            {
                "rules": TARGET,
                "operation": operation.value,
            }
        ),
        # Sending only the label and pointing Jev at the shared state for the rest is two thirds smaller
        # on a dense page, and it was tried. It bought no measured latency, because the request was never
        # the slow part, and a criterion the model has to go and look up is a worse criterion: the choice
        # is what this whole design rests on, so it gets the attributes in front of it.
        criteria={c.id: _target_element(c, operation, compact) for c in candidates},
    )


def _target_element(control: Control, operation: Operation, compact: bool) -> JsonValue:
    # Ported from browser-use/jev-ultrafast (MIT), snapshot.js: name opening a field separately
    # from typing in it so a picker is a useful click target even when its value is already filled.
    element = _element(control, compact=compact)
    if Operation.FILL in control.operations:
        if operation is Operation.CLICK:
            element["label"] = f"Open {element['label']}"
        elif operation is Operation.ENTER:
            element["label"] = f"Press Enter in {element['label']}"
    return element


def _noul(instructions: str, true: str, false: str) -> Question:
    return NoulQuestion(instructions=f"{UNTRUSTED}\n{instructions}", true=true, false=false)


def _choice(evaluation: Evaluation, key: str) -> ChoiceAnswer:
    answer = evaluation.answers.get(key)
    if not isinstance(answer, ChoiceAnswer):
        raise ValueError(f"Jev returned no choice for {key!r}")
    return answer


def _noul_probability(evaluation: Evaluation, key: str) -> float | None:
    answer = evaluation.answers.get(key)
    return answer.probability if isinstance(answer, NoulAnswer) else None


def _control(candidates: Sequence[Control], control_id: str) -> Control:
    for control in candidates:
        if control.id == control_id:
            return control
    raise ValueError(f"Jev chose {control_id!r}, which was not offered")
