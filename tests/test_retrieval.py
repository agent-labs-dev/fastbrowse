import asyncio
import hashlib
import json
from collections.abc import Mapping, Sequence
from datetime import UTC, date, datetime
from decimal import Decimal

import pytest
from pydantic import BaseModel, Field, JsonValue

from fastbrowse.config import Thresholds, TokenBudget
from fastbrowse.jev import (
    MAX_CHOICE_OPTIONS,
    Answer,
    ChoiceAnswer,
    ChoiceQuestion,
    Evaluation,
    JevError,
    JevInputTooLarge,
    NoulAnswer,
    NoulQuestion,
    Question,
)
from fastbrowse.llm import DEFAULT_MAX_OUTPUT_TOKENS, Generation, Message
from fastbrowse.memory import Fact, Notes, Tally, evidence_id, fact_id
from fastbrowse.models import CostBasis, CostComponent, CostLine, Evidence, FactReader, Frozen, Limits, LLMPurpose
from fastbrowse.page import Block, BlockKind, Capture, Observation
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.retrieval import (
    TRANSACTION_CONTRADICTED,
    Claim,
    UnsupportedField,
    _read_message,
    assemble_answer,
    chunk,
    claim_check_questions,
    compose,
    copy_field,
    draft_answer,
    field_candidates,
    field_candidates_from_notes,
    field_question,
    merge_candidates,
    propose_text_fields,
    propose_text_fields_from_notes,
    read,
    read_candidates,
    read_tallies,
    transaction_check_question,
)
from fastbrowse.telemetry import BudgetExceeded, Ledger
from fastbrowse.verification import check_done


def capture(*parts: tuple[BlockKind, str], url: str = "https://example.test") -> Capture:
    text = "\n\n".join(part for _, part in parts)
    blocks: list[Block] = []
    start = 0
    for i, (kind, part) in enumerate(parts):
        blocks.append(Block(source_id=f"s{i}", kind=kind, frame_id=None, start=start, end=start + len(part)))
        start += len(part) + 2
    return Capture(
        url=url,
        title="Example",
        captured_at=datetime(2026, 1, 1, tzinfo=UTC),
        sha256=hashlib.sha256(text.encode()).hexdigest(),
        text=text,
        blocks=tuple(blocks),
    )


class ScriptedLLM:
    def __init__(self, responses: Sequence[JsonValue]) -> None:
        self.responses = list(responses)
        self.calls: list[tuple[LLMPurpose, tuple[Message, ...]]] = []
        self.output_caps: list[int] = []

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = DEFAULT_MAX_OUTPUT_TOKENS,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        # Reserve exactly as the real client does, so a test can see a budget stop a request.
        if ledger is not None:
            ledger.reserve(CostComponent.LLM)
        self.calls.append((purpose, tuple(messages)))
        self.output_caps.append(max_output_tokens)
        return Generation(
            data=schema.model_validate(self.responses.pop(0)),
            cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose),
        )


def block_evidence(page: Capture, source_id: str) -> Evidence:
    """Evidence for one whole block, as the reader's cite of it copies."""
    block = next(block for block in page.blocks if block.source_id == source_id)
    return Evidence(
        source_id=source_id,
        url=page.url,
        frame_id=block.frame_id,
        captured_at=page.captured_at,
        capture_sha256=page.sha256,
        start=block.start,
        end=block.end,
        quote=page.text[block.start : block.end],
        heading_path=block.heading_path,
    )


def test_chunk_prefers_headings_and_preserves_block_coverage() -> None:
    page = capture(
        (BlockKind.HEADING, "Intro"),
        (BlockKind.PARAGRAPH, "First block"),
        (BlockKind.HEADING, "Next"),
        (BlockKind.PARAGRAPH, "Second block with text"),
    )
    parts = chunk(page, 27, overlap_blocks=0)
    assert parts[0].block_ids == ("s0", "s1")
    assert parts[1].block_ids[0] == "s2"
    assert {key for part in parts for key in part.block_ids} == {block.source_id for block in page.blocks}
    assert [part.index for part in parts] == list(range(len(parts)))
    assert all(part.total == len(parts) for part in parts)
    for part in parts:
        assert len(part.text) <= 27
        assert part.start in {block.start for block in page.blocks}
        assert part.end in {block.end for block in page.blocks}


def test_chunk_overlap_makes_progress_and_an_oversized_block_is_split_to_fit() -> None:
    page = capture(*((BlockKind.PARAGRAPH, text) for text in ("aaaa", "bbbb", "cccc", "dddd")))
    parts = chunk(page, 9)
    assert [part.block_ids for part in parts] == [("s0", "s1"), ("s1", "s2"), ("s2", "s3")]
    # A whole results list can arrive as one block; carried whole, it left the reader's notes no room.
    huge = capture((BlockKind.PARAGRAPH, "x" * 25 + "\nyyy"), (BlockKind.PARAGRAPH, "tail"))
    parts = chunk(huge, 10, overlap_blocks=0)
    assert [part.text for part in parts] == ["x" * 10, "x" * 10, "xxxxx\nyyy", "tail"]
    assert [part.block_ids for part in parts] == [("s0",), ("s0",), ("s0",), ("s1",)]
    assert chunk(capture(), 10) == ()
    with pytest.raises(ValueError):
        chunk(page, 0)


def test_chunk_repeats_markdown_table_header_and_keeps_rows_grounded() -> None:
    header = "| Name | Cost |\n| --- | --- |"
    rows = [f"| Item{i} | ${i}.00 |" for i in range(8)]
    page = capture((BlockKind.TABLE, header + "\n" + "\n".join(rows)))
    parts = chunk(page, 65, overlap_blocks=0)
    assert len(parts) > 1
    assert all(part.text.startswith(header) for part in parts)
    assert all(part.block_ids == ("s0",) for part in parts)
    assert all(len(part.text) <= 65 for part in parts)
    for row in rows:
        assert any(row in part.text for part in parts)
    # The reader is shown each continuation under the header too, or its columns lose their names.
    assert all(header in _read_message(page, part, "Find the cost", ["r"]).content for part in parts)


def test_the_reader_is_told_which_blocks_are_headings_and_which_a_frame_holds() -> None:
    """Asked for the heading a framed form shows, the LLM reader and Jev alike gave the page's heading above the
    frame."""
    parts = (BlockKind.HEADING, "Email Subscription"), (BlockKind.HEADING, "Send updates"), (BlockKind.PARAGRAPH, "Go")
    page = capture(*parts)
    page = page.model_copy(
        update={"blocks": (page.blocks[0], *(block.model_copy(update={"frame_id": "f"}) for block in page.blocks[1:]))}
    )
    content = _read_message(page, chunk(page, 1000)[0], "Find the form's heading", ["r"]).content
    assert "[s0] (heading) Email Subscription\n[s1] (heading, inside an embedded frame) Send updates\n" in content
    assert "[s2] (paragraph, inside an embedded frame) Go" in content
    assert [candidate.context for candidate in read_candidates(page)] == [
        "(heading) Email Subscription",
        "(heading, inside an embedded frame) Send updates",
        "(inside an embedded frame) Go",
    ]


def test_chunk_repeats_nearest_header_when_table_rows_are_separate_blocks() -> None:
    page = capture(
        (BlockKind.TABLE, "Name | Cost"),
        (BlockKind.TABLE, "AAA | 100"),
        (BlockKind.TABLE, "BBB | 200"),
        (BlockKind.HEADING, "Other"),
        (BlockKind.TABLE, "Age | Count"),
        (BlockKind.TABLE, "CCC | 300"),
    )
    parts = chunk(page, 22, overlap_blocks=0)
    continued = next(part for part in parts if "BBB" in part.text)
    assert continued.text.startswith("Name | Cost") and "s0" in continued.block_ids
    last = next(part for part in parts if "CCC" in part.text)
    assert "Age | Count" in last.text and "Name | Cost" not in last.text


async def test_read_continues_after_an_unoffered_cite_tracks_coverage_and_cost() -> None:
    page = capture(
        (BlockKind.PARAGRAPH, "Price unknown"),
        (BlockKind.PARAGRAPH, "Price is $12"),
        (BlockKind.PARAGRAPH, "Irrelevant end"),
    )
    llm = ScriptedLLM(
        [
            {
                "claims": [{"requirement_id": "r1", "text": "Free", "cite": {"first": "s1", "last": "s1"}}],
                "answered": True,
            },
            {
                "claims": [{"requirement_id": "r1", "text": "Costs $12", "cite": {"first": "s1", "last": "s1"}}],
                "answered": True,
            },
        ]
    )
    notes = Notes()
    result = await read(
        llm, page, "What price?", ["r1"], notes, max_chars=15, tokens=TokenBudget(read_output_tokens=4096)
    )
    assert llm.output_caps == [4096, 4096]
    assert result.coverage == (0, 1) and result.rejected_claims == 1
    assert len(result.facts) == 1 and next(iter(Notes(result.facts).evidence.values())).quote == "Price is $12"
    assert notes.evidenced("r1")
    assert sum(line.dollars or 0 for line in result.cost_lines) == 0.002
    assert all(purpose is LLMPurpose.READ for purpose, _ in llm.calls)
    assert "s1" in llm.calls[1][1][-1].content


async def test_read_reaches_end_and_does_not_evidence_unknown_requirements() -> None:
    page = capture((BlockKind.PARAGRAPH, "Known fact"), (BlockKind.PARAGRAPH, "Other fact"))
    llm = ScriptedLLM(
        [
            {
                "claims": [{"requirement_id": "invented", "text": "Known fact", "cite": {"first": "s0", "last": "s0"}}],
                "answered": False,
            },
            {"claims": [], "answered": False},
        ]
    )
    notes = Notes()
    result = await read(llm, page, "Need more", ["r1"], notes, max_chars=11)
    assert result.coverage == (0, 1) and len(result.facts) == 1
    assert not notes.evidenced("invented") and not notes.evidenced("r1")


class _ReadJev:
    def __init__(self, answers: Mapping[str, Answer], error: JevError | None = None) -> None:
        self.answers = answers
        self.error = error
        self.requests: list[tuple[JsonValue, Mapping[str, Question]]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        self.requests.append((state, questions))
        if self.error is not None:
            raise self.error
        return Evaluation(
            model="test",
            answers=self.answers,
            input_tokens=10,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0001),
        )


def _choice(key: str, confidence: float = 0.95) -> ChoiceAnswer:
    return ChoiceAnswer(choice=key, probabilities={key: confidence}, confidence=confidence)


async def test_short_read_batches_requirements_and_keeps_citations_without_llm() -> None:
    page = capture((BlockKind.HEADING, "httpx 0.28.1"), (BlockKind.PARAGRAPH, "License: BSD"))
    requirements = (
        Requirement(id="version", text="Find the latest httpx version", kind=RequirementKind.INFORMATION),
        Requirement(id="license", text="Find the httpx license", kind=RequirementKind.INFORMATION),
    )
    jev = _ReadJev({"version": _choice("c0", 0.90), "license": _choice("c1")})
    llm, notes, ledger = ScriptedLLM([]), Notes(), Ledger(Limits())
    result = await read(
        llm, page, "Find both", [r.id for r in requirements], notes, jev=jev, requirements=requirements, ledger=ledger
    )
    assert llm.calls == [] and len(jev.requests) == 1
    assert len(result.facts) == 2 and result.coverage == ()
    assert ledger.jev_calls == 1 and ledger.llm_calls == 0
    assert ledger.lines == list(result.cost_lines) and ledger.breakdown().known_dollars == 0.0001
    for requirement, fact, quote in zip(requirements, result.facts, ("httpx 0.28.1", "License: BSD"), strict=True):
        assert notes.evidenced(requirement.id)
        assert fact.text == quote
        assert fact.reader is FactReader.JEV_CHOICE
        assert fact.evidence is not None and fact.evidence == block_evidence(page, fact.evidence.source_id)
    _, questions = jev.requests[0]
    assert questions.keys() == {"version", "license"}
    assert all(
        isinstance(q, ChoiceQuestion) and {"synthesis", "absent"} <= q.criteria.keys() for q in questions.values()
    )
    assert all("untrusted data" in q.instructions for q in questions.values())
    draft = draft_answer(Plan(requirements=requirements, answer_expected=True), notes)
    assert draft is not None and len(draft.claims) == 2
    # The draft states the values, not the requirement each one answers ahead of it, as it once did.
    assert draft.answer == "httpx 0.28.1\n\nLicense: BSD"
    assert {claim.evidence_ids[0] for claim in draft.claims} == notes.evidence.keys()
    assert "License: BSD" in claim_check_questions(draft, notes)["unsupported_1"].instructions


@pytest.mark.parametrize(
    "answer", [_choice("c1", 0.89), _choice("synthesis"), _choice("invented"), NoulAnswer(probability=1), None]
)
async def test_short_read_falls_back_only_for_the_unanswered_requirement(answer: Answer | None) -> None:
    page = capture((BlockKind.PARAGRAPH, "Version: 1.2.3"), (BlockKind.PARAGRAPH, "License: MIT"))
    requirements = (
        Requirement(id="version", text="Find the latest version", kind=RequirementKind.INFORMATION),
        Requirement(id="license", text="Find the license name", kind=RequirementKind.INFORMATION),
    )
    answers: dict[str, Answer] = {"version": _choice("c0")}
    if answer is not None:
        answers["license"] = answer
    jev = _ReadJev(answers)
    llm = ScriptedLLM(
        [
            {
                "claims": [{"requirement_id": "license", "text": "MIT", "cite": {"first": "s1", "last": "s1"}}],
                "answered": True,
            }
        ]
    )
    notes, ledger = Notes(), Ledger(Limits())
    result = await read(
        llm,
        page,
        "Find version and license",
        [r.id for r in requirements],
        notes,
        jev=jev,
        requirements=requirements,
        ledger=ledger,
    )
    assert len(jev.requests) == 1 and len(llm.calls) == 1
    assert all(notes.evidenced(r.id) for r in requirements)
    assert [evidence.quote for evidence in Notes(result.facts).evidence.values()] == ["Version: 1.2.3", "License: MIT"]
    assert [cost.component for cost in result.cost_lines] == [CostComponent.JEV, CostComponent.LLM]
    assert ledger.lines == list(result.cost_lines) and ledger.jev_calls == ledger.llm_calls == 1


async def test_synthesis_has_an_explicit_route_to_the_reader() -> None:
    text = "List the cities"
    page = capture((BlockKind.PARAGRAPH, "Lyon"))
    requirement = Requirement(id="r", text=text, kind=RequirementKind.INFORMATION)
    jev, llm = _ReadJev({"r": _choice("synthesis")}), ScriptedLLM([{"claims": [], "answered": False}])
    await read(llm, page, text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    question = jev.requests[0][1]["r"]
    assert isinstance(question, ChoiceQuestion)
    assert text in question.instructions and "LLM reader" in str(question.criteria["synthesis"])
    assert len(llm.calls) == 1 and f"# Question\n{text}" in llm.calls[0][1][-1].content
    assert f"- r: {text}" in llm.calls[0][1][-1].content


async def test_repeated_scalar_choices_keep_every_source_in_the_citation() -> None:
    page = capture((BlockKind.PARAGRAPH, "Tipping the Velvet"), (BlockKind.HEADING, "Tipping the Velvet"))
    requirement = Requirement(id="title", text="Find the product title", kind=RequirementKind.INFORMATION)
    jev, llm, notes = _ReadJev({"title": _choice("c0")}), ScriptedLLM([]), Notes()
    result = await read(llm, page, requirement.text, ["title"], notes, jev=jev, requirements=(requirement,))
    question = jev.requests[0][1]["title"]
    assert isinstance(question, ChoiceQuestion)
    assert "c1" not in question.criteria
    group = question.criteria["c0"]
    assert isinstance(group, dict)
    sources = group["sources"]
    assert isinstance(sources, list) and all(isinstance(source, dict) for source in sources)
    assert [source["source_id"] for source in sources if isinstance(source, dict)] == ["s0", "s1"]
    assert llm.calls == []
    answer = next(fact for fact in result.facts if fact.requirement_id == "title")
    assert answer.text == "Tipping the Velvet"
    assert {e.source_id for e in notes.supporting_evidence("title")} == {"s0", "s1"}
    composed = assemble_answer([Claim(text=answer.text, evidence_ids=(fact_id(answer),))], notes, (requirement,))
    assert {citation.quote for citation in composed.citations} == {"Tipping the Velvet"}
    assert len(composed.citations) == 2
    rejected = await propose_text_fields_from_notes(
        ScriptedLLM([{"fields": [{"field": "title", "value": answer.text, "source_id": fact_id(answer)}]}]),
        "Find the product title",
        notes,
        {"title": Field(description="Product title")},
    )
    assert rejected == {}
    source = next(key for key, span in notes.evidence.items() if span.source_id == "s1")
    extracted = await propose_text_fields_from_notes(
        ScriptedLLM([{"fields": [{"field": "title", "value": answer.text, "source_id": source}]}]),
        "Find the product title",
        notes,
        {"title": Field(description="Product title")},
    )
    assert extracted["title"][0] == answer.text and extracted["title"][1].source_id == "s1"


def test_headerless_table_context_does_not_invent_a_column_header() -> None:
    page = capture((BlockKind.TABLE, "| Availability | In stock (20 available) |"))
    candidate = next(c for c in read_candidates(page) if c.value == "In stock (20 available)")
    assert "column" not in candidate.context
    assert "Availability" in candidate.context


async def test_repeated_values_in_different_frames_remain_separate_choices() -> None:
    page = capture((BlockKind.HEADING, "Title"), (BlockKind.HEADING, "Title"))
    framed = page.blocks[1].model_copy(update={"frame_id": "child", "source_url": "https://child.test"})
    page = page.model_copy(update={"blocks": (page.blocks[0], framed)})
    requirement = Requirement(id="title", text="Find the embedded frame title", kind=RequirementKind.INFORMATION)
    jev, llm, notes = _ReadJev({"title": _choice("c1")}), ScriptedLLM([]), Notes()
    result = await read(llm, page, requirement.text, ["title"], notes, jev=jev, requirements=(requirement,))
    question = jev.requests[0][1]["title"]
    assert isinstance(question, ChoiceQuestion) and {"c0", "c1"} <= question.criteria.keys()
    assert llm.calls == [] and len(result.facts) == 1
    assert result.facts[0].evidence is not None and result.facts[0].evidence.url == "https://child.test"


async def test_repeated_table_values_keep_the_identity_of_each_record() -> None:
    page = capture((BlockKind.TABLE, "| North | GBP10 |\n| South | GBP10 |"))
    requirement = Requirement(id="price", text="Find South's price", kind=RequirementKind.INFORMATION)
    jev, llm, notes = _ReadJev({"price": _choice("c1")}), ScriptedLLM([]), Notes()
    result = await read(llm, page, requirement.text, ["price"], notes, jev=jev, requirements=(requirement,))
    question = jev.requests[0][1]["price"]
    assert isinstance(question, ChoiceQuestion)
    assert "North" in str(question.criteria["c1"]) and "South" in str(question.criteria["c1"])
    answer = next(fact for fact in result.facts if fact.requirement_id == "price")
    assert answer.evidence is None
    composed = assemble_answer(
        [Claim(text="South costs GBP10", evidence_ids=(fact_id(answer),))], notes, (requirement,)
    )
    assert [citation.quote for citation in composed.citations] == [
        "| North | GBP10",
        "| North | GBP10 |\n| South | GBP10",
    ]


def test_short_read_spans_keep_dates_versions_and_table_context_grounded() -> None:
    page = capture(
        (BlockKind.HEADING, "Package 1.2.3"),
        (BlockKind.PARAGRAPH, "Born on 10 December 1815. Died on 27 November 1852."),
        (BlockKind.TABLE, "| City | Population |\n| --- | --- |\n| Lyon | 522,250 |"),
    )
    candidates = read_candidates(page)
    assert [candidate.value for candidate in candidates[:3]] == [
        "Package 1.2.3",
        "Born on 10 December 1815.",
        "Died on 27 November 1852.",
    ]
    cell = next(candidate for candidate in candidates if candidate.value == "522,250")
    assert "Population" in cell.evidence.quote and "Lyon" in cell.evidence.quote
    assert "column 'Population'" in cell.context
    for candidate in candidates:
        evidence = candidate.evidence
        assert evidence.quote == page.text[evidence.start : evidence.end]


async def test_short_read_overflow_uses_reader_without_truncating_candidates() -> None:
    page = capture(*((BlockKind.PARAGRAPH, f"Candidate {i}") for i in range(MAX_CHOICE_OPTIONS)))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev, llm = _ReadJev({}), ScriptedLLM([{"claims": [], "answered": False}])
    await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    # Only the focus pass is sent; with no passage judged relevant, no truncated choice follows it.
    assert len(jev.requests) == 1 and len(llm.calls) == 1
    assert all(isinstance(question, NoulQuestion) for question in jev.requests[0][1].values())


def _only_focus(requests: Sequence[tuple[JsonValue, Mapping[str, Question]]]) -> bool:
    return bool(requests) and all(isinstance(q, NoulQuestion) for _, questions in requests for q in questions.values())


class _FocusJev:
    """Judges a passage relevant when it holds `keyword`, then answers the choice with the candidate `value`."""

    def __init__(self, keyword: str, value: str | None, *, fail_focus: bool = False) -> None:
        self.keyword, self.value, self.fail_focus = keyword, value, fail_focus
        self.requests: list[tuple[JsonValue, Mapping[str, Question]]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        self.requests.append((state, questions))
        answers: dict[str, Answer] = {}
        for key, question in questions.items():
            if isinstance(question, NoulQuestion):
                if self.fail_focus:
                    raise JevInputTooLarge("too large")
                answers[key] = NoulAnswer(probability=0.9 if self.keyword in question.instructions else 0.05)
            elif isinstance(question, ChoiceQuestion):
                chosen = "absent"
                for option, criterion in question.criteria.items():
                    if isinstance(criterion, dict) and self.value and self.value in str(criterion.get("value")):
                        chosen = option
                answers[key] = _choice(chosen)
        return Evaluation(
            model="test",
            answers=answers,
            input_tokens=10,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0001),
        )


def _long_page() -> Capture:
    filler = [(BlockKind.PARAGRAPH, f"Navigation link {i}") for i in range(MAX_CHOICE_OPTIONS * 2)]
    return capture(*filler[:300], (BlockKind.PARAGRAPH, "Price: $12"), *filler[300:])


async def test_a_long_page_is_focused_and_its_short_fact_read_by_jev() -> None:
    page = _long_page()
    assert read_candidates(page) == ()
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev, llm, ledger = _FocusJev("Price", "Price: $12"), ScriptedLLM([]), Ledger(Limits())
    result = await read(
        llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,), ledger=ledger
    )
    assert llm.calls == [] and [fact.reader for fact in result.facts] == [FactReader.JEV_CHOICE]
    evidence = result.facts[0].evidence
    assert evidence is not None
    assert evidence.quote == page.text[evidence.start : evidence.end] == "Price: $12"
    state = jev.requests[-1][0]
    assert isinstance(state, dict) and isinstance(state["page"], dict)
    assert "Price: $12" in str(state["page"]["text"]) and len(str(state["page"]["text"])) < len(page.text)
    assert ledger.jev_calls == len(jev.requests) == 2


