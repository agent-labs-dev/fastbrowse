"""Deciding whether a run is actually finished, and whether its answer and extracted data hold up.

Jev checks completion, individual action requirements and draft quality. An LLM looks at a screenshot
when those answers leave completion uncertain.
"""

import asyncio
import hashlib
import json
import re
from collections.abc import Collection, Iterable, Mapping, Sequence
from enum import StrEnum
from typing import assert_never

from pydantic import (
    BaseModel,
    Field,
    JsonValue,
    SerializerFunctionWrapHandler,
    ValidationError,
    model_serializer,
)

from fastbrowse.batches import evaluate_batches
from fastbrowse.config import Config, Thresholds, TokenBudget
from fastbrowse.jev import (
    JevClient,
    NoulAnswer,
    NoulQuestion,
    Question,
)
from fastbrowse.llm import Generation, LLMClient, Message
from fastbrowse.memory import CollectionFact, Notes, NotesTooLarge, Standing, fact_id
from fastbrowse.models import UNTRUSTED, CostLine, Evidence, Frozen, LLMPurpose, SourceControl
from fastbrowse.page import Capture, Control, Observation, cut_text
from fastbrowse.planner import Plan, RequirementKind
from fastbrowse.policy import HistoryEntry
from fastbrowse.retrieval import (
    OUTPUT_MISSING,
    TRANSACTION_CONTRADICTED,
    AnswerCorrection,
    ComposedAnswer,
    FieldAnswer,
    UnsupportedField,
    assemble_answer,
    choose_candidate,
    claim_check_questions,
    field_candidates,
    field_candidates_from_notes,
    merge_candidates,
    propose_text_fields,
    propose_text_fields_from_notes,
    transaction_check_question,
)
from fastbrowse.telemetry import Ledger, trace

_DEFAULT_CONFIG = Config()
# A failed navigation followed by Back was accepted from the visited address alone.
_REACHED = (
    "When the requested outcome is to open or reach a page, that destination must be showing now. "
    "An earlier visit followed by Back does not satisfy it. An executed click proves only the interaction, "
    "not that its destination loaded; an HTTP error is not a successful visit. "
    "For a search or filter, set fields and executed clicks do not prove the resulting content loaded. "
    "Require current matching results or an explicit empty-result state, not a pending search or loading view. "
    "A starting address or an intermediate visit in a longer task may be evidenced by the visited addresses, "
    "the first of which is where the run began; it does not have to remain open."
)


class DoneVerdict(StrEnum):
    ACCEPT = "accept"
    VERIFY = "verify"
    """Uncertain: the LLM verifier decides."""
    REJECT = "reject"


class DoneCheck(Frozen):
    verdict: DoneVerdict
    complete: float
    unmet: tuple[str, ...]
    """Requirement ids that are not satisfied or not evidenced."""
    doubted: tuple[str, ...]
    """Requirement ids Jev did not confidently confirm, which the verifier must see shown rather than assume."""
    answer: ComposedAnswer | None
    """The offered draft when Jev judged it already answers the task, so no composer needs to run."""
    cost: CostLine


VERDICT_TOKENS = 400
"""A verdict is three short lists and a boolean. Measured on a live count-pagination run, an uncapped verdict
spent 1166 output tokens and 13.7s restating the answer it was judging; the cap bounds that pathology while
leaving room for every requirement id on a long plan."""


class LLMVerdict(Frozen):
    missing: tuple[str, ...] = Field(description="Requirement ids the page or notes do not show as satisfied.")
    ungrounded: tuple[str, ...] = Field(
        default=(),
        description=(
            "Requirement ids whose facts were read from a page that is not the one the task described: a "
            "summary, preview or default listing reached by a guessed address, or a page that does not show "
            "the task's own query, filters, dates, sort or category in force. Name one here even when it has "
            "evidence, because the evidence is from the wrong page."
        ),
    )
    complete: bool = Field(description="Whether the evidence shows that every task requirement is satisfied.")


class Extraction(Frozen):
    data: JsonValue | None
    evidence: tuple[Evidence, ...]
    problem: str | None


def page_state(
    observation: Observation,
    notes: Notes,
    tokens: TokenBudget = _DEFAULT_CONFIG.tokens,
    *,
    questions: Sequence[str] = (),
    context: Mapping[str, JsonValue] | None = None,
) -> JsonValue:
    page: dict[str, JsonValue] = {"url": observation.url, "title": observation.title, "text": observation.viewport_text}
    state: dict[str, JsonValue] = {
        **(context or {}),
        # The dropdown choice runs apart from the action choice; it also needs today's date for relative months.
        "date": observation.today,
        "page": page,
        # Inputs and ARIA selection states are absent from innerText; without them a preview can
        # pass completion even though the requested filters were never applied.
        "controls": _controls(observation.controls),
        "notes": "",
    }

    def room() -> int:
        return tokens.remaining_chars(json.dumps(state), questions, jev=True)

    try:
        state["notes"] = notes.render(room(), preserve_requirements=True, json_encoded=True)
    except NotesTooLarge:
        # Preserve requirement evidence by cutting page text, then stateless controls, before refusing a verdict.
        page["text"] = ""
        try:
            state["notes"] = notes.render(room(), preserve_requirements=True, json_encoded=True)
        except NotesTooLarge:
            stateful = _stateful(observation.controls)
            state["controls"] = _controls(stateful)
            state["controls_omitted"] = len(observation.controls) - len(stateful)
            state["notes"] = notes.render(room(), preserve_requirements=True, json_encoded=True)
        page["text"] = cut_text(observation.viewport_text, room(), json_encoded=True)
    return state


def _stateful(controls: Iterable[Control]) -> list[Control]:
    """The controls holding a value, a check or a selection: the state page text does not show."""
    return [c for c in controls if (c.value, c.checked, c.selected) != (None, None, None)]


