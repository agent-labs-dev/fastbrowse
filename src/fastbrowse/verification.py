"""Deciding whether a run is actually finished, and whether its answer and extracted data hold up.

Jev checks completion, individual action requirements and draft quality. An LLM looks at a screenshot
when those answers leave completion uncertain.
"""

import json
from collections.abc import Collection, Iterable, Mapping, Sequence
from enum import StrEnum
from typing import assert_never

from pydantic import BaseModel, Field, JsonValue, ValidationError

from fastbrowse.config import Config, Thresholds, TokenBudget
from fastbrowse.jev import JevClient, NoulAnswer, NoulQuestion, Question
from fastbrowse.llm import Generation, LLMClient, Message
from fastbrowse.memory import Notes, NotesTooLarge
from fastbrowse.models import UNTRUSTED, CostComponent, CostLine, Evidence, Frozen, LLMPurpose, StepResult
from fastbrowse.page import Capture, Control, Observation, cut_text
from fastbrowse.planner import Plan, RequirementKind
from fastbrowse.retrieval import (
    TRANSACTION_CONTRADICTED,
    ComposedAnswer,
    UnsupportedField,
    assemble_answer,
    claim_check_questions,
    copy_field,
    field_candidates,
    field_question,
    propose_text_fields,
    propose_text_fields_from_notes,
)
from fastbrowse.telemetry import Ledger, trace