def test_a_record_field_quotes_the_record_it_belongs_to() -> None:
    page = capture((BlockKind.RECORD, "0.1.0\n\nOct 16, 2023\n17 release files"))
    date = next(candidate for candidate in read_candidates(page) if candidate.value == "Oct 16, 2023")
    # The date alone cannot support "0.1.0 was released on Oct 16, 2023" at claim checking.
    assert date.evidence.quote == "0.1.0\n\nOct 16, 2023"


async def test_absent_after_focus_still_reaches_the_reader() -> None:
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev, llm = _FocusJev("Price", None), ScriptedLLM([{"claims": [], "answered": False}] * 4)
    await read(llm, _long_page(), requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    # The evidence may sit in a passage the focus pass set aside, so a narrowed page cannot prove absence.
    assert len(jev.requests) == 2 and llm.calls


async def test_a_page_with_navigation_and_details_uses_one_read() -> None:
    page = capture((BlockKind.PARAGRAPH, "Site navigation.\n" * 1250 + "Answer: 42"))

    class Reader(ScriptedLLM):
        async def generate[T: BaseModel](
            self, purpose: LLMPurpose, messages: Sequence[Message], schema: type[T], **kwargs
        ) -> Generation[T]:
            found = "Answer: 42" in messages[-1].content
            self.responses = [
                {
                    "claims": [
                        {
                            "text": "The answer is 42",
                            "cite": {"first": "s0", "last": "s0"},
                            "excerpt": "Answer: 42",
                            "requirement_id": "r",
                        }
                    ]
                    if found
                    else [],
                    "answered": found,
                }
            ]
            return await super().generate(purpose, messages, schema, **kwargs)

    llm = Reader([])
    notes = Notes()
    await read(llm, page, "What is the answer?", ["r"], notes)
    assert len(llm.calls) == 1
    assert notes.evidenced("r")
    assert notes.facts[0].evidence is not None and notes.facts[0].evidence.quote == "Answer: 42"


async def test_a_large_block_claim_keeps_only_its_literal_supporting_excerpt() -> None:
    text = "Site navigation. " * 100 + "Charger Example: $12, 40W, two USB-C ports." + " Other products. " * 100
    page = capture((BlockKind.PARAGRAPH, text))
    response: JsonValue = {
        "claims": [
            {
                "cite": {"first": "s0", "last": "s0"},
                "excerpt": "Charger Example: $12, 40W, two USB-C ports.",
                "text": "The charger costs $12 and has 40W total power with two USB-C ports.",
                "requirement_id": "r",
            }
        ],
        "answered": True,
    }
    result = await read(ScriptedLLM([response]), page, "Report charger details", ["r"], Notes())
    assert len(result.facts) == 1
    evidence = result.facts[0].evidence
    assert evidence is not None
    assert evidence.quote == page.text[evidence.start : evidence.end] == "Charger Example: $12, 40W, two USB-C ports."
    assert evidence.capture_sha256 == page.sha256


@pytest.mark.parametrize("excerpt", ["Missing price", "Price: $12", "   "])
async def test_an_excerpt_must_match_one_unique_source_passage(excerpt: str) -> None:
    page = capture((BlockKind.PARAGRAPH, "Price: $12. Another record: Price: $12."))
    response: JsonValue = {
        "claims": [
            {"cite": {"first": "s0", "last": "s0"}, "excerpt": excerpt, "text": "Price is $12", "requirement_id": "r"}
        ],
        "answered": True,
    }
    notes = Notes()
    result = await read(ScriptedLLM([response]), page, "Find price", ["r"], notes)
    assert not result.facts and not notes.evidenced("r")
    assert result.rejected_claims == 1


async def test_excerpt_matching_copies_original_punctuation_and_whitespace() -> None:
    passage = "Ada" + chr(0x2019) + "s charger\nPrice: $12"
    page = capture((BlockKind.PARAGRAPH, "Navigation. " + passage + " Footer."))
    response: JsonValue = {
        "claims": [
            {
                "cite": {"first": "s0", "last": "s0"},
                "excerpt": "Ada's charger Price: $12",
                "text": "Ada's charger costs $12",
                "requirement_id": "r",
            }
        ],
        "answered": True,
    }
    result = await read(ScriptedLLM([response]), page, "Find price", ["r"], Notes())
    evidence = result.facts[0].evidence
    assert evidence is not None
    assert evidence.quote == passage == page.text[evidence.start : evidence.end]


async def test_excerpt_cannot_reach_a_part_of_the_block_not_shown_in_this_chunk() -> None:
    page = capture((BlockKind.PARAGRAPH, "Unrelated. " * 100 + "Price: $12"))
    response: JsonValue = {
        "claims": [
            {
                "cite": {"first": "s0", "last": "s0"},
                "excerpt": "Price: $12",
                "text": "Price is $12",
                "requirement_id": "r",
            }
        ],
        "answered": True,
    }
    result = await read(
        ScriptedLLM([response, {"claims": [], "answered": False}]), page, "Find price", ["r"], Notes(), max_chars=1000
    )
    assert not result.facts and result.rejected_claims == 1


async def test_a_failed_focus_pass_falls_back_to_the_reader() -> None:
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev = _FocusJev("Price", "Price: $12", fail_focus=True)
    llm = ScriptedLLM([{"claims": [], "answered": False}] * 4)
    await read(llm, _long_page(), requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    assert all(isinstance(q, NoulQuestion) for _, questions in jev.requests for q in questions.values())
    assert llm.calls


async def test_relevant_passages_too_many_to_offer_go_to_the_reader() -> None:
    # Every row bears on the price, so keeping only those that fit could offer an outdated one alone.
    page = capture(*((BlockKind.PARAGRAPH, f"Price in {year}: ${year - 1900}") for year in range(1700, 2026)))
    requirement = Requirement(id="r", text="Find the current price", kind=RequirementKind.INFORMATION)
    jev, llm = _FocusJev("Price", "Price in 1700"), ScriptedLLM([{"claims": [], "answered": False}] * 4)
    await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    assert _only_focus(jev.requests) and llm.calls


async def test_a_long_block_is_judged_on_all_of_its_text() -> None:
    passage = "Older builds of this package are listed in the archive below, sorted by their date. " * 18 + "Price: $12"
    page = capture(*((BlockKind.PARAGRAPH, f"Navigation link {i}") for i in range(300)), (BlockKind.PARAGRAPH, passage))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev, llm = _FocusJev("Price", "Price: $12"), ScriptedLLM([])
    result = await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    # The price sits past the first window's worth of the block; scoring its opening alone would drop it.
    assert llm.calls == [] and [fact.reader for fact in result.facts] == [FactReader.JEV_CHOICE]


async def test_a_focus_pass_the_spend_limit_cannot_cover_sends_nothing() -> None:
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev, ledger = _FocusJev("Price", "Price: $12"), Ledger(Limits(max_dollars=1e-9))
    with pytest.raises(BudgetExceeded):
        await read(
            ScriptedLLM([]),
            _long_page(),
            requirement.text,
            ["r"],
            Notes(),
            jev=jev,
            requirements=(requirement,),
            ledger=ledger,
        )
    assert jev.requests == []


async def test_short_read_reserves_jev_budget_before_calling() -> None:
    page = capture((BlockKind.PARAGRAPH, "Price: $12"))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev, llm, ledger = _ReadJev({}), ScriptedLLM([]), Ledger(Limits(max_jev_calls=1))
    ledger.reserve(CostComponent.JEV)
    with pytest.raises(BudgetExceeded):
        await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,), ledger=ledger)
    assert jev.requests == [] and llm.calls == []


@pytest.mark.parametrize("error", [JevInputTooLarge("too large"), JevError("invalid answer")])
async def test_rejected_short_read_still_uses_reader_and_counts_jev_call(error: JevError) -> None:
    page = capture((BlockKind.PARAGRAPH, "Price: $12"))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev = _ReadJev({}, error)
    llm, ledger = ScriptedLLM([{"claims": [], "answered": False}]), Ledger(Limits())
    await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,), ledger=ledger)
    assert len(jev.requests) == len(llm.calls) == ledger.jev_calls == 1


@pytest.mark.parametrize("repetitions", [1, 300])
async def test_choice_sees_unoffered_passages_or_defers_to_chunked_reader(repetitions: int) -> None:
    passage = "This newer release has a long description " * 10 * repetitions
    page = capture((BlockKind.PARAGRAPH, "Old version: 1.0"), (BlockKind.PARAGRAPH, passage))
    requirement = Requirement(id="r", text="Find the latest version", kind=RequirementKind.INFORMATION)
    jev = _ReadJev({"r": _choice("synthesis")})
    response: JsonValue = {"claims": [], "answered": False}
    parts = len(chunk(page, 12_000))
    llm = ScriptedLLM([response] * parts)
    await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,), max_chars=12_000)
    if repetitions == 1:
        assert passage in str(jev.requests[0][0])
    else:
        assert _only_focus(jev.requests) and len(llm.calls) == parts > 2


class Fields(Frozen):
    label: str
    count: int = Field(ge=1000)
    amount: Decimal
    weight: float
    when: date
    available: bool
    records: list[str]


@pytest.mark.parametrize(
    "name,text,expected",
    [
        ("count", "Stock: 1,234 units", 1234),
        ("amount", "Price: $1,234.50 today", Decimal("1234.50")),
        ("weight", "Weight: 12.25 kg", 12.25),
        ("when", "Due on 2026-09-17", date(2026, 9, 17)),
        ("available", "Available: YES", True),
    ],
)
def test_field_copy_is_typed_and_keeps_verbatim_evidence(name: str, text: str, expected: object) -> None:
    page = capture((BlockKind.PARAGRAPH, text))
    field = Fields.model_fields[name]
    candidates = field_candidates(page, field)
    assert not isinstance(candidates, UnsupportedField)
    assert len(candidates) == 1
    question = field_question(field, candidates)
    candidate = candidates[0]
    answer = ChoiceAnswer(choice=candidate.id, probabilities={candidate.id: 1, "none": 0}, confidence=1)
    copied = copy_field(answer, candidates)
    assert copied is not None and copied[0] == expected
    assert type(copied[0]) is type(expected)
    assert copied[1].quote == page.text[copied[1].start : copied[1].end]
    assert question.criteria[candidate.id] == {"source_id": "s0", "quote": copied[1].quote, "context": text}
    assert "none" in question.criteria
    assert copy_field(answer.model_copy(update={"choice": "none"}), candidates) is None
    assert copy_field(answer.model_copy(update={"choice": "invented"}), candidates) is None


def test_field_constraints_and_explicit_unsupported_records() -> None:
    page = capture((BlockKind.PARAGRAPH, "Stock 5 or 1,500.5"))
    assert field_candidates(page, Fields.model_fields["count"]) == ()
    assert isinstance(field_candidates(page, Fields.model_fields["records"]), UnsupportedField)
    assert field_candidates(capture((BlockKind.PARAGRAPH, "2026-02-30")), Fields.model_fields["when"]) == ()


async def test_composer_maps_short_references_without_accepting_truncated_ids() -> None:
    page = capture((BlockKind.PARAGRAPH, "Price is $12"))
    evidence = block_evidence(page, "s0")
    notes = Notes((Fact(reader=FactReader.LLM, text=evidence.quote, evidence=evidence),))
    key = evidence_id(evidence)
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "It is $12. [e0]", "evidence_ids": ["e0"]},
                    {"text": "Invented.", "evidence_ids": [key.rsplit(":", 1)[-1]]},
                    {"text": "Absent.", "evidence_ids": ["e1"]},
                ]
            }
        ]
    )
    result = await compose(
        llm, "Find price", Plan(requirements=(), answer_expected=True), notes, transaction_evidence_ids=(key,)
    )
    prompt = llm.calls[0][1][-1].content
    assert "[e0]" in prompt and key not in prompt
    assert "action: e0." in prompt
    assert result.data.answer == "It is $12."
    assert result.data.claims[0].evidence_ids == (key,)
    assert result.data.dropped_claims == 2