def _controls(controls: Iterable[Control]) -> list[JsonValue]:
    return [
        control.model_dump(
            mode="json", include={"label", "context", "role", "value", "checked", "selected"}, exclude_none=True
        )
        for control in controls
    ]


async def check_done(
    jev: JevClient,
    task: str,
    plan: Plan,
    observation: Observation,
    notes: Notes,
    thresholds: Thresholds,
    draft: ComposedAnswer | None = None,
    *,
    tokens: TokenBudget = _DEFAULT_CONFIG.tokens,
    history: Sequence[HistoryEntry] = (),
    visited: Sequence[str] = (),
) -> DoneCheck:
    """Judge completion, and whether `draft` answers the task as written, in the one Jev call.

    `history` is the run's action record. A page shows only where a process ended, so an accepted check that never
    saw the record would pass "enter one name, go back and correct it" on a run that typed the correction first.
    `visited` is every address the run has been on, first the one it began on, which no action records."""
    questions: dict[str, Question] = {
        "complete": NoulQuestion(
            instructions=(
                f"{UNTRUSTED}\nDoes the page or the notes visibly confirm completion of the task in state? "
                "Requested run_reports are copied from browser state and the action record after completion; "
                "they do not need page quotes or a written answer yet. "
                "Be strict: a matching link, a filled but unsubmitted form, or a partial result is not done. "
                "For comparisons, require the requested constraints and ordering or a comparison of all matching "
                "results. A highlighted result or query preview alone is insufficient."
            ),
            true="Everything the task asks for is visibly done.",
            false="Something the task asks for is missing, unsubmitted or unconfirmed.",
        )
    }
    unevidenced = {
        r.id for r in plan.requirements if r.kind is RequirementKind.INFORMATION and not notes.evidenced(r.id)
    }
    unmet = sorted(unevidenced)
    # A requirement that only directs the browsing -- go through every page, browse the category, visit each
    # listing -- is how the run reaches the information, not something the caller receives. Once every
    # information requirement is evidenced, holding the run open for one of these buys nothing: a count read off
    # the site's own exhaustive tag listing still looped on "navigate through every page" until its recoveries ran
    # out. The answer stands; the browsing step is reported as unconfirmed rather than blocking an evidenced answer.
    # The exemption stands in for an answer that is already proved. A plan that asks for no answer, or one with no
    # information requirement to prove, has no answer for a visit to stand in for, so its navigation is the task.
    standing_answer = plan.page_answer_expected and any(
        r.kind is RequirementKind.INFORMATION for r in plan.requirements
    )
    instrumental = (
        {r.id for r in plan.requirements if r.kind is RequirementKind.ACTION and _traversal_only(r.text)}
        if standing_answer and not unevidenced
        else set()
    )
    for requirement in plan.requirements:
        if requirement.id in instrumental:
            continue
        match requirement.kind:
            case RequirementKind.ACTION:
                questions[f"unmet_{requirement.id}"] = NoulQuestion(
                    instructions=(
                        f"{UNTRUSTED}\nIs this requirement not visibly satisfied? The actions in state are the run's "
                        "own record; a requirement that orders actions is satisfied only when they show that order. "
                        f"{_REACHED}\n\n{requirement.text}"
                    ),
                    true="It is not satisfied, or there is no visible confirmation.",
                    false="The page visibly confirms it is satisfied.",
                )
            case RequirementKind.INFORMATION if requirement.id not in unevidenced:
                # Evidence proves a quote came from a page, not that it answers: the wrong package's date is
                # evidenced too. Asked per requirement, a lookup gets the per-requirement confirmation an action
                # has, rather than every lookup going to the verifier on the strict holistic question alone.
                questions[f"unmet_{requirement.id}"] = NoulQuestion(
                    instructions=(
                        f"{UNTRUSTED}\nDo the notes lack the facts this requirement needs? A comparison or conclusion "
                        "needs every fact it is drawn from, for the right entities; the conclusion itself need not be "
                        f"written.\n\n{requirement.text}"
                    ),
                    true="A fact it needs is missing, about something else, or only a preview.",
                    false="The notes hold every fact it needs.",
                )
            case RequirementKind.INFORMATION:
                pass
            case unreachable:
                assert_never(unreachable)
    if draft is not None:
        # Asked here rather than on its own because this call is already being paid for: judging the
        # draft costs one more answer in a request the run makes anyway, where a composer costs seconds.
        questions["draft_needs_writing"] = NoulQuestion(
            instructions=(
                f"{UNTRUSTED}\nDoes the draft in state need rewriting before it answers the task? It does if it "
                "misses part of what was asked, repeats or contradicts itself, includes unrelated facts, leaves "
                "a comparison, count or calculation undone, or gives a value without saying which part of the "
                "task it answers when that is not plain. A short quoted value or labelled field can answer as "
                "written; it does not need a full sentence. A comparison that states its result can include the "
                "values it compared. These are supporting facts, not unrelated ones. Judge the requested answer: "
                "when a task asks to perform actions and report a fact, the answer need only report that fact, "
                "not recap each action. "
                "Completion of the actions is checked separately. Requested run_reports are appended by code from "
                "browser state and the action record, so their absence from this page-fact draft is not an omission."
            ),
            true="Yes, it needs rewriting before it answers the task.",
            false="No, it answers the task as written.",
        )
    context: dict[str, JsonValue] = {"task": task, "run_reports": [report.value for report in plan.run_reports]}
    if history:
        context["actions"] = "\n".join(map(_step, history))
    if visited:
        context["visited"] = list(visited)
    if draft is not None:
        context["draft"] = draft.answer
    try:
        state = page_state(
            observation, notes, tokens, questions=[q.model_dump_json() for q in questions.values()], context=context
        )
    except NotesTooLarge:
        if draft is None:
            raise
        # The optional answer draft crowded out the quotes required to judge completion.
        context.pop("draft")
        questions.pop("draft_needs_writing")
        draft = None
        state = page_state(
            observation, notes, tokens, questions=[q.model_dump_json() for q in questions.values()], context=context
        )
    evaluation = await jev.evaluate(state, questions)
    for requirement in plan.requirements:
        if requirement.id in instrumental:
            continue
        if _probability(evaluation.answers, f"unmet_{requirement.id}") > thresholds.claim_problem_above:
            unmet.append(requirement.id)
    complete = _probability(evaluation.answers, "complete")
    # Every requirement confirmed one by one is stronger evidence than the strict holistic question alone,
    # which asks about the whole task at once and doubts a right page as often as it confirms it.
    # An answer Jev did not give confirms nothing, and a task with nothing to do keeps the verifier.
    doubted = tuple(
        r.id
        for r in plan.requirements
        if r.id not in instrumental
        and not (
            isinstance(doubt := evaluation.answers.get(f"unmet_{r.id}"), NoulAnswer)
            and doubt.probability < thresholds.requirement_confirmed_below
        )
    )
    confirmed = bool(plan.requirements) and not doubted
    # Jev reliably confirms a visible result but is too strict to reject one on its own, so apart from
    # information nobody has read, doubt goes to the verifier rather than straight back to work.
    if unevidenced:
        verdict = DoneVerdict.REJECT
    elif not unmet and (
        complete >= thresholds.done_accept_from or (confirmed and complete >= thresholds.done_confirmed_from)
    ):
        verdict = DoneVerdict.ACCEPT
    else:
        verdict = DoneVerdict.VERIFY
    # An answer Jev did not give is not a yes: only a present, confident "no rewrite needed" skips the composer.
    doubt = evaluation.answers.get("draft_needs_writing")
    ready = isinstance(doubt, NoulAnswer) and doubt.probability < thresholds.rewrite_from
    if draft is not None:
        trace(
            "draft",
            rewrite=doubt.probability if isinstance(doubt, NoulAnswer) else None,
            ready=ready,
            claims=len(draft.claims),
        )
    return DoneCheck(
        verdict=verdict,
        complete=complete,
        unmet=tuple(unmet),
        doubted=doubted,
        answer=draft if ready else None,
        cost=evaluation.cost,
    )


