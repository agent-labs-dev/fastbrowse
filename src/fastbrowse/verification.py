"""Deciding whether a run is actually finished, and whether its answer and extracted data hold up.

Jev checks completion, individual action requirements and draft quality. An LLM looks at a screenshot
when those answers leave completion uncertain.
"""

import asyncio
import hashlib
import json
from collections.abc import Collection, Iterable, Mapping, Sequence
from enum import StrEnum
from typing import Literal, assert_never

from pydantic import BaseModel, Field, JsonValue, ValidationError

from fastbrowse.batches import evaluate_batches
from fastbrowse.config import Config, Thresholds, TokenBudget
from fastbrowse.jev import (
    MAX_CHOICE_OPTIONS,
    ChoiceAnswer,
    ChoiceQuestion,
    JevClient,
    NoulAnswer,
    NoulQuestion,
    Question,
)
from fastbrowse.llm import Generation, LLMClient, Message
from fastbrowse.memory import Notes, NotesTooLarge, fact_id
from fastbrowse.models import UNTRUSTED, CostLine, Evidence, FactReader, Frozen, LLMPurpose
from fastbrowse.page import Capture, Control, Observation, cut_text
from fastbrowse.planner import Plan, RequirementKind
from fastbrowse.policy import HistoryEntry
from fastbrowse.retrieval import (
    TRANSACTION_CONTRADICTED,
    AnswerCorrection,
    Claim,
    ComposedAnswer,
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
    for requirement in plan.requirements:
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
        if _probability(evaluation.answers, f"unmet_{requirement.id}") > thresholds.claim_problem_above:
            unmet.append(requirement.id)
    complete = _probability(evaluation.answers, "complete")
    # Every requirement confirmed one by one is stronger evidence than the strict holistic question alone,
    # which asks about the whole task at once and doubts a right page as often as it confirms it.
    # An answer Jev did not give confirms nothing, and a task with nothing to do keeps the verifier.
    doubted = tuple(
        r.id
        for r in plan.requirements
        if not (
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
    """`doubted` names the requirements the done check doubted, which the verifier must see shown, not assume.

    `invented` names addresses this run built from the task rather than reached by clicking, which are the ones
    that can land on a page that looks like the answer without being the search the task asked for. `visited` is
    every address the run has been on, first the one it began on."""
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
    instruction = (
        "\n\n## Verdict\nDecide from the screenshot, set controls, page text and notes whether the task is finished. "
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
        "evidence. Addresses this run built from the task rather than reached by clicking are the ones to "
        "weigh hardest, since a guessed address can land on a page of the right shape and the wrong search."
    )
    messages = [
        Message(
            role="system",
            content=f"# Verifier\n{UNTRUSTED}",
        ),
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
        prompt = "".join(message.content for message in messages) + "".join(parts) + instruction
        return config.tokens.remaining_chars(prompt + json.dumps(LLMVerdict.model_json_schema()))

    # As in the done check's page state, the notes' requirement evidence is placed first and the page text is cut
    # to what remains: a "View more" results page once left the notes no room at all and ended the run.
    rendered = notes.render(room("\n\n## Notes\n"), preserve_requirements=True)
    text = cut_text(observation.viewport_text, room("\n\n## Notes\n", rendered))
    messages[-1] = messages[-1].model_copy(
        update={"content": f"{messages[-1].content}{text}\n\n## Notes\n{rendered}{instruction}"}
    )
    return await llm.generate(
        LLMPurpose.VERIFY,
        messages,
        LLMVerdict,
        ledger=ledger,
    )


class _OutputSource(Frozen):
    url_ref: str
    quote: str
    source_id: str
    frame_id: str | None
    page_title: str | None


class _OutputClaim(Frozen):
    text: str
    cited_sources: tuple[_OutputSource, ...]
    derived: bool


class _OutputContext(Frozen):
    answer: str
    claims: tuple[_OutputClaim, ...]
    urls: dict[str, str]


class _OutputAssessment(Frozen):
    judgments: dict[str, Literal["yes", "no", "uncertain"]]
    reason: str


class OutputAuditVerdict(Frozen):
    judgment: Literal["yes", "no", "uncertain"]
    reason: str


class _QuotedIdentity(Frozen):
    source_ref: str
    quote: str


class _IdentityScope(Frozen):
    scope: Literal["entities", "subjectless", "unresolved"]
    identities: tuple[_QuotedIdentity, ...] = ()


class _OutputIdentities(Frozen):
    bindings: dict[str, _IdentityScope]


type OutputAuditCache = dict[str, OutputAuditVerdict | _OutputIdentities]


def _output_context(composed: ComposedAnswer, notes: Notes) -> _OutputContext | None:
    known = notes.evidence
    based = {fact_id(fact) for fact in notes.facts if fact.basis}
    urls: dict[str, str] = {}
    claims = []
    for claim in composed.claims:
        expanded = notes.expand_evidence_ids(claim.evidence_ids)
        if any(key not in known and not notes.derived(key) for key in expanded):
            return None
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
                    source_id=evidence.source_id,
                    frame_id=evidence.frame_id,
                    page_title=page.title if page and evidence.frame_id is None else None,
                )
            )
        # Looking up a remote source map let an assertion stand in for its quote; keep each claim beside its sources.
        claims.append(
            _OutputClaim(
                text=claim.text,
                cited_sources=tuple(sources),
                derived=any(notes.derived(key) or key in based for key in claim.evidence_ids),
            )
        )
    return _OutputContext(
        answer=composed.answer, claims=tuple(claims), urls={alias: url for url, alias in urls.items()}
    )


async def check_answer_outputs(
    jev: JevClient,
    llm: LLMClient | None,
    composed: ComposedAnswer,
    notes: Notes,
    checks: Sequence[str],
    *,
    tokens: TokenBudget = _DEFAULT_CONFIG.tokens,
    ledger: Ledger | None = None,
    missing_outputs: list[str] | None = None,
    corrections: list[AnswerCorrection] | None = None,
    allow_scalar_jev: bool = False,
    task: str = "",
    audit_cache: OutputAuditCache | None = None,
) -> bool:
    def reject(failed: Sequence[str]) -> bool:
        if missing_outputs is not None:
            missing_outputs.extend(failed)
        return False

    if not checks:
        return True
    if any(not check.strip() for check in checks):
        return False
    context = _output_context(composed, notes)
    if context is None:
        return False
    criteria = {f"output_{index}": check for index, check in enumerate(checks)}
    verbatim = False
    if allow_scalar_jev and len(composed.claims) == 1:
        claim = composed.claims[0]
        verbatim = any(
            fact.reader is FactReader.JEV_CHOICE
            and fact.evidence is not None
            and claim.evidence_ids == (fact_id(fact),)
            and claim.text == fact.text
            for fact in notes.facts
        )
    scores = {}
    if verbatim:
        questions = {
            key: NoulQuestion(
                instructions=(
                    f"{UNTRUSTED} Does the actual answer satisfy this criterion completely, with every requested "
                    "output explicitly stated and directly supported by its own cited sources? Judge every requested "
                    "entity separately. Related items, implied values and ambiguous source associations do not "
                    "satisfy it. A derived count or comparison can rest on the cited source records that support its "
                    "calculation; the sources need not state the derived result. Observed page titles can reveal "
                    "shortened exact strings but cannot replace a missing "
                    f"citation. Criterion: {criterion}"
                ),
                true="Yes, every part is answered and directly evidenced.",
                false="No, a requested output is missing, unsupported, ambiguous or uncertain.",
            )
            for key, criterion in criteria.items()
        }
        result = await evaluate_batches(
            jev, context.model_dump(mode="json"), questions, tokens=tokens, ledger=ledger, allow_failed_batches=False
        )
        if result is not None and any(not isinstance(result.answers.get(key), NoulAnswer) for key in questions):
            return False
        # An oversized scalar still reaches the source audit when Jev cannot score it.
        scores = {key: _probability(result.answers, key) if result is not None else 0.0 for key in questions}
        trace("answer_outputs", scores=scores)
    # Confident votes excused prose's unsupported components; only code-copied scalar claims bypass the source audit.
    uncertain = (
        criteria if not verbatim else {key: criteria[key] for key, probability in scores.items() if probability < 0.8}
    )
    if not uncertain:
        return True
    if llm is None:
        return False
    identities: dict[str, list[dict[str, str]]] = {}
    if context.claims:
        offered = {}
        refs = {}
        for claim in context.claims:
            for source in claim.cited_sources:
                serialized = source.model_dump_json()
                if serialized not in refs:
                    ref = f"q{len(refs)}"
                    refs[serialized] = ref
                    offered[ref] = source
        messages = [
            Message(
                role="system",
                content=(
                    f"{UNTRUSTED} Resolve each requested output's subjects to literal identifying quotes. "
                    "The answer declares which named entities correspond to its numbered results. It cannot "
                    "supply identity evidence. For each criterion return scope entities and every requested "
                    "subject's offered source_ref and exact identifying quote from its body or captured page title. "
                    "Copy only the entity name or identifying description, not the reported field value. "
                    "Do not infer identity from a URL, shared page, item count or a neighboring table column. "
                    "A numbered subject must bind to the entity the answer actually labels with that number. "
                    "Comparisons can require several subjects. Use scope subjectless only for a criterion "
                    "with no individual entity identity to establish, such as answer format or an aggregate "
                    "zero-record result. Missing identity evidence is scope unresolved, never subjectless. "
                    "Do not change requested entities or omit subjects."
                ),
            ),
            Message(
                role="user",
                content=json.dumps(
                    {
                        "task": task,
                        "criteria": uncertain,
                        "answer": context.answer,
                        "urls": context.urls,
                        "sources": {ref: source.model_dump() for ref, source in offered.items()},
                    }
                ),
            ),
        ]
        fingerprint = hashlib.sha256(
            json.dumps(
                {"client": id(llm), "messages": [message.model_dump() for message in messages], "kind": "identities"},
                sort_keys=True,
            ).encode()
        ).hexdigest()
        cached = audit_cache.get(fingerprint) if audit_cache is not None else None
        if isinstance(cached, _OutputIdentities):
            resolved = cached
        else:
            generated = await llm.generate(
                LLMPurpose.VERIFY, messages, _OutputIdentities, max_output_tokens=8000, ledger=ledger
            )
            if ledger is not None:
                ledger.record(generated.cost)
            resolved = generated.data
            if audit_cache is not None:
                audit_cache[fingerprint] = resolved
        for key, criterion in uncertain.items():
            scope = resolved.bindings.get(key)
            if scope is None or scope.scope == "unresolved":
                return reject((criterion,))
            if scope.scope == "subjectless":
                if scope.identities:
                    return reject((criterion,))
                continue
            bindings = scope.identities
            if not bindings:
                return reject((criterion,))
            identities[key] = []
            for binding in bindings:
                source = offered.get(binding.source_ref)
                if (
                    source is None
                    or not binding.quote.strip()
                    or not any(binding.quote in text for text in (source.quote, source.page_title or ""))
                ):
                    return reject((criterion,))
                identities[key].append({"reference": criterion, "url_ref": source.url_ref, "quote": binding.quote})
    choices = {f"claim_{index}": claim.text for index, claim in enumerate(context.claims)}
    choices["all"] = "The requested output spans multiple claims; no single claim states all of it."
    choices["none"] = "The requested output is missing from the actual answer."
    selecting = {
        key: ChoiceQuestion(
            instructions=(
                f"{UNTRUSTED} Select the claim that explicitly states the requested output for the correct entity. "
                "Preserve the original task's each-item and all-item scope. One item's value cannot discharge "
                "a field requested for several items. A truthful statement about another item does not report "
                "that field for the requested item. "
                "Select all only when the criterion needs multiple claims, and none when absent or only implied. "
                f"Task: {task}\nCriterion: {criterion}"
            ),
            criteria=choices,
        )
        for key, criterion in uncertain.items()
    }
    # Large list answers exceed Jev's option ceiling; the source and assertion audits still check every claim.
    selected = (
        await evaluate_batches(
            jev, {"answer": composed.answer}, selecting, tokens=tokens, ledger=ledger, allow_failed_batches=False
        )
        if len(choices) <= MAX_CHOICE_OPTIONS
        else None
    )
    if selected is not None and any(not isinstance(selected.answers.get(key), ChoiceAnswer) for key in selecting):
        return False
    fields = {}
    source_fields = {}
    covered: set[int] = set()
    selected_claims: dict[str, tuple[Claim, ...]] = {}
    for key, criterion in uncertain.items():
        chosen = selected.answers.get(key) if selected is not None else None
        choice = chosen.choice if isinstance(chosen, ChoiceAnswer) else "all"
        if choice == "none":
            return reject((criterion,))
        if choice == "all":
            selected_claims[key] = composed.claims
            claims = context.claims
            if len(claims) == 1:
                covered.add(0)
        elif choice in choices:
            index = int(choice.removeprefix("claim_"))
            claims = (context.claims[index],)
            selected_claims[key] = (composed.claims[index],)
            covered.add(index)
        else:
            return False
        fields[key] = {
            "criterion": criterion,
            "requested_entities": identities.get(key, []),
            "reported_claims": [
                claim.model_dump(exclude={"cited_sources": {"__all__": {"page_title"}}}) for claim in claims
            ],
        }
        source_fields[key] = {
            "criterion": criterion,
            "requested_entities": identities.get(key, []),
            "sources": [
                {
                    "cited_sources": [source.model_dump(exclude={"page_title"}) for source in claim.cited_sources],
                    "derived": claim.derived,
                }
                for claim in claims
            ],
        }

    reasons: dict[str, str] = {}

    async def audit(messages: list[Message], fields: Mapping[str, object]) -> dict[str, str]:
        semaphore = asyncio.Semaphore(4)

        async def one(key: str, field: object) -> tuple[str, str]:
            records = field.get("reported_claims", field.get("sources", ())) if isinstance(field, dict) else ()
            refs = {source["url_ref"] for record in records for source in record["cited_sources"]}
            if isinstance(field, dict):
                refs.update(identity["url_ref"] for identity in field.get("requested_entities", ()))
            urls = {alias: url for alias, url in context.urls.items() if alias in refs}
            request = [
                *messages,
                Message(role="user", content=json.dumps({"task": task, "criteria": {key: field}, "urls": urls})),
            ]
            fingerprint = hashlib.sha256(
                json.dumps(
                    {
                        "client": id(llm),
                        "purpose": LLMPurpose.VERIFY,
                        "messages": [message.model_dump(mode="json") for message in request],
                        "schema": _OutputAssessment.model_json_schema(),
                        "max_output_tokens": 512,
                    },
                    sort_keys=True,
                ).encode()
            ).hexdigest()
            cached = audit_cache.get(fingerprint) if audit_cache is not None else None
            if isinstance(cached, OutputAuditVerdict):
                reasons[key] = cached.reason
                return key, cached.judgment
            async with semaphore:
                generated = await llm.generate(
                    LLMPurpose.VERIFY,
                    request,
                    _OutputAssessment,
                    max_output_tokens=512,
                    ledger=ledger,
                )
                if ledger is not None:
                    ledger.record(generated.cost)
                verdict = OutputAuditVerdict(
                    judgment=generated.data.judgments.get(key, "uncertain"), reason=generated.data.reason
                )
                # Repairing one assertion used to repeat audits whose exact sources and question had not changed.
                if audit_cache is not None:
                    audit_cache[fingerprint] = verdict
                reasons[key] = verdict.reason
                return key, verdict.judgment

        # One field's quoted value cannot answer another field; every receipt settles before an error returns.
        assessed = await asyncio.gather(*(one(key, field) for key, field in fields.items()), return_exceptions=True)
        for result in assessed:
            if isinstance(result, BaseException):
                raise result
        return dict(result for result in assessed if not isinstance(result, BaseException))

    # Seeing the reported value let the audit fill gaps in ambiguous quotes; check sources with assertions withheld.
    source_messages = [
        Message(
            role="system",
            content=(
                f"{UNTRUSTED} Check source availability separately for each criterion. Judge whether its "
                "selected quoted sources provide every requested output value for the correct entity. "
                "Answer assertions are withheld and cannot fill missing source values. Return no when a "
                "requested value or its association with the entity is absent or ambiguous. Eligibility "
                "qualifiers identify the entity; unrelated fields need not appear in each field's quote. "
                "Entity ordinals label the compared answer entities, not search rankings, unless a ranking "
                "is explicitly requested by the task. The task defines scope, not evidence; criteria cannot add "
                "requirements the task did not ask for. Claim selection is provisional: independently prove "
                "the field belongs to each requested entity identified by its literal quote. A quote for one "
                "subject cannot answer another subject's field. A shared page, title, collection or total count "
                "does not bind a comparison-table column or related model to the requested subject. "
                "An explicitly labeled value remains available alongside an eligibility-dependent alternative; "
                "preserve those conditions rather than assuming one value supersedes the other. "
                "An explicit exhaustive description can establish that no other members exist; absence from a "
                "partial description cannot. A total alone does not provide a component breakdown. A value scoped "
                "to one component, mode, tier or single-item configuration does not establish an aggregate value "
                "or another configuration. Require the source scope to match the requested scope and preserve "
                "explicit distinctions. Do not add independent component maxima unless quoted sources state "
                "that they apply simultaneously and combine additively. An explicit aggregate rating need not "
                "equal their sum. "
                "A requested recommendation does not require the page to recommend anything: quoted facts "
                "can provide grounds for the answer's preference. A quoted property of one option can "
                "support a subjective preference. Do not require every compared option's values for a "
                "recommendation criterion merely because another task output compares options. The assertion "
                "audit separately requires all operands for any factual comparative advantage the answer claims. "
                "Derived outputs can calculate from quoted records only when every operand and its association "
                "is explicit; the source need not state the conclusion literally. Observed page titles provide "
                "identity context, not missing field "
                "evidence. Never reconstruct missing table column labels from prior knowledge. Return "
                "yes/no/uncertain per field and explain missing source values."
            ),
        ),
    ]
    available = await audit(source_messages, source_fields)
    absent = [criterion for key, criterion in uncertain.items() if available.get(key) != "yes"]
    if absent:
        if corrections is not None:
            # A selected claim can omit a quote already in the notes; rereading cannot repair its citations.
            corrections.extend(
                AnswerCorrection(stage="source", criterion=criterion, claims=selected_claims[key], reason=reasons[key])
                for key, criterion in uncertain.items()
                if available.get(key) != "yes"
            )
        return reject(absent)
    assertion_messages = [
        Message(
            role="system",
            content=(
                f"{UNTRUSTED} Judge each requested output independently. First determine its value from each "
                "selected claim's cited quotes alone, without using the reported claim or prior knowledge to "
                "fill missing subject associations. Literal requested-entity quotes identify the subjects, "
                "but their URL alone does not bind a field in a related model's table column. Never transfer "
                "a field value from one subject to another. Determine the evidenced value without filling "
                "missing information. Then check that the claim explicitly states that value for the "
                "correct requested entity in the actual answer. Missing fields, unsupported component "
                "breakdowns, incomplete exact strings, ambiguous sources and claims for another entity fail. "
                "Eligibility qualifiers identify the entity and are checked across the answer, not demanded "
                "in every individual quote. Entity ordinals label compared answer entities, not search rankings "
                "unless explicitly requested by the task. The task defines scope, not evidence; do not add "
                "requirements beyond it. Preserve explicitly stated conditions on alternative values; "
                "do not assume an eligibility-dependent alternative supersedes an unrestricted value. "
                "An explicit exhaustive description can establish absence of other "
                "members, but a total alone does not evidence a component breakdown. A value scoped to one "
                "component, mode, tier or single-item configuration does not establish an aggregate value or "
                "another configuration. Preserve the quoted scope and explicit distinctions in every assertion. "
                "Do not add independent component maxima unless quoted sources state that they apply "
                "simultaneously and combine additively. An explicit aggregate rating need not equal their sum. "
                "Derived outputs can "
                "calculate from quoted records only when every operand and its association is explicit. "
                "Observed page titles provide identity "
                "context, not missing field evidence. Return yes only if every part of the requested output "
                "is stated and evidenced by its own cited sources. Every factual assertion in every selected "
                "claim must also be supported, including extra details the user did not request. One supported "
                "value cannot excuse another unsupported value in the same claim. A subjective recommendation "
                "may rest on a quoted property. A factual comparative advantage, including highest, lowest "
                "or best on a measured property, requires all compared operands in that claim's own citations. "
                "Quotes cited only by another claim cannot supply missing operands or support. "
                "Preserve uncertainty and explain failures."
            ),
        ),
    ]
    for index, claim in enumerate(context.claims):
        if index not in covered:
            key = f"claim_only_{index}"
            selected_claims[key] = (composed.claims[index],)
            fields[key] = {
                "criterion": "Every factual assertion is supported by its own cited sources.",
                "reported_claims": [claim.model_dump(exclude={"cited_sources": {"__all__": {"page_title"}}})],
            }
    generated = await audit(assertion_messages, fields)
    failed = [
        uncertain[key]
        if key in uncertain
        else "Unsupported answer detail: " + context.claims[int(key.removeprefix("claim_only_"))].text
        for key in fields
        if generated.get(key) != "yes"
    ]
    if failed and corrections is not None:
        corrections.extend(
            AnswerCorrection(criterion=str(fields[key]["criterion"]), claims=selected_claims[key], reason=reasons[key])
            for key in fields
            if generated.get(key) != "yes"
        )
    return reject(failed) if failed else True


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
    llm: LLMClient | None = None,
    missing_outputs: list[str] | None = None,
    corrections: list[AnswerCorrection] | None = None,
    allow_scalar_jev: bool = False,
    task: str = "",
    audit_cache: OutputAuditCache | None = None,
) -> ComposedAnswer | None:
    """The answer without any claim a check doubts, or None when a requirement is omitted from what is left or the
    pages where the run committed an action contradict it.

    The composer adds claims its quotes do not cover (a login page's nav links, a Log out button), and one of
    those failed three runs in four of a correct sign-in answer. Removing a doubted claim asserts nothing new,
    so it is honest as long as the rest still answers: the omission check is asked again of what remains.
    """
    questions = claim_check_questions(composed, notes, tokens=tokens)
    if answer_checks:
        questions = {key: question for key, question in questions.items() if key != _OMITTED}
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

    checked, outputs_supported = await asyncio.gather(
        claims(),
        check_answer_outputs(
            jev,
            llm,
            composed,
            notes,
            answer_checks,
            tokens=tokens,
            ledger=ledger,
            missing_outputs=missing_outputs,
            corrections=corrections,
            allow_scalar_jev=allow_scalar_jev,
            task=task,
            audit_cache=audit_cache,
        ),
        return_exceptions=True,
    )
    if isinstance(checked, BaseException):
        raise checked
    if isinstance(outputs_supported, BaseException):
        raise outputs_supported
    if not outputs_supported:
        return None
    answers = checked
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
    if answer_checks:
        return (
            pruned
            if await check_answer_outputs(
                jev,
                llm,
                pruned,
                notes,
                answer_checks,
                tokens=tokens,
                ledger=ledger,
                missing_outputs=missing_outputs,
                corrections=corrections,
                allow_scalar_jev=allow_scalar_jev,
                task=task,
                audit_cache=audit_cache,
            )
            else None
        )
    omission = {key: q for key, q in claim_check_questions(pruned, notes, tokens=tokens).items() if key == _OMITTED}
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
            or _probability(answers, key) > (limit if key == _OMITTED else min(limit, 1 - limit))
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


def _probability(answers: Mapping[str, object], key: str) -> float:
    answer = answers.get(key)
    return answer.probability if isinstance(answer, NoulAnswer) else 0.0