async def test_compose_drops_uncited_and_unknown_claims_including_answer_text(caplog: pytest.LogCaptureFixture) -> None:
    page = capture((BlockKind.PARAGRAPH, "Price is $12"))
    evidence = block_evidence(page, "s0")
    notes = Notes((Fact(reader=FactReader.LLM, requirement_id="r1", text="Price is $12", evidence=evidence),))
    key = evidence_id(evidence)
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "It is $12. [99](https://invented.test)", "evidence_ids": [key, key]},
                    {"text": "Shipping is free.", "evidence_ids": []},
                    {"text": "It arrives tomorrow.", "evidence_ids": ["invented"]},
                ],
            }
        ]
    )
    plan = Plan(
        requirements=(
            Requirement(id="r1", text="Find price", kind=RequirementKind.INFORMATION),
            Requirement(id="r2", text="Find shipping", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=True,
    )
    result = await compose(llm, "Find price and shipping", plan, notes, tokens=TokenBudget(compose_output_tokens=4096))
    assert llm.output_caps == [4096]
    assert result.data.answer == "It is $12."
    assert result.data.linked_answer == "It is $12. [1](<https://example.test#:~:text=Price%20is%20%2412>)"
    assert len(result.data.citations) == 1
    citation = result.data.citations[0]
    assert (citation.id, citation.text, citation.requirement_id) == (1, "Price is $12", "r1")
    assert (citation.url, citation.quote) == (evidence.url, evidence.quote)
    assert "invented" in caplog.text and "99" in caplog.text
    assert all(record.levelname == "WARNING" for record in caplog.records)
    assert result.data.dropped_claims == 2 and len(result.data.claims) == 1
    assert result.cost.dollars == 0.001
    questions = claim_check_questions(result.data, notes)
    assert set(questions) == {"unsupported_0", "contradicted_0", "requirement_omitted"}
    assert "Price is $12" in questions["unsupported_0"].instructions
    assert "Find shipping" in questions["requirement_omitted"].instructions
    assert all(question.true is not None and question.true.startswith("Yes,") for question in questions.values())


async def test_composer_cannot_cite_a_note_omitted_from_its_input(caplog: pytest.LogCaptureFixture) -> None:
    page = capture((BlockKind.PARAGRAPH, "Price is $12"), (BlockKind.PARAGRAPH, "Shipping is free"))
    notes = Notes()
    for index, text in enumerate(("Price is $12", "Unused context " * 1000)):
        evidence = block_evidence(page, f"s{index}")
        notes.add(Fact(reader=FactReader.LLM, text=text, evidence=evidence))
    key, omitted = tuple(notes.evidence)
    llm = ScriptedLLM(
        [{"claims": [{"text": "It is $12.", "evidence_ids": [key]}, {"text": "Free", "evidence_ids": [omitted]}]}]
    )
    result = await compose(
        llm,
        "Find the price",
        Plan(requirements=(), answer_expected=True),
        notes,
        tokens=TokenBudget(state_plus_largest_question=2000),
    )
    assert "[e0]" in llm.calls[0][1][-1].content and "[e1]" not in llm.calls[0][1][-1].content
    assert result.data.dropped_claims == 1
    assert len(result.data.citations) == 1 and result.data.citations[0].quote == "Price is $12"
    assert omitted in caplog.text


async def test_compose_cannot_return_uncited_free_text_without_claims() -> None:
    llm = ScriptedLLM([{"claims": []}])
    result = await compose(llm, "Do it", Plan(requirements=(), answer_expected=True), Notes())
    assert result.data.answer == "" and result.data.claims == ()


async def test_committed_action_evidence_is_named_to_the_composer_and_contradiction_check() -> None:
    listing = capture((BlockKind.PARAGRAPH, "Black pen £7"))
    checkout = capture((BlockKind.PARAGRAPH, "Red pen £11.55"))
    notes = Notes(
        Fact(reader=FactReader.LLM, text=page.text, evidence=block_evidence(page, "s0")) for page in (listing, checkout)
    )
    listed, committed = tuple(notes.evidence)
    answer = assemble_answer((Claim(text="Bought a black pen for £7", evidence_ids=(listed,)),), notes, ())
    ordinary = claim_check_questions(answer, notes)
    checked = transaction_check_question(answer, notes, (committed,))

    assert checked is not None
    assert checkout.text in checked.instructions
    assert listing.text not in checked.instructions
    assert TRANSACTION_CONTRADICTED not in ordinary
    llm = ScriptedLLM([{"claims": []}])
    await compose(
        llm,
        "Buy a pen",
        Plan(requirements=(), answer_expected=True),
        notes,
        transaction_evidence_ids=(committed,),
    )
    prompt = "\n".join(message.content for message in llm.calls[0][1])
    transaction = prompt.split("# Transaction evidence\n", 1)[1].split("# Notes", 1)[0]
    assert "action: e1." in transaction and "e0" not in transaction
    assert "cites" in transaction and "committed" in transaction


def test_transaction_evidence_keeps_the_latest_pages_within_its_own_budget() -> None:
    page = capture(
        (BlockKind.PARAGRAPH, "Black pen £7"),
        (BlockKind.PARAGRAPH, "Basket\n" * 1000),
        (BlockKind.PARAGRAPH, "Order placed: red pen £11.55"),
    )
    notes = Notes(
        Fact(reader=FactReader.LLM, text=block.source_id, evidence=block_evidence(page, block.source_id))
        for block in page.blocks
    )
    listed, basket, placed = tuple(notes.evidence)
    answer = assemble_answer((Claim(text="Bought a black pen for £7", evidence_ids=(listed,)),), notes, ())
    question = transaction_check_question(
        answer, notes, (basket, placed), tokens=TokenBudget(state_plus_largest_question=3000)
    )
    assert question is not None
    instructions = question.instructions
    assert "Order placed" in instructions and "Basket" not in instructions
    assert (
        transaction_check_question(answer, notes, (placed,), tokens=TokenBudget(state_plus_largest_question=1)) is None
    )
    assert transaction_check_question(answer, notes, ()) is None


def test_a_receipt_larger_than_the_budget_is_cut_to_fit_not_dropped() -> None:
    page = capture((BlockKind.PARAGRAPH, "Black pen £7"), (BlockKind.PARAGRAPH, "Order placed: red pen " + "x" * 20000))
    notes = Notes(
        Fact(reader=FactReader.LLM, text=block.source_id, evidence=block_evidence(page, block.source_id))
        for block in page.blocks
    )
    listed, placed = tuple(notes.evidence)
    answer = assemble_answer((Claim(text="Bought a black pen for £7", evidence_ids=(listed,)),), notes, ())
    tokens = TokenBudget(state_plus_largest_question=3000)
    question = transaction_check_question(answer, notes, (placed,), tokens=tokens)
    assert question is not None
    assert "Order placed: red pen" in question.instructions
    assert tokens.remaining_chars(json.dumps({"answer": answer.answer}), [question.model_dump_json()]) >= 0


async def test_transaction_evidence_does_not_spend_the_omission_notes_budget() -> None:
    from fastbrowse.verification import check_claims
    from tests.test_policy import ScriptedJev

    page = capture(*((BlockKind.PARAGRAPH, f"Item {index}: " + "x" * 510) for index in range(40)))
    notes = Notes(
        Fact(
            reader=FactReader.LLM,
            requirement_id="r1",
            text=page.text[block.start : block.end],
            evidence=block_evidence(page, block.source_id),
        )
        for block in page.blocks
    )
    ids = tuple(notes.evidence)
    requirement = Requirement(id="r1", text="What was bought?", kind=RequirementKind.INFORMATION)
    answer = assemble_answer((Claim(text="Bought the items", evidence_ids=(ids[0],)),), notes, (requirement,))
    ordinary = claim_check_questions(answer, notes)
    jev = ScriptedJev({}, noul=0.0)
    ledger = Ledger(Limits())

    assert await check_claims(jev, answer, notes, Thresholds(), ledger=ledger, transaction_evidence_ids=ids) == answer

    assert len(jev.requests) == ledger.jev_calls == len(ledger.lines) == 3
    claims = {
        key: question for batch in jev.requests for key, question in batch.items() if key != TRANSACTION_CONTRADICTED
    }
    assert claims == ordinary
    omission = claims["requirement_omitted"].instructions
    assert all(fact.text in omission for fact in notes.facts)
    assert any(set(q) == {TRANSACTION_CONTRADICTED} for q in jev.requests)


def test_currency_sentence_punctuation_and_candidate_context() -> None:
    page = capture((BlockKind.PARAGRAPH, "Revenue: $1,234.50. Costs: $200.00."))
    field = Fields.model_fields["amount"]
    candidates = field_candidates(page, field)
    assert not isinstance(candidates, UnsupportedField)
    assert [candidate.value for candidate in candidates] == [Decimal("1234.50"), Decimal("200.00")]
    assert [candidate.evidence.quote for candidate in candidates] == ["$1,234.50", "$200.00"]
    question = field_question(field, candidates, name="revenue")
    assert "revenue" in question.instructions
    assert question.criteria[candidates[0].id] == {
        "source_id": "s0",
        "quote": "$1,234.50",
        "context": page.text,
    }


async def test_text_fields_are_kept_only_when_their_block_holds_the_value_verbatim() -> None:
    page = capture((BlockKind.HEADING, "httpx 0.28.1"), (BlockKind.PARAGRAPH, "License: BSD"))
    llm = ScriptedLLM(
        [
            {
                "fields": [
                    {"field": "label", "value": "0.28.1", "source_id": "s0"},
                    {"field": "license", "value": "MIT", "source_id": "s1"},
                    {"field": "owner", "value": "encode", "source_id": "s1"},
                ]
            }
        ]
    )
    fields = {name: Fields.model_fields["label"] for name in ("label", "license", "owner")}
    found, _ = await propose_text_fields(llm, "Get the version", page, fields)
    assert found.keys() == {"label"}
    value, evidence = found["label"]
    assert value == "0.28.1" and evidence.quote == "httpx 0.28.1"


# A page writes its own punctuation; a model quoting it does not. These are the shapes seen in the wild.
_APOSTROPHE, _EN_DASH, _ELLIPSIS_CHAR = chr(0x2019), chr(0x2013), chr(0x2026)
_HEADLINE = "AX " + _EN_DASH + " Google" + _APOSTROPHE + "s Open Agentic Orchestrator"


@pytest.mark.parametrize(
    "typed",
    [
        pytest.param(_HEADLINE, id="verbatim"),
        pytest.param(_HEADLINE.replace(_APOSTROPHE, "'"), id="straight-apostrophe"),
        pytest.param(_HEADLINE.replace(_EN_DASH, "-"), id="hyphen-for-en-dash"),
        pytest.param(_HEADLINE.replace(_APOSTROPHE, "'").replace(_EN_DASH, "-"), id="both-normalized"),
    ],
)
async def test_a_text_field_survives_the_punctuation_shape_a_model_rewrites(typed: str) -> None:
    page = capture((BlockKind.HEADING, _HEADLINE))
    llm = ScriptedLLM([{"fields": [{"field": "label", "value": typed, "source_id": "s0"}]}])
    found, _ = await propose_text_fields(llm, "Get the title", page, {"label": Fields.model_fields["label"]})
    assert found.keys() == {"label"}
    # The page's own bytes are the value and the evidence however the model typed it: a model's shape never is.
    assert found["label"] == (_HEADLINE, block_evidence(page, "s0"))


async def test_a_text_field_in_a_table_cell_survives_the_backslash_the_capture_adds_to_a_pipe() -> None:
    page = capture((BlockKind.TABLE, r"| Mystery \| 12.99 |"))
    llm = ScriptedLLM([{"fields": [{"field": "label", "value": "Mystery | 12.99", "source_id": "s0"}]}])
    found, _ = await propose_text_fields(llm, "Get the row", page, {"label": Fields.model_fields["label"]})
    assert found["label"][0] == "Mystery | 12.99"


@pytest.mark.parametrize(
    ("written", "typed"),
    [
        pytest.param("Read more" + _ELLIPSIS_CHAR, "Read more...", id="page-ellipsis-model-dots"),
        pytest.param("Read more...", "Read more" + _ELLIPSIS_CHAR, id="page-dots-model-ellipsis"),
        pytest.param(chr(0x201C) + "quoted" + chr(0x201D), '"quoted"', id="curly-double-quotes"),
    ],
)
async def test_a_text_field_matches_across_a_punctuation_family_in_either_direction(written: str, typed: str) -> None:
    page = capture((BlockKind.PARAGRAPH, written))
    llm = ScriptedLLM([{"fields": [{"field": "label", "value": typed, "source_id": "s0"}]}])
    found, _ = await propose_text_fields(llm, "Get the text", page, {"label": Fields.model_fields["label"]})
    assert found.keys() == {"label"}


@pytest.mark.parametrize(
    "typed",
    [
        pytest.param("Bing " + _EN_DASH + " Google" + _APOSTROPHE + "s Open Agentic Orchestrator", id="other-words"),
        pytest.param("AX, Google" + _APOSTROPHE + "s Open Agentic Orchestrator", id="comma-where-page-has-a-dash"),
        pytest.param("AX " + _EN_DASH + " Googles Open Agentic Orchestrator", id="letter-where-page-has-an-apostrophe"),
    ],
)
async def test_tolerating_punctuation_does_not_widen_a_text_field_past_its_shape(typed: str) -> None:
    page = capture((BlockKind.HEADING, _HEADLINE))
    llm = ScriptedLLM([{"fields": [{"field": "label", "value": typed, "source_id": "s0"}]}])
    found, _ = await propose_text_fields(llm, "Get the title", page, {"label": Fields.model_fields["label"]})
    assert found == {}


async def test_a_text_field_is_not_taken_from_a_block_other_than_the_one_it_cites() -> None:
    page = capture((BlockKind.HEADING, _HEADLINE), (BlockKind.PARAGRAPH, "Unrelated"))
    llm = ScriptedLLM([{"fields": [{"field": "label", "value": _HEADLINE, "source_id": "s1"}]}])
    found, _ = await propose_text_fields(llm, "Get the title", page, {"label": Fields.model_fields["label"]})
    assert found == {}


async def test_a_text_field_off_the_final_page_is_taken_from_a_note_that_quotes_it() -> None:
    earlier = capture((BlockKind.PARAGRAPH, "requests 2.33.0 released May 14, 2026"))
    quote = block_evidence(earlier, "s0")
    notes = Notes((Fact(reader=FactReader.LLM, text="requests was released on May 14, 2026", evidence=quote),))
    key = next(iter(notes.evidence))
    llm = ScriptedLLM(
        [
            {
                "fields": [
                    {"field": "label", "value": "requests", "source_id": key},
                    {"field": "license", "value": "httpx", "source_id": key},
                ]
            }
        ]
    )
    fields = {name: Fields.model_fields["label"] for name in ("label", "license")}
    found = await propose_text_fields_from_notes(llm, "Which is newer?", notes, fields)
    # A value the cited quote does not contain is not taken, whatever the model says.
    assert found == {"label": ("requests", quote)}


async def test_a_text_field_from_a_note_keeps_the_notes_punctuation_not_the_models() -> None:
    earlier = capture((BlockKind.HEADING, _HEADLINE))
    quote = block_evidence(earlier, "s0")
    notes = Notes((Fact(reader=FactReader.LLM, text=_HEADLINE, evidence=quote),))
    typed = _HEADLINE.replace(_APOSTROPHE, "'").replace(_EN_DASH, "-")
    proposal: dict[str, JsonValue] = {"field": "label", "value": typed, "source_id": next(iter(notes.evidence))}
    fields = {"label": Fields.model_fields["label"]}
    found = await propose_text_fields_from_notes(ScriptedLLM([{"fields": [proposal]}]), "Title?", notes, fields)
    assert found == {"label": (_HEADLINE, quote)}


async def test_text_fields_from_notes_drop_a_superseded_quote() -> None:
    """A corrected value stays as context but a citation of it does not carry into the extracted text."""
    page = capture((BlockKind.PARAGRAPH, "Version: old"))
    corrected = capture((BlockKind.PARAGRAPH, "Version: new"))
    old = Fact(requirement_id="r", reader=FactReader.LLM, text="Version: old", evidence=block_evidence(page, "s0"))
    new = Fact(requirement_id="r", reader=FactReader.LLM, text="Version: new", evidence=block_evidence(corrected, "s0"))
    notes = Notes((old, new))
    notes.supersede("r", corrected.url, corrected.sha256, corrected.text)
    stale = next(iter(notes.evidence))
    proposal: dict[str, JsonValue] = {"field": "label", "value": "old", "source_id": stale}
    fields = {"label": Fields.model_fields["label"]}
    assert not await propose_text_fields_from_notes(ScriptedLLM([{"fields": [proposal]}]), "Version?", notes, fields)


async def test_a_name_the_task_gives_can_be_chosen_on_a_note_that_quotes_only_a_date() -> None:
    earlier = capture((BlockKind.PARAGRAPH, "May 14, 2026"))
    quote = block_evidence(earlier, "s0")
    notes = Notes((Fact(reader=FactReader.LLM, text="requests: May 14, 2026", evidence=quote),))
    key = next(iter(notes.evidence))
    proposal: dict[str, JsonValue] = {"field": "label", "value": "requests", "source_id": key}
    fields = {"label": Fields.model_fields["label"]}
    task = "Which has the more recent release, httpx or requests?"
    assert await propose_text_fields_from_notes(ScriptedLLM([{"fields": [proposal]}]), task, notes, fields) == {
        "label": ("requests", quote)
    }
    # Part of a word the task uses is not a name it gives.
    partial: dict[str, JsonValue] = {**proposal, "value": "request"}
    assert not await propose_text_fields_from_notes(ScriptedLLM([{"fields": [partial]}]), task, notes, fields)


async def test_a_name_chosen_on_a_derived_comparison_is_evidenced_by_the_record_read_for_it() -> None:
    page = capture((BlockKind.PARAGRAPH, "Dec 6, 2024"), (BlockKind.PARAGRAPH, "May 14, 2026"))
    records = [
        Fact(reader=FactReader.LLM, text=f"{name}: {date}", evidence=block_evidence(page, source))
        for name, source, date in (("httpx", "s0", "Dec 6, 2024"), ("requests", "s1", "May 14, 2026"))
    ]
    notes = Notes(records)
    winner = Fact(reader=FactReader.LLM, text="requests is newer", evidence=None, basis=tuple(notes.evidence))
    notes.add(winner)
    proposal: dict[str, JsonValue] = {"field": "label", "value": "requests", "source_id": fact_id(winner)}
    fields = {"label": Fields.model_fields["label"]}
    task = "Which has the more recent release, httpx or requests?"
    found = await propose_text_fields_from_notes(ScriptedLLM([{"fields": [proposal]}]), task, notes, fields)
    assert found == {"label": ("requests", records[1].evidence)}
    # A value the task does not name is not taken from a conclusion that quotes nothing.
    invented: dict[str, JsonValue] = {**proposal, "value": "urllib3"}
    assert not await propose_text_fields_from_notes(ScriptedLLM([{"fields": [invented]}]), task, notes, fields)


@pytest.mark.parametrize("limit", ["calls", "dollars"])
@pytest.mark.parametrize("reader", ["read", "fields"])
async def test_each_chunk_reserves_budget_before_request(limit: str, reader: str) -> None:
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger

    page = capture((BlockKind.PARAGRAPH, "First chunk"), (BlockKind.PARAGRAPH, "Second chunk"))
    response: JsonValue = {"claims": [], "answered": False} if reader == "read" else {"fields": []}
    llm = ScriptedLLM([response, response])
    ledger = Ledger(Limits(max_llm_calls=1) if limit == "calls" else Limits(max_dollars=0.001))
    with pytest.raises(BudgetExceeded):
        if reader == "read":
            await read(llm, page, "Find it", (), Notes(), max_chars=12, ledger=ledger)
        else:
            await propose_text_fields(
                llm, "Find it", page, {"label": Fields.model_fields["label"]}, max_chars=12, ledger=ledger
            )
    assert len(llm.calls) == 1
    assert ledger.llm_calls == 1 and ledger.breakdown().known_dollars == 0.001


async def test_extraction_reserves_each_scalar_field() -> None:
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger
    from fastbrowse.verification import extract
    from tests.test_policy import ScriptedJev

    class Record(Frozen):
        first: int
        second: int

    page = capture((BlockKind.PARAGRAPH, "First: 10. Second: 20."))
    jev = ScriptedJev({})
    ledger = Ledger(Limits(max_jev_calls=1))
    with pytest.raises(BudgetExceeded):
        await extract(jev, ScriptedLLM([]), "Get both fields", page, Record, ledger=ledger)
    assert len(jev.requests) == 1 and ledger.jev_calls == 1


async def test_extract_takes_a_scalar_quoted_on_an_earlier_page_from_notes() -> None:
    """A sorted listing's later page holds only the pricier side: the winner's price is in the notes, not the
    capture, and asked only of the capture it came back as no data at all."""
    from fastbrowse.verification import extract
    from tests.test_policy import ScriptedJev

    class Product(Frozen):
        name: str
        price: float

    earlier = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 4.60"), url="https://example.test/search?page=1")
    final = capture(
        (BlockKind.LIST_ITEM, "Chrome Toaster - GBP 41.00"),
        (BlockKind.LIST_ITEM, "Burr Grinder - GBP 89.99"),
        url="https://example.test/search?page=2",
    )
    evidence = block_evidence(earlier, "s0")
    fact = Fact(reader=FactReader.LLM, text="Paper Filters - GBP 4.60", evidence=evidence)
    notes = Notes((fact,))
    llm = ScriptedLLM([{"fields": [{"field": "name", "value": "Paper Filters", "source_id": fact_id(fact)}]}])
    # The notes candidate follows the two current-page ones; without them the winning 4.60 is not offered.
    jev = ScriptedJev({"price": "c2"})
    result = await extract(
        jev, llm, "Which product is the cheapest? Give its name and price.", final, Product, notes=notes
    )
    assert result.problem is None
    assert result.data == {"name": "Paper Filters", "price": 4.6}
    # The winner keeps the earlier page's own address, hash and offsets, not the final capture's.
    quoted = next(item for item in result.evidence if item.quote == "4.60")
    start = earlier.text.index("4.60")
    assert (quoted.url, quoted.capture_sha256) == (earlier.url, earlier.sha256)
    assert (quoted.start, quoted.end) == (start, start + 4)
    assert quoted.quote == earlier.text[start : start + 4]


def test_scalar_candidates_from_notes_drop_a_superseded_quote() -> None:
    """A price the final page corrected stays as context but is not offered as a current value."""
    page = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 4.60"))
    corrected = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 5.20"))
    old = Fact(
        requirement_id="r", reader=FactReader.LLM, text="Paper Filters - GBP 4.60", evidence=block_evidence(page, "s0")
    )
    new = Fact(
        requirement_id="r",
        reader=FactReader.LLM,
        text="Paper Filters - GBP 5.20",
        evidence=block_evidence(corrected, "s0"),
    )
    notes = Notes((old, new))
    notes.supersede("r", corrected.url, corrected.sha256, corrected.text)
    candidates = field_candidates_from_notes(notes, Fields.model_fields["weight"])
    assert not isinstance(candidates, UnsupportedField)
    assert [candidate.value for candidate in candidates] == [5.20]


def test_scalar_candidates_from_notes_drop_a_changed_quote_on_the_final_address() -> None:
    """A context quote the final capture of its own address no longer shows is not a current value."""
    old_page = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 4.60"))
    final = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 5.20"))
    context = Fact(reader=FactReader.LLM, text="Paper Filters - GBP 4.60", evidence=block_evidence(old_page, "s0"))
    candidates = field_candidates_from_notes(Notes((context,)), Fields.model_fields["weight"], capture=final)
    assert not isinstance(candidates, UnsupportedField)
    assert candidates == ()


def test_scalar_candidates_from_notes_keep_an_earlier_pages_date() -> None:
    """A different page's timestamp is that page's evidence and is not dropped for the final page's date."""
    earlier = capture((BlockKind.PARAGRAPH, "Due on 2026-09-17"), url="https://example.test/search?page=1")
    final = capture((BlockKind.PARAGRAPH, "Due on 2026-10-01"), url="https://example.test/search?page=2")
    fact = Fact(reader=FactReader.LLM, text="Due on 2026-09-17", evidence=block_evidence(earlier, "s0"))
    candidates = field_candidates_from_notes(Notes((fact,)), Fields.model_fields["when"], capture=final)
    assert not isinstance(candidates, UnsupportedField)
    assert [candidate.value for candidate in candidates] == [date(2026, 9, 17)]


def test_scalar_candidates_from_notes_keep_an_earlier_page_and_its_provenance() -> None:
    """A different page's quote is not contradicted by the final page and keeps its own hash and offsets."""
    earlier = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 4.60"), url="https://example.test/search?page=1")
    final = capture((BlockKind.LIST_ITEM, "Chrome Toaster - GBP 41.00"), url="https://example.test/search?page=2")
    fact = Fact(reader=FactReader.LLM, text="Paper Filters - GBP 4.60", evidence=block_evidence(earlier, "s0"))
    candidates = field_candidates_from_notes(Notes((fact,)), Fields.model_fields["weight"], capture=final)
    assert not isinstance(candidates, UnsupportedField)
    assert [(candidate.value, candidate.evidence.url) for candidate in candidates] == [(4.60, earlier.url)]
    quoted = candidates[0].evidence
    start = earlier.text.index("4.60")
    assert quoted.capture_sha256 == earlier.sha256
    assert (quoted.start, quoted.end) == (start, start + 4)


async def test_extract_selects_across_a_candidate_pool_over_jevs_option_cap() -> None:
    """A pool wider than Jev's ceiling is grouped, not truncated: the last earlier-page value can still win."""
    from fastbrowse.verification import extract
    from tests.test_policy import ScriptedJev

    class Price(Frozen):
        price: float

    count = MAX_CHOICE_OPTIONS + 1
    earlier = capture(
        *((BlockKind.LIST_ITEM, f"Weight: {index}.25 kg") for index in range(count)),
        url="https://example.test/search?page=1",
    )
    final = capture((BlockKind.PARAGRAPH, "No weights here"), url="https://example.test/search?page=2")
    notes = Notes(
        tuple(
            Fact(reader=FactReader.LLM, text=f"Weight: {index}.25 kg", evidence=block_evidence(earlier, f"s{index}"))
            for index in range(count)
        )
    )
    winner = count - 1
    jev = ScriptedJev({"price_group": str(winner // 30), "price": f"c{winner}"})
    result = await extract(jev, ScriptedLLM([]), "What is the last weight?", final, Price, notes=notes)
    assert result.problem is None
    assert result.data == {"price": winner + 0.25}
    assert len(jev.requests) == 2
    assert next(item for item in result.evidence if item.quote == f"{winner}.25").url == earlier.url


async def test_extract_group_selection_reserves_each_jev_call() -> None:
    """Grouping asks twice, so each Jev call reserves and the second is refused once the call limit is spent."""
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger
    from fastbrowse.verification import extract
    from tests.test_policy import ScriptedJev

    class Price(Frozen):
        price: float

    count = MAX_CHOICE_OPTIONS + 1
    earlier = capture(
        *((BlockKind.LIST_ITEM, f"Weight: {index}.25 kg") for index in range(count)),
        url="https://example.test/search?page=1",
    )
    final = capture((BlockKind.PARAGRAPH, "No weights here"), url="https://example.test/search?page=2")
    notes = Notes(
        tuple(
            Fact(reader=FactReader.LLM, text=f"Weight: {index}.25 kg", evidence=block_evidence(earlier, f"s{index}"))
            for index in range(count)
        )
    )
    jev = ScriptedJev({})
    ledger = Ledger(Limits(max_jev_calls=1))
    with pytest.raises(BudgetExceeded):
        await extract(jev, ScriptedLLM([]), "What is the last weight?", final, Price, notes=notes, ledger=ledger)
    assert len(jev.requests) == 1 and ledger.jev_calls == 1


@pytest.mark.parametrize(
    "name,text,expected",
    [
        ("count", "Stock: 1,234 units", 1234),
        ("amount", "Price: $1,234.50 today", Decimal("1234.50")),
        ("weight", "Weight: 12.25 kg", 12.25),
        ("when", "Due on 2026-09-17", date(2026, 9, 17)),
        ("available", "Available: YES", True),
    ],
)
def test_scalar_candidates_from_notes_are_typed_and_keep_verbatim_evidence(
    name: str, text: str, expected: object
) -> None:
    page = capture((BlockKind.PARAGRAPH, text))
    notes = Notes((Fact(reader=FactReader.LLM, text=text, evidence=block_evidence(page, "s0")),))
    candidates = field_candidates_from_notes(notes, Fields.model_fields[name])
    assert not isinstance(candidates, UnsupportedField)
    assert len(candidates) == 1
    assert candidates[0].value == expected
    assert type(candidates[0].value) is type(expected)
    assert candidates[0].evidence.quote == page.text[candidates[0].evidence.start : candidates[0].evidence.end]


def test_scalar_candidates_from_notes_never_read_a_written_fact_value() -> None:
    """A conclusion's number is the model's prose, not a page span: it cannot become an extracted value."""
    page = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 4.60"))
    read = Fact(reader=FactReader.LLM, text="Paper Filters - GBP 4.60", evidence=block_evidence(page, "s0"))
    derived = Fact(reader=FactReader.LLM, text="The cheapest is 999", evidence=None, basis=(fact_id(read),))
    notes = Notes((read, derived))
    candidates = field_candidates_from_notes(notes, Fields.model_fields["weight"])
    assert not isinstance(candidates, UnsupportedField)
    candidate = candidates[0]
    assert (candidate.value, type(candidate.value)) == (4.60, float)
    assert candidate.evidence.quote == "4.60"
    assert candidate.evidence.url == page.url
    assert candidate.evidence.capture_sha256 == page.sha256
    start = page.text.index("4.60")
    assert (candidate.evidence.start, candidate.evidence.end) == (start, start + 4)
    assert "Paper Filters" in candidate.context
    assert not any(candidate.value == 999 for candidate in candidates)
    assert isinstance(field_candidates_from_notes(notes, Fields.model_fields["records"]), UnsupportedField)


def test_merge_candidates_dedupes_the_current_capture_and_renumbers() -> None:
    """The notes and the capture hold the same page; one span is one option, with one id."""
    page = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 4.60"))
    notes = Notes((Fact(reader=FactReader.LLM, text="Paper Filters - GBP 4.60", evidence=block_evidence(page, "s0")),))
    from_capture = field_candidates(page, Fields.model_fields["weight"])
    from_notes = field_candidates_from_notes(notes, Fields.model_fields["weight"])
    assert not isinstance(from_capture, UnsupportedField) and not isinstance(from_notes, UnsupportedField)
    merged = merge_candidates(from_capture, from_notes)
    assert len(merged) == 1
    assert merged[0].id == "c0"
    assert merged[0].value == 4.60
    assert merged[0].evidence.capture_sha256 == page.sha256


def test_merge_candidates_keeps_equal_text_on_different_urls() -> None:
    """Capture hash is text-only, so two pages with the same text must not collapse into one candidate."""
    first = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 4.60"), url="https://example.test/search?page=1")
    second = capture((BlockKind.LIST_ITEM, "Paper Filters - GBP 4.60"), url="https://example.test/search?page=2")
    assert first.sha256 == second.sha256
    from_first = field_candidates(first, Fields.model_fields["weight"])
    from_second = field_candidates(second, Fields.model_fields["weight"])
    assert not isinstance(from_first, UnsupportedField) and not isinstance(from_second, UnsupportedField)
    merged = merge_candidates(from_first, from_second)
    assert [candidate.evidence.url for candidate in merged] == [first.url, second.url]
    assert [candidate.id for candidate in merged] == ["c0", "c1"]


def test_merge_candidates_keeps_changed_values_at_the_same_offsets() -> None:
    first = capture((BlockKind.PARAGRAPH, "Price: 4.60"))
    second = capture((BlockKind.PARAGRAPH, "Price: 5.20"))
    old = field_candidates(first, Fields.model_fields["weight"])
    new = field_candidates(second, Fields.model_fields["weight"])
    assert not isinstance(old, UnsupportedField) and not isinstance(new, UnsupportedField)
    assert [item.value for item in merge_candidates(old, new)] == [4.60, 5.20]


async def test_candidate_selection_groups_again_when_the_inner_group_exceeds_the_cap(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fastbrowse import retrieval
    from tests.test_policy import ScriptedJev

    monkeypatch.setattr(retrieval, "MAX_CHOICE_OPTIONS", 8)
    monkeypatch.setattr(retrieval, "_FIELD_GROUP_SIZE", 3)
    page = capture((BlockKind.PARAGRAPH, "Price: 4.60"))
    observed = field_candidates(page, Fields.model_fields["weight"])
    assert not isinstance(observed, UnsupportedField)
    candidates = tuple(observed[0].model_copy(update={"id": f"c{index}"}) for index in range(80))
    jev = ScriptedJev({"price_group": "0", "price": "c0"})
    result = await retrieval.choose_candidate(
        jev, {}, Fields.model_fields["weight"], candidates, name="price", task="What is the price?"
    )
    assert result == (4.60, candidates[0].evidence)
    assert len(jev.requests) == 3


async def test_composition_and_claims_share_budget() -> None:
    from fastbrowse.config import Thresholds
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger
    from fastbrowse.verification import check_claims
    from tests.test_policy import ScriptedJev

    llm = ScriptedLLM([{"claims": []}])
    ledger = Ledger(Limits(max_dollars=0.001))
    plan = Plan(
        requirements=(Requirement(id="r1", text="Find the price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    composed = await compose(llm, "Find it", plan, Notes(), ledger=ledger)
    jev = ScriptedJev({})
    with pytest.raises(BudgetExceeded):
        await check_claims(jev, composed.data, Notes(), Thresholds(), ledger=ledger)
    assert len(llm.calls) == 1 and jev.requests == []


class _DraftJev:
    """Answers the done check with `doubt` for the draft question, or omits that answer when None."""

    def __init__(self, doubt: float | None) -> None:
        self.doubt = doubt

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        answers: dict[str, Answer] = {"complete": NoulAnswer(probability=0.95)}
        if self.doubt is not None and "draft_needs_writing" in questions:
            answers["draft_needs_writing"] = NoulAnswer(probability=self.doubt)
        cost = CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0)
        return Evaluation(model="test", answers=answers, input_tokens=1, cost=cost)


@pytest.mark.parametrize(("doubt", "skips_composer"), [(0.1, True), (0.6, False), (None, False)])
async def test_only_a_confident_jev_no_lets_the_read_facts_stand_as_the_answer(
    doubt: float | None, skips_composer: bool
) -> None:
    page = capture((BlockKind.PARAGRAPH, "Price is $12"))
    evidence = block_evidence(page, "s0")
    notes = Notes((Fact(reader=FactReader.LLM, requirement_id="r1", text="The price is $12.", evidence=evidence),))
    plan = Plan(
        requirements=(Requirement(id="r1", text="Find price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    draft = draft_answer(plan, notes)
    assert draft is not None and draft.linked_answer.startswith("The price is $12. [1](<")
    assert draft.citations[0].quote == evidence.quote
    assert draft.claims[0].evidence_ids == (evidence_id(evidence),)
    observation = Observation(
        url=page.url,
        title=page.title,
        page_key="k",
        captured_at=page.captured_at,
        controls=(),
        omitted_controls=0,
        viewport_text=page.text,
        tabs=(),
    )
    check = await check_done(_DraftJev(doubt), "Find the price", plan, observation, notes, Thresholds(), draft)
    assert (check.answer is draft) is skips_composer


@pytest.mark.parametrize("answer,dropped,expected", [("", 0, True), ("", 1, False), ("Uncited claim", 0, False)])
async def test_action_only_completion_never_sends_empty_claim_check(answer: str, dropped: int, expected: bool) -> None:
    from unittest.mock import AsyncMock, Mock

    from fastbrowse.jev import JevClient
    from fastbrowse.retrieval import ComposedAnswer
    from fastbrowse.verification import check_claims

    jev = Mock(spec=JevClient)
    jev.evaluate = AsyncMock(side_effect=AssertionError("empty request must not reach the provider"))
    composed = ComposedAnswer(answer=answer, linked_answer=answer, claims=(), dropped_claims=dropped)
    assert (await check_claims(jev, composed, Notes(), Thresholds()) is not None) is expected
    jev.evaluate.assert_not_called()


@pytest.mark.parametrize("kind", [RequirementKind.ACTION, RequirementKind.INFORMATION])
async def test_a_doubted_restatement_of_an_action_leaves_no_answer_rather_than_failing(kind: RequirementKind) -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.models import CostBasis, CostComponent, CostLine
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import check_claims

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            answers = {key: NoulAnswer(probability=0.9 if key == "unsupported_0" else 0.05) for key in questions}
            free = CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0)
            return Evaluation(model="test", answers=answers, input_tokens=1, cost=free)

    requirement = Requirement(id="r1", text="Send support a message as Ada", kind=kind)
    page = capture(
        (BlockKind.PARAGRAPH, "Thanks, we received it."),
    )
    notes = Notes((Fact(reader=FactReader.LLM, text="received", evidence=block_evidence(page, "s0")),))
    claim = Claim(text="A message from Ada about the damaged kettle was received.", evidence_ids=tuple(notes.evidence))
    held = await check_claims(Jev(), assemble_answer((claim,), notes, (requirement,)), notes, Thresholds())
    if kind is RequirementKind.ACTION:
        assert held is not None and held.answer == ""
    else:
        assert held is None


@pytest.mark.parametrize("omitted_after", [0.1, 0.9])
async def test_a_doubted_claim_is_dropped_only_if_the_rest_still_answers(omitted_after: float) -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.models import CostBasis, CostComponent, CostLine
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import check_claims

    class Jev:
        def __init__(self) -> None:
            self.calls = 0

        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            self.calls += 1
            doubted = {"unsupported_1": 0.9, "requirement_omitted": 0.1 if self.calls == 1 else omitted_after}
            answers = {key: NoulAnswer(probability=doubted.get(key, 0.05)) for key in questions}
            free = CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0)
            return Evaluation(model="test", answers=answers, input_tokens=1, cost=free)

    requirement = Requirement(id="r1", text="What does the page say?", kind=RequirementKind.INFORMATION)
    page = capture((BlockKind.PARAGRAPH, "Logged in"), (BlockKind.PARAGRAPH, "Log out"))
    notes = Notes()
    for index, text in enumerate(("Logged in", "Log out")):
        evidence = block_evidence(page, f"s{index}")
        notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text=text, evidence=evidence))
    first, second = tuple(notes.evidence)
    claims = (
        Claim(text="It says you are logged in.", evidence_ids=(first,)),
        Claim(text="It has a Log out button.", evidence_ids=(second,)),
    )
    composed = assemble_answer(claims, notes, (requirement,))
    held = await check_claims(Jev(), composed, notes, Thresholds())
    if omitted_after > 0.5:
        assert held is None
    else:
        assert held is not None and held.linked_answer.startswith("It says you are logged in. [1](<")
        assert held.citations == composed.citations[:1]


@pytest.mark.parametrize("contradicted", [0.1, 0.9])
async def test_an_answer_the_committed_pages_contradict_is_rejected(contradicted: float) -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.models import CostBasis, CostComponent, CostLine
    from fastbrowse.verification import check_claims

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            scores = {TRANSACTION_CONTRADICTED: contradicted}
            answers = {key: NoulAnswer(probability=scores.get(key, 0.05)) for key in questions}
            free = CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0)
            return Evaluation(model="test", answers=answers, input_tokens=1, cost=free)

    page = capture((BlockKind.PARAGRAPH, "Black pen £7"), (BlockKind.PARAGRAPH, "Order placed: red pen £11.55"))
    notes = Notes(
        Fact(reader=FactReader.LLM, text=block.source_id, evidence=block_evidence(page, block.source_id))
        for block in page.blocks
    )
    listed, placed = tuple(notes.evidence)
    composed = assemble_answer((Claim(text="Bought a black pen for £7", evidence_ids=(listed,)),), notes, ())
    held = await check_claims(Jev(), composed, notes, Thresholds(), transaction_evidence_ids=(placed,))
    assert (held is None) is (contradicted > 0.5)


async def test_a_failed_claim_ask_waits_for_the_transaction_ask_before_raising() -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.verification import check_claims

    finished: list[str] = []

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            if TRANSACTION_CONTRADICTED not in questions:
                raise JevError("claims failed")
            await asyncio.sleep(0.01)
            finished.append(TRANSACTION_CONTRADICTED)
            free = CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0)
            answers = {key: NoulAnswer(probability=0.05) for key in questions}
            return Evaluation(model="test", answers=answers, input_tokens=1, cost=free)

    page = capture((BlockKind.PARAGRAPH, "Black pen £7"), (BlockKind.PARAGRAPH, "Order placed: black pen £7"))
    notes = Notes(
        Fact(reader=FactReader.LLM, text=block.source_id, evidence=block_evidence(page, block.source_id))
        for block in page.blocks
    )
    listed, placed = tuple(notes.evidence)
    composed = assemble_answer((Claim(text="Bought a black pen for £7", evidence_ids=(listed,)),), notes, ())
    ledger = Ledger(Limits())
    with pytest.raises(JevError):
        await check_claims(Jev(), composed, notes, Thresholds(), ledger=ledger, transaction_evidence_ids=(placed,))
    assert finished == [TRANSACTION_CONTRADICTED]
    assert ledger.lines, "the finished ask was billed before check_claims returned"


async def test_pruning_the_only_claim_for_a_requirement_is_an_omission() -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.models import CostBasis, CostComponent, CostLine, Evidence
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import check_claims

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            # The superlative is doubted, and the omission check would wave the price through alone.
            answers = {key: NoulAnswer(probability=0.9 if key == "unsupported_0" else 0.05) for key in questions}
            free = CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0)
            return Evaluation(model="test", answers=answers, input_tokens=1, cost=free)

    def fact(requirement: str, start: int, quote: str) -> Fact:
        evidence = Evidence(
            source_id="s",
            url="https://books.test/travel",
            frame_id=None,
            captured_at=datetime.now(UTC),
            capture_sha256="c",
            start=start,
            end=start + len(quote),
            quote=quote,
        )
        return Fact(reader=FactReader.LLM, requirement_id=requirement, text=quote, evidence=evidence)

    # The price was read as the winner's price, so its fact draws on the winner; citing the price must still not
    # count as stating which book won.
    winner = fact("r1", 0, "A Year in Provence")
    price = fact("r2", 40, "£56.88").model_copy(update={"basis": (fact_id(winner),)})
    notes = Notes((winner, price))
    requirements = (
        Requirement(id="r1", text="Which book is the most expensive?", kind=RequirementKind.INFORMATION),
        Requirement(id="r2", text="What does it cost?", kind=RequirementKind.INFORMATION),
    )
    claims = (
        Claim(text="The most expensive is A Year in Provence.", evidence_ids=(fact_id(winner),)),
        Claim(text="It costs £56.88.", evidence_ids=(fact_id(price),)),
    )
    composed = assemble_answer(claims, notes, requirements)
    assert await check_claims(Jev(), composed, notes, Thresholds()) is None


@pytest.mark.parametrize(("required", "missing"), [(True, 0.9), (True, 0.5), (False, 0.05)])
async def test_pruning_part_of_a_grouped_requirement_checks_the_missing_output(required: bool, missing: float) -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.models import CostBasis, CostComponent, CostLine
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import check_claims

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            scores = {"unsupported_1": 0.9, "removed_output_1": missing}
            answers = {key: NoulAnswer(probability=scores.get(key, 0.05)) for key in questions}
            free = CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0)
            return Evaluation(model="test", answers=answers, input_tokens=1, cost=free)

    page = capture((BlockKind.PARAGRAPH, "Museum: admission £12"), (BlockKind.PARAGRAPH, "Opening hours vary"))
    notes = Notes()
    for block in page.blocks:
        notes.add(
            Fact(
                reader=FactReader.LLM,
                requirement_id="r1",
                text=block_evidence(page, block.source_id).quote,
                evidence=block_evidence(page, block.source_id),
            )
        )
    first, second = tuple(notes.evidence)
    requirement = Requirement(
        id="r1",
        text="Report the museum's admission price and opening hours."
        if required
        else "Report the museum's admission price.",
        kind=RequirementKind.INFORMATION,
    )
    claims = (
        Claim(text="Admission costs £12.", evidence_ids=(first,)),
        Claim(text="It opens at 9am.", evidence_ids=(second,)),
    )
    held = await check_claims(Jev(), assemble_answer(claims, notes, (requirement,)), notes, Thresholds())
    assert (held is None) is required


async def test_a_winner_from_part_of_a_list_is_kept_but_does_not_answer() -> None:
    page = capture((BlockKind.PARAGRAPH, "Sharp Objects £47.82"), (BlockKind.PARAGRAPH, "Page 1 of 2"))
    notes = Notes()
    llm = ScriptedLLM(
        [
            {
                "claims": [{"requirement_id": "r1", "text": "cheapest", "cite": {"first": "s0", "last": "s0"}}],
                "answered": True,
                "continues": [
                    {"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]},
                    {"requirement_id": "not-asked", "records": [{"first": "s0", "last": "s0"}]},
                ],
            }
        ]
    )
    outcome = await read(llm, page, "Cheapest mystery?", ["r1"], notes, notice="This page has a next-page control.")
    assert outcome.continues == ("r1",)
    assert len(notes.facts) == 1 and not notes.evidenced("r1")
    assert "This page has a next-page control." in llm.calls[0][1][-1].content


async def test_a_continuing_page_keeps_every_record_it_compared() -> None:
    """The prose used to ask for the records and the reader complied about half the time. The schema requires
    them, and code copies each quote from the blocks named, so a later page's winner can show what it beat."""
    from tests.test_policy import ScriptedJev

    page = capture(
        (BlockKind.PARAGRAPH, "Sharp Objects 47.82"),
        (BlockKind.PARAGRAPH, "Tastes Like Fear 10.69"),
        (BlockKind.PARAGRAPH, "A Murder in Time 16.64"),
    )
    requirement = Requirement(id="r1", text="The cheapest book", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "continues": [
                    {
                        "requirement_id": "r1",
                        "records": [
                            {"first": "s0", "last": "s0"},
                            {"first": "s1", "last": "s1"},
                            {"first": "s2", "last": "s2"},
                        ],
                    }
                ],
            }
        ]
    )
    notes = Notes()
    outcome = await read(
        llm, page, "Cheapest?", ["r1"], notes, jev=ScriptedJev({"r1": "none"}), requirements=[requirement]
    )
    assert outcome.continues == ("r1",)
    assert outcome.uncovered == 0
    quotes = [fact.evidence.quote for fact in notes.facts if fact.evidence is not None]
    assert quotes == ["Sharp Objects 47.82", "Tastes Like Fear 10.69", "A Murder in Time 16.64"]
    # The list still goes on, so nothing here closes the requirement.
    assert not notes.evidenced("r1")


async def test_a_record_naming_blocks_the_page_did_not_offer_is_counted_not_credited() -> None:
    from tests.test_policy import ScriptedJev

    page = capture((BlockKind.PARAGRAPH, "Sharp Objects 47.82"))
    requirement = Requirement(id="r1", text="The cheapest book", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "continues": [
                    {
                        "requirement_id": "r1",
                        "records": [{"first": "s0", "last": "s0"}, {"first": "s9", "last": "s9"}],
                    }
                ],
            }
        ]
    )
    notes = Notes()
    outcome = await read(
        llm, page, "Cheapest?", ["r1"], notes, jev=ScriptedJev({"r1": "none"}), requirements=[requirement]
    )
    assert outcome.uncovered == 1
    assert [fact.evidence.quote for fact in notes.facts if fact.evidence is not None] == ["Sharp Objects 47.82"]


