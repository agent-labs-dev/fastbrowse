"""Deciding whether a run is actually finished, and whether its answer and extracted data hold up.

Jev checks completion, individual action requirements and draft quality. An LLM looks at a screenshot
when those answers leave completion uncertain.
"""

import asyncio
import json
from collections.abc import Collection, Iterable, Mapping, Sequence
from enum import StrEnum
from typing import Literal, assert_never

from pydantic import BaseModel, Field, JsonValue, ValidationError

from fastbrowse.batches import evaluate_batches
from fastbrowse.config import Config, Thresholds, TokenBudget
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, JevClient, NoulAnswer, NoulQuestion, Question
from fastbrowse.llm import Generation, LLMClient, Message
from fastbrowse.memory import Notes, NotesTooLarge, fact_id
from fastbrowse.models import UNTRUSTED, CostLine, Evidence, FactReader, Frozen, LLMPurpose
from fastbrowse.page import Capture, Control, Observation, cut_text
from fastbrowse.planner import Plan, RequirementKind
from fastbrowse.policy import HistoryEntry
from fastbrowse.retrieval import (
    TRANSACTION_CONTRADICTED,
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
    heading_path: tuple[str, ...]
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


def _output_context(composed: ComposedAnswer, notes: Notes) -> _OutputContext | None:
    known = notes.evidence
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
            page = notes.captured_page(evidence.capture_sha256, evidence.url)
            sources.append(
                _OutputSource(
                    url_ref=urls.setdefault(evidence.url, f"u{len(urls)}"),
                    quote=evidence.quote,
                    source_id=evidence.source_id,
                    frame_id=evidence.frame_id,
                    heading_path=evidence.heading_path,
                    page_title=page.title if page and evidence.frame_id is None else None,
                )
            )
        # Looking up a remote source map let an assertion stand in for its quote; keep each claim beside its sources.
        claims.append(
            _OutputClaim(
                text=claim.text,
                cited_sources=tuple(sources),
                derived=any(notes.derived(key) for key in claim.evidence_ids),
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
    allow_scalar_jev: bool = False,
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
    if result is None or any(not isinstance(result.answers.get(key), NoulAnswer) for key in questions):
        return False
    scores = {key: _probability(result.answers, key) for key in questions}
    trace("answer_outputs", scores=scores)
    # A single aggregate vote accepted a missing breakdown; confident failures cannot be excused by other fields.
    if any(probability <= 0.2 for probability in scores.values()):
        return reject([criteria[key] for key, probability in scores.items() if probability < 0.8])
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
    # Confident votes excused prose's unsupported components; only code-copied scalar claims bypass the source audit.
    uncertain = (
        criteria if not verbatim else {key: criteria[key] for key, probability in scores.items() if probability < 0.8}
    )
    if not uncertain:
        return True
    if llm is None:
        return False
    choices = {f"claim_{index}": claim.text for index, claim in enumerate(context.claims)}
    choices["all"] = "The requested output spans multiple claims; no single claim states all of it."
    choices["none"] = "The requested output is missing from the actual answer."
    selecting = {
        key: ChoiceQuestion(
            instructions=(
                f"{UNTRUSTED} Select the claim that explicitly states the requested output for the correct entity. "
                "Select all only when the criterion needs multiple claims, and none when absent or only implied. "
                f"Criterion: {criterion}"
            ),
            criteria=choices,
        )
        for key, criterion in uncertain.items()
    }
    selected = await evaluate_batches(
        jev, {"answer": composed.answer}, selecting, tokens=tokens, ledger=ledger, allow_failed_batches=False
    )
    if selected is None or any(not isinstance(selected.answers.get(key), ChoiceAnswer) for key in selecting):
        return False
    fields = {}
    for key, criterion in uncertain.items():
        chosen = selected.answers[key]
        if not isinstance(chosen, ChoiceAnswer):
            return False
        if chosen.choice == "none":
            return reject((criterion,))
        if chosen.choice == "all":
            claims = context.claims
        elif chosen.choice in choices:
            claims = (context.claims[int(chosen.choice.removeprefix("claim_"))],)
        else:
            return False
        fields[key] = {"criterion": criterion, "reported_claims": [claim.model_dump() for claim in claims]}
    generated = await llm.generate(
        LLMPurpose.VERIFY,
        [
            Message(
                role="system",
                content=(
                    f"{UNTRUSTED} Judge each requested output independently. First determine its value from each "
                    "selected claim's cited quotes alone, without using the reported claim or prior knowledge to "
                    "fill missing information. Then check that the claim explicitly states that value for the "
                    "correct requested entity in the actual answer. Missing fields, unsupported component "
                    "breakdowns, incomplete exact strings, ambiguous sources and claims for another entity fail. "
                    "Eligibility qualifiers identify the entity and are checked across the answer, not demanded "
                    "in every individual quote. A total does not evidence a component breakdown. Only marked "
                    "derived claims may calculate from source records. Observed page titles provide identity "
                    "context, not missing field evidence. Return yes only if every part of the requested output "
                    "is stated and evidenced by its own cited sources. Preserve uncertainty and explain failures."
                ),
            ),
            Message(
                role="user",
                content=json.dumps({"actual_answer": composed.answer, "criteria": fields, "urls": context.urls}),
            ),
        ],
        _OutputAssessment,
        ledger=ledger,
    )
    if ledger is not None:
        ledger.record(generated.cost)
    failed = [criterion for key, criterion in uncertain.items() if generated.data.judgments.get(key) != "yes"]
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
    allow_scalar_jev: bool = False,
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
            allow_scalar_jev=allow_scalar_jev,
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
                allow_scalar_jev=allow_scalar_jev,
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
