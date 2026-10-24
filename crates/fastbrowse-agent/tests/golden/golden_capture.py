"""Record every retrieval and verification call the Python tests make, for the Rust port to replay.

    PYTHONPATH=crates/fastbrowse-agent/tests/golden uv run pytest -q -p golden_capture tests/test_retrieval.py ...

`run_capture.sh` beside this file lists the test files. Each outermost call to a wrapped function becomes one case:
the arguments it was given (serialized before the call, since `read` mutates its notes), every Jev and LLM request
it made with the answer the test's fake gave, what it returned or raised, the notes and ledgers it was handed as
they were afterwards, and the trace it emitted. A hand-written case can only cover what its author thought of; the
tests already pin the behavior, so replaying their calls holds Rust to all of it.

Requests are written as `generate.py` writes them, so the Rust replay compares them byte for byte; the cases are
written one per line, ASCII only, to `retrieval.json` and `verification.json`.
"""

import asyncio
import base64
import contextvars
import functools
import hashlib
import inspect
import json
import os
import sys
from collections.abc import Callable
from datetime import date, datetime
from decimal import Decimal
from enum import Enum
from pathlib import Path
from typing import Any

import pytest
from pydantic import BaseModel, TypeAdapter
from pydantic.fields import FieldInfo
from pydantic_core import PydanticUndefined

from fastbrowse import retrieval, verification
from fastbrowse.config import TokenBudget
from fastbrowse.jev import JevError, JevInputTooLarge, JevRetriesExhausted
from fastbrowse.llm import LLMError, LLMRetriesExhausted
from fastbrowse.memory import Notes, NotesTooLarge, evidence_id
from fastbrowse.telemetry import BudgetExceeded, Ledger, traced

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))
from generate import compact, sort_operations  # noqa: E402

RETRIEVAL = (
    "chunk",
    "leading_quotes",
    "read",
    "read_tallies",
    "compose",
    "partial_answer",
    "draft_answer",
    "assemble_answer",
    "retain_fields",
    "transaction_check_question",
    "claim_check_questions",
    "field_question",
    "field_group_question",
    "field_candidate_groups",
    "choose_candidate",
    "propose_text_fields",
    "propose_text_fields_from_notes",
    "copy_field",
    "read_candidates",
    "field_candidates",
    "field_candidates_from_notes",
    "merge_candidates",
    "names_subject",
    "subject_words",
    "_read_message",
    "_read_choices",
    "_counted_collections",
)
VERIFICATION = (
    "page_state",
    "check_done",
    "verify_messages",
    "verify_cache_key",
    "llm_verify",
    "visual_only_state",
    "check_answer_outputs",
    "check_claims",
    "extract",
    "_output_context",
    "_share_output_sources",
    "_grounding",
    "_identity_schema",
    "_assessment_schema",
)
# Lists a function appends its findings to; they are written again after the call.
OUT_LISTS = ("missing_outputs", "corrections", "verified_outputs", "verified_fields")
PER_TEST = int(os.environ.get("GOLDEN_PER_TEST", "3"))
# These drive the whole loop, so one test calls the same function many times with near-identical inputs.
LOOP_TESTS = ("tests/test_agent.py", "tests/test_partitioned_finish.py")
PER_LOOP_TEST = 1
# A case holding a long answer's every audit ran to 27 MB; one that size proves nothing a smaller one does not.
MAX_CASE_BYTES = int(os.environ.get("GOLDEN_MAX_CASE_BYTES", "100000"))
# Long-page cases (the relevance pass, multi-chunk reads) are written to their own files, kept apart so the common
# cases stay small to review: `GOLDEN_SUFFIX=_large` with a larger byte cap and only the tests that need it.
SUFFIX = os.environ.get("GOLDEN_SUFFIX", "")

_ORIGINAL_CONSTANTS = {
    "MAX_CHOICE_OPTIONS": retrieval.MAX_CHOICE_OPTIONS,
    "_FIELD_GROUP_SIZE": retrieval._FIELD_GROUP_SIZE,
}
_ORIGINAL_REMAINING = TokenBudget.remaining_chars

_depth: contextvars.ContextVar[int] = contextvars.ContextVar("capture_depth", default=0)
_recorder: contextvars.ContextVar["_Case | None"] = contextvars.ContextVar("capture_case", default=None)
_current_test = ""
_cases: dict[str, list[str]] = {"retrieval": [], "verification": []}
_seen: set[str] = set()
_counts: dict[tuple[str, str], int] = {}
_dropped: dict[str, int] = {}
_originals: dict[int, Callable[..., Any]] = {}
_DATETIME = TypeAdapter(datetime)
_DATE = TypeAdapter(date)