@pytest.mark.parametrize("loss", ["outside_chunk", "overflow"])
async def test_a_lost_continuation_record_blocks_a_later_pages_winner(loss: str) -> None:
    first = capture((BlockKind.PARAGRAPH, "Book A 12.00"), (BlockKind.PARAGRAPH, "Book B 15.00"))
    records: list[JsonValue] = (
        [{"first": "s0", "last": "s0"}, {"first": "s1", "last": "s1"}]
        if loss == "outside_chunk"
        else [{"first": "s0", "last": "s0"}] * 60 + [{"first": "s1", "last": "s1"}]
    )
    llm = ScriptedLLM(
        [
            {"claims": [], "answered": False, "continues": [{"requirement_id": "r1", "records": records}]},
            {"claims": [], "answered": False, "continues": [{"requirement_id": "r1", "records": []}]},
            {
                "claims": [
                    {"text": "Book C is cheapest", "cite": {"first": "s0", "last": "s0"}, "requirement_id": "r1"},
                    {"text": "Delivery is free", "cite": {"first": "s1", "last": "s1"}, "requirement_id": "r2"},
                ],
                "answered": True,
            },
        ]
    )
    notes = Notes()
    outcome = await read(llm, first, "Cheapest?", ["r1", "r2"], notes, max_chars=15)
    last = capture((BlockKind.PARAGRAPH, "Book C 10.00"), (BlockKind.PARAGRAPH, "Delivery is free"))
    await read(llm, last, "Cheapest and delivery?", ["r1", "r2"], notes, incomplete=outcome.incomplete)

    assert not notes.evidenced("r1")
    assert notes.evidenced("r2")
    assert any(f.text == "Book C is cheapest" and f.requirement_id is None for f in notes.facts)
    assert outcome.uncovered == 1 and outcome.incomplete == ("r1",)


async def test_overflow_records_a_claim_already_holds_are_not_lost() -> None:
    page = capture((BlockKind.PARAGRAPH, "Book A 12.00"), (BlockKind.PARAGRAPH, "Book B 15.00"))
    records: list[JsonValue] = [{"first": "s0", "last": "s0"}] * 60 + [{"first": "s1", "last": "s1"}]
    llm = ScriptedLLM(
        [
            {
                "claims": [{"text": "Book B costs 15.00", "cite": {"first": "s1", "last": "s1"}}],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": records}],
            }
        ]
    )
    outcome = await read(llm, page, "Cheapest?", ["r1"], Notes())
    assert outcome.uncovered == 0 and outcome.incomplete == ()


async def test_a_lost_continuation_record_blocks_the_last_chunks_winner() -> None:
    page = capture((BlockKind.PARAGRAPH, "Book A 12.00"), (BlockKind.PARAGRAPH, "Book B 10.00"))
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": [{"first": "s1", "last": "s1"}]}],
            },
            {
                "claims": [
                    {"text": "Book B is cheapest", "cite": {"first": "s1", "last": "s1"}, "requirement_id": "r1"}
                ],
                "answered": True,
            },
        ]
    )
    notes = Notes()
    outcome = await read(llm, page, "Cheapest?", ["r1"], notes, max_chars=15)

    assert not notes.evidenced("r1")
    assert outcome.continues == () and outcome.incomplete == ("r1",)
    assert notes.facts[0].text == "Book B is cheapest" and notes.facts[0].requirement_id is None


async def test_an_incomplete_comparison_skips_scalar_choice_but_still_reaches_the_reader() -> None:
    from tests.test_policy import ScriptedJev

    page = capture((BlockKind.PARAGRAPH, "Book A 12.00"))
    jev = ScriptedJev({})
    requirement = Requirement(id="r1", text="The cheapest book", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [{"text": "Book A", "cite": {"first": "s0", "last": "s0"}, "requirement_id": "r1"}],
                "answered": True,
            }
        ]
    )
    notes = Notes()
    await read(llm, page, "Cheapest?", ["r1"], notes, jev=jev, requirements=[requirement], incomplete={"r1"})

    assert jev.requests == []
    assert "# Requirement ids\nr1" in llm.calls[0][1][-1].content
    assert notes.facts and not notes.evidenced("r1")


@pytest.mark.parametrize("earlier_page", [False, True])
async def test_a_stated_order_settles_a_winner_despite_a_lost_continuation_record(earlier_page: bool) -> None:
    page = capture((BlockKind.PARAGRAPH, "Sorted by price, lowest first"), (BlockKind.PARAGRAPH, "Book A 12.00"))
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {
                        "text": "Book A is cheapest",
                        "cite": {"first": "s1", "last": "s1"},
                        "orders_list": {"first": "s0", "last": "s0"},
                        "requirement_id": "r1",
                    }
                ],
                "answered": True,
                "continues": [{"requirement_id": "r1", "records": [{"first": "s9", "last": "s9"}]}],
            }
        ]
    )
    notes = Notes()
    outcome = await read(llm, page, "Cheapest?", ["r1"], notes, incomplete={"r1"} if earlier_page else ())

    assert notes.evidenced("r1")
    assert outcome.incomplete == ("r1",) and outcome.continues == ()
    assert any(f.evidence is not None and f.evidence.quote == "Sorted by price, lowest first" for f in notes.facts)


@pytest.mark.parametrize(("records", "uncovered"), [(65, 5), (0, 0)])
async def test_a_reply_listing_more_records_than_the_cap_or_none_is_read_not_rejected(
    records: int, uncovered: int
) -> None:
    """A dense results table lists more rows than the cap in one chunk, and a chunk holding only the pager lists
    none. Rejecting either reply ended the run with an error over a page that read fine; the records past the cap
    are counted as uncovered instead."""
    page = capture((BlockKind.PARAGRAPH, "Sharp Objects 47.82"))
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}] * records}],
            }
        ]
    )
    outcome = await read(llm, page, "Cheapest?", ["r1"], Notes())
    assert outcome.continues == ("r1",)
    assert outcome.uncovered == uncovered


async def test_a_count_over_a_list_that_goes_on_is_not_settled_by_the_pages_sort_order() -> None:
    """The page's order settles only the leading record. A count draws on every record it counts, and page one
    of two holds only some of them however the site sorts them."""
    page = capture(
        (BlockKind.PARAGRAPH, "Sort: price, low to high"),
        (BlockKind.PARAGRAPH, "Book A 12.00"),
        (BlockKind.PARAGRAPH, "Book B 15.00"),
        (BlockKind.PARAGRAPH, "Page 1 of 2"),
    )
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "Book A 12.00", "cite": {"first": "s1", "last": "s1"}},
                    {"text": "Book B 15.00", "cite": {"first": "s2", "last": "s2"}},
                    {
                        "cite": None,
                        "draws_on": ["claim:0", "claim:1"],
                        "orders_list": {"first": "s0", "last": "s0"},
                        "text": "2 books cost under 20.",
                        "requirement_id": "r1",
                    },
                ],
                "answered": False,
                "continues": [
                    {"requirement_id": "r1", "records": [{"first": "s1", "last": "s1"}, {"first": "s2", "last": "s2"}]}
                ],
            }
        ]
    )
    notes = Notes()
    outcome = await read(llm, page, "How many books cost under 20?", ["r1"], notes)
    assert outcome.continues == ("r1",)
    assert not notes.evidenced("r1")


async def test_a_page_that_states_its_own_order_settles_a_superlative_on_the_leading_record() -> None:
    """Three reads of a filtered results page returned nothing while the cheapest row was on screen, because
    the reader saw the list go on and never assigned the requirement. A site that sorts by the quantity being
    compared has already answered: the rest of the list cannot beat the leading row."""
    from tests.test_policy import ScriptedJev

    page = capture(
        (BlockKind.PARAGRAPH, "Sorted by price, lowest first"),
        (BlockKind.PARAGRAPH, "From 1061 US dollars. Nonstop flight"),
    )
    requirement = Requirement(id="r1", text="The cheapest nonstop fare", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {
                        "cite": {"first": "s1", "last": "s1"},
                        "orders_list": {"first": "s0", "last": "s0"},
                        "text": "The cheapest nonstop fare is 1061 US dollars.",
                        "requirement_id": "r1",
                    }
                ],
                "answered": True,
            }
        ]
    )
    notes = Notes()
    outcome = await read(
        llm, page, "Cheapest nonstop?", ["r1"], notes, jev=ScriptedJev({"r1": "none"}), requirements=[requirement]
    )
    assert outcome.continues == ()
    assert notes.evidenced("r1")
    # The page's own statement of its order is kept, so the claim rests on it and the check can judge it.
    assert any(f.evidence is not None and f.evidence.quote == "Sorted by price, lowest first" for f in notes.facts)


