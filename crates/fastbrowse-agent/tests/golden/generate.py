"""Write the model requests Python makes, and the decisions it takes from scripted answers, for Rust to match.

    uv run python crates/fastbrowse-agent/tests/golden/generate.py

Each case runs a ported function against fake Jev and LLM clients that record every request and answer it from a
script. A Jev request is kept as the bytes of its state and its questions, written compactly and in UTF-8 as a
request body is; an LLM request as its purpose, its messages written the same way, the response model's name, its
`model_json_schema()` as `json.dumps` writes it, and the output cap. The Rust tests in `tests/golden.rs` replay the
recorded answers in order, compare every request byte for byte, and compare what each function returned, raised or
left in the ledger structurally.

A `Control.operations` frozenset is written in hash order, which changes per process, so outcomes that hold a
control list its operations sorted, as Rust writes them. No request a case records contains one: the policy writes
`sorted(control.operations)` itself.

The files are written with `ensure_ascii`, so they stay plain ASCII whatever the strings inside hold.
"""

import asyncio
import hashlib
import json
from collections.abc import Callable, Mapping, Sequence
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from pydantic import BaseModel, JsonValue

from fastbrowse.batches import evaluate_batches
from fastbrowse.comparison import NumericComparison, QuotedField, complete_comparison
from fastbrowse.config import Config, ObservationLimits, TokenBudget
from fastbrowse.jev import (
    Answer,
    ChoiceAnswer,
    ChoiceQuestion,
    Evaluation,
    JevError,
    JevInputTooLarge,
    JevRetriesExhausted,
    NoulAnswer,
    NoulQuestion,
    Question,
)
from fastbrowse.llm import Generation, LLMError, Message
from fastbrowse.memory import (
    CollectionFact,
    Comparison,
    Fact,
    Notes,
    NotesTooLarge,
    Tally,
    evidence_id,
    fact_id,
    shows,
)
from fastbrowse.models import (
    CostBasis,
    CostComponent,
    CostLine,
    Evidence,
    FactReader,
    Limits,
    LLMPurpose,
    Operation,
    SourceControl,
    StepOutcome,
)
from fastbrowse.page import Block, BlockKind, Capture, Control, Dialog, Observation, Tab
from fastbrowse.planner import Plan, Requirement, RequirementKind, RunReport, make_plan, partition_plan
from fastbrowse.policy import (
    HistoryEntry,
    ReadAssessment,
    RelevanceCache,
    StepContext,
    build_request,
    decide,
    fits,
)
from fastbrowse.scripted import ScriptedJev, steps
from fastbrowse.shortcut import accept, accept_start, propose_shortcut, propose_start
from fastbrowse.telemetry import BudgetExceeded, Ledger, traced

HERE = Path(__file__).parent
NOW = datetime(2026, 9, 26, 20, 0, tzinfo=UTC)


def compact(value: Any) -> str:
    """A request body's bytes: what httpx sends for `json=`."""
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def sort_operations(value: Any) -> Any:
    if isinstance(value, dict):
        return {
            key: sorted(item) if key == "operations" and isinstance(item, list) else sort_operations(item)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [sort_operations(item) for item in value]
    return value


Responder = Callable[[JsonValue, Mapping[str, Question]], Evaluation]


class RecordingJev:
    def __init__(self, calls: list[dict[str, Any]], respond: Responder) -> None:
        self.calls = calls
        self.respond = respond

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        request = {
            "state": compact(state),
            "questions": compact({key: question.model_dump(mode="json") for key, question in questions.items()}),
        }
        try:
            evaluation = self.respond(state, questions)
        except JevRetriesExhausted as error:
            self.calls.append(
                {
                    "jev": request,
                    "response": {
                        "error": "retries_exhausted",
                        "message": str(error),
                        "seconds": error.seconds,
                        "unaccounted_requests": error.unaccounted_requests,
                        "requests": error.requests,
                    },
                }
            )
            raise
        except JevInputTooLarge as error:
            self.calls.append({"jev": request, "response": {"error": "input_too_large", "message": str(error)}})
            raise
        except JevError as error:
            self.calls.append({"jev": request, "response": {"error": "failed", "message": str(error)}})
            raise
        self.calls.append({"jev": request, "response": {"evaluation": evaluation.model_dump(mode="json")}})
        return evaluation


FREE_JEV = CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0)


def evaluation(answers: Mapping[str, Answer], *, cost: CostLine = FREE_JEV, tokens: int = 10) -> Evaluation:
    return Evaluation(model="test", answers=answers, input_tokens=tokens, cost=cost)


