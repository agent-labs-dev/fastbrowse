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
import json
from collections.abc import Callable, Mapping, Sequence
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from pydantic import BaseModel, JsonValue

from fastbrowse.batches import evaluate_batches
from fastbrowse.config import TokenBudget
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
from fastbrowse.models import CostBasis, CostComponent, CostLine, Limits, LLMPurpose
from fastbrowse.planner import Plan, Requirement, RequirementKind, RunReport, answer_checks, make_plan, partition_plan
from fastbrowse.telemetry import BudgetExceeded, Ledger

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
    state = {"rules": "Is it relevant? é東京", "page": {"url": "https://example.test/", "n": 1.5e-7}}
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
        {"a": NoulQuestion(instructions="first"), "b": NoulQuestion(instructions="second  ")},
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

    def plan_reply(plan: JsonValue, checks: Sequence[str] = ("Find the price",)) -> Reply:
        def reply(purpose: LLMPurpose, messages: Sequence[Message], schema: type[BaseModel]) -> JsonValue:
            return plan if schema is Plan else {"checks": list(checks)}

        return reply

    def make(
        name: str,
        task: str,
        reply: Reply,
        *,
        start: str | None = None,
        ledger: Limits | None = None,
        normalize_checks: bool = True,
        cost: CostLine | None = None,
    ) -> None:
        calls: list[dict[str, Any]] = []
        book = Ledger(ledger) if ledger is not None else None
        llm = RecordingLLM(calls, reply, cost or llm_cost())
        result, error = run(make_plan(llm, task, start=start, ledger=book, normalize_checks=normalize_checks))
        cases.append(
            {
                "name": name,
                "function": "make_plan",
                "task": task,
                "start": start,
                "ledger": None if ledger is None else ledger.model_dump(mode="json"),
                "normalize_checks": normalize_checks,
                "calls": calls,
                "outcome": error
                or {"plan": result.data.model_dump(mode="json"), "cost": result.cost.model_dump(mode="json")},
                "ledger_after": ledger_state(book),
            }
        )

    example = example_plan().model_dump(mode="json")
    make("plan and repair its checks", "Find the price", plan_reply(example), start="https://shop.test/")
    make(
        "a task in other scripts",
        "Trouve le prix à Tokyo 東京 \U0001f600 \"quoted\"\nline two",
        plan_reply(example),
    )
    for expected in (False, True):
        make(
            f"an empty plan keeps the outcome, answer {expected}",
            "Check whether the document can be opened without signing in.",
            plan_reply({"requirements": [], "answer_expected": expected}),
        )
    for checks in ([], [" "]):
        plan = {
            "requirements": [
                {"id": "r", "text": "Report admission price and opening hours.", "kind": "information"}
            ],
            "answer_expected": True,
            "answer_checks": checks,
        }
        make(
            f"omitted checks {len(checks)}",
            "Report admission price and opening hours.",
            plan_reply(plan, (" Report admission price. ", "Report opening hours.", "Report admission price.")),
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
    make("both receipts in the ledger", "Find the price", plan_reply(unchecked), ledger=limits(max_llm_calls=2))
    make("the repair over the call limit", "Find the price", plan_reply(unchecked), ledger=limits(max_llm_calls=1))
    make("an unpriced repair", "Find the price", plan_reply(unchecked), cost=llm_cost(None))
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
    make(
        "checks left for later",
        "Find the price",
        plan_reply(example),
        normalize_checks=False,
    )

    def partition(
        name: str, task: str, plan: Plan, groups: JsonValue, *, independent_fields: bool = True
    ) -> None:
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
    partition("an unknown requirement", task, plan, [{**groups[0], "subjects": [{"name": "X", "requirement_ids": ["x"]}]}])
    partition("a check left uncovered", task, plan, groups[:1])
    partition("an empty field", task, plan, [{**groups[0], "fields": [" "]}])
    partition("nothing to decompose", task, plan.model_copy(update={"answer_checks": ()}), groups)

    calls: list[dict[str, Any]] = []
    llm = RecordingLLM(calls, lambda purpose, messages, schema: {"checks": ["a", " b ", "a"]}, llm_cost())
    checked = asyncio.run(answer_checks(llm, "é task", plan))
    cases.append(
        {
            "name": "answer checks for a mixed plan",
            "function": "answer_checks",
            "task": "é task",
            "plan": plan.model_dump(mode="json"),
            "calls": calls,
            "outcome": {
                "plan": checked.data.model_dump(mode="json"),
                "cost": None if checked.cost is None else checked.cost.model_dump(mode="json"),
            },
            "ledger_after": None,
        }
    )
    return cases


def write(name: str, value: Any) -> None:
    (HERE / name).write_text(json.dumps(sort_operations(value), indent=1) + "\n", encoding="ascii")


if __name__ == "__main__":
    write("batches.json", batches())
    write("planner.json", planner())