@pytest.mark.parametrize("said_it_continues", [True, False])
async def test_an_order_the_page_does_not_state_cannot_settle_a_superlative(said_it_continues: bool) -> None:
    """The reader is told to cite the order rather than say the list goes on, so an order cited from a block that
    does not exist cannot close the superlative whether or not it also said so."""
    from tests.test_policy import ScriptedJev

    page = capture((BlockKind.PARAGRAPH, "From 1061 US dollars. Nonstop flight"))
    requirement = Requirement(id="r1", text="The cheapest nonstop fare", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {
                        "cite": {"first": "s0", "last": "s0"},
                        "orders_list": {"first": "s7", "last": "s7"},
                        "text": "The cheapest nonstop fare is 1061 US dollars.",
                        "requirement_id": "r1",
                    }
                ],
                "answered": not said_it_continues,
                "continues": [
                    {"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}], "expands": "View more"}
                ]
                if said_it_continues
                else [],
            }
        ]
    )
    notes = Notes()
    outcome = await read(
        llm, page, "Cheapest nonstop?", ["r1"], notes, jev=ScriptedJev({"r1": "none"}), requirements=[requirement]
    )
    assert not notes.evidenced("r1")
    assert outcome.continues == (("r1",) if said_it_continues else ())


async def test_a_read_that_settles_a_list_carries_no_records() -> None:
    """Records are the price of a page that cannot conclude. A read that concludes pays nothing for them."""
    from tests.test_policy import ScriptedJev

    page = capture((BlockKind.PARAGRAPH, "Tastes Like Fear 10.69"))
    requirement = Requirement(id="r1", text="The cheapest book", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "Tastes Like Fear 10.69", "cite": {"first": "s0", "last": "s0"}, "requirement_id": "r1"}
                ],
                "answered": True,
            }
        ]
    )
    notes = Notes()
    outcome = await read(
        llm, page, "Cheapest?", ["r1"], notes, jev=ScriptedJev({"r1": "none"}), requirements=[requirement]
    )
    assert outcome.continues == ()
    assert outcome.uncovered == 0
    assert notes.evidenced("r1")


async def test_the_choice_shortcut_is_skipped_when_a_next_page_control_qualifies_the_read() -> None:
    # The choice model picks quotes without weighing a caveat, and answered "the first book on the page the link
    # opens" from the page the link was on. The reader follows the notice, so the notice goes to it alone.
    from tests.test_policy import ScriptedJev

    page = capture((BlockKind.PARAGRAPH, "Sharp Objects £47.82"))
    jev = ScriptedJev({"r1": "none"})
    requirement = Requirement(id="r1", text="The cheapest book", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}],
            }
        ]
    )
    outcome = await read(
        llm,
        page,
        "Cheapest?",
        ["r1"],
        Notes(),
        jev=jev,
        requirements=[requirement],
        notice="This page has a next-page control ('next').",
    )
    assert jev.requests == []
    assert outcome.continues == ("r1",)
    assert "next-page control" in llm.calls[0][1][-1].content


async def test_a_later_chunk_saying_the_list_goes_on_reopens_an_earlier_chunks_claim() -> None:
    # The pager sits at the foot of a long listing, so the chunk that names it is read after the winner.
    page = capture(
        (BlockKind.PARAGRAPH, "Sharp Objects £47.82"),
        (BlockKind.PARAGRAPH, "a" * 25000),
        (BlockKind.PARAGRAPH, "Page 1 of 2"),
    )
    notes = Notes()
    llm = ScriptedLLM(
        [
            {
                "claims": [{"requirement_id": "r1", "text": "cheapest", "cite": {"first": "s0", "last": "s0"}}],
                "answered": True,
            },
            {
                "claims": [],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}],
            },
            {
                "claims": [],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}],
            },
        ]
    )
    outcome = await read(llm, page, "Cheapest?", ["r1"], notes, notice="This page has a next-page control.")
    assert len(llm.calls) > 1, "a chunk claiming to have answered cannot end a read of a list that goes on"
    assert outcome.continues == ("r1",)
    assert len(notes.facts) == 1 and not notes.evidenced("r1")


async def test_only_the_last_chunk_names_the_control_that_shows_the_rest() -> None:
    # An early chunk's label named a control that shows more of what it held, a filter drawer or a review toggle;
    # opening it on the strength of the last chunk's "the list goes on" clicks the wrong thing.
    page = capture(
        (BlockKind.PARAGRAPH, "Virgin $1,200"),
        (BlockKind.PARAGRAPH, "a" * 25000),
        (BlockKind.PARAGRAPH, "Page 1 of 2"),
    )
    carried: dict[str, JsonValue] = {"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}
    llm = ScriptedLLM(
        [
            {"claims": [], "answered": False, "continues": [{**carried, "expands": "Filters"}]},
            {"claims": [], "answered": False, "continues": [carried]},
            {"claims": [], "answered": False, "continues": [carried]},
        ]
    )
    outcome = await read(llm, page, "Cheapest?", ["r1"], Notes())
    assert len(llm.calls) == 3
    assert outcome.continues == ("r1",)
    assert outcome.expands is None


async def test_a_list_that_runs_on_into_the_next_chunk_is_settled_by_the_last_one() -> None:
    # A results page too long for one chunk: the first chunk rightly says the list goes on, and the last chunk,
    # holding the rest and the earlier records, names the winner. The first chunk's word must not outlive it.
    page = capture(
        (BlockKind.PARAGRAPH, "Virgin $1,200"),
        (BlockKind.PARAGRAPH, "a" * 25000),
        (BlockKind.PARAGRAPH, "JetBlue $1,061"),
    )
    notes = Notes()
    llm = ScriptedLLM(
        [
            {
                "claims": [{"text": "Virgin $1,200", "cite": {"first": "s0", "last": "s0"}}],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}],
            },
            {
                "claims": [],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": []}],
            },
            {
                "claims": [
                    {"requirement_id": "r1", "text": "JetBlue at $1,061", "cite": {"first": "s2", "last": "s2"}}
                ],
                "answered": True,
            },
        ]
    )
    outcome = await read(llm, page, "Cheapest?", ["r1"], notes)
    assert len(llm.calls) == 3
    assert outcome.continues == ()
    assert notes.evidenced("r1")


async def test_a_later_chunk_is_read_against_what_earlier_chunks_of_the_page_found() -> None:
    """The notes are written once the page is read, so the read carries its own findings between chunks."""
    page = capture(
        (BlockKind.PARAGRAPH, "Einstein: the world as we have created it"),
        (BlockKind.PARAGRAPH, "b" * 25000),
        (BlockKind.PARAGRAPH, "Einstein: there are two ways to live"),
    )
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {
                        "requirement_id": None,
                        "text": "an Einstein quote",
                        "cite": {"first": "s0", "last": "s0"},
                    }
                ],
                "answered": False,
            },
            {"claims": [], "answered": False},
            {"claims": [], "answered": False},
        ]
    )
    await read(llm, page, "How many Einstein quotes?", ["r1"], Notes())
    later = llm.calls[1][1][-1].content
    assert "the world as we have created it" in later, "chunk two cannot count what chunk one found unseen"


@pytest.mark.parametrize("composed", [False, True], ids=["draft", "composer"])
@pytest.mark.parametrize("answer", ["There are 4 books.", "The total is $53.", "Pine is the cheapest book at $7."])
async def test_derived_answer_cites_and_checks_every_record_across_pages(composed: bool, answer: str) -> None:
    from fastbrowse.verification import check_claims

    first = capture(
        (BlockKind.PARAGRAPH, "Oak $19"),
        (BlockKind.PARAGRAPH, "Redwood $12"),
        (BlockKind.PARAGRAPH, "Page 1 of 2"),
    )
    notes = Notes()
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "Oak $19", "cite": {"first": "s0", "last": "s0"}},
                    {"text": "Redwood $12", "cite": {"first": "s1", "last": "s1"}},
                    {
                        "text": "Two books on page one, total $31.",
                        "cite": {"first": "s2", "last": "s2"},
                        "draws_on": ["claim:1", "claim:0"],
                    },
                ],
                "answered": False,
                "continues": [{"requirement_id": "r", "records": [{"first": "s0", "last": "s0"}]}],
            }
        ]
    )
    await read(llm, first, answer, ["r"], notes)
    earlier = tuple(notes.evidence)
    last = capture((BlockKind.PARAGRAPH, "Pine $7"), (BlockKind.PARAGRAPH, "Elm $15"))
    llm.responses.append(
        {
            "claims": [
                {"text": "Pine $7", "cite": {"first": "s0", "last": "s0"}},
                {"text": "Elm $15", "cite": {"first": "s1", "last": "s1"}},
                {
                    "requirement_id": "r",
                    "text": answer,
                    "cite": {"first": "s0", "last": "s0"},
                    "draws_on": ["claim:1", "e2", "claim:0", "e0"],
                },
            ],
            "answered": True,
        }
    )
    outcome = await read(llm, last, answer, ["r"], notes, continuing=("r",))
    keys = tuple(notes.evidence)
    prompt = llm.calls[-1][1][-1].content
    assert all(f"[e{i}]" in prompt and key not in prompt for i, key in enumerate(earlier))
    assert 'basis=["e1", "e0"]' in prompt
    assert outcome.facts[-1].basis == (keys[4], keys[2], keys[3], keys[0])
    assert notes.supporting("r")[0][1].text == answer
    plan = Plan(
        requirements=(Requirement(id="r", text=answer, kind=RequirementKind.INFORMATION),), answer_expected=True
    )
    if composed:
        llm.responses.append({"claims": [{"text": answer, "evidence_ids": [keys[3]]}]})
        result = (await compose(llm, answer, plan, notes)).data
    else:
        result = draft_answer(plan, notes)
    assert result is not None and result.answer == answer
    assert notes.expand_evidence_ids(result.claims[0].evidence_ids) == keys
    assert [citation.quote for citation in result.citations] == [evidence.quote for evidence in notes.evidence.values()]
    assert [citation.id for citation in result.citations] == list(range(1, 6))
    for citation in result.citations:
        assert f"[{citation.id}](<{citation.deep_link}>)" in result.linked_answer
    jev = _ReadJev(
        {key: NoulAnswer(probability=0.05) for key in ("unsupported_0", "contradicted_0", "requirement_omitted")}
    )
    assert await check_claims(jev, result, notes, Thresholds()) is result
    for name in ("unsupported_0", "contradicted_0"):
        question = jev.requests[0][1][name]
        assert all(evidence.model_dump_json() in question.instructions for evidence in notes.evidence.values())


async def test_a_count_the_page_does_not_state_is_derived_and_judged_by_its_records() -> None:
    page = capture((BlockKind.PARAGRAPH, "Oak $19"), (BlockKind.PARAGRAPH, "Pine $7"))
    requirement = Requirement(id="r", text="How many trees are listed?", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "Oak $19", "cite": {"first": "s0", "last": "s0"}},
                    {"text": "Pine $7", "cite": {"first": "s1", "last": "s1"}},
                    {"requirement_id": "r", "text": "Two trees", "cite": None, "draws_on": ["claim:0", "claim:1"]},
                ],
                "answered": True,
            }
        ]
    )
    notes = Notes()
    await read(llm, page, requirement.text, ["r"], notes, requirements=(requirement,))
    ((key, count),) = notes.supporting("r")
    assert count.evidence is None and notes.derived(key) and "derived" in notes.render(10_000)
    draft = draft_answer(Plan(requirements=(requirement,), answer_expected=True), notes)
    assert draft is not None
    assert [citation.quote for citation in draft.citations] == ["Oak $19", "Pine $7"]
    unsupported = claim_check_questions(draft, notes)["unsupported_0"].instructions
    assert "Oak $19" in unsupported and "Pine $7" in unsupported and "MISSING" not in unsupported


async def test_compact_comparison_records_keep_each_quote_once_in_the_answer_basis() -> None:
    page = capture(
        (BlockKind.HEADING, "Travel books"),
        (BlockKind.RECORD, "Oak\n$19"),
        (BlockKind.RECORD, "Pine\n$7"),
    )
    requirement = Requirement(id="r", text="Which travel book is cheapest?", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "Travel books", "cite": {"first": "s0", "last": "s0"}},
                    {
                        "requirement_id": "r",
                        "text": "Pine is cheapest at $7.",
                        "cite": None,
                        "draws_on": ["claim:0"],
                        "records": [
                            {"first": "s1", "last": "s1"},
                            {"first": "s2", "last": "s2"},
                            {"first": "s1", "last": "s1"},
                        ],
                    },
                ],
                "answered": True,
            }
        ]
    )
    notes = Notes()
    outcome = await read(llm, page, requirement.text, ["r"], notes)
    assert len(outcome.facts) == 4 and not outcome.rejected_claims
    ((_, winner),) = notes.supporting("r")
    assert winner.evidence is None and winner.basis == tuple(notes.evidence)
    draft = draft_answer(Plan(requirements=(requirement,), answer_expected=True), notes)
    assert draft is not None and draft.answer == winner.text
    assert [citation.quote for citation in draft.citations] == ["Travel books", "Oak\n$19", "Pine\n$7"]
    unsupported = claim_check_questions(draft, notes)["unsupported_0"].instructions
    assert all(evidence.model_dump_json() in unsupported for evidence in notes.evidence.values())


@pytest.mark.parametrize("bad_record", ["unknown", "reversed", "frame", "overflow"])
async def test_a_compact_comparison_with_a_lost_record_cannot_close_later(bad_record: str) -> None:
    page = capture((BlockKind.RECORD, "Oak $19"), (BlockKind.RECORD, "Pine $7"))
    records: list[JsonValue] = [{"first": "s0", "last": "s0"}]
    if bad_record == "overflow":
        records *= 61
    elif bad_record == "frame":
        page = page.model_copy(
            update={"blocks": (page.blocks[0], page.blocks[1].model_copy(update={"frame_id": "other"}))}
        )
        records.append({"first": "s0", "last": "s1"})
    else:
        records.append({"first": "s1", "last": "missing" if bad_record == "unknown" else "s0"})
    llm = ScriptedLLM(
        [
            {
                "claims": [{"requirement_id": "r", "text": "Pine is cheapest", "cite": None, "records": records}],
                "answered": True,
            },
            {
                "claims": [{"requirement_id": "r", "text": "Pine is cheapest", "cite": {"first": "s1", "last": "s1"}}],
                "answered": True,
            },
        ]
    )
    notes = Notes()
    outcome = await read(llm, page, "Cheapest?", ["r"], notes)
    assert outcome.rejected_claims == outcome.uncovered == 1
    assert outcome.incomplete == ("r",) and not notes.evidenced("r")
    await read(llm, page, "Cheapest?", ["r"], notes, incomplete=outcome.incomplete)
    assert not notes.evidenced("r")


async def test_compact_records_survive_chunks_and_join_an_earlier_pages_evidence() -> None:
    earlier = capture((BlockKind.RECORD, "Oak $19"))
    source = block_evidence(earlier, "s0")
    notes = Notes([Fact(text=source.quote, evidence=source, reader=FactReader.LLM)])
    page = capture((BlockKind.RECORD, "Elm $12"), (BlockKind.RECORD, "Pine $7"))
    llm = ScriptedLLM(
        [
            {
                "claims": [{"text": "Elm costs $12", "cite": None, "records": [{"first": "s0", "last": "s0"}]}],
                "answered": False,
            },
            {
                "claims": [
                    {
                        "requirement_id": "r",
                        "text": "Pine is cheapest at $7",
                        "cite": None,
                        "draws_on": [evidence_id(source), evidence_id(block_evidence(page, "s0"))],
                        "records": [{"first": "s1", "last": "s1"}],
                    }
                ],
                "answered": True,
            },
        ]
    )
    outcome = await read(llm, page, "Cheapest?", ["r"], notes, max_chars=7, continuing={"r"})
    assert outcome.coverage == (0, 1)
    assert [evidence.quote for evidence in notes.supporting_evidence("r")] == ["Oak $19", "Elm $12", "Pine $7"]
    assert "Elm $12" in llm.calls[1][1][-1].content


async def test_basis_references_cannot_name_rejected_or_later_claims() -> None:
    page = capture((BlockKind.PARAGRAPH, "A $3"), (BlockKind.PARAGRAPH, "B $5"))
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "Forged", "cite": {"first": "s9", "last": "s9"}},
                    {"text": "A $3", "cite": {"first": "s0", "last": "s0"}},
                    {
                        "requirement_id": "r",
                        "text": "A is cheaper",
                        "cite": {"first": "s0", "last": "s0"},
                        "draws_on": ["claim:0", "claim:1", "claim:1", "claim:3", "invented:0:99"],
                    },
                    {"text": "B $5", "cite": {"first": "s1", "last": "s1"}},
                ],
                "answered": True,
            }
        ]
    )
    notes = Notes()
    result = await read(llm, page, "Cheapest?", ["r"], notes)
    assert result.rejected_claims == 1
    assert notes.supporting("r")[0][1].basis == (fact_id(notes.facts[0]),)


@pytest.mark.parametrize("confidence", [0.89, 0.95])
async def test_only_confident_absence_skips_the_llm_without_evidencing_a_requirement(
    confidence: float, caplog: pytest.LogCaptureFixture
) -> None:
    page = capture((BlockKind.PARAGRAPH, "Set a destination"))
    requirement = Requirement(id="r", text="Find the submitted fare", kind=RequirementKind.INFORMATION)
    jev = _ReadJev({"r": _choice("absent", confidence)})
    llm, notes = ScriptedLLM([{"claims": [], "answered": False}]), Notes()
    with caplog.at_level("DEBUG", logger="fastbrowse.retrieval"):
        outcome = await read(llm, page, requirement.text, ["r"], notes, jev=jev, requirements=(requirement,))
    assert len(llm.calls) == (confidence < 0.90)
    assert not notes.evidenced("r") and not outcome.facts
    assert page.text not in caplog.text


async def test_mixed_read_routes_keep_provenance_and_narrow_the_llm_request() -> None:
    page = capture((BlockKind.PARAGRAPH, "Version 1.2.3"), (BlockKind.PARAGRAPH, "One\n\tTwo"))
    requirements = tuple(
        Requirement(id=key, text=text, kind=RequirementKind.INFORMATION)
        for key, text in (("version", "Version"), ("names", "List the names"), ("fare", "Find the fare"))
    )
    jev = _ReadJev({"version": _choice("c0"), "names": _choice("synthesis"), "fare": _choice("absent")})
    llm = ScriptedLLM(
        [
            {
                "claims": [{"requirement_id": "names", "text": "One and Two", "cite": {"first": "s1", "last": "s1"}}],
                "answered": True,
            }
        ]
    )
    notes = Notes()
    result = await read(llm, page, "Read all", [r.id for r in requirements], notes, jev=jev, requirements=requirements)
    assert [fact.reader for fact in result.facts] == [FactReader.JEV_CHOICE, FactReader.LLM]
    assert [evidence.quote for evidence in notes.evidence.values()] == ["Version 1.2.3", "One\n\tTwo"]
    assert all(evidence.url == page.url for evidence in notes.evidence.values())
    assert not notes.evidenced("fare")
    assert notes.evidenced("version") and notes.evidenced("names")
    request = llm.calls[0][1][-1].content
    assert request.endswith("# Requirement ids\nnames")
    assert "Version 1.2.3" in request.split("# Collected evidence\n")[1]


@pytest.mark.parametrize(
    "cite",
    [{"first": "s2", "last": "s0"}, {"first": "unverified " * 50, "last": "s0"}, None],
    ids=["reversed", "invented", "none"],
)
async def test_the_reader_keeps_only_claims_citing_an_offered_run_of_blocks(
    cite: JsonValue, caplog: pytest.LogCaptureFixture
) -> None:
    page = capture((BlockKind.PARAGRAPH, "A $3"), (BlockKind.PARAGRAPH, "B $5"), (BlockKind.PARAGRAPH, "C $7"))
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"requirement_id": "r", "text": "Rejected", "cite": cite},
                    {"requirement_id": "r", "text": "A, B and C", "cite": {"first": "s0", "last": "s2"}},
                ],
                "answered": True,
            }
        ]
    )
    notes = Notes()
    with caplog.at_level("DEBUG", logger="fastbrowse.retrieval"):
        result = await read(llm, page, "Find the prices", ["r"], notes)
    assert result.rejected_claims == 1
    assert [evidence.quote for evidence in notes.evidence.values()] == ["A $3\n\nB $5\n\nC $7"]
    assert "read rejected claim" in caplog.text and "unverified " * 5 not in caplog.text


@pytest.mark.parametrize("count", [MAX_CHOICE_OPTIONS - 2, MAX_CHOICE_OPTIONS - 1])
async def test_short_read_leaves_room_for_both_non_candidate_choices(count: int) -> None:
    page = capture(*((BlockKind.PARAGRAPH, f"Candidate {i}") for i in range(count)))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev = _ReadJev({"r": _choice("absent")})
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    if count == MAX_CHOICE_OPTIONS - 2:
        question = jev.requests[0][1]["r"]
        assert isinstance(question, ChoiceQuestion) and len(question.criteria) == MAX_CHOICE_OPTIONS
        assert not llm.calls
    else:
        # Only the focus pass runs; an overflowing page never gets a truncated choice.
        assert _only_focus(jev.requests) and len(llm.calls) == 1


async def test_tally_read_counts_unique_records_across_pages_and_closes_only_at_the_end() -> None:
    notes = Notes()
    first = capture((BlockKind.PARAGRAPH, "Quote one by Ada"), (BlockKind.PARAGRAPH, "Quote two by Ben"))
    last = capture((BlockKind.PARAGRAPH, "Quote one by Ada"), (BlockKind.PARAGRAPH, "Quote three by Ben"))
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "continues": [
                    {
                        "requirement_id": "r",
                        "records": [],
                        "tallies": [
                            {"key": "Ada", "records": [{"first": "s0", "last": "s0"}]},
                            {"key": "Ben", "records": [{"first": "s1", "last": "s1"}]},
                        ],
                    }
                ],
            },
            {
                "claims": [],
                "answered": True,
                "tallies": [
                    {
                        "requirement_id": "r",
                        "groups": [
                            {"key": "Ada", "records": [{"first": "s0", "last": "s0"}]},
                            {"key": "Ben", "records": [{"first": "s1", "last": "s1"}]},
                        ],
                        "complete": True,
                    }
                ],
            },
        ]
    )
    result = await read(llm, first, "Rank authors by count", ["r"], notes)
    assert result.continues == ("r",)
    assert not notes.evidenced("r")
    await read(llm, last, "Rank authors by count", ["r"], notes, continuing={"r"})
    assert [(t.key, t.count) for t in notes.tallies] == [("Ben", 2), ("Ada", 1)]
    assert notes.evidenced("r")
    assert "Quote one by Ada" not in llm.calls[1][1][-1].content.split("# Capture")[0]
    plan = Plan(
        requirements=(Requirement(id="r", text="Rank authors", kind=RequirementKind.INFORMATION),), answer_expected=True
    )
    draft = draft_answer(plan, notes)
    assert draft is not None and len(draft.citations) == 3
    assert draft.answer.index("Ben: 2") < draft.answer.index("Ada: 1")