class Unsupported(Exception):
    pass


def notes_state(notes: Notes) -> dict[str, Any]:
    guess = notes._on_guess
    return {
        "facts": [[key, fact.model_dump(mode="json")] for key, fact in notes._facts.items()],
        "requirements": [[key, sorted(ids)] for key, ids in notes._requirements.items()],
        "owners": [[key, sorted(ids)] for key, ids in notes._owners.items()],
        "retired": sorted(notes._retired),
        "discredited": sorted(notes._discredited),
        "guessed": None
        if guess is None
        else sorted(
            {
                evidence_id(fact.evidence)
                for fact in notes._facts.values()
                if fact.evidence is not None and guess(fact.evidence)
            }
        ),
        "tally_records": sorted(notes._tally_records),
        "record_ids": [[list(key), [dict(entry) for entry in entries]] for key, entries in notes._record_ids.items()],
        "continuation_records": [[key, sorted(ids)] for key, ids in notes._continuation_records.items()],
        "collection_pages": [
            [key, [read.model_dump(mode="json") for read in reads]] for key, reads in notes._collection_pages.items()
        ],
        "pages": [
            [[sha, url, _DATETIME.dump_python(at, mode="json")], page.model_dump(mode="json")]
            for (sha, url, at), page in notes._pages.items()
        ],
    }


def ledger_state(ledger: Ledger) -> dict[str, Any]:
    return {
        "limits": ledger.limits.model_dump(mode="json"),
        "lines": [line.model_dump(mode="json") for line in ledger.lines],
        "steps": ledger.steps,
        "jev_calls": ledger.jev_calls,
        "llm_calls": ledger.llm_calls,
    }


def _annotation(annotation: Any) -> str:
    names = {int: "int", float: "float", Decimal: "Decimal", date: "date", bool: "bool", str: "str"}
    return names.get(annotation, "other")


def _metadata(field: FieldInfo) -> list[dict[str, Any]]:
    described: list[dict[str, Any]] = []
    for item in field.metadata:
        found = False
        for name in ("ge", "gt", "le", "lt", "multiple_of", "max_digits", "decimal_places", "min_length", "max_length"):
            value = getattr(item, name, None)
            if value is not None:
                described.append({name: serialize(value)})
                found = True
        if getattr(item, "strict", None) is True:
            described.append({"strict": True})
            found = True
        if not found:
            described.append({"other": type(item).__name__})
    return described


def field_info(field: FieldInfo) -> dict[str, Any]:
    described: dict[str, Any] = {
        "annotation": _annotation(field.annotation),
        "title": field.title,
        "description": field.description,
        "required": field.is_required(),
        "metadata": _metadata(field),
    }
    if not field.is_required():
        default = field.get_default(call_default_factory=True)
        if default is not PydanticUndefined:
            described["default"] = serialize(default)
    return described


def serialize(value: Any) -> Any:
    if value is None or (isinstance(value, bool | int | float) and not isinstance(value, Enum)):
        return value
    if isinstance(value, Enum):
        return serialize(value.value)
    if isinstance(value, str):
        return str(value)
    if isinstance(value, _Proxy):
        return {f"${value.kind}": True}
    if isinstance(value, retrieval.ReadOutcome):
        return {
            **value.model_dump(mode="json"),
            "collection_facts": [fact.model_dump(mode="json") for fact in value.collection_facts],
        }
    if isinstance(value, BaseModel):
        return value.model_dump(mode="json")
    if isinstance(value, Notes):
        return {"$notes": notes_state(value)}
    if isinstance(value, Ledger):
        return {"$ledger": ledger_state(value)}
    if isinstance(value, Decimal):
        return str(value)
    if isinstance(value, datetime):
        return _DATETIME.dump_python(value, mode="json")
    if isinstance(value, date):
        return _DATE.dump_python(value, mode="json")
    if isinstance(value, bytes | bytearray):
        return {"$bytes": base64.b64encode(bytes(value)).decode()}
    if isinstance(value, FieldInfo):
        return {"$field": field_info(value)}
    if isinstance(value, type) and issubclass(value, BaseModel):
        return {
            "$schema": {
                "name": value.__name__,
                "fields": [{"name": name, **field_info(field)} for name, field in value.model_fields.items()],
                "json_schema": value.model_json_schema(),
            }
        }
    if isinstance(value, dict):
        if all(isinstance(key, str) for key in value):
            return {str(key): serialize(item) for key, item in value.items()}
        return [[serialize(key), serialize(item)] for key, item in value.items()]
    if isinstance(value, list | tuple):
        return [serialize(item) for item in value]
    if isinstance(value, set | frozenset):
        return sorted((serialize(item) for item in value), key=lambda item: json.dumps(item, sort_keys=True))
    raise Unsupported(type(value).__name__)