def _grounding(notes: Notes, plan: Plan, invented: Sequence[str]) -> str:
    """Where each requirement's facts were read, and which addresses the run guessed rather than clicked to.

    A requirement can be evidenced from a page that is not the search the task asked for: a proposed address
    opened a flights summary, the reader quoted a price from it, and every check passed on evidence the run
    should never have had. The verifier cannot see that without being told which page each fact came from.
    """
    lines = []
    for requirement in plan.requirements:
        urls = sorted({item.url for item in notes.supporting_evidence(requirement.id)})
        if urls:
            lines.append(f"- {requirement.id}: {', '.join(urls)}")
    parts = []
    if lines:
        parts.append("## Where each requirement's facts were read\n" + "\n".join(lines))
    if invented:
        parts.append("## Addresses this run built from the task\n" + "\n".join(f"- {u}" for u in invented))
    return ("\n\n".join(parts) + "\n\n") if parts else ""


def _visited(visited: Sequence[str]) -> str:
    return "## Visited addresses\n" + "".join(f"- {url}\n" for url in visited) + "\n" if visited else ""


def _step(entry: HistoryEntry) -> str:
    """One action as the run recorded it: the value typed and what it visibly did, both already redacted."""
    action = f"{entry.operation.value if entry.operation else 'open'} {entry.target or ''}".strip()
    typed = f" = {entry.text!r}" if entry.text is not None else ""
    effect = f" ({entry.effect})" if entry.effect else ""
    return f"- {action}{typed} -> {entry.outcome.value}{effect}"


_VISUAL_ONLY_INPUTS = frozenset({"file", "color", "range", "date", "time", "month", "week", "datetime-local"})


def visual_only_state(observation: Observation) -> bool:
    """Whether this page's state is carried by pixels, so a verifier needs the screenshot.

    Page text, the controls and the notes cover a form, a listing and a quoted answer. What they do not
    carry is a checked box, a selected option, a calendar's highlighted day, a canvas or a chart, a file
    input's chosen file, or a dialog drawn over the page. A page with almost no text is judged the same
    way: there is nothing in it to read, so the screenshot is the only evidence there is.
    """
    if observation.dialog is not None:
        return True
    # A page can carry its state in pixels without a single visual control: snapshot.js offers ordinary images
    # and canvases as content, not as controls, so the control list alone cannot see a chart or a picture.
    if observation.visual_content:
        return True
    if len(observation.viewport_text.strip()) < 40:
        return True
    return any(
        c.checked is not None
        or c.selected is not None
        or (c.input_type or "").lower() in _VISUAL_ONLY_INPUTS
        or c.role.lower() in {"canvas", "img", "image"}
        for c in observation.controls
    )