@pytest.mark.parametrize("records_only", [False, True])
@pytest.mark.parametrize("last_matches", [False, True])
@pytest.mark.parametrize("plain_records", [False, True])
@pytest.mark.parametrize("kind", [BlockKind.RECORD, BlockKind.LIST_ITEM])
async def test_filtered_tally_counts_record_ranges_through_empty_pages_and_recaptures(
    records_only: bool, last_matches: bool, plain_records: bool, kind: BlockKind
) -> None:
    question = "How many open records belong to Ada across the whole list?"
    plan = Plan(
        requirements=(Requirement(id="r", text=question, kind=RequirementKind.INFORMATION, count_records=True),),
        answer_expected=True,
    )
    first = capture(
        (kind, "One - Ada - open"),
        (kind, "Two - Ada - open"),
        (kind, "Three - Ben - open"),
        (kind, "Four - Ada - open"),
        (kind, "Five - Ada - closed"),
    )
    group: dict[str, JsonValue] = {
        "key": "Ada",
        "records": [
            {"first": "s0", "last": "s1"},
            {"first": "s1", "last": "s1"},
            {"first": "s3", "last": "s3"},
        ],
    }
    notes = Notes()
    result = await read(
        ScriptedLLM(
            [
                {
                    "claims": [],
                    "answered": False,
                    "continues": [
                        {
                            "requirement_id": "r",
                            "through_end": True,
                            **({"records": group["records"]} if plain_records else {"tallies": [group]}),
                        }
                    ],
                }
            ]
        ),
        first,
        question,
        ["r"],
        notes,
        requirements=plan.requirements,
    )
    assert result.through_end == ("r",)
    assert notes.tallies[0].count == 3
    assert not notes.evidenced("r") and not notes.has_untallied_records("r")

    recaptured = capture(
        *((block.kind, first.text[block.start : block.end]) for block in first.blocks),
        (BlockKind.PARAGRAPH, "Updated footer"),
    )
    empty = capture((BlockKind.RECORD, "Six - Ben - open")).model_copy(update={"url": "https://example.test/2"})
    intermediate: tuple[tuple[Capture, JsonValue], ...] = (
        (recaptured, {**group, "key": None}),
        (empty, {"key": "Ada", "records": []}),
    )
    for page, page_group in intermediate:
        outcome = await read(
            ScriptedLLM([{"continues": [{"requirement_id": "r", "tallies": [page_group]}]}]),
            page,
            question,
            ["r"],
            Notes(),
            requirements=plan.requirements,
            records_only=True,
        )
        assert not outcome.incomplete and not outcome.uncovered
        outcome.merge_records(notes)
        assert notes.tallies[0].count == 3
        assert not notes.evidenced("r") and not notes.has_untallied_records("r")

    last = capture((BlockKind.RECORD, "Seven - Ada - open" if last_matches else "Seven - Ben - open")).model_copy(
        update={"url": "https://example.test/3"}
    )
    groups: list[JsonValue] = [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}] if last_matches else []}]
    response: JsonValue = (
        {
            "continues": [
                {
                    "requirement_id": "r",
                    **(
                        {"records": [{"first": "s0", "last": "s0"}] if last_matches else []}
                        if plain_records
                        else {"tallies": groups}
                    ),
                }
            ],
            "ended": ["r"],
        }
        if records_only
        else {"claims": [], "answered": True, "tallies": [{"requirement_id": "r", "groups": groups, "complete": True}]}
    )
    outcome = await read(
        ScriptedLLM([response]),
        last,
        question,
        ["r"],
        Notes() if records_only else notes,
        requirements=plan.requirements,
        records_only=records_only,
    )
    assert not outcome.incomplete and not outcome.uncovered
    if records_only:
        outcome.merge_records(notes)
        assert not notes.evidenced("r") and not notes.has_untallied_records("r")
        assert outcome.ended == ("r",)
        notes.complete_tallies("r")
    assert notes.evidenced("r")
    draft = draft_answer(plan, notes)
    assert draft is not None and draft.answer == f"{question}: {3 + last_matches}"
    assert draft.claims[0].evidence_ids == (fact_id(notes.supporting("r")[0][1]),)
    assert draft.claims[0].evidence_ids[0].startswith("tally:")
    assert [citation.quote for citation in draft.citations] == [
        "One - Ada - open",
        "Two - Ada - open",
        "Four - Ada - open",
        *(["Seven - Ada - open"] if last_matches else []),
    ]


def _field_tally(last: str = "s2") -> dict[str, JsonValue]:
    return {
        "key": None,
        "field": {"span": {"first": "s0", "last": last}, "prefix": "\nOwner: ", "suffix": "\nState:"},
    }


async def test_tally_field_copies_distinct_records_and_merges_captures_without_recounting() -> None:
    page = capture(
        *(
            (BlockKind.RECORD, f"Record {i}\nOwner: {owner}\nState: open")
            for i, owner in enumerate(["Ada", "Ben", "Ada"])
        )
    )
    notes = Notes()
    response: JsonValue = {"continues": [{"requirement_id": "r", "tallies": [_field_tally()]}]}
    second = capture(
        *(
            (BlockKind.RECORD, f"Another {i}\nOwner: {owner}\nState: open")
            for i, owner in enumerate(["Ada", "Ben", "Ada"])
        )
    ).model_copy(update={"url": page.url + "/2"})
    for current in [page, page, second]:
        isolated = Notes()
        result = await read(
            ScriptedLLM([response]),
            current,
            "Count by owner",
            ["r"],
            isolated,
            records_only=True,
        )
        assert not result.incomplete and not isolated.evidenced("r")
        for fact in isolated.facts:
            if fact.evidence is not None:
                e = fact.evidence
                assert e.quote == current.text[e.start : e.end] and e.url == current.url
        result.merge_records(notes)
    assert [(t.key, t.count) for t in notes.tallies] == [("Ada", 4), ("Ben", 2)]


@pytest.mark.parametrize("fault", ["paragraph", "missing", "ambiguous", "empty", "frame", "reversed", "cap"])
async def test_invalid_tally_field_cannot_close_or_partially_count_a_list(fault: str) -> None:
    parts = [(BlockKind.RECORD, f"Record {i}\nOwner: Ada\nState: open") for i in range(61 if fault == "cap" else 3)]
    if fault == "paragraph":
        parts[1] = (BlockKind.PARAGRAPH, parts[1][1])
    elif fault == "missing":
        parts[1] = (BlockKind.RECORD, "Record without an owner")
    elif fault == "ambiguous":
        parts[1] = (BlockKind.RECORD, parts[1][1] + "\nOwner: Ben\nState: open")
    elif fault == "empty":
        parts[1] = (BlockKind.RECORD, "Record\nOwner: \nState: open")
    page = capture(*parts)
    if fault == "frame":
        page = page.model_copy(
            update={
                "blocks": (page.blocks[0], page.blocks[1].model_copy(update={"frame_id": "other"}), *page.blocks[2:])
            }
        )
    field = _field_tally(f"s{len(parts) - 1}")
    if fault == "reversed":
        field["field"] = {"span": {"first": "s2", "last": "s0"}, "prefix": "\nOwner: ", "suffix": "\nState:"}
    response: JsonValue = {
        "claims": [],
        "answered": True,
        "tallies": [{"requirement_id": "r", "complete": True, "groups": [field]}],
    }
    notes = Notes()
    llm = ScriptedLLM([response, response])
    result = await read(llm, page, "Count by owner", ["r"], notes)
    assert result.incomplete == ("r",) and result.uncovered == 1
    assert not notes.evidenced("r") and not notes.tallies
    assert len(llm.calls) == 2 and len(result.cost_lines) == 2


@pytest.mark.parametrize("reuse", [False, True])
@pytest.mark.parametrize("count_records", [False, True])
@pytest.mark.parametrize("record_kind", [BlockKind.RECORD, BlockKind.LIST_ITEM])
async def test_tally_reader_requires_explicit_unfiltered_scope_and_revalidates_every_record(
    reuse: bool, count_records: bool, record_kind: BlockKind
) -> None:
    page = capture(
        *((record_kind, f"Record {i}\nOwner: {owner}\nState: open") for i, owner in enumerate(["Ada", "Ben", "Ada"]))
    )
    requirement = Requirement(
        id="r",
        text="Count all records" if count_records else "Count all records by owner",
        kind=RequirementKind.INFORMATION,
        count_records=count_records,
    )
    response: JsonValue = {
        "claims": [],
        "answered": False,
        "continues": [{"requirement_id": "r", "reuse_field": reuse, "through_end": True, "tallies": [_field_tally()]}],
    }
    result = await read(ScriptedLLM([response]), page, requirement.text, ["r"], Notes(), requirements=[requirement])
    assert bool(result.tally_readers) is reuse
    if not reuse:
        return
    counted = read_tallies(page, result.tally_readers, ["r"])
    assert counted is not None and not counted.cost_lines
    assert [(f.tally.key, f.tally.count) for f in counted.facts if f.tally] == (
        [(requirement.text, 3)] if count_records else [("Ada", 2), ("Ben", 1)]
    )
    assert not counted.ended
    for changed in [
        page.model_copy(update={"title": "Another list"}),
        page.model_copy(update={"inaccessible_frames": 1}),
        capture((BlockKind.RECORD, "A record without the field")),
        capture(
            (BlockKind.RECORD, "Record\nOwner: Ada\nState: open"),
            (BlockKind.PARAGRAPH, "Interrupted list"),
            (BlockKind.RECORD, "Record\nOwner: Ben\nState: open"),
        ),
        capture((BlockKind.RECORD, "Record\nOwner: Ada\nState: open"), (BlockKind.RECORD, "Unrelated record")),
        page.model_copy(
            update={"blocks": tuple(b.model_copy(update={"heading_path": ("Other",)}) for b in page.blocks)}
        ),
        page.model_copy(update={"text": page.text + "x" * 12000}),
    ]:
        assert read_tallies(changed, result.tally_readers, ["r"]) is None
    assert read_tallies(page, result.tally_readers, ["r", "other"]) is None


@pytest.mark.parametrize("incomplete", [False, True])
@pytest.mark.parametrize(("stated", "value"), [(False, "1"), (True, "3"), (True, "999")])
@pytest.mark.parametrize("with_context", [False, True])
async def test_a_quoted_whole_list_total_can_answer_beside_an_unfinished_tally(
    stated: bool, value: str, with_context: bool, incomplete: bool
) -> None:
    page = capture(
        (BlockKind.LIST_ITEM, "Item A"),
        (BlockKind.PARAGRAPH, "Total matching items: 3"),
        (BlockKind.PARAGRAPH, "Scope: all entries"),
    )
    notes = Notes()
    record = Fact(reader=FactReader.LLM, text="Item A", evidence=block_evidence(page, "s0"))
    notes.add(record)
    context = Fact(reader=FactReader.LLM, text="Scope: all entries", evidence=block_evidence(page, "s2"))
    notes.add(context)
    tally = notes.add_tally(Tally(requirement_id="r", key="items", records=(fact_id(record),)))
    requirement = Requirement(id="r", text="Total item count", kind=RequirementKind.INFORMATION, count_records=True)
    claim: JsonValue = {
        "requirement_id": "r",
        "text": value,
        "cite": {"first": "s1", "last": "s1"} if stated else None,
        "draws_on": ([] if stated else [fact_id(tally)]) + ([fact_id(context)] if with_context else []),
    }
    await read(
        ScriptedLLM([{"claims": [claim], "answered": True}]),
        page,
        "Total item count",
        ["r"],
        notes,
        requirements=[requirement],
        incomplete=["r"] if incomplete else [],
    )
    answered = stated and value == "3"
    assert notes.evidenced("r") is answered
    if answered:
        fact = next(f for f in notes.facts if f.requirement_id == "r")
        assert fact.evidence is not None and fact.evidence.quote == "Total matching items: 3"
        assert bool(fact.basis) is with_context


async def test_tally_field_and_a_range_of_the_same_records_leave_no_untallied_basis() -> None:
    page = capture(*((BlockKind.RECORD, f"Record {i}\nOwner: Ada\nState: open") for i in range(3)))
    notes = Notes()
    response: JsonValue = {
        "continues": [{"requirement_id": "r", "tallies": [_field_tally()], "records": [{"first": "s0", "last": "s2"}]}]
    }
    result = await read(ScriptedLLM([response]), page, "Count by owner", ["r"], Notes(), records_only=True)
    result.merge_records(notes)
    assert len(notes.comparison_records("r")) == 3
    assert not notes.has_untallied_records("r")


@pytest.fixture
def author_tallies() -> Notes:
    notes = Notes()
    authors = [author for author, count in enumerate((10, 9, 8, *([2] * 26), *([1] * 21))) for _ in range(count)]
    records = []
    for page_number in range(10):
        page = capture(
            *(
                (
                    BlockKind.RECORD,
                    f"Quote {number:03}: Reading gives us somewhere to go when we stay where we are; "
                    f"each page holds another life. By Author {authors[number]:02}.",
                )
                for number in range(page_number * 10, (page_number + 1) * 10)
            ),
            (BlockKind.PARAGRAPH, f"Page {page_number + 1} of 10. Quotes from all authors, without a tag filter."),
        ).model_copy(update={"url": f"https://quotes.toscrape.com/page/{page_number + 1}/"})
        for row in range(10):
            item = block_evidence(page, f"s{row}")
            fact = Fact(text=item.quote, evidence=item, reader=FactReader.LLM)
            notes.add(fact)
            records.append(fact_id(fact))
            notes.add_tally(
                Tally(
                    requirement_id="counts",
                    key=f"Author {authors[page_number * 10 + row]:02}",
                    records=(fact_id(fact),),
                )
            )
        context = block_evidence(page, "s10")
        notes.add(Fact(text=context.quote, evidence=context, reader=FactReader.LLM))
    notes.complete_tallies("counts")
    # A separate ranking requirement can refer to the records already counted under another requirement.
    notes.add(
        Fact(
            requirement_id="ranking",
            text="Author 00, Author 01 and Author 02 have the most quotes, with 10, 9 and 8 respectively.",
            evidence=None,
            basis=tuple(records),
            reader=FactReader.LLM,
        )
    )
    return notes


@pytest.mark.parametrize("json_encoded", [False, True])
def test_tallied_quotes_fit_protected_notes_budget(author_tallies: Notes, json_encoded: bool) -> None:
    notes = author_tallies
    rendered = notes.render_with_ids(15706, preserve_requirements=True, json_encoded=json_encoded)
    size = len(json.dumps(rendered.text)) - 2 if json_encoded else len(rendered.text)
    assert size <= 15706
    assert len(notes.tallies) == 50 and sum(tally.count for tally in notes.tallies) == 100
    assert "100 distinct records" in rendered.text
    assert "Author 00: 10" in rendered.text and "Author 01: 9" in rendered.text and "Author 02: 8" in rendered.text
    assert "Reading gives us" not in rendered.text
    assert "without a tag filter" in rendered.text
    assert notes.evidenced("counts") and notes.evidenced("ranking")
    assert {record for tally in notes.tallies for record in tally.records} <= set(
        notes.expand_evidence_ids(rendered.evidence_ids)
    )


def test_tally_claim_checks_judge_counted_records_by_their_tally(author_tallies: Notes) -> None:
    notes = author_tallies
    groups = notes.supporting("counts")[:3]
    answer = assemble_answer([Claim(text=fact.text, evidence_ids=(key,)) for key, fact in groups], notes, ())
    assert len(answer.citations) == 27
    questions = claim_check_questions(answer, notes)
    for index, (_, fact) in enumerate(groups):
        for issue in ("unsupported", "contradicted"):
            instructions = questions[f"{issue}_{index}"].instructions
            assert f"TALLY: {json.dumps(fact.text)}" in instructions
            assert not any(item.model_dump_json() in instructions for item in notes.evidence.values())
    assert {citation.quote for citation in answer.citations} == {
        item.quote
        for key, _ in groups
        for record in notes.expand_evidence_ids((key,))
        if (item := notes.evidence.get(record)) is not None
    }


def test_a_ranking_over_every_counted_record_leaves_the_omission_check_its_notes(author_tallies: Notes) -> None:
    notes = author_tallies
    plan = tuple(Requirement(id=key, text=key, kind=RequirementKind.INFORMATION) for key in ("counts", "ranking"))
    claims = [Claim(text=fact.text, evidence_ids=(key,)) for key, fact in notes.supporting("counts")[:3]]
    # The live answer ranked each leading author in its own claim, each citing every record.
    claims += [Claim(text=fact.text, evidence_ids=(key,)) for key, fact in notes.supporting("ranking")] * 3
    questions = claim_check_questions(assemble_answer(claims, notes, plan), notes)
    omitted = questions["requirement_omitted"].instructions.split("# Notes\n", 1)[1]
    assert "Author 00: 10" in omitted and "without a tag filter" in omitted


@pytest.mark.parametrize("caller", ["read", "compose", "fields", "done", "verify", "claims"])
async def test_tallied_quotes_fit_agent_render_paths(
    author_tallies: Notes, monkeypatch: pytest.MonkeyPatch, caller: str
) -> None:
    from fastbrowse.verification import llm_verify

    monkeypatch.setattr(TokenBudget, "remaining_chars", lambda *_: 15706)
    notes = author_tallies
    plan = Plan(
        requirements=tuple(
            Requirement(id=key, text=text, kind=RequirementKind.INFORMATION)
            for key, text in (("counts", "Count each author's quotes"), ("ranking", "Name the three leading authors"))
        ),
        answer_expected=True,
    )
    task = "Across every page, which three authors have the most quotes, and how many does each have?"
    page = capture((BlockKind.PARAGRAPH, "Page 10 of 10"))
    observation = Observation(
        url=page.url,
        title=page.title,
        page_key="last",
        captured_at=page.captured_at,
        controls=(),
        omitted_controls=0,
        viewport_text=page.text,
        tabs=(),
    )
    draft = assemble_answer(
        [Claim(text=fact.text, evidence_ids=(key,)) for key, fact in notes.supporting("counts")[:3]],
        notes,
        plan.requirements,
    )
    match caller:
        case "done":
            jev = _ReadJev({"complete": NoulAnswer(probability=0.95)})
            await check_done(jev, task, plan, observation, notes, Thresholds(), draft)
            state = jev.requests[0][0]
            assert isinstance(state, dict) and isinstance(state["notes"], str)
            rendered = state["notes"]
        case "claims":
            questions = claim_check_questions(draft, notes)
            rendered = questions["requirement_omitted"].instructions.split("# Notes\n", 1)[1]
        case _:
            response: JsonValue
            match caller:
                case "read":
                    response = {"claims": [], "answered": False}
                case "compose":
                    response = {"claims": [claim.model_dump(mode="json") for claim in draft.claims]}
                case "fields":
                    response = {"fields": []}
                case _:
                    response = {"missing": [], "ungrounded": [], "complete": True}
            llm = ScriptedLLM([response])
            match caller:
                case "read":
                    await read(llm, page, task, ["counts", "ranking"], notes)
                case "compose":
                    answer = (await compose(llm, task, plan, notes)).data
                    assert len(answer.citations) == 27 and answer.dropped_claims == 0
                case "fields":
                    await propose_text_fields_from_notes(llm, task, notes, {"author": Field(description="Top author")})
                case _:
                    await llm_verify(llm, task, plan, observation, (), notes, ())
            rendered = llm.calls[0][1][-1].content
    assert "Author 00: 10" in rendered and "100 distinct records" in rendered
    assert "Reading gives us" not in rendered


@pytest.mark.parametrize("requirement_id", ["r", "other"])
async def test_completed_tally_leaves_earlier_continuation_records_open(requirement_id: str) -> None:
    notes = Notes()
    first = capture((BlockKind.RECORD, "Quote one by Ada"), (BlockKind.RECORD, "Quote two by Ada"))
    last = capture((BlockKind.RECORD, "Quote three by Ada"))
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "continues": [
                    {
                        "requirement_id": requirement_id,
                        "records": [{"first": "s0", "last": "s0"}, {"first": "s1", "last": "s1"}],
                    }
                ],
            },
            {
                "claims": [],
                "answered": True,
                "tallies": [
                    {
                        "requirement_id": "r",
                        "groups": [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}]}],
                        "complete": True,
                    }
                ],
            },
        ]
    )
    await read(llm, first, "Count Ada's quotes", [requirement_id], notes)
    result = await read(llm, last, "Count Ada's quotes", ["r"], notes, continuing={"r"})
    assert len(notes.evidence) == 3
    assert notes.evidenced("r") is (requirement_id != "r")
    assert result.incomplete == (("r",) if requirement_id == "r" else ())


@pytest.mark.parametrize("missing", [True, False])
async def test_partial_tallies_never_close_a_requirement_from_a_generated_total(missing: bool) -> None:
    notes = Notes()
    page = capture((BlockKind.PARAGRAPH, "A quote by Ada"))
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"cite": {"first": "s0", "last": "s0"}, "text": "Ada has 999 quotes", "requirement_id": "r"}
                ],
                "answered": True,
                "tallies": [
                    {
                        "requirement_id": "r",
                        "complete": missing,
                        "groups": [
                            {"key": "Ada", "records": [{"first": "missing" if missing else "s0", "last": "s0"}]},
                        ],
                    }
                ],
            }
        ]
    )
    await read(llm, page, "Count quotes", ["r"], notes)
    assert not notes.evidenced("r")


async def test_tally_chunks_count_overlap_once_and_keep_completion_open_until_last_chunk() -> None:
    notes = Notes()
    page = capture(*((BlockKind.PARAGRAPH, f"Quote {number} by Ada") for number in range(4)))
    parts = chunk(page, 35)
    assert len(parts) > 1
    responses: list[JsonValue] = [
        {
            "claims": [],
            "answered": True,
            "tallies": [
                {
                    "requirement_id": "r",
                    "complete": True,
                    "groups": [
                        {"key": "Ada", "records": [{"first": key, "last": key} for key in part.block_ids]},
                    ],
                }
            ],
        }
        for part in parts
    ]
    llm = ScriptedLLM(responses)
    result = await read(llm, page, "Count Ada's quotes", ["r"], notes, max_chars=35)
    assert len(result.coverage) == len(parts)
    assert notes.tallies[0].count == 4
    assert notes.evidenced("r")


async def test_a_failed_tally_read_reopens_previous_coverage() -> None:
    notes = Notes()
    page = capture((BlockKind.RECORD, "Quote one by Ada"))
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": True,
                "tallies": [
                    {
                        "requirement_id": "r",
                        "complete": True,
                        "groups": [
                            {"key": "Ada", "records": [{"first": "s0", "last": "s0"}]},
                        ],
                    }
                ],
            },
            {
                "claims": [],
                "answered": True,
                "tallies": [
                    {
                        "requirement_id": "r",
                        "complete": True,
                        "groups": [
                            {"key": "Ada", "records": [{"first": "absent", "last": "absent"}]},
                        ],
                    }
                ],
            },
        ]
    )
    await read(llm, page, "Count quotes", ["r"], notes)
    assert notes.evidenced("r")
    result = await read(llm, page, "Count quotes", ["r"], notes)
    assert result.incomplete == ("r",)
    assert not notes.evidenced("r")