def error_kind(error: BaseException) -> str:
    kinds: tuple[tuple[type[BaseException], str], ...] = (
        (BudgetExceeded, "budget"),
        (JevRetriesExhausted, "jev_retries_exhausted"),
        (JevInputTooLarge, "jev_input_too_large"),
        (JevError, "jev"),
        (LLMError, "llm"),
        (NotesTooLarge, "NotesTooLarge"),
        (ValueError, "ValueError"),
    )
    for kind, name in kinds:
        if isinstance(error, kind):
            return name
    return type(error).__name__


class _Case:
    def __init__(self, ledgers: list[Ledger]) -> None:
        self.calls: list[dict[str, Any]] = []
        self.ledgers = ledgers
        self.skip: str | None = None
        self.fake_raised: set[str] = set()
        """What an LLM fake raised that is not an LLM failure, such as IndexError once its script ran out."""

    def counts(self) -> list[tuple[int, int, int]]:
        return [(len(ledger.lines), ledger.jev_calls, ledger.llm_calls) for ledger in self.ledgers]

    def effect(self, before: list[tuple[int, int, int]]) -> list[dict[str, Any]] | None:
        effects = []
        for ledger, (lines, jev_calls, llm_calls) in zip(self.ledgers, before, strict=True):
            effects.append(
                {
                    "lines": [line.model_dump(mode="json") for line in ledger.lines[lines:]],
                    "jev_calls": ledger.jev_calls - jev_calls,
                    "llm_calls": ledger.llm_calls - llm_calls,
                }
            )
        return effects or None


class _Proxy:
    """Stands in for a test's fake client and writes down what was asked and answered.

    One proxy per fake for the whole session, so `id(llm)` (part of the output audit cache's key) is as stable as
    the fake's own, and an audit a test expects to be cached is still cached. Which case a request belongs to is
    read from the context the request runs in, since calls from concurrent tasks interleave."""

    def __init__(self, inner: Any, kind: str) -> None:
        self._inner = inner
        self.kind = kind

    def __getattr__(self, name: str) -> Any:
        return getattr(self._inner, name)

    async def evaluate(self, state: Any, questions: Any) -> Any:
        case = _recorder.get()
        if case is None:
            return await self._inner.evaluate(state, questions)
        entry: dict[str, Any] = {
            "jev": {
                "state": compact(state),
                "questions": compact({key: q.model_dump(mode="json") for key, q in questions.items()}),
            }
        }
        case.calls.append(entry)
        before = case.counts()
        try:
            evaluation = await self._inner.evaluate(state, questions)
        except JevRetriesExhausted as error:
            entry["response"] = {
                "error": "retries_exhausted",
                "message": str(error),
                "seconds": error.seconds,
                "unaccounted_requests": error.unaccounted_requests,
                "requests": error.requests,
            }
            raise
        except JevInputTooLarge as error:
            entry["response"] = {"error": "input_too_large", "message": str(error)}
            raise
        except JevError as error:
            entry["response"] = {"error": "failed", "message": str(error)}
            raise
        except BudgetExceeded as error:
            entry["response"] = {"error": "budget", "message": str(error)}
            raise
        except BaseException as error:
            entry["response"] = {"error": type(error).__name__, "message": str(error)}
            raise
        finally:
            if (effect := case.effect(before)) is not None:
                entry["ledger_effect"] = effect
        entry["response"] = {"evaluation": evaluation.model_dump(mode="json")}
        return evaluation

    async def generate(self, purpose: Any, messages: Any, schema: Any, **kwargs: Any) -> Any:
        case = _recorder.get()
        if case is None:
            return await self._inner.generate(purpose, messages, schema, **kwargs)
        entry: dict[str, Any] = {
            "llm": {
                "purpose": purpose.value,
                "messages": compact([message.model_dump(mode="json") for message in messages]),
                "schema_name": schema.__name__,
                "schema": json.dumps(schema.model_json_schema()),
                "max_output_tokens": kwargs.get("max_output_tokens", 2000),
            }
        }
        case.calls.append(entry)
        before = case.counts()
        try:
            generation = await self._inner.generate(purpose, messages, schema, **kwargs)
        except LLMRetriesExhausted as error:
            entry["response"] = {"error": "llm_retries_exhausted", "message": str(error)}
            raise
        except LLMError as error:
            entry["response"] = {"error": "llm", "message": str(error)}
            raise
        except BudgetExceeded as error:
            entry["response"] = {"error": "budget", "message": str(error)}
            raise
        except BaseException as error:
            entry["response"] = {"error": type(error).__name__, "message": str(error)}
            if isinstance(error, Exception):
                case.fake_raised.add(error_kind(error))
            raise
        finally:
            if (effect := case.effect(before)) is not None:
                entry["ledger_effect"] = effect
        if not isinstance(generation.data, BaseModel):
            case.skip = "the LLM fake returned data that is not a model"
            entry["response"] = {"data": None, "cost": generation.cost.model_dump(mode="json")}
        else:
            entry["response"] = {
                "data": generation.data.model_dump(mode="json"),
                "cost": generation.cost.model_dump(mode="json"),
            }
        return generation