def verify_messages(
    task: str,
    plan: Plan,
    observation: Observation,
    screenshots: tuple[bytes, ...],
    notes: Notes,
    history: Sequence[HistoryEntry],
    *,
    doubted: Sequence[str] = (),
    invented: Sequence[str] = (),
    visited: Sequence[str] = (),
    config: Config = _DEFAULT_CONFIG,
) -> list[Message]:
    """The verifier's request, in the order that keeps the stable prefix ahead of the page.

    `doubted` names the requirements the done check doubted, which the verifier must see shown, not assume.

    `invented` names addresses this run built from the task rather than reached by clicking, which are the ones
    that can land on a page that looks like the answer without being the search the task asked for. `visited` is
    every address the run has been on, first the one it began on.

    The task, its requirements and the verdict rules are the same on every finish attempt of a run, so they sit
    in the system message and at the head of the user message; the page text and notes, which change every time,
    come last. A provider that caches a request prefix can then reuse it across attempts, which is the ordering
    Browser Use reports as its own speed win.
    """
    # A flights search passed here with Jev doubting its one requirement at 0.14: the rows matched, and nothing
    # asked whether the nonstop filter the task named had ever been applied.
    requirements = "\n".join(
        f"- {r.id}: {r.text}{' (doubted: show it is satisfied, or name it missing)' if r.id in doubted else ''}"
        for r in plan.requirements
    )
    # What was typed and what each action visibly did, not only that it ran: told "fill First Name -> executed",
    # the verifier could not tell a run that corrected a value from one that typed the correction first.
    steps = "\n".join(map(_step, history))
    # A filter's checked state is absent from page text, and a screenshot shows it only when it is in view: a flights
    # search the verifier passed had matching rows and no nonstop filter applied.
    # Only what is set: every empty field on a long form would crowd the request without saying anything.
    stateful = [c for c in observation.controls if c.checked or c.selected or c.value]
    evidence = (
        "the screenshot, set controls, page text and notes" if screenshots else "the set controls, page text and notes"
    )
    instruction = (
        f"## Verdict\nDecide from {evidence} whether the task is finished. "
        "Be strict and name every requirement id that is not visibly satisfied. An action requirement (clicking a "
        "link or button, filling a field, submitting a form, navigating) is satisfied when the steps taken show it "
        "executed on the control the task meant and produced the requested outcome. The task may paraphrase "
        "the control's label; the acted-on control "
        "is its equivalent when the page offered no closer match, which is the match the agent made when it acted. "
        "The steps are the run's own record, and a click that navigated is not visible on the page it left. Be "
        "strict about whether the action happened, not about the task's wording of the label. A requirement that "
        "orders actions (enter one value, then go back and change it) is satisfied only when the steps show that "
        f"order. {_REACHED} A date relative to today is judged against the current date. A requirement to "
        "compare, count or conclude from facts is satisfied when the notes hold those facts: the answer "
        "draws the conclusion, and no page shows it. A requirement to narrow a search or listing (a filter, "
        "option or sort) is satisfied when the page shows it applied, in a set control, the address or the "
        "page's own filter text, not when the rows in view happen to match it.\n\n"
        "Then judge where the facts came from. A page holding values of the right kind is not the page the "
        "task described unless it shows that task's own query, filters, dates, sort or category in force. A "
        "summary, preview, default or related listing can show prices or rows that are not the ones asked "
        "for. Name in ungrounded every requirement whose facts were read from such a page, even when it has "
        "evidence. An address this run built from the task can land on the wrong search: judge such a page by "
        "what it shows, not by how the run reached it. An answer states the terms a value was quoted under, "
        "such as what it covers and the dates it applies to."
    )
    messages = [
        Message(role="system", content=f"# Verifier\n{UNTRUSTED}\n\n{instruction}"),
        Message(
            role="user",
            content=(
                f"## Task\n{task}\n\n## Current date\n{observation.today}\n\n"
                f"## Requirements\n{requirements}\n\n"
                f"## Run reports\n{[report.value for report in plan.run_reports]}\n"
                "These reports are copied from browser state and the action record after completion; "
                "they do not need page quotes or a written answer yet.\n\n"
                f"## Steps taken\n{steps}\n\n"
                f"## Set controls\n{json.dumps(_controls(stateful))}\n\n{_grounding(notes, plan, invented)}"
                f"{_visited(visited)}## Page\n{observation.url}\n"
            ),
            images=screenshots,
        ),
    ]

    def room(*parts: str) -> int:
        prompt = "".join(message.content for message in messages) + "".join(parts)
        return config.tokens.remaining_chars(prompt + json.dumps(LLMVerdict.model_json_schema()))

    # As in the done check's page state, the notes' requirement evidence is placed first and the page text is cut
    # to what remains: a "View more" results page once left the notes no room at all and ended the run.
    rendered = notes.render(room("\n\n## Notes\n"), preserve_requirements=True)
    text = cut_text(observation.viewport_text, room("\n\n## Notes\n", rendered))
    messages[-1] = messages[-1].model_copy(
        update={"content": f"{messages[-1].content}\n\n## Notes\n{rendered}\n\n{text}"}
    )
    return messages


def verify_cache_key(messages: Sequence[Message]) -> str:
    """A digest of the exact request: the prompt and every image byte, in order.

    Two finish attempts whose rendered request is byte-identical cannot need a second verdict, so the
    verdict paid for by the first is reused (see `Agent._verify_done`).
    """
    digest = hashlib.sha256()
    for message in messages:
        digest.update(message.role.encode())
        digest.update(message.content.encode())
        for image in message.images:
            digest.update(hashlib.sha256(image).digest())
    return digest.hexdigest()


async def llm_verify(
    llm: LLMClient,
    task: str,
    plan: Plan,
    observation: Observation,
    screenshots: tuple[bytes, ...],
    notes: Notes,
    history: Sequence[HistoryEntry],
    *,
    doubted: Sequence[str] = (),
    invented: Sequence[str] = (),
    visited: Sequence[str] = (),
    config: Config = _DEFAULT_CONFIG,
    ledger: Ledger | None = None,
) -> Generation[LLMVerdict]:
    """The verifier's verdict for one rendered request; see `verify_messages` for the shape of the evidence."""
    messages = verify_messages(
        task,
        plan,
        observation,
        screenshots,
        notes,
        history,
        doubted=doubted,
        invented=invented,
        visited=visited,
        config=config,
    )
    return await llm.generate(
        LLMPurpose.VERIFY,
        messages,
        LLMVerdict,
        max_output_tokens=VERDICT_TOKENS,
        ledger=ledger,
    )


