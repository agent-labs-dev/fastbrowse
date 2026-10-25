"""A later page of a filtered count or comparison is matched record by record by Jev, not read again by the LLM."""

import json
import logging
from collections.abc import Mapping
from pathlib import Path

import pytest
from pydantic import JsonValue

from fastbrowse.jev import Evaluation, NoulAnswer, NoulQuestion, Question
from fastbrowse.memory import Notes
from fastbrowse.page import BlockKind
from fastbrowse.planner import Requirement, RequirementKind
from fastbrowse.retrieval import filter_tallies, read, read_tallies
from fastbrowse.telemetry import TRACE, trace, trace_to, traced
from tests.test_policy import FREE
from tests.test_retrieval import ScriptedLLM, capture

ADA = Requirement(id="r", text="Count records owned by Ada", kind=RequirementKind.INFORMATION, count_records=True)
CHEAPEST = Requirement(id="r", text="The cheapest open record", kind=RequirementKind.INFORMATION)


def records(*owners: str) -> tuple[tuple[BlockKind, str], ...]:
    return tuple((BlockKind.RECORD, f"Record {i}\nOwner: {owner}\nPrice: ${i + 1}") for i, owner in enumerate(owners))


class RecordJev:
    """Answers each record question with the probability `match` gives the record's text."""

    def __init__(self, match: Mapping[str, float]) -> None:
        self.match = match
        self.requests: list[Mapping[str, Question]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        self.requests.append(questions)
        answers = {}
        for key, question in questions.items():
            assert isinstance(question, NoulQuestion)
            record = question.instructions.split("# Record\n", 1)[1]
            answers[key] = NoulAnswer(
                probability=next((p for owner, p in self.match.items() if f"Owner: {owner}" in record), 0.0)
            )
        return Evaluation(model="test", answers=answers, input_tokens=1, cost=FREE)


async def first_page(requirement: Requirement, continuation: JsonValue) -> tuple:
    page = capture(*records("Ada", "Ben", "Ada"))
    response: JsonValue = {"claims": [], "answered": False, "continues": [continuation]}
    result = await read(ScriptedLLM([response]), page, requirement.text, ["r"], Notes(), requirements=[requirement])
    return page, result


async def test_a_filtered_count_is_matched_by_jev_on_later_pages() -> None:
    page, result = await first_page(
        ADA,
        {
            "requirement_id": "r",
            "through_end": True,
            "tallies": [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}, {"first": "s2", "last": "s2"}]}],
        },
    )
    (reader,) = result.tally_readers
    assert reader.filter == ADA.text and not reader.compares
    # The field reader cannot count a filter; only Jev's record matches can.
    assert read_tallies(page, result.tally_readers, ["r"]) is None
    later = capture(*records("Ben", "Ada", "Ada", "Cy"))
    jev = RecordJev({"Ada": 0.97, "Ben": 0.02, "Cy": 0.03})
    counted = await filter_tallies(jev, later, result.tally_readers, ["r"])
    assert counted is not None and len(jev.requests) == 1
    # Keyed like the LLM reader's filtered count, so every page adds to one tally.
    assert [(f.tally.key, f.tally.count) for f in counted.facts if f.tally] == [(ADA.text, 2)]
    assert all(f.evidence is not None for f in counted.facts if f.tally is None)


@pytest.mark.parametrize("unsure", [0.4, 0.5, 0.6])
async def test_an_uncertain_record_sends_the_page_back_to_the_llm_reader(unsure: float) -> None:
    _, result = await first_page(
        ADA,
        {
            "requirement_id": "r",
            "through_end": True,
            "tallies": [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}]}],
        },
    )
    later = capture(*records("Ada", "Ben"))
    assert await filter_tallies(RecordJev({"Ada": 0.99, "Ben": unsure}), later, result.tally_readers, ["r"]) is None


async def test_a_page_of_another_shape_is_not_matched() -> None:
    _, result = await first_page(
        ADA,
        {
            "requirement_id": "r",
            "through_end": True,
            "tallies": [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}]}],
        },
    )
    jev = RecordJev({"Ada": 0.99})
    later = capture(*records("Ada"))
    for changed in [
        later.model_copy(update={"title": "Another list"}),
        later.model_copy(update={"inaccessible_frames": 1}),
        later.model_copy(
            update={"blocks": tuple(b.model_copy(update={"heading_path": ("Other",)}) for b in later.blocks)}
        ),
    ]:
        assert await filter_tallies(jev, changed, result.tally_readers, ["r"]) is None
    assert await filter_tallies(jev, later, result.tally_readers, ["r", "other"]) is None
    assert not jev.requests


async def test_a_list_bounded_to_its_pages_gets_no_record_filter() -> None:
    _, result = await first_page(
        ADA,
        {
            "requirement_id": "r",
            "through_end": False,
            "tallies": [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}]}],
        },
    )
    assert not result.tally_readers


async def test_a_comparison_keeps_jevs_matches_as_its_records() -> None:
    comparison: JsonValue = {
        "order": "lowest",
        "limit": 1,
        "label": {"prefix": "", "suffix": "\nOwner"},
        "value": {"prefix": "Price: ", "suffix": ""},
    }
    _, result = await first_page(
        CHEAPEST,
        {
            "requirement_id": "r",
            "through_end": True,
            "comparison": comparison,
            "records": [{"first": "s0", "last": "s0"}],
        },
    )
    (reader,) = result.tally_readers
    assert reader.compares
    later = capture(*records("Ada", "Ben", "Ada"))
    matched = await filter_tallies(RecordJev({"Ada": 0.99, "Ben": 0.01}), later, result.tally_readers, ["r"])
    assert matched is not None and not any(f.tally for f in matched.facts)
    assert len(matched.continuation_records["r"]) == 2
    assert matched.continues == ("r",)