_proxies: dict[int, tuple[Any, _Proxy]] = {}


def proxy(inner: Any, kind: str) -> Any:
    if inner is None or isinstance(inner, _Proxy):
        return inner
    held = _proxies.get(id(inner))
    if held is None or held[0] is not inner:
        held = (inner, _Proxy(inner, kind))
        _proxies[id(inner)] = held
    return held[1]


def _monkeypatched() -> str | None:
    for name, value in _ORIGINAL_CONSTANTS.items():
        if getattr(retrieval, name) != value:
            return f"retrieval.{name} is monkeypatched"
    if TokenBudget.remaining_chars is not _ORIGINAL_REMAINING:
        return "TokenBudget.remaining_chars is monkeypatched"
    return None


class _Recording:
    """One outermost call: its arguments before, and everything after."""

    def __init__(self, module: str, name: str, signature: inspect.Signature, args: Any, kwargs: Any) -> None:
        self.module = module
        self.name = name
        bound = signature.bind(*args, **kwargs)
        for key in ("jev", "llm"):
            if key in bound.arguments:
                bound.arguments[key] = proxy(bound.arguments[key], key)
        self.bound = bound
        self.unsupported = False
        self.args: dict[str, Any] = {}
        for key, value in bound.arguments.items():
            if key == "audit_cache" and isinstance(value, dict):
                self.args[key] = {"$audit_cache": len(value)}
                continue
            try:
                self.args[key] = serialize(value)
            except Unsupported as error:
                self.args[key] = {"$unsupported": str(error)}
                self.unsupported = True
        ledgers = [value for value in bound.arguments.values() if isinstance(value, Ledger)]
        self.case = _Case(ledgers)
        self.skip = _monkeypatched()

    def notes_after(self, key: str, notes: Notes) -> Any:
        # Most calls only read their notes; a marker instead of a second copy keeps the files a quarter smaller.
        state = notes_state(notes)
        return {"$same": True} if self.args.get(key) == {"$notes": state} else state

    def finish(self, outcome: dict[str, Any], events: list[object]) -> None:
        arguments = self.bound.arguments
        record: dict[str, Any] = {
            "name": "",
            "function": self.name,
            "args": self.args,
            "calls": self.case.calls,
            "outcome": outcome,
            "notes_after": {key: self.notes_after(key, v) for key, v in arguments.items() if isinstance(v, Notes)},
            "ledger_after": {key: ledger_state(v) for key, v in arguments.items() if isinstance(v, Ledger)},
            "mutated": {key: _safe(v) for key, v in arguments.items() if key in OUT_LISTS and isinstance(v, list)},
            "trace": [_safe(event) for event in events],
        }
        skip = self.skip or self.case.skip
        if skip is None and outcome.get("error") in self.case.fake_raised:
            # A call that ends on the fake's own exception cannot end that way in Rust, whose LLM client can only fail
            # with an LLM error.
            skip = f"the LLM fake raised {outcome['error']}, which no LLM client raises"
        if skip is not None:
            record["skip"] = skip
        if self.unsupported:
            record["unsupported"] = True
        _store(self.module, record)


def _safe(value: Any) -> Any:
    try:
        return serialize(value)
    except Unsupported as error:
        return {"$unsupported": str(error)}


def _store(module: str, record: dict[str, Any]) -> None:
    record = sort_operations(record)
    digest = hashlib.sha256(json.dumps(record, sort_keys=True).encode()).hexdigest()
    if digest in _seen:
        return
    test = _current_test or "?"
    key = (test, record["function"])
    cap = PER_LOOP_TEST if test.startswith(LOOP_TESTS) else PER_TEST
    if _counts.get(key, 0) >= cap:
        return
    _seen.add(digest)
    record["name"] = f"{test}#{record['function']}#{_counts.get(key, 0) + 1}"
    line = json.dumps(record, separators=(",", ":"), ensure_ascii=True)
    if len(line) > MAX_CASE_BYTES:
        _dropped[record["function"]] = _dropped.get(record["function"], 0) + 1
        return
    _counts[key] = _counts.get(key, 0) + 1
    _cases[module].append(line)