class _OutputSource(Frozen):
    url_ref: str
    quote: str
    complete_source: bool = False
    source_id: str
    frame_id: str | None
    page_title: str | None
    control_context: SourceControl | None = None


class _CollectionRead(Frozen):
    read: int
    """The read's place in reading order, from 1: one address can be read in several states."""
    url_ref: str


class _CollectionMetadata(Frozen):
    @model_serializer(mode="wrap")
    def _omit_unset(self, handler: SerializerFunctionWrapHandler) -> dict[str, object]:
        # A single-page list or a lone group has no pages or standing to show; an empty field there would only
        # describe a proof that does not apply.
        return {key: value for key, value in handler(self).items() if key not in _OPTIONAL_METADATA or value}


_OPTIONAL_METADATA = frozenset({"standing", "pages_read"})


class _OutputCount(_CollectionMetadata):
    scope: str | None
    group: str
    count: int
    complete: bool
    record_indices: tuple[int, ...]
    standing: Standing | None = None
    """This group's rank among every group counted in the same scope; None when the scope has one group."""
    pages_read: tuple[_CollectionRead, ...] = ()
    """Every read code made of this collection, in order, including reads with no counted record."""


class _OutputComparison(_CollectionMetadata):
    scope: str | None
    requirement: str
    """The comparison's code-owned requirement id: two different lists can share a heading, so the scope text the
    reader wrote cannot identify a collection on its own."""
    complete: bool
    record_indices: tuple[int, ...]
    pages_read: tuple[_CollectionRead, ...] = ()
    """Every read code made of this collection, in order, including reads with no compared record."""


class _OutputClaim(Frozen):
    text: str
    cited_sources: tuple[_OutputSource, ...]
    counted_records: tuple[_OutputCount, ...] = ()
    compared_records: tuple[_OutputComparison, ...] = ()


class _OutputContext(Frozen):
    answer: str
    claims: tuple[_OutputClaim, ...]
    urls: dict[str, str]


def _output_context(composed: ComposedAnswer, notes: Notes) -> _OutputContext | None:
    requirements = {requirement.id: requirement.text for requirement in composed.requirements}
    urls: dict[str, str] = {}
    claims = []

    def pages_read(requirement_id: str) -> tuple[_CollectionRead, ...]:
        return tuple(
            _CollectionRead(read=index, url_ref=urls.setdefault(read.url, f"u{len(urls)}"))
            for index, read in enumerate(notes.collection_pages(requirement_id), 1)
        )

    for claim in composed.claims:
        expanded = notes.expand_evidence_ids(claim.evidence_ids)
        if any(not notes.current(key) for key in expanded):
            return None
        # A refused quote is dropped from the check, so a claim resting on it alone has no source and
        # one resting on it beside a valid quote is judged against the valid quote only.
        known = notes.usable_citations(expanded)
        keys = tuple(key for key in expanded if key in known)
        if not keys:
            return None
        sources = []
        for key in keys:
            evidence = known[key]
            page = notes.captured_page(evidence)
            sources.append(
                _OutputSource(
                    url_ref=urls.setdefault(evidence.url, f"u{len(urls)}"),
                    quote=evidence.quote,
                    complete_source=evidence.complete_source,
                    control_context=evidence.control_context,
                    source_id=evidence.source_id,
                    frame_id=evidence.frame_id,
                    page_title=page.title if page and evidence.frame_id is None else None,
                )
            )
        positions = {key: index for index, key in enumerate(keys)}
        counted = []
        compared = []
        for fact in notes.facts:
            if fact.comparison is not None and (
                fact_id(fact) in expanded
                or (
                    isinstance(fact, CollectionFact)
                    and fact.comparison.complete
                    and fact.comparison.records
                    and fact.basis
                    and set(fact.basis) <= positions.keys()
                    and notes.current(fact_id(fact))
                    and set(fact.comparison.records) <= positions.keys()
                )
            ):
                comparison = fact.comparison
                if any(record not in positions for record in comparison.records):
                    return None
                compared.append(
                    _OutputComparison(
                        scope=fact.scope
                        if isinstance(fact, CollectionFact)
                        else requirements.get(comparison.requirement_id),
                        requirement=comparison.requirement_id,
                        complete=comparison.complete
                        and (
                            isinstance(fact, CollectionFact)
                            or comparison.requirement_id in notes.fact_requirements(fact_id(fact))
                        ),
                        record_indices=tuple(dict.fromkeys(positions[record] for record in comparison.records)),
                        pages_read=pages_read(comparison.requirement_id),
                    )
                )
            if fact.tally is None or fact_id(fact) not in expanded:
                continue
            if any(record not in positions for record in fact.tally.records):
                return None
            counted.append(
                _OutputCount(
                    scope=requirements.get(fact.tally.requirement_id),
                    group=fact.tally.key,
                    count=fact.tally.count,
                    complete=fact.tally.requirement_id in notes.fact_requirements(fact_id(fact)),
                    record_indices=tuple(dict.fromkeys(positions[record] for record in fact.tally.records)),
                    standing=notes.standing(fact.tally) if claim.ranks else None,
                    pages_read=pages_read(fact.tally.requirement_id),
                )
            )
        # Flattening a count or comparison loses its collection scope and whether every page was read.
        claims.append(
            _OutputClaim(
                text=claim.text,
                cited_sources=tuple(sources),
                counted_records=tuple(counted),
                compared_records=tuple(compared),
            )
        )
    return _OutputContext(
        answer=composed.answer, claims=tuple(claims), urls={alias: url for url, alias in urls.items()}
    )