async def test_a_record_split_across_chunks_counts_once() -> None:
    notes = Notes()
    page = capture((BlockKind.RECORD, "By Ada: first line\nBy Ada: second line\nBy Ada: third line"))
    parts = chunk(page, 20)
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": True,
                "tallies": [
                    {
                        "requirement_id": "r",
                        "complete": True,
                        "groups": [
                            {"key": "Ada", "records": [{"first": "s0", "last": "s0"}]},
                        ],
                    }
                ],
            }
            for _ in parts
        ]
    )
    await read(llm, page, "Count quotes", ["r"], notes, max_chars=20)
    assert notes.evidenced("r")
    assert notes.tallies[0].count == 1
    assert next(iter(notes.evidence.values())).quote == page.text


async def test_isolated_records_merge_tallies_without_closing_or_double_counting_recaptures() -> None:
    pages = [
        capture((BlockKind.RECORD, "Ada: one"), (BlockKind.RECORD, "Ben: two")),
        capture((BlockKind.RECORD, "Ada: one"), (BlockKind.RECORD, "Ben: two"), (BlockKind.PARAGRAPH, "redrawn")),
        capture((BlockKind.RECORD, "Ada: three")).model_copy(update={"url": "https://example.test/2"}),
    ]
    responses: list[JsonValue] = [
        {
            "continues": [
                {
                    "requirement_id": "r",
                    "tallies": [
                        {"key": "Ada", "records": [{"first": "s0", "last": "s0"}]},
                        {"key": "Ben", "records": [{"first": "s1", "last": "s1"}]},
                    ],
                }
            ],
        },
    ] * 2 + [
        {
            "continues": [
                {
                    "requirement_id": "r",
                    "tallies": [
                        {"key": "Ada", "records": [{"first": "s0", "last": "s0"}]},
                    ],
                }
            ],
        },
    ]
    notes = Notes()
    for page, response in zip(pages, responses, strict=True):
        isolated = Notes()
        outcome = await read(
            ScriptedLLM([response]), page, "Count records by author", ["r"], isolated, records_only=True
        )
        assert not isolated.evidenced("r")
        outcome.merge_records(notes)
        assert not notes.evidenced("r")
    assert [(tally.key, tally.count) for tally in notes.tallies] == [("Ada", 2), ("Ben", 1)]
    notes.complete_tallies("r")
    assert [item.quote for item in notes.supporting_evidence("r")] == ["Ada: one", "Ben: two", "Ada: three"]


@pytest.mark.parametrize("count_records", [False, True])
async def test_isolated_continuation_metadata_and_lost_records_survive_merge(count_records: bool) -> None:
    page = capture((BlockKind.RECORD, "Ada: one"))
    requirement = Requirement(
        id="r", text="Count Ada's records", kind=RequirementKind.INFORMATION, count_records=count_records
    )
    result = await read(
        ScriptedLLM(
            [
                {
                    "continues": [
                        {
                            "requirement_id": "r",
                            "records": [{"first": "s0", "last": "s0"}, {"first": "missing", "last": "missing"}],
                        }
                    ],
                }
            ]
        ),
        page,
        "Count Ada's records",
        ["r"],
        Notes(),
        requirements=[requirement],
        records_only=True,
    )
    notes = Notes()
    result.merge_records(notes)
    assert result.incomplete == ("r",) and result.uncovered == 1
    assert notes.has_untallied_records("r") is not count_records
    last = capture((BlockKind.RECORD, "Ada: two"))
    completed = await read(
        ScriptedLLM(
            [
                {
                    "answered": True,
                    "claims": [],
                    "tallies": [
                        {
                            "requirement_id": "r",
                            "complete": True,
                            "groups": [{"key": "Ada", "records": [{"first": "s0", "last": "s0"}]}],
                        }
                    ],
                }
            ]
        ),
        last,
        "Count Ada's records",
        ["r"],
        notes,
        requirements=[requirement],
        continuing={"r"},
        incomplete=result.incomplete,
    )
    assert completed.incomplete == (() if count_records else ("r",))
    assert not notes.evidenced("r")


async def test_final_page_refuses_to_drop_earlier_records_to_fit_prompt() -> None:
    from fastbrowse.memory import NotesTooLarge

    earlier = capture((BlockKind.RECORD, "earlier " * 2000))
    evidence = block_evidence(earlier, "s0")
    notes = Notes([Fact(text=evidence.quote, evidence=evidence, reader=FactReader.LLM)])
    llm = ScriptedLLM([])
    with pytest.raises(NotesTooLarge, match=r"notes budget|every earlier record"):
        await read(
            llm,
            capture((BlockKind.RECORD, "last")),
            "Compare all pages",
            ["r"],
            notes,
            tokens=TokenBudget(state_plus_largest_question=5000),
            require_all_evidence=True,
        )
    assert not llm.calls


@pytest.mark.parametrize("missing", ["requirement", "context", "none"])
async def test_record_read_keeps_empty_sets_apart_from_missing_evidence(missing: str) -> None:
    page = capture((BlockKind.HEADING, "Active items"), (BlockKind.RECORD, "Ada: $3"))
    response: JsonValue = {
        "continues": [] if missing == "requirement" else [{"requirement_id": "r", "records": []}],
        "context": [{"first": "missing" if missing == "context" else "s0", "last": "s0"}],
    }
    notes = Notes()
    result = await read(ScriptedLLM([response]), page, "Find matches", ["r"], notes, records_only=True)
    result.merge_records(notes)
    assert not notes.evidenced("r")
    assert result.incomplete == (() if missing == "none" else ("r",))
    if missing != "context":
        quote = notes.facts[0].evidence
        assert quote is not None and quote.quote == page.text[quote.start : quote.end]
        assert quote.url == page.url and quote.capture_sha256 == page.sha256


async def test_final_comparison_cannot_drop_earlier_records_from_its_basis() -> None:
    earlier = capture((BlockKind.RECORD, "Oak $19"), (BlockKind.RECORD, "Pine $7"))
    notes = Notes()
    for block in earlier.blocks:
        evidence = block_evidence(earlier, block.source_id)
        fact = Fact(text=evidence.quote, evidence=evidence, reader=FactReader.LLM)
        notes.add(fact)
        notes.add_continuation("r", fact_id(fact))
    last = capture((BlockKind.RECORD, "Elm $15")).model_copy(update={"url": "https://example.test/last"})
    await read(
        ScriptedLLM(
            [
                {
                    "answered": True,
                    "claims": [
                        {
                            "requirement_id": "r",
                            "text": "Pine is cheapest at $7",
                            "cite": None,
                            "draws_on": [],
                            "records": [{"first": "s0", "last": "s0"}],
                        }
                    ],
                }
            ]
        ),
        last,
        "Find the cheapest item",
        ["r"],
        notes,
        require_all_evidence=True,
    )
    assert [e.quote for e in notes.supporting_evidence("r")] == ["Oak $19", "Pine $7", "Elm $15"]
    assert [e.url for e in notes.supporting_evidence("r")] == [earlier.url, earlier.url, last.url]


async def test_final_comparison_keeps_records_from_earlier_chunks_too() -> None:
    page = capture((BlockKind.RECORD, "Oak $19"), (BlockKind.RECORD, "Pine $7"))
    notes = Notes()
    llm = ScriptedLLM(
        [
            {
                "answered": False,
                "claims": [],
                "continues": [{"requirement_id": "r", "records": [{"first": "s0", "last": "s0"}]}],
            },
            {
                "answered": True,
                "claims": [
                    {
                        "requirement_id": "r",
                        "text": "Pine is cheapest at $7",
                        "cite": None,
                        "records": [{"first": "s1", "last": "s1"}],
                    }
                ],
            },
        ]
    )
    await read(llm, page, "Find the cheapest item", ["r"], notes, max_chars=8, require_all_evidence=True)
    assert [e.quote for e in notes.supporting_evidence("r")] == ["Oak $19", "Pine $7"]


@pytest.mark.parametrize("order,expected", [("lowest", "Beta: $2.00"), ("highest", "Alpha: $10.00")])
def test_numeric_comparison_uses_decimal_values_and_preserves_every_source(order: str, expected: str) -> None:
    from fastbrowse.comparison import NumericComparison, complete_comparison

    notes = Notes()
    pages = [capture((BlockKind.RECORD, text)) for text in ("Alpha: $10.00", "Beta: $2.00")]
    for index, page in enumerate(pages):
        page = page.model_copy(update={"url": f"https://example.test/{index}"})
        fact = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
        notes.add(fact)
        notes.add_continuation("r", fact_id(fact))
    spec = NumericComparison.model_validate(
        {"order": order, "limit": 1, "label": {"prefix": "", "suffix": ": "}, "value": {"prefix": ": ", "suffix": ""}}
    )
    assert complete_comparison(notes, "r", spec)
    assert notes.supporting("r")[0][1].text.endswith(expected)
    evidence = notes.supporting_evidence("r")
    assert [e.url for e in evidence] == ["https://example.test/0", "https://example.test/1"]
    assert [e.quote for e in evidence] == [p.text for p in pages]


@pytest.mark.parametrize(
    "records",
    [
        [],
        ["A: $2", "B: $2"],
        ["A: $2", "A: $3"],
        ["A: $2", "B: 3"],
        ["A: $2", "B: $1,000"],
        ["A: $2", "B: $NaN"],
        ["A: $2", "B: $1 to $3"],
        ["A: $2", "B: $3\nOther: $1"],
    ],
)
def test_numeric_comparison_falls_back_for_missing_ambiguous_or_tied_values(records: list[str]) -> None:
    from fastbrowse.comparison import NumericComparison, complete_comparison

    notes = Notes()
    for text in records:
        page = capture((BlockKind.RECORD, text))
        fact = Fact(text=text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
        notes.add(fact)
        notes.add_continuation("r", fact_id(fact))
    spec = NumericComparison.model_validate(
        {
            "order": "lowest",
            "limit": 1,
            "label": {"prefix": "", "suffix": ": "},
            "value": {"prefix": ": ", "suffix": ""},
        }
    )
    assert not complete_comparison(notes, "r", spec)
    assert not notes.evidenced("r")


@pytest.mark.parametrize("count_records", [False, True])
@pytest.mark.parametrize("lost_record", [False, True])
async def test_numeric_comparison_requires_explicit_full_list_scope(count_records: bool, lost_record: bool) -> None:
    page = capture((BlockKind.RECORD, "A: $2"))
    comparison: JsonValue = {
        "order": "lowest",
        "limit": 1,
        "label": {"prefix": "", "suffix": ": "},
        "value": {"prefix": ": ", "suffix": ""},
    }
    for through_end in (False, True):
        response: JsonValue = {
            "claims": [],
            "answered": False,
            "continues": [
                {
                    "requirement_id": "r",
                    "through_end": through_end,
                    "comparison": comparison,
                    "records": [{"first": "absent" if lost_record else "s0", "last": "s0"}],
                }
            ],
        }
        result = await read(
            ScriptedLLM([response]),
            page,
            "Find the cheapest",
            ["r"],
            Notes(),
            requirements=[
                Requirement(
                    id="r", text="Find the cheapest", kind=RequirementKind.INFORMATION, count_records=count_records
                )
            ],
        )
        assert bool(result.comparisons) is (through_end and not lost_record and not count_records)


@pytest.mark.parametrize(
    "prefix,suffix,quote,expected",
    [
        ("", "", "Alpha", "Alpha"),
        ("Name: ", "\n", "Name: Alpha\nPrice: $3", "Alpha"),
        ("[", "]", "[Alpha] [Beta]", None),
        ("[", "]", "[Alpha [Beta]", None),
        ("|", "|", "|Alpha|Beta|", None),
        ("[", "]", "[Alpha\nBeta]", None),
        ("[", "]", "[]", None),
        ("[", "]", "Alpha", None),
    ],
)
def test_comparison_fields_refuse_ambiguous_delimiters(
    prefix: str, suffix: str, quote: str, expected: str | None
) -> None:
    from fastbrowse.comparison import QuotedField

    assert QuotedField(prefix=prefix, suffix=suffix).extract(quote) == expected


@pytest.mark.parametrize("limit,expected", [(1, None), (2, "Highest 2:\nAlpha: $10\nBeta: $10"), (4, None)])
def test_numeric_top_n_requires_enough_records_and_an_unambiguous_cutoff(limit: int, expected: str | None) -> None:
    from fastbrowse.comparison import NumericComparison, QuotedField, complete_comparison

    page = capture(*((BlockKind.RECORD, text) for text in ("Alpha: $10", "Beta: $10", "Gamma: $2")))
    notes = Notes()
    for block in page.blocks:
        evidence = block_evidence(page, block.source_id)
        fact = Fact(text=evidence.quote, evidence=evidence, reader=FactReader.LLM)
        notes.add(fact)
        notes.add_continuation("r", fact_id(fact))
    spec = NumericComparison(
        order="highest",
        limit=limit,
        label=QuotedField(prefix="", suffix=": "),
        value=QuotedField(prefix=": ", suffix=""),
    )
    assert complete_comparison(notes, "r", spec) is (expected is not None)
    assert notes.evidenced("r") is (expected is not None)
    if expected is not None:
        assert notes.supporting("r")[0][1].text == expected


def test_counted_comparison_quotes_remain_in_notes_and_cannot_be_dropped_to_fit() -> None:
    from fastbrowse.comparison import NumericComparison, QuotedField, complete_comparison
    from fastbrowse.memory import NotesTooLarge

    page = capture((BlockKind.RECORD, "Alpha: $10"), (BlockKind.RECORD, "Beta: $2"))
    notes = Notes()
    for block in page.blocks:
        evidence = block_evidence(page, block.source_id)
        fact = Fact(text=evidence.quote, evidence=evidence, reader=FactReader.LLM)
        notes.add(fact)
        notes.add_continuation("rank", fact_id(fact))
        notes.add_tally(Tally(requirement_id="count", key="Matching records", records=(fact_id(fact),)))
    spec = NumericComparison(
        order="highest", limit=1, label=QuotedField(prefix="", suffix=": "), value=QuotedField(prefix=": ", suffix="")
    )
    assert complete_comparison(notes, "rank", spec)
    notes.complete_tallies("count")
    rendered = notes.render(10_000, preserve_requirements=True)
    assert "Alpha: $10" in rendered and "Beta: $2" in rendered
    with pytest.raises(NotesTooLarge):
        notes.render(len(rendered) - 1, preserve_requirements=True)


async def test_tally_field_counts_leaf_list_items_from_their_start_boundary() -> None:
    page = capture((BlockKind.LIST_ITEM, "Kettle - GBP 34.50"), (BlockKind.LIST_ITEM, "Mug - GBP 8.75"))
    notes = Notes()
    outcome = await read(
        ScriptedLLM(
            [
                {
                    "answered": True,
                    "claims": [],
                    "tallies": [
                        {
                            "requirement_id": "r",
                            "complete": True,
                            "groups": [
                                {
                                    "key": None,
                                    "field": {"span": {"first": "s0", "last": "s1"}, "prefix": "", "suffix": " - GBP "},
                                }
                            ],
                        }
                    ],
                }
            ]
        ),
        page,
        "Count products",
        ["r"],
        notes,
        requirements=[Requirement(id="r", text="Count products", kind=RequirementKind.INFORMATION, count_records=True)],
    )
    assert not outcome.incomplete
    assert notes.evidenced("r")
    assert notes.tallies[0].count == 2


async def test_record_count_ignores_group_totals_and_counts_matching_children() -> None:
    page = capture(
        (BlockKind.HEADING, "Available packages: 2"),
        (BlockKind.LIST_ITEM, "Oak - available"),
        (BlockKind.LIST_ITEM, "Elm - available"),
        (BlockKind.HEADING, "Archived packages: 2"),
        (BlockKind.LIST_ITEM, "Pine - archived"),
        (BlockKind.LIST_ITEM, "Birch - archived"),
    )
    jev = _ReadJev({"r": _choice("c0")})
    llm = ScriptedLLM(
        [
            {
                "answered": True,
                "claims": [],
                "tallies": [
                    {
                        "requirement_id": "r",
                        "complete": True,
                        "groups": [
                            {
                                "key": None,
                                "records": [{"first": f"s{i}", "last": f"s{i}"} for i in (1, 2, 4, 5)],
                            }
                        ],
                    }
                ],
            }
        ]
    )
    notes = Notes()
    requirement = Requirement(id="r", text="Count every package", kind=RequirementKind.INFORMATION, count_records=True)
    outcome = await read(llm, page, requirement.text, ["r"], notes, jev=jev, requirements=(requirement,))
    assert not outcome.incomplete
    assert notes.evidenced("r")
    assert sum(tally.count for tally in notes.tallies) == 4
    assert len(llm.calls) == 1


@pytest.mark.parametrize("limit", [None, "calls", "dollars"])
async def test_a_malformed_tally_field_is_repaired_before_it_poisons_the_count(limit: str | None) -> None:
    page = capture((BlockKind.LIST_ITEM, "Entry A"), (BlockKind.PARAGRAPH, "List finished"))
    invalid: JsonValue = {
        "answered": True,
        "claims": [],
        "tallies": [
            {
                "requirement_id": "r",
                "complete": True,
                "groups": [
                    {
                        "key": None,
                        "field": {"span": {"first": "s0", "last": "s1"}, "prefix": "", "suffix": ""},
                    }
                ],
            }
        ],
    }
    corrected: JsonValue = {
        "answered": True,
        "claims": [],
        "tallies": [
            {
                "requirement_id": "r",
                "complete": True,
                "groups": [
                    {
                        "key": None,
                        "records": [{"first": "s0", "last": "s0"}],
                    }
                ],
            }
        ],
    }
    notes = Notes()
    llm = ScriptedLLM([invalid, corrected])
    ledger = Ledger(Limits(max_llm_calls=1) if limit == "calls" else Limits(max_dollars=0.001) if limit else Limits())
    if limit:
        with pytest.raises(BudgetExceeded):
            await read(llm, page, "Count entries", ["r"], notes, ledger=ledger)
        assert len(llm.calls) == 1 and ledger.breakdown().known_dollars == 0.001
        assert not notes.evidenced("r") and not notes.tallies
        return
    outcome = await read(llm, page, "Count entries", ["r"], notes, ledger=ledger)
    assert notes.evidenced("r")
    assert not outcome.incomplete
    assert outcome.uncovered == 0
    assert len(llm.calls) == 2
    assert len(outcome.cost_lines) == 2
    assert ledger.llm_calls == 2 and ledger.breakdown().known_dollars == 0.002


@pytest.mark.parametrize(
    "basis_kind", ["tally", "record", "derived", "untracked", "claim_record", "continuation", "prior_context"]
)
async def test_a_partial_count_cannot_borrow_a_matching_number_from_page_context(basis_kind: str) -> None:
    page = capture(
        *((BlockKind.LIST_ITEM, f"Entry {label}") for label in ("A", "B", "C")),
        (BlockKind.PARAGRAPH, "Page 3 of 10"),
        (BlockKind.LIST_ITEM, "Another entry"),
    )
    notes = Notes()
    records = [
        Fact(reader=FactReader.LLM, text=f"Entry {label}", evidence=block_evidence(page, f"s{i}"))
        for i, label in enumerate(("A", "B", "C"))
    ]
    for record in records:
        notes.add(record)
    tally = notes.add_tally(Tally(requirement_id="r", key="entries", records=tuple(fact_id(r) for r in records)))
    basis = tally if basis_kind == "tally" else records[0]
    if basis_kind == "derived":
        basis = Fact(
            reader=FactReader.LLM,
            text="A contextual conclusion",
            evidence=block_evidence(page, "s3"),
            basis=(fact_id(records[0]),),
        )
        notes.add(basis)
    elif basis_kind == "untracked":
        basis = Fact(reader=FactReader.LLM, text="Another entry", evidence=block_evidence(page, "s4"))
        notes.add(basis)
    elif basis_kind == "continuation":
        basis = Fact(reader=FactReader.LLM, text="Page 3 of 10", evidence=block_evidence(page, "s3"))
        notes.add(basis)
        notes.add_continuation("r", fact_id(basis))
    elif basis_kind == "prior_context":
        prior = capture((BlockKind.PARAGRAPH, "Scope: all entries"))
        basis = Fact(reader=FactReader.LLM, text=prior.text, evidence=block_evidence(prior, "s0"))
        notes.add(basis)
    requirement = Requirement(id="r", text="Total entry count", kind=RequirementKind.INFORMATION, count_records=True)
    await read(
        ScriptedLLM(
            [
                {
                    "claims": [
                        {
                            "requirement_id": "r",
                            "text": "3",
                            "cite": {"first": "s3", "last": "s3"},
                            "draws_on": [] if basis_kind == "claim_record" else [fact_id(basis)],
                            "records": [{"first": "s4", "last": "s4"}] if basis_kind == "claim_record" else [],
                        }
                    ],
                    "answered": True,
                }
            ]
        ),
        page,
        requirement.text,
        ["r"],
        notes,
        requirements=[requirement],
    )
    assert not notes.evidenced("r")


@pytest.mark.parametrize(
    "probability,skip",
    [(0.1, True), (0.14, True), (0.18, True), (0.2, True), (0.21, False), (0.7, False), (None, False)],
)
async def test_repeated_page_read_requires_confident_novelty_assessment(probability: float | None, skip: bool) -> None:
    page = capture((BlockKind.PARAGRAPH, "Price: GBP25.99"))
    fact = Fact(text="Price: GBP25.99", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    answers: dict[str, Answer] = {"r": _choice("synthesis")}
    if probability is not None:
        answers["novelty"] = NoulAnswer(probability=probability)
    jev = _ReadJev(answers)
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    ledger = Ledger(Limits())
    result = await read(llm, page, requirement.text, ["r"], notes, jev=jev, requirements=(requirement,), ledger=ledger)
    assert len(llm.calls) == (0 if skip else 1)
    assert len(jev.requests) == ledger.jev_calls == 1
    state, questions = jev.requests[0]
    assert "novelty" in questions
    assert isinstance(state, dict)
    previous = state["previous"]
    assert isinstance(previous, list) and isinstance(previous[0], dict)
    assert fact.evidence is not None and previous[0]["quote"] == fact.evidence.quote
    assert not notes.evidenced("r") and notes.facts == (fact,)
    assert result.cost_lines[0].component is CostComponent.JEV


async def test_requirement_named_novelty_cannot_skip_a_read() -> None:
    page = capture((BlockKind.PARAGRAPH, "Price: GBP25.99"))
    notes = Notes((Fact(text="Price", evidence=block_evidence(page, "s0"), reader=FactReader.LLM),))
    requirement = Requirement(id="novelty", text="Find the price", kind=RequirementKind.INFORMATION)
    jev = _ReadJev({"novelty": NoulAnswer(probability=0)})
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    await read(llm, page, requirement.text, ["novelty"], notes, jev=jev, requirements=(requirement,))
    assert len(llm.calls) == 1
    assert isinstance(jev.requests[0][1]["novelty"], ChoiceQuestion)


async def test_previous_quotes_from_another_address_do_not_suppress_reading() -> None:
    page = capture((BlockKind.PARAGRAPH, "Price: GBP25.99"))
    earlier = block_evidence(page, "s0").model_copy(update={"url": "https://example.test/other"})
    notes = Notes((Fact(text="Price", evidence=earlier, reader=FactReader.LLM),))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev = _ReadJev({"r": _choice("synthesis"), "novelty": NoulAnswer(probability=0)})
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    await read(llm, page, requirement.text, ["r"], notes, jev=jev, requirements=(requirement,))
    assert len(llm.calls) == 1 and "novelty" not in jev.requests[0][1]


async def test_novelty_is_omitted_when_comparing_quotes_exceeds_input_budget() -> None:
    page = capture((BlockKind.PARAGRAPH, "Price: GBP25.99"))
    earlier = block_evidence(page, "s0").model_copy(update={"quote": "Price: GBP25.99 " * 5000})
    notes = Notes((Fact(text="Price", evidence=earlier, reader=FactReader.LLM),))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev = _ReadJev({"r": _choice("synthesis"), "novelty": NoulAnswer(probability=0)})
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    await read(
        llm,
        page,
        requirement.text,
        ["r"],
        notes,
        jev=jev,
        requirements=(requirement,),
        tokens=TokenBudget(state_plus_largest_question=10000, state_plus_all_questions=10000),
    )
    assert len(llm.calls) == 1 and "novelty" not in jev.requests[0][1]


async def test_reopened_requirement_reads_earlier_quotes_again() -> None:
    page = capture((BlockKind.PARAGRAPH, "Price: GBP25.99"))
    notes = Notes((Fact(text="Price", evidence=block_evidence(page, "s0"), requirement_id="r", reader=FactReader.LLM),))
    notes.unevidence(("r",))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev = _ReadJev({"r": _choice("synthesis"), "novelty": NoulAnswer(probability=0)})
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    await read(llm, page, requirement.text, ["r"], notes, jev=jev, requirements=(requirement,))
    assert len(llm.calls) == 1 and "novelty" not in jev.requests[0][1]


async def test_independent_claim_checks_fit_separate_parallel_batches() -> None:
    from fastbrowse.verification import check_claims

    class SmallRequests:
        active = 0
        peak = 0
        calls = 0

        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            if len(questions) > 1:
                raise JevInputTooLarge("individual checks fit, the combined request does not")
            self.calls += 1
            self.active += 1
            self.peak = max(self.peak, self.active)
            await asyncio.sleep(0)
            self.active -= 1
            return Evaluation(
                model="test",
                answers={key: NoulAnswer(probability=0.0) for key in questions},
                input_tokens=1,
                cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.001),
            )

    page = capture((BlockKind.PARAGRAPH, "Black pen GBP7"))
    fact = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    composed = assemble_answer((Claim(text=fact.text, evidence_ids=(fact_id(fact),)),), notes, ())
    jev, ledger = SmallRequests(), Ledger(Limits())
    held = await check_claims(jev, composed, notes, Thresholds(), tokens=TokenBudget(batch_tokens=1), ledger=ledger)
    assert held is composed and jev.calls == 2 and jev.peak == 2
    assert ledger.jev_calls == 2 and ledger.breakdown().known_dollars == pytest.approx(0.002)


async def test_a_claim_check_that_cannot_fit_cannot_verify_an_answer() -> None:
    from fastbrowse.verification import check_claims
    from tests.test_policy import ScriptedJev

    page = capture((BlockKind.PARAGRAPH, "Black pen GBP7"))
    fact = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    composed = assemble_answer((Claim(text=fact.text, evidence_ids=(fact_id(fact),)),), notes, ())
    jev = ScriptedJev({}, noul=0.0)
    held = await check_claims(jev, composed, notes, Thresholds(), tokens=TokenBudget(state_plus_largest_question=1))
    assert held is None and jev.requests == []


async def test_omission_evidence_fits_when_unrelated_claim_questions_need_other_batches() -> None:
    from fastbrowse.verification import check_claims
    from tests.test_policy import ScriptedJev

    page = capture((BlockKind.PARAGRAPH, "Black pen GBP7. " + "description " * 300))
    requirement = Requirement(id="r1", text="Describe the item and its price", kind=RequirementKind.INFORMATION)
    fact = Fact(requirement_id="r1", text="Black pen GBP7", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    claims = tuple(
        Claim(text=text, evidence_ids=(fact_id(fact),))
        for text in ("The pen is black.", "It costs GBP7.", "The item is a pen.")
    )
    composed = assemble_answer(claims, notes, (requirement,))
    tokens = TokenBudget(state_plus_largest_question=5000, state_plus_all_questions=5000, batch_tokens=1000)
    jev = ScriptedJev({}, noul=0.0)
    assert await check_claims(jev, composed, notes, Thresholds(), tokens=tokens) is composed
    omission = next(q["requirement_omitted"] for q in jev.requests if "requirement_omitted" in q)
    assert page.text in omission.instructions
    assert len(jev.requests) > 1


async def test_a_quoted_count_does_not_repair_another_requirements_missing_records() -> None:
    page = capture((BlockKind.PARAGRAPH, "Total matching items: 3"))
    notes = Notes()
    requirements = [
        Requirement(id="count", text="Total item count", kind=RequirementKind.INFORMATION, count_records=True),
        Requirement(id="names", text="Names of all matching items", kind=RequirementKind.INFORMATION),
    ]
    claims: JsonValue = [
        {"requirement_id": key, "text": "3", "cite": {"first": "s0", "last": "s0"}} for key in ("count", "names")
    ]
    await read(
        ScriptedLLM([{"claims": claims, "answered": True}]),
        page,
        "Total item count and item names",
        ["count", "names"],
        notes,
        requirements=requirements,
        incomplete=["count", "names"],
    )
    assert notes.evidenced("count")
    assert not notes.evidenced("names")


@pytest.mark.parametrize(
    ("probability", "judgment", "expected"),
    [
        (0.99, "yes", True),
        (0.99, "no", False),
        (0.5, "yes", True),
        (0.5, "no", False),
        (0.5, None, False),
        (0.01, "yes", True),
        (None, "yes", False),
    ],
)
async def test_atomic_outputs_use_source_audits_instead_of_confidence_alone(
    probability: float | None,
    judgment: str | None,
    expected: bool,
) -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import check_answer_outputs

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            answers = (
                {}
                if probability is None
                else {
                    key: ChoiceAnswer(choice="claim_0", probabilities={"claim_0": 1.0}, confidence=1.0)
                    if isinstance(question, ChoiceQuestion)
                    else NoulAnswer(probability=probability)
                    for key, question in questions.items()
                }
            )
            return Evaluation(
                model="test",
                answers=answers,
                input_tokens=1,
                cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0),
            )

    page = capture((BlockKind.PARAGRAPH, "Price £12"))
    notes = Notes((Fact(reader=FactReader.LLM, text="£12", evidence=block_evidence(page, "s0")),))
    answer = assemble_answer((Claim(text="It costs £12.", evidence_ids=tuple(notes.evidence)),), notes, ())
    llm = ScriptedLLM(
        [{"judgments": {} if judgment is None else {"output_0": judgment}, "reason": "test"}] * (2 if expected else 1)
    )
    held = await check_answer_outputs(Jev(), llm, answer, notes, ("Report the price.",))
    assert held is expected
    assert len(llm.calls) == (2 if expected else int(probability is not None))


async def test_atomic_output_context_keeps_cited_sources_and_page_titles_without_uncited_notes() -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import check_answer_outputs

    shown = []

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            shown.append(state)
            return Evaluation(
                model="test",
                answers={
                    key: _choice("claim_0") if isinstance(question, ChoiceQuestion) else NoulAnswer(probability=0.99)
                    for key, question in questions.items()
                },
                input_tokens=1,
                cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0),
            )

    page = capture((BlockKind.PARAGRAPH, "Price £12"), (BlockKind.PARAGRAPH, "Hours 9am"))
    page = page.model_copy(update={"title": "Complete museum catalog title"})
    notes = Notes()
    notes.remember_capture(page)
    notes.remember_capture(page.model_copy(update={"url": "https://other.test", "title": "Unrelated title"}))
    for block in page.blocks:
        notes.add(Fact(reader=FactReader.LLM, text=block.source_id, evidence=block_evidence(page, block.source_id)))
    price, _ = tuple(notes.evidence)
    answer = assemble_answer((Claim(text="It costs £12.", evidence_ids=(price,)),), notes, ())
    llm = ScriptedLLM([{"judgments": {"output_0": "yes"}, "reason": "The price is quoted."}] * 2)
    assert await check_answer_outputs(Jev(), llm, answer, notes, ("Report the price.",))
    assert "Unrelated title" not in json.dumps(shown)
    assert "9am" not in json.dumps(shown)
    assert "Complete museum catalog title" in json.dumps(shown)
    assert "Price £12" in json.dumps(shown, ensure_ascii=False)