def scripted(
    pick: Mapping[str, str], *, noul: float = 0.8, nouls: Mapping[str, float] | None = None, confidence: float = 0.9
) -> Responder:
    """tests/test_policy.py's ScriptedJev: `pick` for every choice whose key it names, else the first option."""

    def respond(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        answers: dict[str, Answer] = {}
        for key, question in questions.items():
            match question:
                case ChoiceQuestion():
                    options = list(question.criteria)
                    choice = pick.get(key, options[0])
                    answers[key] = ChoiceAnswer(
                        choice=choice, probabilities={o: float(o == choice) for o in options}, confidence=confidence
                    )
                case NoulQuestion():
                    answers[key] = NoulAnswer(probability=(nouls or {}).get(key, noul))
                case _:
                    raise AssertionError(question)
        return evaluation(answers)

    return respond


Reply = Callable[[LLMPurpose, Sequence[Message], type[BaseModel]], JsonValue]


class RecordingLLM:
    def __init__(self, calls: list[dict[str, Any]], reply: Reply, cost: CostLine) -> None:
        self.calls = calls
        self.reply = reply
        self.cost = cost

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = 2000,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        # Reserved as the real client reserves, so a budget stop is seen where it would be.
        if ledger is not None:
            ledger.reserve(CostComponent.LLM)
        request = {
            "purpose": purpose.value,
            "messages": compact([message.model_dump(mode="json") for message in messages]),
            "schema_name": schema.__name__,
            "schema": json.dumps(schema.model_json_schema()),
            "max_output_tokens": max_output_tokens,
        }
        data = self.reply(purpose, messages, schema)
        cost = self.cost.model_copy(update={"purpose": purpose})
        self.calls.append({"llm": request, "response": {"data": data, "cost": cost.model_dump(mode="json")}})
        return Generation(data=schema.model_validate(data), cost=cost)


def llm_cost(dollars: float | None = 0.01) -> CostLine:
    return CostLine(
        component=CostComponent.LLM,
        basis=CostBasis.METERED if dollars is not None else CostBasis.UNKNOWN,
        dollars=dollars,
    )


def ledger_state(ledger: Ledger | None) -> JsonValue:
    if ledger is None:
        return None
    return {
        "lines": [line.model_dump(mode="json") for line in ledger.lines],
        "jev_calls": ledger.jev_calls,
        "llm_calls": ledger.llm_calls,
    }


def error_outcome(error: BaseException) -> dict[str, Any]:
    kinds = {
        BudgetExceeded: "budget",
        JevRetriesExhausted: "jev_retries_exhausted",
        JevInputTooLarge: "jev_input_too_large",
        JevError: "jev",
        LLMError: "llm",
    }
    for kind, name in kinds.items():
        if isinstance(error, kind):
            return {"error": name, "message": str(error)}
    return {"error": type(error).__name__, "message": str(error)}


def run(coroutine: Any) -> tuple[Any, dict[str, Any] | None]:
    try:
        return asyncio.run(coroutine), None
    except (BudgetExceeded, JevError, LLMError, ValueError, RuntimeError) as error:
        return None, error_outcome(error)


def limits(**values: Any) -> Limits:
    return Limits(**values)


# --- batches ------------------------------------------------------------------------------------------------------


def batches() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    def case(
        name: str,
        state: JsonValue,
        questions: Mapping[str, Question],
        tokens: TokenBudget,
        respond: Responder,
        *,
        ledger: Limits | None = None,
        allow_failed_batches: bool = True,
    ) -> None:
        calls: list[dict[str, Any]] = []
        book = Ledger(ledger) if ledger is not None else None
        seen: list[str] = []
        result, error = run(
            evaluate_batches(
                RecordingJev(calls, respond),
                state,
                questions,
                tokens=tokens,
                ledger=book,
                on_answer=lambda key, _: seen.append(key),
                allow_failed_batches=allow_failed_batches,
            )
        )
        cases.append(
            {
                "name": name,
                "state": state,
                "questions": {key: question.model_dump(mode="json") for key, question in questions.items()},
                "tokens": tokens.model_dump(mode="json"),
                "ledger": None if ledger is None else ledger.model_dump(mode="json"),
                "allow_failed_batches": allow_failed_batches,
                "calls": calls,
                "outcome": error
                or {
                    "answered": None
                    if result is None
                    else {
                        "answers": {key: answer.model_dump(mode="json") for key, answer in result.answers.items()},
                        "cost": [line.model_dump(mode="json") for line in result.cost],
                        "input_tokens": result.input_tokens,
                        "requests": result.requests,
                    },
                    "on_answer": seen,
                },
                "ledger_after": ledger_state(book),
            }
        )

    def nouls(probability: float, cost: CostLine = FREE_JEV) -> Responder:
        return lambda state, questions: evaluation(
            {key: NoulAnswer(probability=probability) for key in questions}, cost=cost
        )

    many = {f"q{i}": NoulQuestion(instructions="x" * 300 + f" {i}") for i in range(40)}
    cent = CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.01, input_tokens=12)
    state = {"rules": "Is it relevant? \u00e9\u6771\u4eac", "page": {"url": "https://example.test/", "n": 1.5e-7}}
    case("many questions split small", state, many, TokenBudget(batch_tokens=1000), nouls(0.25, cent))
    case(
        "billed against a ledger",
        state,
        many,
        TokenBudget(batch_tokens=1000),
        nouls(0.75, cent),
        ledger=limits(max_dollars=10.0, max_jev_calls=100),
    )
    case(
        "a pass the call limit cannot cover",
        state,
        many,
        TokenBudget(batch_tokens=1000),
        nouls(0.75, cent),
        ledger=limits(max_jev_calls=2),
    )
    tiny = TokenBudget(state_plus_largest_question=40, state_plus_all_questions=40)
    case(
        "a question too large for the state",
        {"text": "x" * 100},
        {"a": NoulQuestion(instructions="first", true="yes", false="no")},
        tiny,
        nouls(0.5),
    )
    case(
        "one question per batch",
        {},
        {"a": NoulQuestion(instructions="first"), "b": NoulQuestion(instructions="second \u2028")},
        tiny,
        nouls(0.5),
    )

    def failing_second(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        if "b" in questions:
            raise JevError("provider down")
        return evaluation({key: NoulAnswer(probability=0.4) for key in questions})

    pair = {"a": NoulQuestion(instructions="first"), "b": NoulQuestion(instructions="second")}
    case("a failed batch goes unscored", {}, pair, tiny, failing_second, ledger=limits())
    case("a failed batch raises when asked to", {}, pair, tiny, failing_second, allow_failed_batches=False)

    def all_failing(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        raise JevError("provider down")

    case("no batch answered", {}, pair, tiny, all_failing)
    choices = {
        f"c{i}": ChoiceQuestion(instructions="Pick one", criteria={"a": "A", "b": {"what": "B", "not_for": None}})
        for i in range(3)
    }
    case("choices in one batch", {"k": [1, 2.0, None, True]}, choices, TokenBudget(), scripted({"c1": "b"}))
    return cases


# --- planner ------------------------------------------------------------------------------------------------------


def example_plan() -> Plan:
    return Plan(
        requirements=(Requirement(id="r1", text="Find the price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("Find the price",),
    )


def planner() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    def plan_reply(plan: JsonValue) -> Reply:
        def reply(purpose: LLMPurpose, messages: Sequence[Message], schema: type[BaseModel]) -> JsonValue:
            assert schema is Plan
            return plan

        return reply

    def make(
        name: str,
        task: str,
        reply: Reply,
        *,
        start: str | None = None,
        ledger: Limits | None = None,
        spent: bool = False,
        cost: CostLine | None = None,
    ) -> None:
        calls: list[dict[str, Any]] = []
        book = Ledger(ledger) if ledger is not None else None
        if book is not None and spent:
            book.reserve(CostComponent.LLM)
        llm = RecordingLLM(calls, reply, cost or llm_cost())
        result, error = run(make_plan(llm, task, start=start, ledger=book))
        cases.append(
            {
                "name": name,
                "function": "make_plan",
                "task": task,
                "start": start,
                "ledger": None if ledger is None else ledger.model_dump(mode="json"),
                "spent": spent,
                "calls": calls,
                "outcome": error
                or {"plan": result.data.model_dump(mode="json"), "cost": result.cost.model_dump(mode="json")},
                "ledger_after": ledger_state(book),
            }
        )

    example = example_plan().model_dump(mode="json")
    make("a plan with its checks", "Find the price", plan_reply(example), start="https://shop.test/")
    make(
        "a task in other scripts",
        'Trouve le prix \u00e0 Tokyo \u6771\u4eac \U0001f600 "quoted"\nline two',
        plan_reply(example),
    )
    for expected in (False, True):
        make(
            f"an empty plan keeps the outcome, answer {expected}",
            "Check whether the document can be opened without signing in.",
            plan_reply({"requirements": [], "answer_expected": expected}),
        )
    for checks in ([], [" "], [" Report admission price. ", "Report opening hours.", "Report admission price."]):
        plan = {
            "requirements": [{"id": "r", "text": "Report admission price and opening hours.", "kind": "information"}],
            "answer_expected": True,
            "answer_checks": checks,
        }
        make(
            f"checks as written, {len(checks)}",
            "Report admission price and opening hours.",
            plan_reply(plan),
        )
    make(
        "an action plan keeps no checks",
        "Open the museum page.",
        plan_reply(
            {
                "requirements": [{"id": "r", "text": "Open the museum page.", "kind": "action"}],
                "answer_expected": True,
                "answer_checks": ["Report the museum's opening hours."],
            }
        ),
    )
    unchecked = {"requirements": example["requirements"], "answer_expected": True}
    make("one receipt in the ledger", "Find the price", plan_reply(unchecked), ledger=limits(max_llm_calls=1))
    make(
        "a plan over the call limit",
        "Find the price",
        plan_reply(unchecked),
        ledger=limits(max_llm_calls=1),
        spent=True,
    )
    make("an unpriced plan", "Find the price", plan_reply(unchecked), cost=llm_cost(None))
    make(
        "run reports beside a page answer",
        "Report the price and final URL.",
        plan_reply({**example, "run_reports": ["final_url"]}),
    )
    make(
        "run reports alone",
        "Report the final URL.",
        plan_reply({"requirements": [], "answer_expected": True, "run_reports": ["final_url", "screenshot"]}),
    )

    def partition(name: str, task: str, plan: Plan, groups: JsonValue, *, independent_fields: bool = True) -> None:
        calls: list[dict[str, Any]] = []
        book = Ledger(Limits())
        llm = RecordingLLM(calls, lambda purpose, messages, schema: {"groups": groups}, llm_cost())
        result, error = run(partition_plan(llm, task, plan, ledger=book, independent_fields=independent_fields))
        cases.append(
            {
                "name": name,
                "function": "partition_plan",
                "task": task,
                "plan": plan.model_dump(mode="json"),
                "independent_fields": independent_fields,
                "calls": calls,
                "outcome": error
                or {
                    "plans": [p.model_dump(mode="json") for p in result.plans],
                    "targets": {key: target.model_dump(mode="json") for key, target in result.targets.items()},
                },
                "ledger_after": ledger_state(book),
            }
        )

    task = "Log in, then report the price and rating of Atlas and Beacon, and which is cheaper."
    plan = Plan(
        requirements=(
            Requirement(id="login", text="Log in", kind=RequirementKind.ACTION),
            Requirement(id="atlas", text="Find Atlas's price and rating", kind=RequirementKind.INFORMATION),
            Requirement(id="beacon", text="Find Beacon's price and rating", kind=RequirementKind.INFORMATION),
            Requirement(id="cheaper", text="Find which is cheaper", kind=RequirementKind.INFORMATION),
            Requirement(id="open", text="Open the cart", kind=RequirementKind.ACTION),
        ),
        answer_expected=True,
        answer_checks=("price and rating of each", "which is cheaper"),
        run_reports=(RunReport.FINAL_URL,),
    )
    groups = [
        {
            "subjects": [
                {"name": " Atlas ", "requirement_ids": ["atlas"]},
                {"name": "Beacon for Classic", "requirement_ids": ["beacon"]},
            ],
            "fields": ["price", " rating"],
            "check_indices": [0],
        },
        {
            "subjects": [{"name": "Atlas and Beacon", "requirement_ids": ["atlas", "beacon", "cheaper"]}],
            "fields": ["cheaper product"],
            "check_indices": [1, "1"],
        },
    ]
    partition("partition by subject", task, plan, groups)
    partition("partition without categories", task, plan, groups, independent_fields=False)
    partition(
        "an unknown requirement", task, plan, [{**groups[0], "subjects": [{"name": "X", "requirement_ids": ["x"]}]}]
    )
    partition("a check left uncovered", task, plan, groups[:1])
    partition("an empty field", task, plan, [{**groups[0], "fields": [" "]}])
    partition("nothing to decompose", task, plan.model_copy(update={"answer_checks": ()}), groups)

    return cases


# --- memory and comparison ----------------------------------------------------------------------------------------


class RecordedNotes:
    """`Notes`, with every call and what it returned written down for Rust to make and compare."""

    def __init__(self, facts: Sequence[Fact] = ()) -> None:
        self.ops: list[dict[str, Any]] = [{"op": "new", "facts": [dump_fact(f) for f in facts]}]
        self.notes = Notes(facts)

    def _call(self, op: str, call: Callable[[], Any], encode: Callable[[Any], Any], **arguments: Any) -> Any:
        try:
            value = call()
        except (NotesTooLarge, ValueError, KeyError) as error:
            self.ops.append({"op": op, **arguments, "result": {"error": type(error).__name__}})
            return None
        self.ops.append({"op": op, **arguments, "result": encode(value)})
        return value

    def add(self, fact: Fact) -> bool:
        return self._call("add", lambda: self.notes.add(fact), lambda added: added, fact=dump_fact(fact))

    def add_tally(self, tally: Tally) -> Fact:
        return self._call(
            "add_tally",
            lambda: self.notes.add_tally(tally),
            lambda fact: {"id": fact_id(fact), "fact": dump_fact(fact)},
            tally=tally.model_dump(mode="json"),
        )

    def complete_tallies(self, requirement_id: str) -> None:
        self._call(
            "complete_tallies", lambda: self.notes.complete_tallies(requirement_id), none, requirement_id=requirement_id
        )

    def add_continuation(self, requirement_id: str, record_id: str) -> None:
        self._call(
            "add_continuation",
            lambda: self.notes.add_continuation(requirement_id, record_id),
            none,
            requirement_id=requirement_id,
            record_id=record_id,
        )

    def add_collection_page(self, requirement_id: str, url: str, sha: str) -> None:
        self._call(
            "add_collection_page",
            lambda: self.notes.add_collection_page(requirement_id, url, sha),
            none,
            requirement_id=requirement_id,
            url=url,
            capture_sha256=sha,
        )

    def supersede(self, requirement_id: str, url: str, sha: str, text: str) -> None:
        self._call(
            "supersede",
            lambda: self.notes.supersede(requirement_id, url, sha, text),
            none,
            requirement_id=requirement_id,
            url=url,
            sha256=sha,
            text=text,
        )

    def unevidence(self, requirement_ids: Sequence[str]) -> None:
        self._call(
            "unevidence", lambda: self.notes.unevidence(requirement_ids), none, requirement_ids=list(requirement_ids)
        )

    def discredit(self, requirement_ids: Sequence[str], *, urls: Sequence[str] = (), shas: Sequence[str] = ()) -> None:
        def guessed(evidence: Evidence) -> bool:
            return evidence.url in urls or evidence.capture_sha256 in shas

        self._call(
            "discredit",
            lambda: self.notes.discredit(requirement_ids, guessed),
            none,
            requirement_ids=list(requirement_ids),
            urls=list(urls),
            shas=list(shas),
        )

    def retain_for(self, requirement_ids: Sequence[str], fact_ids: Sequence[str]) -> bool:
        return self._call(
            "retain_for",
            lambda: self.notes.retain_for(requirement_ids, fact_ids),
            lambda added: added,
            requirement_ids=list(requirement_ids),
            fact_ids=list(fact_ids),
        )

    def remember_capture(self, capture: Capture) -> None:
        self._call(
            "remember_capture",
            lambda: self.notes.remember_capture(capture),
            none,
            capture=capture.model_dump(mode="json"),
        )

    def complete_comparison(self, requirement_id: str, comparison: NumericComparison) -> bool:
        return self._call(
            "complete_comparison",
            lambda: complete_comparison(self.notes, requirement_id, comparison),
            lambda done: done,
            requirement_id=requirement_id,
            comparison=comparison.model_dump(mode="json"),
        )

    def render_with_ids(self, max_chars: int, **options: Any) -> None:
        self._call(
            "render_with_ids",
            lambda: self.notes.render_with_ids(max_chars, **options),
            lambda rendered: {"text": rendered.text, "evidence_ids": list(rendered.evidence_ids)},
            max_chars=max_chars,
            options=options,
        )

    def render_for_navigation(self, max_chars: int, *, quotes: bool = False) -> None:
        self._call(
            "render_for_navigation",
            lambda: self.notes.render_for_navigation(max_chars, quotes=quotes),
            lambda text: text,
            max_chars=max_chars,
            quotes=quotes,
        )

    def current_evidence(self, capture: Capture | None) -> None:
        self._call(
            "current_evidence",
            lambda: self.notes.current_evidence(capture),
            lambda found: list(found),
            capture=None if capture is None else capture.model_dump(mode="json"),
        )

    def usable_citations(self, keys: Sequence[str]) -> None:
        self._call(
            "usable_citations", lambda: self.notes.usable_citations(keys), lambda found: list(found), keys=list(keys)
        )

    def captured_page(self, evidence: Evidence) -> None:
        self._call(
            "captured_page",
            lambda: self.notes.captured_page(evidence),
            lambda page: None if page is None else page.model_dump(mode="json"),
            evidence=evidence.model_dump(mode="json"),
        )

    def for_requirements(self, requirement_ids: Sequence[str], evidence_ids: Sequence[str] = ()) -> None:
        self._call(
            "for_requirements",
            lambda: self.notes.for_requirements(requirement_ids, evidence_ids=evidence_ids),
            lambda scoped: snapshot(scoped, requirement_ids),
            requirement_ids=list(requirement_ids),
            evidence_ids=list(evidence_ids),
        )

    def snapshot(self, requirement_ids: Sequence[str]) -> None:
        self.ops.append(
            {
                "op": "snapshot",
                "requirement_ids": list(requirement_ids),
                "result": snapshot(self.notes, requirement_ids),
            }
        )


def none(_: Any) -> None:
    return None


def dump_fact(fact: Fact) -> JsonValue:
    return fact.model_dump(mode="json")


def snapshot(notes: Notes, requirement_ids: Sequence[str]) -> JsonValue:
    keys = [fact_id(fact) for fact in notes.facts]
    return {
        "facts": [[fact_id(fact), dump_fact(fact)] for fact in notes.facts],
        "per_fact": {
            key: {
                "current": notes.current(key),
                "derived": notes.derived(key),
                "refused": notes.refused(key),
                "held": notes.held(key),
                "requirements": list(notes.fact_requirements(key)),
            }
            for key in keys
        },
        "requirements": {
            rid: {
                "supporting": [key for key, _ in notes.supporting(rid)],
                "evidenced": notes.evidenced(rid),
                "read_for": [evidence_id(e) for e in notes.read_for(rid)],
                "supporting_evidence": [evidence_id(e) for e in notes.supporting_evidence(rid)],
                "collection_pages": [read.model_dump(mode="json") for read in notes.collection_pages(rid)],
                "comparison_records": list(notes.comparison_records(rid)),
                "has_untallied_records": notes.has_untallied_records(rid),
            }
            for rid in requirement_ids
        },
        "evidence": list(notes.evidence),
        "usable": list(notes.usable_evidence()),
        "current": list(notes.current_evidence()),
        "refused_ids": list(notes.refused_ids()),
        "comparison_records": list(notes.comparison_records()),
        "tallies": [tally.model_dump(mode="json") for tally in notes.tallies],
        "standings": [
            None if (standing := notes.standing(tally)) is None else standing.model_dump(mode="json")
            for tally in notes.tallies
        ],
        "expanded": list(notes.expand_evidence_ids(reversed(keys))),
        "rendered": notes.render_with_ids(10**6, preserve_collections=True).text,
        "navigation": notes.render_for_navigation(10**6, quotes=True),
    }


def span(
    *,
    sha: str = "capture",
    start: int = 0,
    end: int = 4,
    quote: str = "fact",
    url: str = "https://example.test",
    **values: Any,
) -> Evidence:
    return Evidence(
        source_id="s1",
        url=url,
        frame_id=None,
        captured_at=datetime(2026, 1, 1, tzinfo=UTC),
        capture_sha256=sha,
        start=start,
        end=end,
        quote=quote,
        **values,
    )


def quoted_fact(
    quote: str,
    sha: str,
    url: str = "https://example.test",
    requirement_id: str | None = "r1",
    basis: tuple[str, ...] = (),
) -> Fact:
    return Fact(
        requirement_id=requirement_id,
        text=quote,
        evidence=span(sha=sha, quote=quote, url=url, end=len(quote)),
        reader=FactReader.LLM,
        basis=basis,
    )


def page_capture(*parts: str, url: str = "https://example.test", title: str = "Example") -> Capture:
    text = "\n\n".join(parts)
    blocks, start = [], 0
    for i, part in enumerate(parts):
        blocks.append(
            Block(source_id=f"s{i}", kind=BlockKind.PARAGRAPH, frame_id=None, start=start, end=start + len(part))
        )
        start += len(part) + 2
    return Capture(
        url=url,
        title=title,
        captured_at=datetime(2026, 1, 1, tzinfo=UTC),
        sha256=hashlib.sha256(text.encode()).hexdigest(),
        text=text,
        blocks=tuple(blocks),
    )


def memory() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    def case(name: str, notes: RecordedNotes) -> None:
        cases.append({"name": name, "ops": notes.ops})

    # A new read of an address retires quotes it no longer shows: gone, still shown, at another address.
    for shown, url in (
        ("SelectedDate: 28/11/2026", "https://example.test"),
        ("SelectedDate: 28/11/2026 was SelectedDate: 31/10/2026", "https://example.test"),
        ("SelectedDate: 28/11/2026", "https://example.test/2#frag"),
    ):
        notes = RecordedNotes(
            [quoted_fact("SelectedDate: 31/10/2026", "before"), quoted_fact("SelectedDate: 28/11/2026", "after", url)]
        )
        notes.supersede("r1", url, "after", shown)
        notes.snapshot(["r1"])
        case(f"supersede {shown} at {url}", notes)

    notes = RecordedNotes([quoted_fact("Priya Sharma", "before"), quoted_fact("Priya Sharman", "after")])
    notes.supersede("r1", "https://example.test", "after", "Name: Priya Sharman")
    notes.snapshot(["r1"])
    case("a quote inside a longer word", notes)

    old = quoted_fact("Subtotal: GBP 34.50", "before", requirement_id=None)
    fresh = quoted_fact("Subtotal: GBP 43.25", "after")
    notes = RecordedNotes([old, fresh])
    notes.supersede("r1", "https://example.test", "after", fresh.text)
    notes.snapshot(["r1"])
    notes.add(old)
    notes.snapshot(["r1"])
    case("vanished context quotes retire", notes)

    first = quoted_fact("Order A: 12", "before", requirement_id=None)
    last = quoted_fact("Order B: 7", "after", requirement_id=None)
    notes = RecordedNotes([first, last])
    notes.add_tally(Tally(requirement_id="r1", key="Orders", records=(fact_id(first), fact_id(last))))
    notes.complete_tallies("r1")
    notes.add_continuation("r2", fact_id(first))
    notes.supersede("r1", "https://example.test", "after", last.text)
    notes.snapshot(["r1", "r2"])
    case("tallied and compared rows survive a redraw", notes)

    for source_url in ("https://example.test", "https://child.test"):
        context = quoted_fact("Availability: 20", "before", source_url, requirement_id=None)
        repeated = quoted_fact("20", "before", source_url, requirement_id=None)
        answer = Fact(
            requirement_id="r1",
            text="20",
            evidence=None,
            reader=FactReader.JEV_CHOICE,
            basis=(fact_id(context), fact_id(repeated)),
        )
        current = quoted_fact("Availability: 22", "after", source_url)
        notes = RecordedNotes([context, repeated, answer, current])
        notes.snapshot(["r1"])
        notes.supersede("r1", "https://example.test", "after", "20\nAvailability: 22")
        notes.snapshot(["r1"])
        case(f"repeated choice sources from {source_url}", notes)

    notes = RecordedNotes()
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="First fact", evidence=span()))
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r2", text="Same span, another requirement", evidence=span()))
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r2", text="First fact", evidence=span()))
    notes.add(Fact(reader=FactReader.LLM, text="Another capture \u00e9\u6771", evidence=span(sha="different")))
    notes.add(Fact(reader=FactReader.LLM, text="Another span", evidence=span(start=10, end=14)))
    notes.snapshot(["r1", "r2", "r3"])
    for budget in (1000, 400, 200, 20, 1, 0):
        notes.render_with_ids(budget)
        notes.render_with_ids(budget, preserve_requirements=True)
        notes.render_for_navigation(budget)
        notes.render_for_navigation(budget, quotes=True)
    notes.render_for_navigation(-1)
    case("deduplicated spans and budgets", notes)

    context = Fact(reader=FactReader.LLM, text="Context", evidence=span(sha="context"))
    early = Fact(reader=FactReader.LLM, requirement_id="r1", text="First answer", evidence=span(sha="early"))
    late = Fact(reader=FactReader.LLM, text="Checkout total", evidence=span(sha="late"))
    notes = RecordedNotes([context, early, late])
    notes.add(late.model_copy(update={"requirement_id": "r2"}))
    for budget in range(150, 400, 25):
        notes.render_with_ids(budget, preserve_requirements=True)
        notes.render_with_ids(budget, preserve_requirements=True, json_encoded=True)
    case("requirement evidence first", notes)

    records = [Fact(reader=FactReader.LLM, text=name, evidence=span(sha=name)) for name in ("A", "B", "C")]
    notes = RecordedNotes(records)
    a, b, c = (fact_id(f) for f in records)
    notes.add(records[0].model_copy(update={"requirement_id": "r", "text": "A wins", "basis": (b,)}))
    notes.add(records[0].model_copy(update={"basis": (c, b, a)}))
    notes.snapshot(["r"])
    case("a reused span keeps the answer", notes)

    record = Fact(reader=FactReader.LLM, text="Compared record", evidence=span(sha="record"))
    subtotal = Fact(reader=FactReader.LLM, text="Subtotal", evidence=span(sha="subtotal"), basis=(fact_id(record),))
    total = Fact(
        reader=FactReader.LLM,
        requirement_id="r",
        text="Total",
        evidence=span(sha="total"),
        basis=(fact_id(subtotal),),
    )
    unrelated = Fact(reader=FactReader.LLM, text='Unrelated "quoted"\n' * 30, evidence=span(sha="context"))
    notes = RecordedNotes([unrelated, total, record, subtotal])
    for budget in range(300, 1400, 100):
        for json_encoded in (False, True):
            notes.render_with_ids(budget, preserve_requirements=True, json_encoded=json_encoded)
            notes.render_with_ids(budget, json_encoded=json_encoded)
    case("a budget keeps the transitive basis", notes)

    notes = RecordedNotes()
    for page in range(4):
        for number in range(5):
            quote = f"Record {page * 5 + number} by Ada: " + "A long quotation. " * 10
            item = span(
                sha=f"page-{page}",
                start=number * 1000,
                end=number * 1000 + len(quote),
                quote=quote,
                url=f"https://example.test/page/{page}",
            )
            fact = Fact(text=quote, evidence=item, reader=FactReader.LLM)
            notes.add(fact)
            notes.add_tally(Tally(requirement_id="r", key="Ada" if number % 2 else "Grace", records=(fact_id(fact),)))
    notes.snapshot(["r"])
    notes.complete_tallies("r")
    notes.snapshot(["r"])
    for budget in (5000, 3000, 1500, 800):
        notes.render_with_ids(budget, preserve_requirements=True)
        notes.render_with_ids(budget)
    case("tallies render compactly", notes)

    notes = RecordedNotes()
    for sha, starts in (("first", (0, 30)), ("reread", (10, 40)), ("overlap", (20,)), ("first", (0, 30))):
        ids = []
        for start in starts:
            quote = "Ada: approved" if sha == "first" else "Ada:  approved"
            fact = Fact(
                text=quote,
                evidence=span(sha=sha, start=start, end=start + len(quote), quote=quote),
                reader=FactReader.LLM,
            )
            notes.add(fact)
            ids.append(fact_id(fact))
        notes.add_tally(Tally(requirement_id="r", key="Ada", records=tuple(ids)))
    notes.snapshot(["r"])
    case("identical rows and recaptures", notes)

    for second in (
        "https://example.test/rows?page=2",
        "HTTP://example.test/rows?page=1",
        "https://example.test/rows?page=1#row",
        "https://example.test/rows?page=1?",
        "https://example.test/rows?",
        "//example.test/rows",
        "mailto:ada@example.test",
        "  https://example.test/ro\tws",
        "relative/path#x",
    ):
        notes = RecordedNotes()
        quote = "Ada | Widget | $10"
        for index, url in enumerate(("https://example.test/rows?page=1", second, second)):
            fact = Fact(
                text=quote,
                evidence=span(sha=f"capture-{index}", end=len(quote), url=url, quote=quote),
                reader=FactReader.LLM,
            )
            notes.add(fact)
            notes.add_tally(Tally(requirement_id="r", key="Ada", records=(fact_id(fact),)))
        notes.complete_tallies("r")
        notes.snapshot(["r"])
        case(f"row identity at {second}", notes)

    record = Fact(text="Ada", evidence=span(sha="ada"), reader=FactReader.LLM)
    context = Fact(text="All authors", evidence=span(sha="context"), reader=FactReader.LLM)
    notes = RecordedNotes([record, context])
    tally = notes.add_tally(Tally(requirement_id="r1", key="Ada", records=(fact_id(record),)))
    notes.add_tally(Tally(requirement_id="r1", key="Unknown", records=("missing",)))
    notes.complete_tallies("r1")
    notes.add(
        Fact(
            text="Ada leads",
            evidence=None,
            basis=(fact_id(record), fact_id(context)),
            requirement_id="r2",
            reader=FactReader.LLM,
        )
    )
    notes.render_with_ids(1000, preserve_requirements=True)
    notes.render_with_ids(1000, labels={fact_id(context): "n2", fact_id(tally): "n1"}, source_only=True)
    notes.snapshot(["r1", "r2"])
    case("tally aliases", notes)

    address = "https://shop.test/product?tracking=" + "x" * 300
    facts = [
        Fact(text=text, evidence=span(url=address, start=nth, end=nth + 1), reader=FactReader.LLM)
        for nth, text in enumerate(("Title: Charger A", "Price: GBP20", "Power: 65W", "Ports: two USB-C"))
    ]
    notes = RecordedNotes(facts)
    for budget in (700, 400, 200):
        notes.render_for_navigation(budget)
        notes.render_with_ids(budget * 3, json_encoded=True)
    case("a long shared source url", notes)

    quote = "catalogue " * 80
    basis = Fact(text=quote, evidence=span(quote=quote, end=len(quote)), reader=FactReader.LLM)
    progress = Fact(
        text="The first charger has been checked.", evidence=None, basis=(fact_id(basis),), reader=FactReader.LLM
    )
    for order in ((progress, basis), (basis, progress)):
        notes = RecordedNotes(order)
        notes.render_for_navigation(500)
        notes.render_for_navigation(900, quotes=True)
        case(f"navigation keeps progress {len(cases)}", notes)

    notes = RecordedNotes()
    for position in ("1 of 2", "2 of 2"):
        control = SourceControl(role="figure", label="Avatar \u00e9", context=position)
        notes.add(Fact(text="fact", reader=FactReader.LLM, evidence=span(control_context=control)))
    notes.render_with_ids(2000)
    case("equal quotes on different controls", notes)

    first_url = "https://first.test/?ref=" + 'quote"\\' * 30
    second_url = "https://second.test/?ref=" + "context" * 30
    first = quoted_fact("First price: 10", "first", first_url)
    second = quoted_fact("First availability: yes", "second", first_url)
    other = quoted_fact("Other price: 20", "other", second_url)
    last = quoted_fact("Other availability: no", "last", second_url)
    derived = Fact(
        requirement_id="comparison",
        text="The first price is lower",
        evidence=None,
        basis=(fact_id(first), fact_id(other)),
        reader=FactReader.LLM,
    )
    notes = RecordedNotes([first, second, derived, other, last])
    notes.render_with_ids(10000, preserve_requirements=True, json_encoded=True)
    notes.render_with_ids(10000)
    notes.for_requirements(["comparison"])
    case("shared url groups", notes)

    review = quoted_fact("3. Review\n\nName: Ada Lovelace", "review", "https://qa.test/#lab", None)
    heading = quoted_fact("Automation Practice Lab", "submitted", "https://qa.test/#lab", None)
    message = quoted_fact(
        "Wizard submitted successfully!",
        "submitted",
        "https://qa.test/#lab",
        "msg",
        (fact_id(review), fact_id(heading)),
    )
    notes = RecordedNotes([review, heading, message])
    notes.supersede(
        "msg", "https://qa.test/#lab", "submitted", "Automation Practice Lab\n\nWizard submitted successfully!"
    )
    notes.snapshot(["msg"])
    case("a fresh quote on a vanished step", notes)

    guessed, right = "https://example.test/search?q=guess", "https://example.test/search?tfs=set"
    old_rows = [quoted_fact(f"Order {n}", f"guess-{n}", guessed, None) for n in "AB"]
    notes = RecordedNotes(old_rows)
    notes.add_tally(Tally(requirement_id="r1", key="Orders", records=tuple(fact_id(f) for f in old_rows)))
    notes.complete_tallies("r1")
    notes.discredit(["r1"], urls=[guessed])
    notes.complete_tallies("r1")
    notes.snapshot(["r1"])
    notes.add_tally(Tally(requirement_id="r1", key="Orders", records=(fact_id(old_rows[0]),)))
    notes.complete_tallies("r1")
    fresh_rows = [quoted_fact(f"Order {n}", f"right-{n}", right, None) for n in "CDE"]
    for fact in fresh_rows:
        notes.add(fact)
    notes.add_tally(Tally(requirement_id="r1", key="Orders", records=tuple(fact_id(f) for f in fresh_rows)))
    notes.complete_tallies("r1")
    notes.add_collection_page("r1", guessed, "guess-A")
    notes.add_collection_page("r1", right, "right-C")
    notes.snapshot(["r1"])
    case("a refused count and a recount", notes)

    on_guess, on_right = quoted_fact("Fare $320", "guess", guessed), quoted_fact("Fare $305", "right", right)
    notes = RecordedNotes([on_guess, on_right])
    notes.discredit(["r1"], urls=[guessed])
    notes.add(on_guess)
    notes.add(on_right)
    notes.add(Fact(requirement_id="r1", text="$305", evidence=None, basis=(fact_id(on_right),), reader=FactReader.LLM))
    notes.add(Fact(requirement_id="r1", text="$320", evidence=None, basis=(fact_id(on_guess),), reader=FactReader.LLM))
    assert on_guess.evidence is not None
    moved = on_guess.evidence.model_copy(update={"url": guessed + "#x"})
    notes.add(on_guess.model_copy(update={"evidence": moved}))
    notes.snapshot(["r1"])
    notes.usable_citations([fact_id(on_guess), fact_id(on_right), fact_id(on_right), "missing"])
    notes.render_with_ids(10**6)
    case("a refusal holds only guessed quotes", notes)

    ranking = NumericComparison(
        order="lowest", limit=1, label=QuotedField(prefix="", suffix=": "), value=QuotedField(prefix=": ", suffix="")
    )
    for build in ("quote", "conclusion", "tally", "comparison", "recount"):
        for which in ("guessed", "own"):
            held = quoted_fact("BA: $320", "guess", guessed)
            own = quoted_fact("VS: $305", "own", right, None)
            notes = RecordedNotes([held, own])
            notes.discredit(["r1"], shas=["guess"])
            target = held if which == "guessed" else own
            key = fact_id(target)
            match build:
                case "quote":
                    notes.add(target.model_copy(update={"requirement_id": "r1"}))
                case "conclusion":
                    notes.add(
                        Fact(requirement_id="r1", text="Cheapest", evidence=None, basis=(key,), reader=FactReader.LLM)
                    )
                case "tally":
                    notes.add_tally(Tally(requirement_id="r1", key="Fares", records=(key,)))
                    notes.complete_tallies("r1")
                case "comparison":
                    notes.add_continuation("r1", key)
                    notes.complete_comparison("r1", ranking)
                case _:
                    notes.add_tally(Tally(requirement_id="r1", key="Fares", records=(key,)))
                    assert target.evidence is not None
                    reread = target.evidence.model_copy(update={"capture_sha256": "recount"})
                    again = target.model_copy(update={"requirement_id": None, "evidence": reread})
                    notes.add(again)
                    notes.add_tally(Tally(requirement_id="r1", key="Fares", records=(fact_id(again),)))
                    notes.complete_tallies("r1")
            notes.snapshot(["r1"])
            case(f"held readings build nothing: {build} on {which}", notes)

    rows = [quoted_fact(f"Fare {n}: {price}", "own", right, None) for n, price in (("A", 305), ("B", 340))]
    notes = RecordedNotes([*rows, quoted_fact("Fare $320", "guess", guessed)])
    for row in rows:
        notes.add_continuation("cheapest", fact_id(row))
    notes.complete_comparison("cheapest", ranking)
    notes.discredit(["r1"], urls=[guessed])
    notes.snapshot(["r1", "cheapest"])
    notes.for_requirements(["cheapest"], [fact_id(rows[0])])
    case("refusing one requirement leaves another's ranking", notes)

    shared_rows = [quoted_fact(f"Order {n}", "shared", guessed, None) for n in "AB"]
    notes = RecordedNotes([*shared_rows, quoted_fact("Fare $320", "shared", guessed)])
    notes.add_tally(Tally(requirement_id="orders", key="Orders", records=tuple(fact_id(f) for f in shared_rows)))
    notes.add_collection_page("orders", guessed, "shared")
    notes.add_collection_page("orders", guessed, "shared")
    notes.discredit(["r1"], urls=[guessed])
    notes.snapshot(["r1", "orders"])
    case("a list read stays read", notes)

    first_page = page_capture("Price: 12", url="https://example.test")
    later = datetime(2026, 1, 1, 0, 0, 1, tzinfo=UTC)
    second_page = first_page.model_copy(update={"title": "Beacon", "captured_at": later})
    read = span(sha=first_page.sha256, quote="Price: 12", end=9)
    notes = RecordedNotes([Fact(text="Price: 12", evidence=read, reader=FactReader.JEV_CHOICE)])
    notes.remember_capture(first_page)
    notes.remember_capture(first_page.model_copy(update={"title": "Renamed"}))
    notes.remember_capture(second_page)
    notes.captured_page(read)
    notes.captured_page(read.model_copy(update={"captured_at": later}))
    notes.current_evidence(page_capture("Price: 13", url="https://example.test#top"))
    notes.current_evidence(page_capture("Price: 12 today", url="https://example.test"))
    notes.current_evidence(page_capture("Price: 13", url="https://other.test"))
    case("captured pages", notes)

    # Groups that rank: a lone group, ties, and a long neighbour's label.
    notes = RecordedNotes()
    counts = {"Ada": 3, "Grace": 3, "Linus " + "x" * 200: 1, "Barbara": 2, "Edsger": 1, "Alan": 4, "Ken": 1}
    serial = 0
    for key, count in counts.items():
        ids = []
        for _ in range(count):
            fact = quoted_fact(f"Row {serial}", f"sha-{serial}", requirement_id=None)
            serial += 1
            notes.add(fact)
            ids.append(fact_id(fact))
        notes.add_tally(Tally(requirement_id="r", key=key, records=tuple(ids)))
    notes.complete_tallies("r")
    lone = quoted_fact("Lone", "lone", requirement_id=None)
    notes.add(lone)
    notes.add_tally(Tally(requirement_id="other", key="Only", records=(fact_id(lone),)))
    notes.snapshot(["r", "other"])
    notes.retain_for(["r", "x"], [fact_id(lone), "missing"])
    notes.retain_for(["r"], [fact_id(lone)])
    notes.unevidence(["r"])
    notes.snapshot(["r"])
    notes.for_requirements(["other"])
    case("standings", notes)

    # Rankings over a list, and a collection's coverage fact beside them.
    items = [
        quoted_fact(f"Item {n}: {p}", "list", "https://shop.test/list", None)
        for n, p in (("A", "$5"), ("B", "$3.50"), ("C", "$3.5"), ("D", "$10"), ("E", "$\u0661\u0662"))
    ]
    for limit, order in ((1, "lowest"), (2, "lowest"), (2, "highest"), (6, "lowest"), (1, "highest")):
        notes = RecordedNotes(items)
        for item in items:
            notes.add_continuation("cheap", fact_id(item))
        comparison = NumericComparison(
            order=order,
            limit=limit,
            label=QuotedField(prefix="", suffix=": "),
            value=QuotedField(prefix=": ", suffix=""),
        )
        notes.complete_comparison("cheap", comparison)
        coverage = CollectionFact(
            requirement_id=None,
            text="Every item read",
            evidence=None,
            basis=tuple(fact_id(r) for r in items),
            reader=FactReader.LLM,
            scope="All items",
            comparison=Comparison(requirement_id="cheap", records=(fact_id(items[0]),), complete=True),
        )
        notes.add(coverage)
        notes.snapshot(["cheap"])
        notes.render_with_ids(10**6, preserve_collections=True, source_only=True)
        case(f"comparison {order} {limit}", notes)
    return cases


def quoted_fields() -> list[dict[str, Any]]:
    cases = []
    for prefix, suffix, quote in (
        ("", ": ", "Fare A: $305"),
        (": ", "", "Fare A: $305"),
        (": ", "", "A: 1: 2"),
        ("(", ")", "x (a) (b)"),
        ("(", ")", "x (a)"),
        ("", "", "  whole  "),
        ("a", "a", "aaa"),
        ("<", ">", "<line\nbreak>"),
        ("<", ">", "< >"),
        ("\u00e9", "\u6771", "x\u00e9 v \u6771y"),
        ("[", "]", "no delimiters"),
        ("", "|", "left | right"),
    ):
        value = QuotedField(prefix=prefix, suffix=suffix).extract(quote)
        cases.append({"prefix": prefix, "suffix": suffix, "quote": quote, "value": value})
    return cases


def shows_cases() -> list[dict[str, Any]]:
    cases = []
    for text, quote in (
        ("Name: Priya Sharman", "Priya Sharma"),
        ("Name: Priya Sharma.", "Priya Sharma"),
        ("x", ""),
        ("", ""),
        ("ab", "b"),
        ("a b", "b"),
        ("\u00e9b", "b"),
        ("_b", "b"),
        ("b_", "b"),
        ("$5", "$5"),
        ("a$5", "$5"),
        ("b b", "b b"),
        ("bb b", "b"),
    ):
        cases.append({"text": text, "quote": quote, "shows": shows(text, quote)})
    return cases


# --- policy -------------------------------------------------------------------------------------------------------


def button(i: int, *, offscreen: bool = False, **values: Any) -> Control:
    fields: dict[str, Any] = {
        "id": f"b{i}",
        "frame_id": None,
        "role": "button",
        "label": f"Button {i}",
        "operations": frozenset({Operation.CLICK}),
        "offscreen": offscreen,
    }
    fields.update(values)
    return Control(**fields)


def observation(controls: Sequence[Control] = (), **values: Any) -> Observation:
    fields: dict[str, Any] = {
        "url": "https://example.test/",
        "title": "t",
        "page_key": "k",
        "captured_at": NOW,
        "controls": tuple(controls),
        "omitted_controls": 0,
        "viewport_text": "",
        "tabs": (),
    }
    fields.update(values)
    return Observation(**fields)


def step_context(**changes: Any) -> StepContext:
    base: dict[str, Any] = {
        "task": "press button 3",
        "subgoal": None,
        "requirements": (),
        "notes": "",
        "history": (),
        "check_login": False,
        "check_bot": False,
        "has_attachments": False,
        "secrets": (),
    }
    base.update(changes)
    return StepContext(**base)


def rejecting_offscreen(respond: Responder) -> Responder:
    def wrapped(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        if "offscreen" in str(state):
            raise JevInputTooLarge("max_tokens_exceeded")
        return respond(state, questions)

    return wrapped


def relevance(pick: Mapping[str, str], relevant: set[str], *, fail: bool = False) -> Responder:
    """tests/test_policy.py's RelevanceJev: a high score for an element naming one of `relevant`."""
    choose = scripted(pick)

    def respond(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        if "operation" in questions:
            return choose(state, questions)
        if fail:
            raise JevInputTooLarge("max_tokens_exceeded")
        return evaluation(
            {
                key: NoulAnswer(probability=0.9 if any(f'"{label}"' in str(q) for label in relevant) else 0.1)
                for key, q in questions.items()
            }
        )

    return respond


def shedding(pick: Mapping[str, str], answers_up_to: int) -> Responder:
    """Answers 503 until its retries run out for any request offering more than `answers_up_to` elements."""
    choose = scripted(pick)

    def respond(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        assert isinstance(state, dict) and isinstance(state["elements"], list)
        if len(state["elements"]) > answers_up_to:
            raise JevRetriesExhausted("HTTP 503", seconds=1.0, unaccounted_requests=0, requests=6)
        return choose(state, questions)

    return respond


def answering(answers: Mapping[str, Answer]) -> Responder:
    return lambda state, questions: evaluation({key: answers[key] for key in questions if key in answers})


def choice_answer(choice: str, confidence: float = 0.9) -> ChoiceAnswer:
    return ChoiceAnswer(choice=choice, probabilities={choice: 1.0}, confidence=confidence)


def policy() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    def case(
        name: str,
        respond: Responder,
        steps: Sequence[tuple[Observation, StepContext, Config]],
        *,
        ledger: Limits | None = None,
        cache: bool = False,
    ) -> None:
        calls: list[dict[str, Any]] = []
        jev = RecordingJev(calls, respond)
        book = Ledger(ledger) if ledger is not None else None
        relevance_cache = RelevanceCache() if cache else None
        outcomes = []
        for page, context, config in steps:
            with traced() as events:
                result, error = run(decide(jev, page, context, config, ledger=book, relevance_cache=relevance_cache))
            outcomes.append({"result": error or {"decision": result.model_dump(mode="json")}, "trace": list(events)})
        cases.append(
            {
                "name": name,
                "ledger": None if ledger is None else ledger.model_dump(mode="json"),
                "cache": cache,
                "steps": [
                    {
                        "observation": page.model_dump(mode="json"),
                        "context": context.model_dump(mode="json"),
                        "config": config.model_dump(mode="json"),
                    }
                    for page, context, config in steps
                ],
                "calls": calls,
                "outcome": outcomes,
                "ledger_after": ledger_state(book),
            }
        )

    def one(name: str, respond: Responder, page: Observation, context: StepContext, config: Config, **kw: Any) -> None:
        case(name, respond, [(page, context, config)], **kw)

    history = (HistoryEntry(operation=Operation.SCROLL, target=None, outcome=StepOutcome.EXECUTED, page_changed=True),)
    buttons = tuple(button(i) for i in range(5))
    one(
        "picks the target",
        scripted({"operation": "click", "click_target": "b3"}),
        observation(buttons),
        step_context(history=history),
        Config(),
    )
    one(
        "scroll back up",
        scripted({"operation": "scroll_up"}),
        observation(),
        step_context(task="Show the article headline", subgoal="Scroll up from the footer to the headline"),
        Config(),
    )
    for assessment in ReadAssessment:
        one(
            f"read assessment {assessment.value}",
            scripted({"operation": "click", "click_target": "b0", "read_assessment": assessment.value}),
            observation((button(0),)),
            step_context(unread_requirements=("Report why login failed",)),
            Config(),
        )
    one(
        "nothing left to read",
        scripted({"operation": "click", "click_target": "b0"}),
        observation((button(0),)),
        step_context(unread_requirements=()),
        Config(),
    )
    groups = Config(observation=ObservationLimits(max_choice_options=10, group_size=4))
    one(
        "group then element",
        scripted({"operation": "click", "click_group": "2", "click_target": "b9"}),
        observation(tuple(button(i) for i in range(12))),
        step_context(),
        groups,
    )
    one(
        "a group chosen from the end",
        scripted({"operation": "click", "click_group": "-1", "click_target": "b11"}),
        observation(tuple(button(i) for i in range(12))),
        step_context(),
        groups,
    )
    pair = (button(0), button(1, offscreen=True))
    one(
        "oversized drops offscreen",
        rejecting_offscreen(scripted({"operation": "click"})),
        observation(pair),
        step_context(),
        Config(),
    )
    one(
        "too large with no controls",
        scripted({}),
        observation(pair),
        step_context(),
        Config(tokens=TokenBudget(state_plus_largest_question=1, state_plus_all_questions=1)),
    )
    shed = Config(tokens=TokenBudget(batch_tokens=200))
    onscreen = tuple(button(i) for i in range(10))
    one(
        "shed to onscreen",
        shedding({"operation": "click", "click_target": "b3"}, 10),
        observation((*onscreen, *(button(i, offscreen=True) for i in range(10, 40)))),
        step_context(),
        shed,
    )
    one("shed by halves", shedding({"operation": "click"}, 5), observation(onscreen), step_context(), shed)
    one(
        "a small shed request is an outage",
        shedding({"operation": "click"}, 0),
        observation(pair),
        step_context(),
        Config(),
    )
    dense = Config(
        tokens=TokenBudget(state_plus_largest_question=2_000, state_plus_all_questions=2_000),
        observation=ObservationLimits(max_offered_controls=20),
    )
    one(
        "a dense page offers what fits",
        relevance({"operation": "scroll"}, set(), fail=True),
        observation((*(button(i) for i in range(39)), button(39, checked=True))),
        step_context(),
        dense,
    )
    title = "Zebra Pen Z Grip Black Ballpoint Pens with Pocket Clip 8pk, Retractable Black Ink Ballpoint Pens, " * 2
    links = tuple(
        Control(
            id=f"l{i}",
            frame_id=None,
            role="link",
            label=f"{title}listing {i}",
            operations=frozenset({Operation.CLICK}),
            href=f"/Zebra-Ballpoint-Retractable-Reliable-Multipack/dp/B0CT3JS5{i:02d}/ref=sr_1_{i}?dib={'x' * 420}",
        )
        for i in range(112)
    )
    one(
        "long titles compact",
        scripted({"operation": "click", "click_target": "l7"}),
        observation(links),
        step_context(),
        Config(),
    )
    for retry in (False, True):
        one(
            f"every request needs budget, retry {retry}",
            rejecting_offscreen(scripted({"operation": "click", "click_group": "0"}))
            if retry
            else scripted({"operation": "click", "click_group": "0"}),
            observation(tuple(button(i, offscreen=retry and i == 0) for i in range(12))),
            step_context(),
            groups,
            ledger=Limits(max_jev_calls=1),
        )
    twins = tuple(
        button(i).model_copy(update={"label": "Add to cart", "context": name})
        for i, name in enumerate(("Sauce Labs Backpack", "Sauce Labs Bike Light"))
    )
    one(
        "twins carry their context",
        scripted({"operation": "click", "click_target": "b1"}),
        observation(twins),
        step_context(subgoal="Add the Bike Light"),
        Config(),
        ledger=Limits(max_dollars=1.0),
    )
    click_history = (
        HistoryEntry(
            operation=Operation.CLICK,
            target="b0",
            outcome=StepOutcome.EXECUTED,
            page_changed=True,
            text="secret:x",
            effect="nothing visible changed",
            read_progress=True,
            setting=True,
        ),
    )
    for back in (False, True):
        one(
            f"back offered {back}",
            scripted({"operation": "click", "click_target": "b0"}),
            observation((button(0),), can_go_back=back),
            step_context(history=click_history),
            Config(),
        )
    for login, bot in ((True, True), (False, False), (False, True)):
        one(
            f"walls login {login} bot {bot}",
            scripted({}, noul=0.9, nouls={"bot_check": 0.2}),
            observation((button(1),)),
            step_context(check_login=login, check_bot=bot),
            Config(),
        )
    ten = Config(observation=ObservationLimits(max_offered_controls=10))
    one(
        "relevance finds a late control",
        relevance({"operation": "click", "click_target": "b30"}, {"Button 30"}),
        observation(tuple(button(i) for i in range(40))),
        step_context(),
        ten,
    )
    five = Config(observation=ObservationLimits(max_offered_controls=5))
    one(
        "protected controls skip the filter",
        relevance({"operation": "click"}, set()),
        observation(
            (
                *(button(i) for i in range(35)),
                button(35, label="Destination", value="Paris"),
                button(36, blocking=True),
                Control(
                    id="next",
                    frame_id=None,
                    role="link",
                    label="Next \u203a",
                    operations=frozenset({Operation.CLICK}),
                    href="/page/2",
                ),
                button(37, label="Show 20 more"),
                button(38, expanded=True),
            )
        ),
        step_context(),
        five,
    )
    one(
        "unset options do not crowd out apply",
        relevance({"operation": "click", "click_target": "b40"}, {"Apply filters"}),
        observation(
            (*(button(i, role="checkbox", checked=False) for i in range(40)), button(40, label="Apply filters"))
        ),
        step_context(),
        ten,
    )
    one(
        "a failed relevance pass keeps document order",
        relevance({"operation": "click"}, {"Button 30"}, fail=True),
        observation(tuple(button(i) for i in range(40))),
        step_context(),
        ten,
    )
    one(
        "a relevance pass over the call limit",
        relevance({"operation": "click"}, set()),
        observation(tuple(button(i) for i in range(40))),
        step_context(),
        Config(
            observation=ObservationLimits(max_offered_controls=10), tokens=TokenBudget(state_plus_all_questions=600)
        ),
        ledger=Limits(max_jev_calls=2),
    )
    one(
        "a relevance pass over the spend limit",
        relevance({"operation": "click"}, set()),
        observation(tuple(button(i) for i in range(40))),
        step_context(),
        ten,
        ledger=Limits(max_dollars=1e-9),
    )
    one(
        "under the limit no relevance pass",
        relevance({"operation": "click"}, set()),
        observation(tuple(button(i) for i in range(20))),
        step_context(),
        Config(),
    )
    drag = Control(
        id="drag1",
        frame_id=None,
        role="generic",
        label="Card A",
        operations=frozenset({Operation.CLICK, Operation.DRAG}),
    )
    one(
        "a drag and where it lands",
        scripted({"operation": "drag", "drag_target": "drag1", "drag_destination": "b1"}),
        observation((drag, button(1), button(2, label=""))),
        step_context(task="Move Card A onto Done"),
        Config(),
    )
    limit = 10
    one(
        "drag destinations in groups",
        scripted(
            {
                "operation": "drag",
                "drag_target": "drag1",
                "drag_destination_group": str(limit // 10),
                "drag_destination": f"b{limit - 1}",
            }
        ),
        observation((drag.model_copy(update={"selected": True}), *(button(i, selected=True) for i in range(limit)))),
        step_context(task="Move Card A onto Done"),
        Config(observation=ObservationLimits(max_choice_options=limit)),
    )
    one(
        "identical relevance questions",
        relevance({"operation": "click", "click_target": "b3"}, {"Button 0"}),
        observation(tuple(button(0).model_copy(update={"id": f"b{i}"}) for i in range(10))),
        step_context(),
        five,
    )
    page = observation(tuple(button(i) for i in range(10)))
    case(
        "a redraw reuses relevance",
        relevance({"operation": "click", "click_target": "b9"}, {"Button 9"}),
        [(page, step_context(), five), (page.model_copy(update={"page_key": "redrawn"}), step_context(), five)],
        cache=True,
    )
    two = Config(observation=ObservationLimits(max_offered_controls=2))
    base = observation(tuple(button(i) for i in range(5)))
    for change in ("document", "url", "task", "requirements", "unread", "subgoal", "control"):
        changed_page, changed_step = base, step_context()
        match change:
            case "document":
                changed_page = base.model_copy(update={"document_key": "new-document"})
            case "url":
                changed_page = base.model_copy(update={"url": "https://example.test/other"})
            case "task":
                changed_step = step_context(task="another task")
            case "requirements":
                changed_step = step_context(requirements=("another requirement",))
            case "unread":
                changed_step = step_context(unread_requirements=())
            case "subgoal":
                changed_step = step_context(subgoal="different obstacle")
            case _:
                changed_page = base.model_copy(
                    update={"controls": (*base.controls[:-1], button(4).model_copy(update={"label": "changed"}))}
                )
        case(
            f"the relevance cache asks again after a {change} change",
            relevance({"operation": "click"}, {"Button 4"}),
            [(base, step_context(), two), (changed_page, changed_step, two)],
            cache=True,
        )
    field = button(1).model_copy(update={"role": "textbox", "operations": frozenset({Operation.FILL}), "value": "kept"})
    one(
        "a redirected start is not a destination",
        scripted({"operation": "fill"}),
        observation((field,), url="https://example.com/form/"),
        step_context(task="Fill the form", start_url="https://example.com/"),
        Config(),
    )
    one(
        "a bare page offers the start",
        scripted({"operation": "navigate", "navigate_target": "0"}),
        observation(url="chrome-error://chromewebdata/"),
        step_context(task="Sign in", start_url="https://example.com/login"),
        Config(),
    )
    for omitted in (0, 3):
        one(
            f"controls hold back the start {omitted}",
            scripted({"operation": "click"}),
            observation(() if omitted else (button(1),), url="https://example.com/code", omitted_controls=omitted),
            step_context(task="Sign in", start_url="https://example.com/login"),
            Config(),
        )
    one(
        "already at the start landing",
        scripted({"operation": "scroll"}),
        observation(url="https://example.com/form/"),
        step_context(task="Sign in", start_url="https://example.com/", start_landing_url="https://example.com/form/"),
        Config(),
    )
    one(
        "an explicit start redirect",
        scripted({"operation": "click"}),
        observation((button(1),), url="https://example.com/form/"),
        step_context(
            task="Inspect https://example.com/",
            start_url="https://example.com/",
            start_landing_url="https://example.com/form/",
        ),
        Config(),
    )
    urls = " ".join(f"https://site{i}.test/page" for i in range(12))
    one(
        "addresses in groups",
        scripted({"operation": "navigate", "navigate_group": "1", "navigate_target": "1"}, confidence=0.8),
        observation(),
        step_context(task=f"Compare these: {urls}"),
        Config(observation=ObservationLimits(max_choice_options=10, group_size=4)),
    )
    tabs = (
        Tab(id="t1", url="https://example.test/", title="Home \u00e9", active=True, opener_id=None),
        Tab(id="t2", url="https://example.test/popup", title="Popup", active=False, opener_id="t1"),
    )
    upload = Control(
        id="f1",
        frame_id="frame-1",
        role="textbox",
        label='Resume \u6771\u4eac "cv"',
        operations=frozenset({Operation.UPLOAD, Operation.CLICK, Operation.FILL, Operation.ENTER}),
        context="Apply form " + "c" * 120,
        href="https://example.test/" + "h" * 200,
        options=("a", "b"),
        input_type="file",
        input_name="resume",
        autocomplete="off",
        checked=False,
        offscreen=True,
        blocking=True,
    )
    rich = observation(
        (upload, button(2, selected=True), button(3, expanded=False)),
        tabs=tabs,
        viewport_text="Viewport \u00e9 text\nline \U0001f600",
        response_status=404,
        omitted_controls=7,
        history_entry="h1",
        document_key="doc",
    )
    one(
        "every state field",
        scripted({"operation": "switch_tab", "switch_tab_target": "t2"}),
        rich,
        step_context(
            task="Upload my resume \u00e9",
            subgoal="Open the popup",
            requirements=("Upload the resume",),
            unread_requirements=("Find the status",),
            notes="[n1] note",
            history=click_history,
            has_attachments=True,
            secrets=("password",),
            recovery_memory="Tried the button twice.",
            check_login=True,
        ),
        Config(),
    )
    for operation in ("click", "enter", "fill", "upload"):
        one(
            f"every state field compacted, {operation}",
            rejecting_offscreen(scripted({"operation": operation, f"{operation}_target": "f1"})),
            rich,
            step_context(task="Upload", has_attachments=True),
            Config(),
        )
    one(
        "an open dialog",
        scripted({"operation": "dialog"}),
        observation((button(1),), dialog=Dialog(kind="confirm", message="Leave?", default_prompt=None)),
        step_context(),
        Config(),
    )
    one(
        "a target that was not offered",
        scripted({"operation": "click", "click_target": "nope"}),
        observation((button(1),)),
        step_context(),
        Config(),
    )
    one(
        "no operation answered",
        answering({}),
        observation((button(1),)),
        step_context(),
        Config(),
    )
    one(
        "an operation that does not exist",
        answering({"operation": choice_answer("fly")}),
        observation((button(1),)),
        step_context(),
        Config(),
    )
    return cases


def build_requests() -> list[dict[str, Any]]:
    cases = []
    dated = observation().model_copy(update={"captured_at": datetime(2026, 9, 26, 20, tzinfo=UTC)})
    for page, context, config, compact in (
        (dated, step_context(task="Select next month's first Friday"), Config(), False),
        (observation((button(1),)), step_context(), Config(), True),
    ):
        request = build_request(page, page.controls, context, config, compact=compact)
        cases.append(
            {
                "observation": page.model_dump(mode="json"),
                "context": context.model_dump(mode="json"),
                "config": config.model_dump(mode="json"),
                "compact": compact,
                "state": compact_json(request.state),
                "questions": compact_json({k: q.model_dump(mode="json") for k, q in request.questions.items()}),
                "fits": fits(request, config),
            }
        )
    return cases


def compact_json(value: Any) -> str:
    return compact(value)


# --- shortcut and scripted ----------------------------------------------------------------------------------------


def shortcuts() -> dict[str, Any]:
    proposals = []
    for function, task, start, url in (
        ("propose_shortcut", "Find the httpx package \u00e9", "https://pypi.org/", "https://pypi.org/project/httpx/"),
        ("propose_shortcut", "Open my orders", "https://shop.test/", None),
        ("propose_start", "Find the weather in \u6771\u4eac", None, "https://weather.test/"),
    ):
        calls: list[dict[str, Any]] = []
        llm = RecordingLLM(calls, lambda purpose, messages, schema, url=url: {"url": url}, llm_cost())
        book = Ledger(Limits())
        if function == "propose_shortcut":
            assert start is not None
            generated = asyncio.run(propose_shortcut(llm, task, start, ledger=book))
        else:
            generated = asyncio.run(propose_start(llm, task, ledger=book))
        proposals.append(
            {
                "name": f"{function} {task}",
                "function": function,
                "task": task,
                "start": start,
                "calls": calls,
                "outcome": {"url": generated.data.url, "cost": generated.cost.model_dump(mode="json")},
                "ledger_after": ledger_state(book),
            }
        )
    accepted = []
    start = "https://pypi.org/"
    for proposed in (
        None,
        "https://pypi.org/project/httpx/",
        "https://pypi.org",
        "https://pypi.org//",
        "HTTPS://pypi.org/search?q=x",
        "http://pypi.org/project/httpx/",
        "https://evil.test/project",
        "javascript:alert(1)",
        "pypi.org/project",
        "  https://pypi.org/x",
        "https://PYPI.org/y",
        "https://pypi.org:443/z",
    ):
        accepted.append(
            {
                "proposed": proposed,
                "start": start,
                "accept": accept(proposed, start),
                "accept_start": accept_start(proposed),
            }
        )
    scripts = []
    for task in ("click Add one; fill Token = secret:token", " ; ;", "scroll", "select Size = M = L;done", "fly away"):
        try:
            parsed: JsonValue = [step.model_dump(mode="json") for step in steps(task)]
        except ValueError:
            parsed = {"error": "ValueError"}
        scripts.append({"task": task, "steps": parsed})
    operation = ChoiceQuestion(instructions="next", criteria={"read": "Read", "click": "Click", "fill": "Fill"})
    target = ChoiceQuestion(
        instructions="which",
        criteria={"c1": {"label": "Remove"}, "c2": "Add one", "c3": {"label": "Add one", "role": "button"}},
    )
    pick = ChoiceQuestion(instructions="value", criteria={"none": "Nothing", "secret:token": "The token"})
    ask = NoulQuestion(instructions="?")
    jev = ScriptedJev(steps("click Add one; fill Token = secret:token"))
    exchanges = []
    for questions in (
        {"operation": operation, "click_target": target, "read_assessment": pick, "complete": ask},
        {"pick": pick, "fill_target": target, "irreversible": ask},
        {"operation": operation, "fill_target": target, "pick": pick},
        {"operation": operation, "click_target": target, "complete": ask},
    ):
        answered = asyncio.run(jev.evaluate({}, questions))
        exchanges.append(
            {
                "questions": {key: question.model_dump(mode="json") for key, question in questions.items()},
                "evaluation": answered.model_dump(mode="json"),
            }
        )
    return {"proposals": proposals, "accept": accepted, "steps": scripts, "scripted_jev": exchanges}


def write(name: str, value: Any) -> None:
    (HERE / name).write_text(json.dumps(sort_operations(value), indent=1) + "\n", encoding="ascii")


if __name__ == "__main__":
    write("batches.json", batches())
    write("planner.json", planner())
    write("policy.json", {"cases": policy(), "build_request": build_requests()})
    write("shortcut.json", shortcuts())
    write("memory.json", {"cases": memory(), "quoted_fields": quoted_fields(), "shows": shows_cases()})