def _outcome(value: Any) -> dict[str, Any]:
    return {"result": _safe(value)}


def _failure(error: BaseException) -> dict[str, Any]:
    return {"error": error_kind(error), "message": str(error)}


def _forward(outer: list[object] | None, events: list[object]) -> None:
    # A nested collector takes the records from any collector the test opened, so they are handed on to it.
    if outer is not None:
        outer.extend(events)


def wrap(module: str, name: str, original: Callable[..., Any]) -> Callable[..., Any]:
    signature = inspect.signature(original)
    from fastbrowse.telemetry import _trace_events

    if inspect.iscoroutinefunction(original):

        @functools.wraps(original)
        async def recorded_async(*args: Any, **kwargs: Any) -> Any:
            if _depth.get():
                return await original(*args, **kwargs)
            recording = _Recording(module, name, signature, args, kwargs)
            depth = _depth.set(1)
            case = _recorder.set(recording.case)
            outer = _trace_events.get()
            try:
                with traced() as events:
                    try:
                        value = await original(*recording.bound.args, **recording.bound.kwargs)
                    except BaseException as error:
                        if not isinstance(error, asyncio.CancelledError):
                            recording.finish(_failure(error), list(events))
                        raise
                    recording.finish(_outcome(value), list(events))
                    return value
            finally:
                _forward(outer, list(events))
                _recorder.reset(case)
                _depth.reset(depth)

        _originals[id(recorded_async)] = original
        return recorded_async

    @functools.wraps(original)
    def recorded(*args: Any, **kwargs: Any) -> Any:
        if _depth.get():
            return original(*args, **kwargs)
        recording = _Recording(module, name, signature, args, kwargs)
        depth = _depth.set(1)
        case = _recorder.set(recording.case)
        outer = _trace_events.get()
        try:
            with traced() as events:
                try:
                    value = original(*recording.bound.args, **recording.bound.kwargs)
                except Exception as error:
                    recording.finish(_failure(error), list(events))
                    raise
                recording.finish(_outcome(value), list(events))
                return value
        finally:
            _forward(outer, list(events))
            _recorder.reset(case)
            _depth.reset(depth)

    _originals[id(recorded)] = original
    return recorded


_replacements: dict[int, Callable[..., Any]] = {}


def _install() -> None:
    for module, names in (("retrieval", RETRIEVAL), ("verification", VERIFICATION)):
        source = retrieval if module == "retrieval" else verification
        for name in names:
            original = getattr(source, name)
            replacement = wrap(module, name, original)
            _replacements[id(original)] = replacement
            setattr(source, name, replacement)
    merge = retrieval.ReadOutcome.merge_records
    wrapped = wrap("retrieval", "merge_records", merge)
    _replacements[id(merge)] = wrapped
    retrieval.ReadOutcome.merge_records = wrapped  # type: ignore[method-assign]
    _rebind()


def _rebind() -> None:
    """Point every module that imported a wrapped function by name at the wrapper."""
    for module in list(sys.modules.values()):
        attributes = getattr(module, "__dict__", None)
        if not isinstance(attributes, dict):
            continue
        for key, value in list(attributes.items()):
            replacement = _replacements.get(id(value))
            if replacement is not None and replacement is not value:
                attributes[key] = replacement


def pytest_configure(config: pytest.Config) -> None:
    import fastbrowse.agent  # noqa: F401  # imported so its by-name imports are rebound too

    _install()


def pytest_collection_modifyitems(session: pytest.Session, config: pytest.Config, items: list[pytest.Item]) -> None:
    _rebind()


@pytest.hookimpl(wrapper=True)
def pytest_runtest_call(item: pytest.Item) -> Any:
    global _current_test
    _current_test = item.nodeid
    try:
        return (yield)
    finally:
        _current_test = ""


def pytest_sessionfinish(session: pytest.Session, exitstatus: int) -> None:
    for module, lines in _cases.items():
        text = "[\n" + ",\n".join(lines) + "\n]\n" if lines else "[]\n"
        (HERE / f"{module}{SUFFIX}.json").write_text(text, encoding="ascii")
    if _dropped:
        print(f"\ngolden_capture: dropped oversized cases {_dropped}")