def test_trace_to_writes_each_event_as_a_json_line(tmp_path: Path) -> None:
    path = tmp_path / "trace.jsonl"
    before = list(TRACE.handlers), TRACE.level
    try:
        trace_to(str(path))
        trace_to(str(path))  # a second call adds no second writer
        with traced():
            trace("read_route", reader="jev_choice", reason="scalar_candidate")
    finally:
        for handler in TRACE.handlers[:]:
            if handler not in before[0]:
                TRACE.removeHandler(handler)
        TRACE.setLevel(before[1])
    (line,) = path.read_text().splitlines()
    event = json.loads(line)
    assert event["event"] == "read_route" and event["reader"] == "jev_choice" and "at" in event


def test_trace_to_without_a_destination_changes_nothing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("FASTBROWSE_TRACE", raising=False)
    before = list(TRACE.handlers)
    trace_to()
    assert TRACE.handlers == before
    assert logging.getLogger("fastbrowse.trace") is TRACE


async def test_only_blocks_shaped_like_the_cited_records_are_asked_about() -> None:
    _, result = await first_page(
        ADA,
        {
            "requirement_id": "r",
            "through_end": True,
            "tallies": [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}]}],
        },
    )
    later = capture(*records("Ada", "Ben"), (BlockKind.LIST_ITEM, "Owner: Ada (sidebar link)"))
    sidebar = later.blocks[-1].model_copy(update={"heading_path": ("Categories",)})
    later = later.model_copy(update={"blocks": (*later.blocks[:-1], sidebar)})
    jev = RecordJev({"Ada": 0.99, "Ben": 0.01})
    counted = await filter_tallies(jev, later, result.tally_readers, ["r"])
    assert counted is not None and len(jev.requests[0]) == 2
    assert [f.tally.count for f in counted.facts if f.tally] == [1]


async def ada_reader() -> tuple:
    _, result = await first_page(
        ADA,
        {
            "requirement_id": "r",
            "through_end": True,
            "tallies": [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}]}],
        },
    )
    return result.tally_readers


async def test_a_featured_strip_under_its_own_heading_is_not_counted() -> None:
    """A featured strip repeated on every page would be counted once a page if it shared the records' shape."""
    later = capture(*records("Ada", "Ben"), (BlockKind.HEADING, "Featured"), (BlockKind.RECORD, "Owner: Ada"))
    featured = later.blocks[-1].model_copy(update={"heading_path": ("Featured",)})
    later = later.model_copy(update={"blocks": (*later.blocks[:-1], featured)})
    jev = RecordJev({"Ada": 0.99, "Ben": 0.01})
    counted = await filter_tallies(jev, later, await ada_reader(), ["r"])
    assert counted is not None and len(jev.requests[0]) == 2
    assert [f.tally.count for f in counted.facts if f.tally] == [1]
    # The records' shape interleaved with another list's blocks stops the filter.
    mixed = capture(*records("Ada"), (BlockKind.LIST_ITEM, "Owner: Ada (sidebar)"), *records("Ben"))
    assert await filter_tallies(jev, mixed, await ada_reader(), ["r"]) is None


async def test_a_rejection_needs_more_certainty_than_a_match() -> None:
    later = capture(*records("Ada", "Ben"))
    assert await filter_tallies(RecordJev({"Ada": 0.99, "Ben": 0.25}), later, await ada_reader(), ["r"]) is None
    counted = await filter_tallies(RecordJev({"Ada": 0.72, "Ben": 0.15}), later, await ada_reader(), ["r"])
    assert counted is not None and [f.tally.count for f in counted.facts if f.tally] == [1]


async def test_jev_sees_the_first_pages_matches_as_examples() -> None:
    class Recording(RecordJev):
        def __init__(self, match: Mapping[str, float]) -> None:
            super().__init__(match)
            self.states: list[JsonValue] = []

        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            self.states.append(state)
            return await super().evaluate(state, questions)

    jev = Recording({"Ada": 0.99, "Ben": 0.01})
    await filter_tallies(jev, capture(*records("Ada")), await ada_reader(), ["r"])
    assert jev.states and "Owner: Ada" in json.dumps(jev.states[0])


async def test_a_comparison_record_without_a_readable_value_goes_to_the_llm_reader() -> None:
    comparison: JsonValue = {
        "order": "lowest",
        "limit": 1,
        "label": {"prefix": "", "suffix": "\nOwner"},
        "value": {"prefix": "Price: ", "suffix": ""},
    }
    _, result = await first_page(
        CHEAPEST,
        {
            "requirement_id": "r",
            "through_end": True,
            "comparison": comparison,
            "records": [{"first": "s0", "last": "s0"}],
        },
    )
    later = capture((BlockKind.RECORD, "Record 9\nOwner: Ada\nPrice on request"))
    assert await filter_tallies(RecordJev({"Ada": 0.99}), later, result.tally_readers, ["r"]) is None