def check_answer_outputs(
    composed: ComposedAnswer,
    notes: Notes,
    checks: Sequence[str],
    *,
    missing_outputs: list[str] | None = None,
    corrections: list[AnswerCorrection] | None = None,
    verified_outputs: list[str] | None = None,
    independent_fields: bool = True,
) -> bool:
    """Whether code can ground every answer check: each one has a claim, and every claim cites current quotes
    and the records its count or comparison rests on.

    No model is asked. A second LLM opinion on quoted answers used to follow, and over the 0.7.1 campaign and 27
    traced reruns it never caught a wrong answer while rejecting correct ones (a version cited without the
    "latest release" label beside it, a table total without proof the rows were all the rows); it was most of a
    lookup's wall time. Whether the claims are supported, contradicted or leave a requirement out is Jev's claim
    check beside this one.
    """
    if independent_fields and isinstance(composed, FieldAnswer):
        passed = []
        for check in checks:
            indices = composed.output_claims.get(check, ())
            if any(index < 0 or index >= len(composed.claims) for index in indices):
                indices = ()
            # A field with no claim is still checked, so its rejection carries the correction the composer needs.
            claims = tuple(composed.claims[index] for index in indices)
            base = assemble_answer(claims, notes, composed.requirements)
            field = FieldAnswer(**base.model_dump(), output_claims={check: tuple(range(len(claims)))})
            if _outputs_grounded(field, notes, (check,), missing_outputs=missing_outputs, corrections=corrections):
                passed.append(check)
        if verified_outputs is not None:
            verified_outputs.extend(passed)
        missing = [check for check in checks if check not in passed]
        if missing and missing_outputs is not None:
            missing_outputs.extend(c for c in missing if c not in missing_outputs)
        return not missing
    return _outputs_grounded(composed, notes, checks, missing_outputs=missing_outputs, corrections=corrections)


def _outputs_grounded(
    composed: ComposedAnswer,
    notes: Notes,
    checks: Sequence[str],
    *,
    missing_outputs: list[str] | None,
    corrections: list[AnswerCorrection] | None,
) -> bool:
    def reject(failed: Sequence[str], *, repair: str) -> bool:
        if missing_outputs is not None:
            missing_outputs.extend(check for check in failed if check not in missing_outputs)
        if corrections is not None:
            # Missing outputs can be repaired from uncited notes before browsing again.
            corrected = {(correction.stage, correction.criterion) for correction in corrections}
            corrections.extend(
                AnswerCorrection(stage="source", criterion=criterion, claims=composed.claims, reason=repair)
                for criterion in failed
                if ("source", criterion) not in corrected
            )
        return False

    if not checks:
        return True
    if any(not check.strip() for check in checks):
        return False
    if isinstance(composed, FieldAnswer):
        missing = [
            check
            for check in checks
            if not composed.output_claims.get(check)
            or any(index < 0 or index >= len(composed.claims) for index in composed.output_claims[check])
        ]
        if missing:
            return reject(missing, repair="This field has no claim. Report its value or a supported bounded absence.")
    context = _output_context(composed, notes)
    if context is None:
        known = {fact_id(fact) for fact in notes.facts}
        if any(key not in known for claim in composed.claims for key in notes.expand_evidence_ids(claim.evidence_ids)):
            return False
        return reject(
            checks,
            repair="Some cited facts are no longer current or collection records are missing. "
            "Use current source quotes and preserve their collection scope; do not cite superseded conclusions.",
        )

    # A count, ranking, winner or absence is only as good as the reading of its list: code knows whether every
    # page and chunk was read, so an unfinished one is refused here rather than argued over by a model. A partial
    # comparison read is different: reading the rest of a sorted list records the whole list as its own comparison,
    # and a claim that carries that complete one beside the earlier partial one rests on the list read to its end.
    # Only a partial comparison no complete comparison of the same scope covers leaves the claim unfinished.
    def unfinished(claim: _OutputClaim) -> bool:
        if any(not record.complete for record in claim.counted_records):
            return True
        finished = [record for record in claim.compared_records if record.complete]
        return any(
            not record.complete
            and not any(
                other.requirement == record.requirement
                and other.scope == record.scope
                and set(record.record_indices) <= set(other.record_indices)
                for other in finished
            )
            for record in claim.compared_records
        )

    if any(unfinished(claim) for claim in context.claims):
        return reject(
            checks,
            repair="A count, ranking, winner or absence rests on a list that was not read to its end. Read the rest "
            "of the list before reporting it.",
        )
    return True