_DEFAULT_CONFIG = Config()


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
        "page": page,
        # Inputs and ARIA selection states are absent from innerText; without them a preview can
        # pass completion even though the requested filters were never applied.
        "controls": _controls(observation.controls),
        "notes": "",
    }

    def room() -> int:
        return tokens.remaining_chars(json.dumps(state), questions)

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
) -> DoneCheck:
    """Judge completion, and whether `draft` answers the task as written, in the one Jev call."""
    questions: dict[str, Question] = {
        "complete": NoulQuestion(
            instructions=(
                f"{UNTRUSTED}\nDoes the page or the notes visibly confirm completion of the task in state? "
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
                    instructions=f"{UNTRUSTED}\nIs this requirement not visibly satisfied?\n\n{requirement.text}",
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
                "misses part of what was asked, repeats or contradicts itself, includes facts the task did not ask "
                "for, or leaves a comparison, count or calculation undone."
            ),
            true="Yes, it needs rewriting before it answers the task.",
            false="No, it answers the task as written.",
        )
    context: dict[str, JsonValue] = {"task": task}
    if draft is not None:
        context["draft"] = draft.answer
    evaluation = await jev.evaluate(
        page_state(
            observation, notes, tokens, questions=[q.model_dump_json() for q in questions.values()], context=context
        ),
        questions,
    )
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
        urls = sorted({fact.evidence.url for _, fact in notes.supporting(requirement.id) if fact.evidence})
        if urls:
            lines.append(f"- {requirement.id}: {', '.join(urls)}")
    parts = []
    if lines:
        parts.append("## Where each requirement's facts were read\n" + "\n".join(lines))
    if invented:
        parts.append("## Addresses this run built from the task\n" + "\n".join(f"- {u}" for u in invented))
    return ("\n\n".join(parts) + "\n\n") if parts else ""


async def llm_verify(
    llm: LLMClient,
    task: str,
    plan: Plan,
    observation: Observation,
    screenshots: tuple[bytes, ...],
    notes: Notes,
    steps: Sequence[StepResult],
    *,
    doubted: Sequence[str] = (),
    invented: Sequence[str] = (),
    config: Config = _DEFAULT_CONFIG,
    ledger: Ledger | None = None,
) -> Generation[LLMVerdict]:
    """`doubted` names the requirements the done check doubted, which the verifier must see shown, not assume.

    `invented` names addresses this run built from the task rather than reached by clicking, which are the ones
    that can land on a page that looks like the answer without being the search the task asked for."""
    # A flights search passed here with Jev doubting its one requirement at 0.14: the rows matched, and nothing
    # asked whether the nonstop filter the task named had ever been applied.
    requirements = "\n".join(
        f"- {r.id}: {r.text}{' (doubted: show it is satisfied, or name it missing)' if r.id in doubted else ''}"
        for r in plan.requirements
    )
    count = config.observation.history_entries + config.observation.earlier_history_entries
    history = "\n".join(
        f"- {s.operation.value} {s.target or ''} -> {s.outcome.value}" for s in steps[max(0, len(steps) - count) :]
    )
    # A filter's checked state is absent from page text, and a screenshot shows it only when it is in view: a flights
    # search the verifier passed had matching rows and no nonstop filter applied.
    # Only what is set: every empty field on a long form would crowd the request without saying anything.
    stateful = [c for c in observation.controls if c.checked or c.selected or c.value]
    instruction = (
        "\n\n## Verdict\nDecide from the screenshot, set controls, page text and notes whether the task is finished. "
        "Be strict and name every requirement id that is not visibly satisfied. An action requirement (clicking a "
        "link or button, filling a field, submitting a form, navigating) is satisfied when the steps taken show it "
        "executed on the control the task meant. The task may paraphrase the control's label; the acted-on control "
        "is its equivalent when the page offered no closer match, which is the match the agent made when it acted. "
        "The steps are the run's own record, and a click that navigated is not visible on the page it left. Be "
        "strict about whether the action happened, not about the task's wording of the label. A requirement to "
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
                f"## Task\n{task}\n\n## Requirements\n{requirements}\n\n## Steps taken\n{history}\n\n"
                f"## Set controls\n{json.dumps(_controls(stateful))}\n\n{_grounding(notes, plan, invented)}"
                f"## Page\n{observation.url}\n"
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


async def check_claims(
    jev: JevClient,
    composed: ComposedAnswer,
    notes: Notes,
    thresholds: Thresholds,
    *,
    tokens: TokenBudget = _DEFAULT_CONFIG.tokens,
    ledger: Ledger | None = None,
    transaction_evidence_ids: Collection[str] = (),
) -> ComposedAnswer | None:
    """The answer without any claim a check doubts, or None when a requirement is omitted from what is left or the
    pages where the run committed an action contradict it.

    The composer adds claims its quotes do not cover (a login page's nav links, a Log out button), and one of
    those failed three runs in four of a correct sign-in answer. Removing a doubted claim asserts nothing new,
    so it is honest as long as the rest still answers: the omission check is asked again of what remains.
    """
    questions = claim_check_questions(composed, notes, tokens=tokens, transaction_evidence_ids=transaction_evidence_ids)
    # An action-only task can finish without factual claims. Its completion was checked already,
    # and Jev rejects an empty question batch; dropped or uncited answer text still cannot pass.
    if not questions:
        return composed if not composed.answer and composed.dropped_claims == 0 else None
    answers = await _ask(jev, composed, questions, ledger)
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
    if not kept:
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
    omission = {key: q for key, q in claim_check_questions(pruned, notes, tokens=tokens).items() if key == _OMITTED}
    if omission and _probability(await _ask(jev, pruned, omission, ledger), _OMITTED) > limit:
        return None
    return pruned


async def _ask(
    jev: JevClient, composed: ComposedAnswer, questions: Mapping[str, NoulQuestion], ledger: Ledger | None
) -> Mapping[str, object]:
    if ledger is not None:
        ledger.reserve(CostComponent.JEV)
    evaluation = await jev.evaluate({"answer": composed.answer}, questions)
    if ledger is not None:
        ledger.record(evaluation.cost)
    return evaluation.answers


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
            await propose_text_fields_from_notes(llm, task, notes, text_fields, tokens=tokens, ledger=ledger)
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
        if not candidates:
            continue
        try:
            question = field_question(field, candidates, name=name, task=task, record_fields=tuple(schema.model_fields))
        except ValueError as error:
            return Extraction(data=None, evidence=tuple(evidence), problem=f"{name}: {error}")
        if ledger is not None:
            ledger.reserve(CostComponent.JEV)
        evaluation = await jev.evaluate(
            {"task": task, "page": {"url": capture.url, "title": capture.title}}, {name: question}
        )
        if ledger is not None:
            ledger.record(evaluation.cost)
        answer = evaluation.answers.get(name)
        copied = copy_field(answer, candidates) if answer is not None and answer.type == "choice" else None
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