async def test_atomic_outputs_reject_an_empty_answer_instead_of_skipping_the_check() -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.retrieval import assemble_answer
    from fastbrowse.verification import check_claims

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            return Evaluation(
                model="test",
                answers={key: NoulAnswer(probability=0.01) for key in questions},
                input_tokens=1,
                cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0),
            )

    requirement = Requirement(id="r", text="Report the price.", kind=RequirementKind.INFORMATION)
    notes = Notes()
    answer = assemble_answer((), notes, (requirement,))
    assert await check_claims(Jev(), answer, notes, Thresholds(), answer_checks=(requirement.text,)) is None


@pytest.mark.parametrize("separate", [False, True])
async def test_output_audits_isolate_fields_and_check_extra_answer_claims(separate: bool) -> None:
    from fastbrowse.verification import check_answer_outputs

    class Jev:
        async def evaluate(self, state: object, questions: Mapping[str, Question]) -> Evaluation:
            return Evaluation(
                model="test",
                input_tokens=1,
                answers={
                    key: _choice("claim_0") if isinstance(question, ChoiceQuestion) else NoulAnswer(probability=0.99)
                    for key, question in questions.items()
                },
                cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0),
            )

    class AuditLLM(ScriptedLLM):
        active = 0
        peak = 0
        saw_extra = False

        async def generate(self, purpose, messages, schema, **kwargs):
            payload = json.loads(messages[-1].content)
            assert payload["task"] == "Report the current price of the selected item."
            fields = payload["criteria"]
            assert len(fields) == 1
            key, field = next(iter(fields.items()))
            self.active += 1
            self.peak = max(self.peak, self.active)
            await asyncio.sleep(0)
            self.active -= 1
            actual = field.get("reported_claims", [])
            self.saw_extra |= any("member price 9" in claim["text"] for claim in actual)
            judgment = "no" if actual and "member price 9" in json.dumps(actual) else "yes"
            assert kwargs["max_output_tokens"] == 512
            return Generation(
                data=schema.model_validate({"judgments": {key: judgment}, "reason": "test"}),
                cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose),
            )

    page = capture((BlockKind.PARAGRAPH, "Current price 12"))
    fact = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    claims = [
        Claim(text="Current price 12" + ("; member price 9" if not separate else ""), evidence_ids=(fact_id(fact),))
    ]
    if separate:
        claims.append(Claim(text="A member price 9 is available", evidence_ids=(fact_id(fact),)))
    answer = assemble_answer(claims, notes, ())
    llm = AuditLLM([])
    checks = tuple(f"Report the current price, criterion {index}" for index in range(7))
    ledger = Ledger(Limits())
    assert not await check_answer_outputs(
        Jev(), llm, answer, notes, checks, ledger=ledger, task="Report the current price of the selected item."
    )
    assert llm.saw_extra and llm.peak == 4
    assert sum(line.component is CostComponent.LLM for line in ledger.lines) == 14 + int(separate)


def test_output_audit_marks_a_quoted_comparison_with_its_basis_as_derived() -> None:
    from fastbrowse.verification import _output_context

    page = capture((BlockKind.PARAGRAPH, "Pine costs 4"), (BlockKind.PARAGRAPH, "Oak costs 7"))
    notes = Notes(
        Fact(
            text=block_evidence(page, block.source_id).quote,
            evidence=block_evidence(page, block.source_id),
            reader=FactReader.LLM,
        )
        for block in page.blocks
    )
    pine, oak = notes.facts
    winner = pine.model_copy(update={"text": "Pine is cheapest", "basis": (fact_id(oak),)})
    notes.add(winner)
    answer = assemble_answer((Claim(text=winner.text, evidence_ids=(fact_id(winner),)),), notes, ())
    context = _output_context(answer, notes)
    assert context is not None and context.claims[0].derived
    assert {source.quote for source in context.claims[0].cited_sources} == {"Pine costs 4", "Oak costs 7"}
    assert not notes.derived(fact_id(winner))


async def test_atomic_outputs_check_derived_counts_against_their_source_records() -> None:
    from fastbrowse.verification import _output_context

    page = capture((BlockKind.PARAGRAPH, "A"), (BlockKind.PARAGRAPH, "B"))
    notes = Notes(
        Fact(text=block.source_id, evidence=block_evidence(page, block.source_id), reader=FactReader.LLM)
        for block in page.blocks
    )
    records = tuple(notes.evidence)
    total = notes.add_tally(Tally(requirement_id="r", key="Items", records=records))
    answer = assemble_answer((Claim(text="There are two items.", evidence_ids=(fact_id(total),)),), notes, ())
    context = _output_context(answer, notes)
    assert context is not None
    assert context.claims[0].derived
    assert {source.quote for source in context.claims[0].cited_sources} == {"A", "B"}


async def test_malformed_output_sources_fail_without_navigation_feedback() -> None:
    from fastbrowse.retrieval import ComposedAnswer
    from fastbrowse.verification import check_answer_outputs

    answer = ComposedAnswer(
        answer="It costs £12.",
        linked_answer="It costs £12.",
        claims=(Claim(text="It costs £12.", evidence_ids=("unknown",)),),
    )
    missing: list[str] = []
    assert not await check_answer_outputs(
        _ReadJev({}), ScriptedLLM([]), answer, Notes(), ("Report the price.",), missing_outputs=missing
    )
    assert missing == []


async def test_output_checks_can_bind_an_unbounded_field_to_multiple_claims() -> None:
    from fastbrowse.verification import check_answer_outputs

    class Jev:
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            return Evaluation(
                model="test",
                answers={
                    key: _choice("all") if isinstance(question, ChoiceQuestion) else NoulAnswer(probability=0.5)
                    for key, question in questions.items()
                },
                input_tokens=1,
                cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0),
            )

    page = capture((BlockKind.PARAGRAPH, "Book A"), (BlockKind.PARAGRAPH, "Book B"))
    notes = Notes(
        Fact(text=block.source_id, evidence=block_evidence(page, block.source_id), reader=FactReader.LLM)
        for block in page.blocks
    )
    answer = assemble_answer(
        tuple(Claim(text=source.quote, evidence_ids=(key,)) for key, source in notes.evidence.items()), notes, ()
    )
    llm = ScriptedLLM([{"judgments": {"output_0": "yes"}, "reason": "Both titles are quoted."}] * 2)
    assert await check_answer_outputs(Jev(), llm, answer, notes, ("List the title of each matching book.",))
    claims = json.loads(llm.calls[1][1][-1].content)["criteria"]["output_0"]["reported_claims"]
    assert len(claims) == 2
    assert [claim["cited_sources"][0]["quote"] for claim in claims] == ["Book A", "Book B"]


@pytest.mark.parametrize("rewritten", [False, True])
async def test_only_verbatim_scalar_jev_facts_can_skip_the_llm_source_audit(rewritten: bool) -> None:
    from fastbrowse.verification import check_answer_outputs

    page = capture((BlockKind.PARAGRAPH, "Price £12"))
    fact = Fact(text="£12", evidence=block_evidence(page, "s0"), reader=FactReader.JEV_CHOICE)
    notes = Notes((fact,))
    text = "£12 and it opens at 9am" if rewritten else fact.text
    answer = assemble_answer((Claim(text=text, evidence_ids=(fact_id(fact),)),), notes, ())
    jev = _ReadJev({"output_0": NoulAnswer(probability=0.99)})
    assert await check_answer_outputs(jev, None, answer, notes, ("Report the price.",), allow_scalar_jev=True) is (
        not rewritten
    )


async def test_source_availability_audit_withholds_reported_values() -> None:
    from fastbrowse.verification import check_answer_outputs

    page = capture((BlockKind.TABLE, "| Ports | 1 | 3 |"))
    notes = Notes((Fact(text="Three ports", evidence=block_evidence(page, "s0"), reader=FactReader.LLM),))
    answer = assemble_answer(
        (Claim(text="Device Beta has three ports.", evidence_ids=tuple(notes.evidence)),), notes, ()
    )

    class Jev:
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            return Evaluation(
                model="test",
                answers={
                    key: _choice("claim_0") if isinstance(question, ChoiceQuestion) else NoulAnswer(probability=0.99)
                    for key, question in questions.items()
                },
                input_tokens=1,
                cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.0),
            )

    jev = Jev()
    llm = ScriptedLLM([{"judgments": {"output_0": "no"}, "reason": "The column identity is absent."}])
    missing: list[str] = []
    assert not await check_answer_outputs(
        jev, llm, answer, notes, ("Report Device Beta's port count.",), missing_outputs=missing
    )
    request = json.loads(llm.calls[0][1][-1].content)
    assert "actual_answer" not in request
    source = request["criteria"]["output_0"]["sources"][0]
    assert "text" not in source
    assert source["cited_sources"][0]["quote"] == "| Ports | 1 | 3 |"
    assert missing == ["Report Device Beta's port count."]
    assert len(llm.calls) == 1


def test_large_headerless_tables_repeat_real_context_without_inventing_headers() -> None:
    first = "| Feature | Alpha | Beta |"
    rows = [f"| Attribute {index} | {index} | {index + 1} |" for index in range(20)]
    page = capture((BlockKind.TABLE, first + "\n" + "\n".join(rows)))
    parts = chunk(page, 100, overlap_blocks=0)
    assert len(parts) > 1
    assert all(len(part.text) <= 100 for part in parts)
    assert all(part.text.startswith(first) for part in parts)
    assert all("---" not in part.text for part in parts)
    assert all(any(row in part.text for part in parts) for row in rows)
    candidate = next(candidate for candidate in read_candidates(page) if candidate.value == "Attribute 19")
    assert first in candidate.context
    assert first in candidate.evidence.quote


async def test_a_table_excerpt_retains_cells_identifying_comparison_columns() -> None:
    text = "| Feature | Device Alpha | Device Beta |\n| USB-C ports | 1 | 3 |"
    page = capture((BlockKind.TABLE, text))
    response: JsonValue = {
        "claims": [
            {
                "cite": {"first": "s0", "last": "s0"},
                "excerpt": "| USB-C ports | 1 | 3 |",
                "text": "Device Beta has three USB-C ports.",
                "requirement_id": "r",
            }
        ],
        "answered": True,
    }
    result = await read(ScriptedLLM([response]), page, "Report Device Beta's USB-C ports", ["r"], Notes())
    assert len(result.facts) == 1
    evidence = result.facts[0].evidence
    assert evidence is not None
    assert evidence.quote == text


@pytest.mark.parametrize("same_frame", [True, False])
async def test_read_heading_context_becomes_a_verified_citation(same_frame: bool) -> None:
    heading = "Member price 9"
    page = capture(
        (BlockKind.HEADING, heading), (BlockKind.PARAGRAPH, "Current price 12"), (BlockKind.HEADING, heading)
    )
    blocks = list(page.blocks)
    blocks[0] = blocks[0].model_copy(update={"frame_id": None if same_frame else "child"})
    blocks[1] = blocks[1].model_copy(update={"heading_path": (heading,)})
    page = page.model_copy(update={"blocks": tuple(blocks)})
    notes = Notes()
    result = await read(
        ScriptedLLM(
            [
                {
                    "claims": [
                        {
                            "cite": {"first": "s1", "last": "s1"},
                            "text": "Current price 12 and member price 9",
                            "requirement_id": "r",
                        }
                    ],
                    "answered": True,
                }
            ]
        ),
        page,
        "Report the prices",
        ["r"],
        notes,
    )
    fact = next(fact for fact in result.facts if fact.requirement_id == "r")
    answer = assemble_answer([Claim(text=fact.text, evidence_ids=(fact_id(fact),))], notes, ())
    assert {citation.quote for citation in answer.citations} == (
        {heading, "Current price 12"} if same_frame else {"Current price 12"}
    )
    assert all(citation.quote != heading or citation.url == page.url for citation in answer.citations)
    if same_frame:
        context = notes.evidence[fact.basis[0]]
        assert context.source_id == "s0" and context.start == 0


def test_output_audits_cannot_use_uncited_heading_values() -> None:
    from fastbrowse.verification import _output_context

    page = capture((BlockKind.PARAGRAPH, "Current price 12"))
    evidence = block_evidence(page, "s0").model_copy(update={"heading_path": ("Member price 9",)})
    fact = Fact(text="Current price 12 and member price 9", evidence=evidence, reader=FactReader.LLM)
    notes = Notes((fact,))
    answer = assemble_answer([Claim(text=fact.text, evidence_ids=(fact_id(fact),))], notes, ())
    context = _output_context(answer, notes)
    assert context is not None
    assert "Member price 9" not in json.dumps(context.claims[0].cited_sources[0].model_dump())


async def test_headerless_table_continuation_cites_its_displayed_identity_context() -> None:
    first = "| Feature | Alpha | Beta |"
    rows = [f"| Attribute {index} | {index} | {index + 1} |" for index in range(20)]
    page = capture((BlockKind.TABLE, first + "\n" + "\n".join(rows)))
    parts = chunk(page, 100)
    target = next(part for part in parts if rows[-1] in part.text)
    responses: list[JsonValue] = [
        {"claims": [], "answered": False}
        if part is not target
        else {
            "claims": [
                {
                    "cite": {"first": "s0", "last": "s0"},
                    "excerpt": rows[-1],
                    "text": "Beta's Attribute 19 is 20.",
                    "requirement_id": "r",
                }
            ],
            "answered": True,
        }
        for part in parts
    ]
    notes = Notes()
    result = await read(ScriptedLLM(responses), page, "Report Beta's Attribute 19", ["r"], notes, max_chars=100)
    fact = next(fact for fact in result.facts if fact.requirement_id == "r")
    quoted = [notes.evidence[key].quote for key in notes.expand_evidence_ids((fact_id(fact),))]
    assert first in quoted
    assert any(rows[-1] in quote for quote in quoted)
    assert not notes.derived(fact_id(fact))


def test_inline_citation_stripping_accepts_address_qualified_evidence_ids() -> None:
    from fastbrowse.retrieval import _without_citation_markup

    page = capture((BlockKind.PARAGRAPH, "Price £12"))
    key = evidence_id(block_evidence(page, "s0"))
    assert _without_citation_markup(f"Price £12 [{key}]") == "Price £12"
    assert _without_citation_markup(f"Price £12 [{key}](https://example.test)") == "Price £12"
    assert _without_citation_markup("Value [sha:1:2], [derived:abc], [tally:abc]") == "Value , ,"
    assert _without_citation_markup("Use [optional] fields") == "Use [optional] fields"