async def check_claims(
    jev: JevClient,
    composed: ComposedAnswer,
    notes: Notes,
    thresholds: Thresholds,
    *,
    tokens: TokenBudget = _DEFAULT_CONFIG.tokens,
    ledger: Ledger | None = None,
    transaction_evidence_ids: Collection[str] = (),
    answer_checks: Sequence[str] = (),
    missing_outputs: list[str] | None = None,
    corrections: list[AnswerCorrection] | None = None,
    verified_fields: list[FieldAnswer] | None = None,
    independent_fields: bool = True,
) -> ComposedAnswer | None:
    """The answer without any claim a check doubts, or None when a requirement is omitted from what is left or the
    pages where the run committed an action contradict it.

    The composer adds claims its quotes do not cover (a login page's nav links, a Log out button), and one of
    those failed three runs in four of a correct sign-in answer. Removing a doubted claim asserts nothing new,
    so it is honest as long as the rest still answers: the omission check is asked again of what remains.
    """
    questions = claim_check_questions(composed, notes, tokens=tokens, answer_checks=answer_checks)
    # An action-only task can finish without factual claims. Its completion was checked already,
    # and Jev rejects an empty question batch; dropped or uncited answer text still cannot pass.
    if not questions and not answer_checks:
        return composed if not composed.answer and composed.dropped_claims == 0 else None
    transaction = transaction_check_question(composed, notes, transaction_evidence_ids, tokens=tokens)

    async def claims() -> Mapping[str, object]:
        if transaction is None:
            return await _ask(jev, composed, questions, ledger, tokens)
        # Asked apart so the committed pages cannot take the omission check's notes budget.
        checked = {TRANSACTION_CONTRADICTED: transaction}
        # Both asks finish before either failure propagates, so neither is billed after the check has returned.
        claimed, committed = await asyncio.gather(
            _ask(jev, composed, questions, ledger, tokens),
            _ask(jev, composed, checked, ledger, tokens),
            return_exceptions=True,
        )
        if isinstance(claimed, BaseException):
            raise claimed
        if isinstance(committed, BaseException):
            raise committed
        return {**claimed, **committed}

    verified_outputs: list[str] = []
    outputs_supported = check_answer_outputs(
        composed,
        notes,
        answer_checks,
        missing_outputs=missing_outputs,
        corrections=corrections,
        verified_outputs=verified_outputs,
        independent_fields=independent_fields,
    )
    answers = await claims()
    if transaction is not None:
        questions = {**questions, TRANSACTION_CONTRADICTED: transaction}
    if any(not isinstance(answers.get(key), NoulAnswer) for key in questions):
        return None
    limit = thresholds.claim_problem_above
    trace(
        "claims",
        limit=limit,
        scores={key: round(_probability(answers, key), 3) for key in questions},
        cited=[list(claim.evidence_ids) for claim in composed.claims],
        dropped=composed.dropped_claims,
    )
    if (
        composed.dropped_claims
        or max(_probability(answers, _OMITTED), _probability(answers, TRANSACTION_CONTRADICTED)) > limit
    ):
        return None
    kept = tuple(
        claim
        for index, claim in enumerate(composed.claims)
        if max(_probability(answers, f"unsupported_{index}"), _probability(answers, f"contradicted_{index}")) <= limit
    )
    unstated = [
        check for index, check in enumerate(answer_checks) if _probability(answers, f"{OUTPUT_MISSING}{index}") > limit
    ]
    if verified_fields is not None and isinstance(composed, FieldAnswer):
        # Fields Jev found stated and supported are kept even when a sibling is missing, so the next compose
        # rewrites only what failed.
        outputs = {
            check: indices
            for check, indices in composed.output_claims.items()
            if check in verified_outputs
            and check not in unstated
            and indices
            and all(composed.claims[i] in kept for i in indices)
        }
        accepted = tuple(dict.fromkeys(composed.claims[i] for indices in outputs.values() for i in indices))
        if accepted:
            base = assemble_answer(accepted, notes, composed.requirements)
            verified_fields.append(
                FieldAnswer(
                    **base.model_dump(),
                    output_claims={
                        check: tuple(accepted.index(composed.claims[i]) for i in indices)
                        for check, indices in outputs.items()
                    },
                )
            )
    if unstated:
        # Named, so recovery looks for the missing output and the composer is told what to add.
        if missing_outputs is not None:
            missing_outputs.extend(check for check in unstated if check not in missing_outputs)
        if corrections is not None:
            corrected = {(correction.stage, correction.criterion) for correction in corrections}
            corrections.extend(
                AnswerCorrection(
                    stage="source",
                    criterion=check,
                    claims=composed.claims,
                    reason="The answer does not state this output. Report it from the notes or a supported bounded "
                    "absence.",
                )
                for check in unstated
                if ("source", check) not in corrected
            )
        return None
    if not outputs_supported:
        return None
    if len(kept) == len(composed.claims):
        return composed
    # An action-only task needs no answer: the done check judged its completion, so a restatement of the action
    # that its confirmation quote cannot carry (the name and topic a form was sent with) is dropped, not fatal.
    if not kept and any(r.kind is RequirementKind.INFORMATION for r in composed.requirements):
        return None
    # A claim can be doubted for proving too little (a title cited as "the most expensive" without the prices it
    # beat), and the omission check then passed the price alone as the whole answer. A requirement the answer
    # cited evidence for and no longer does is omitted, whatever the check says, so the composer writes it again.
    cited = {key for claim in kept for key in claim.evidence_ids}
    was_cited = {key for claim in composed.claims for key in claim.evidence_ids}
    for requirement in composed.requirements:
        supporting = {key for key, _ in notes.supporting(requirement.id)}
        if supporting & was_cited and not supporting & cited:
            return None
    pruned = assemble_answer(kept, notes, composed.requirements)
    if isinstance(composed, FieldAnswer):
        pruned = FieldAnswer(
            **pruned.model_dump(),
            output_claims={
                check: tuple(kept.index(composed.claims[i]) for i in indices if composed.claims[i] in kept)
                for check, indices in composed.output_claims.items()
            },
        )
    if answer_checks and not check_answer_outputs(
        pruned,
        notes,
        answer_checks,
        missing_outputs=missing_outputs,
        corrections=corrections,
        independent_fields=independent_fields,
    ):
        return None
    omission = {
        key: q
        for key, q in claim_check_questions(pruned, notes, tokens=tokens, answer_checks=answer_checks).items()
        if key == _OMITTED or key.startswith(OUTPUT_MISSING)
    }
    # A grouped requirement can retain citations while pruning one of its requested fields.
    # Check each removed claim against the remaining answer without notes supplying the missing output.
    information = [r for r in composed.requirements if r.kind is RequirementKind.INFORMATION]
    if information:
        requirements = "\n".join(r.model_dump_json() for r in information)
        for index, claim in enumerate(composed.claims):
            if claim in kept:
                continue
            omission[f"removed_output_{index}"] = NoulQuestion(
                instructions=(
                    f"{UNTRUSTED}\n\n# Requirements\n{requirements}\n\n"
                    f"# Remaining answer\n{pruned.answer}\n\n# Removed claim\n{claim.text}\n\n"
                    "Does removing this claim leave any requested output missing from the remaining answer? "
                    "Check each requested field for each requested entity separately. The removed claim is "
                    "not evidence and need not be true. Judge whether its subject still needs an answer. "
                    "An optional detail or an output already stated elsewhere does not count as missing."
                ),
                true="Yes, a requested output is missing after this removal.",
                false="No, the remaining answer still states every output affected by this removal.",
            )
    if omission:
        answers = await _ask(jev, pruned, omission, ledger, tokens)
        # Removing a requested field cannot be waved through on an uncertain coverage decision.
        if any(
            not isinstance(answers.get(key), NoulAnswer)
            or _probability(answers, key)
            > (limit if key == _OMITTED or key.startswith(OUTPUT_MISSING) else min(limit, 1 - limit))
            for key in omission
        ):
            return None
    return pruned


async def _ask(
    jev: JevClient,
    composed: ComposedAnswer,
    questions: Mapping[str, NoulQuestion],
    ledger: Ledger | None,
    tokens: TokenBudget,
) -> Mapping[str, object]:
    answered = await evaluate_batches(
        jev, {"answer": composed.answer}, questions, tokens=tokens, ledger=ledger, allow_failed_batches=False
    )
    return {} if answered is None else answered.answers


async def extract(
    jev: JevClient,
    llm: LLMClient,
    task: str,
    capture: Capture,
    schema: type[BaseModel],
    *,
    notes: Notes | None = None,
    tokens: TokenBudget = _DEFAULT_CONFIG.tokens,
    ledger: Ledger | None = None,
) -> Extraction:
    """Text fields are proposed by the LLM and kept only when quoted verbatim from the page; other scalars are
    copied from the typed spans Jev points at. A field with no supported value fails the extraction.
    """
    values: dict[str, JsonValue] = {}
    evidence: list[Evidence] = []
    text_fields = {name: field for name, field in schema.model_fields.items() if field.annotation is str}
    if text_fields:
        # Notes first: they hold every page a comparison read, where the final page shows one side of it.
        proposed = (
            await propose_text_fields_from_notes(
                llm, task, notes, text_fields, capture=capture, tokens=tokens, ledger=ledger
            )
            if notes is not None
            else {}
        )
        unseen = {name: field for name, field in text_fields.items() if name not in proposed}
        if unseen:
            proposed |= (await propose_text_fields(llm, task, capture, unseen, tokens=tokens, ledger=ledger))[0]
        for name, (value, quoted) in proposed.items():
            values[name] = value
            evidence.append(quoted)
    for name, field in schema.model_fields.items():
        if name in text_fields:
            continue
        candidates = field_candidates(capture, field)
        if isinstance(candidates, UnsupportedField):
            return Extraction(data=None, evidence=(), problem=f"{name}: {candidates.reason}")
        if notes is not None:
            # The notes hold every page the run read; the final capture holds only the one it ended on, which
            # for a sorted comparison is one side of it. Both pages are offered, the current one first.
            quoted = field_candidates_from_notes(notes, field, capture=capture)
            if isinstance(quoted, UnsupportedField):
                return Extraction(data=None, evidence=(), problem=f"{name}: {quoted.reason}")
            candidates = merge_candidates(candidates, quoted)
        if not candidates:
            continue
        try:
            copied = await choose_candidate(
                jev,
                {"task": task, "page": {"url": capture.url, "title": capture.title}},
                field,
                candidates,
                name=name,
                task=task,
                record_fields=tuple(schema.model_fields),
                ledger=ledger,
            )
        except ValueError as error:
            return Extraction(data=None, evidence=tuple(evidence), problem=f"{name}: {error}")
        if copied is not None:
            values[name] = str(copied[0]) if not isinstance(copied[0], int | float | bool | str) else copied[0]
            evidence.append(copied[1])
    # A default the page never showed is the caller's placeholder, not data; only None may stand for absent.
    unsupported = [
        name
        for name, field in schema.model_fields.items()
        if name not in values and not field.is_required() and field.get_default(call_default_factory=True) is not None
    ]
    if unsupported:
        return Extraction(
            data=None, evidence=tuple(evidence), problem=f"no value on the page for {', '.join(unsupported)}"
        )
    try:
        data = schema.model_validate(values).model_dump(mode="json")
    except ValidationError as error:
        return Extraction(data=None, evidence=tuple(evidence), problem=str(error)[:500])
    return Extraction(data=data, evidence=tuple(evidence), problem=None)


_OMITTED = "requirement_omitted"

# How a requirement that only moves through pages is worded, and the verbs that make it something the caller
# receives instead (a submission, an account, a purchase). A requirement naming either of these is not traversal.
_TRAVERSAL = re.compile(
    r"\b(?:navigate|browse|go through|going through|go to each|visit|scroll|page through|paging through|"
    r"read through|look through|open each|move through|click through|walk through|search through|explore)\b",
    re.IGNORECASE,
)
_COMMITTING = re.compile(
    r"\b(?:submit|send|sign in|sign up|log in|log out|login|logout|register|add to (?:the )?(?:cart|basket|bag)|"
    r"place (?:the |an |your )?order|buy|purchase|pay|check ?out|book|reserve|delete|remove|upload|download|"
    r"fill|type|enter|apply|save|create|post|publish|confirm|accept|subscribe|checkout)\b",
    re.IGNORECASE,
)


def _traversal_only(text: str) -> bool:
    """Whether a requirement only directs the browsing, rather than naming something the caller receives.

    A traversal requirement is instrumental: its point is the information it reaches, so once that information is
    evidenced and verified, an unconfirmed visit does not leave the task unanswered. Anything that changes state
    on the site keeps blocking, because a run that never submitted the form or placed the order has not done what
    the caller asked for however good its evidence is.
    """
    return bool(_TRAVERSAL.search(text)) and not _COMMITTING.search(text)


def _probability(answers: Mapping[str, object], key: str) -> float:
    answer = answers.get(key)
    return answer.probability if isinstance(answer, NoulAnswer) else 0.0
