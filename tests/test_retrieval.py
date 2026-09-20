import hashlib
from collections.abc import Mapping, Sequence
from datetime import UTC, date, datetime
from decimal import Decimal

import pytest
from pydantic import BaseModel, Field, JsonValue

from fastbrowse.config import Thresholds
from fastbrowse.jev import (
    MAX_CHOICE_OPTIONS,
    Answer,
    ChoiceAnswer,
    ChoiceQuestion,
    Evaluation,
    JevError,
    JevInputTooLarge,
    NoulAnswer,
    Question,
)
from fastbrowse.llm import Generation, Message
from fastbrowse.memory import Fact, Notes, evidence_id
from fastbrowse.models import CostBasis, CostComponent, CostLine, Frozen, Limits, LLMPurpose
from fastbrowse.page import Block, BlockKind, Capture, Observation
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.retrieval import (
    UnsupportedField,
    chunk,
    claim_check_questions,
    compose,
    copy_field,
    draft_answer,
    field_candidates,
    field_question,
    locate_quote,
    propose_text_fields,
    propose_text_fields_from_notes,
    read,
    read_candidates,
)
from fastbrowse.telemetry import BudgetExceeded, Ledger
from fastbrowse.verification import check_done


def capture(*parts: tuple[BlockKind, str]) -> Capture:
    text = "\n\n".join(part for _, part in parts)
    blocks: list[Block] = []
    start = 0
    for i, (kind, part) in enumerate(parts):
        blocks.append(Block(source_id=f"s{i}", kind=kind, frame_id="frame", start=start, end=start + len(part)))
        start += len(part) + 2
    return Capture(
        url="https://example.test",
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

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = 2000,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        # Reserve exactly as the real client does, so a test can see a budget stop a request.
        if ledger is not None:
            ledger.reserve(CostComponent.LLM)
        self.calls.append((purpose, tuple(messages)))
        return Generation(
            data=schema.model_validate(self.responses.pop(0)),
            cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose),
        )


def test_quote_location_preserves_original_offsets_and_is_block_scoped() -> None:
    page = capture(
        (BlockKind.PARAGRAPH, "Outside match"), (BlockKind.PARAGRAPH, "Prefix  bright\n\t blue\u00a0 sky suffix")
    )
    evidence = locate_quote(page, "s1", "bright blue sky")
    assert evidence is not None
    assert evidence.quote == "bright\n\t blue\u00a0 sky"
    assert evidence.start == page.text.index("bright")
    assert evidence.end == page.text.index(" sky") + len(" sky")
    assert page.text[evidence.start : evidence.end] == evidence.quote
    assert evidence.capture_sha256 == page.sha256 and evidence.frame_id == "frame"
    assert locate_quote(page, "s0", "bright blue sky") is None
    assert locate_quote(page, "s1", "BRIGHT blue sky") is None
    assert locate_quote(page, "missing", "bright") is None
    assert locate_quote(page, "s1", " \n ") is None
    assert locate_quote(page, "s0", "match Prefix") is None


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


def test_chunk_overlap_and_indivisible_blocks_make_progress() -> None:
    page = capture(*((BlockKind.PARAGRAPH, text) for text in ("aaaa", "bbbb", "cccc", "dddd")))
    parts = chunk(page, 9)
    assert [part.block_ids for part in parts] == [("s0", "s1"), ("s1", "s2"), ("s2", "s3")]
    huge = capture((BlockKind.CODE, "x" * 100), (BlockKind.PARAGRAPH, "tail"))
    assert [part.text for part in chunk(huge, 10)] == ["x" * 100, "tail"]
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
        evidence = locate_quote(page, "s0", row)
        assert evidence is not None and page.text[evidence.start : evidence.end] == row


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


async def test_read_continues_after_forged_quote_tracks_coverage_and_cost() -> None:
    page = capture(
        (BlockKind.PARAGRAPH, "Price unknown"),
        (BlockKind.PARAGRAPH, "Price is $12"),
        (BlockKind.PARAGRAPH, "Irrelevant end"),
    )
    llm = ScriptedLLM(
        [
            {
                "claims": [{"requirement_id": "r1", "text": "Free", "source_id": "s0", "quote": "Price is free"}],
                "answered": True,
            },
            {
                "claims": [{"requirement_id": "r1", "text": "Costs $12", "source_id": "s1", "quote": "Price is $12"}],
                "answered": True,
            },
        ]
    )
    notes = Notes()
    result = await read(llm, page, "What price?", ["r1"], notes, max_chars=15)
    assert result.coverage == (0, 1) and result.rejected_quotes == 1
    assert len(result.facts) == 1 and result.facts[0].evidence.quote == "Price is $12"
    assert notes.evidenced("r1")
    assert sum(line.dollars or 0 for line in result.cost_lines) == 0.002
    assert all(purpose is LLMPurpose.READ for purpose, _ in llm.calls)
    assert "s1" in llm.calls[1][1][-1].content


async def test_read_reaches_end_and_does_not_evidence_unknown_requirements() -> None:
    page = capture((BlockKind.PARAGRAPH, "Known fact"), (BlockKind.PARAGRAPH, "Other fact"))
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"requirement_id": "invented", "text": "Known fact", "source_id": "s0", "quote": "Known fact"}
                ],
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
        assert fact.text == f"{requirement.text}\n{quote}"
        assert fact.evidence == locate_quote(page, fact.evidence.source_id, quote)
    _, questions = jev.requests[0]
    assert questions.keys() == {"version", "license"}
    assert all(isinstance(q, ChoiceQuestion) and "none" in q.criteria for q in questions.values())
    assert all("untrusted data" in q.instructions for q in questions.values())
    draft = draft_answer(Plan(requirements=requirements, answer_expected=True), notes)
    assert draft is not None and len(draft.claims) == 2
    assert {claim.evidence_ids[0] for claim in draft.claims} == notes.evidence.keys()
    assert "License: BSD" in claim_check_questions(draft, notes)["unsupported_1"].instructions


@pytest.mark.parametrize(
    "answer", [_choice("c1", 0.89), _choice("none"), _choice("invented"), NoulAnswer(probability=1), None]
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
                "claims": [{"requirement_id": "license", "text": "MIT", "source_id": "s1", "quote": "License: MIT"}],
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
    assert "# Question\n- Find the license name\n\n# Requirement ids\nlicense\n" in llm.calls[0][1][-1].content
    assert all(notes.evidenced(r.id) for r in requirements)
    assert [fact.evidence.quote for fact in result.facts] == ["Version: 1.2.3", "License: MIT"]
    assert [cost.component for cost in result.cost_lines] == [CostComponent.JEV, CostComponent.LLM]
    assert ledger.lines == list(result.cost_lines) and ledger.jev_calls == ledger.llm_calls == 1


@pytest.mark.parametrize(
    "text", ["List the cities", "Compare the cities", "Summarize the cities", "Count all cities on the page"]
)
async def test_synthesis_has_an_explicit_none_route_to_the_reader(text: str) -> None:
    page = capture((BlockKind.PARAGRAPH, "Lyon"))
    requirement = Requirement(id="r", text=text, kind=RequirementKind.INFORMATION)
    jev, llm = _ReadJev({"r": _choice("none")}), ScriptedLLM([{"claims": [], "answered": False}])
    await read(llm, page, text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    question = jev.requests[0][1]["r"]
    assert isinstance(question, ChoiceQuestion)
    assert all(
        word in question.instructions for word in ("lists", "comparisons", "summaries", "counts across the page")
    )
    assert text in question.instructions and "synthesis" in str(question.criteria["none"])
    assert len(llm.calls) == 1 and f"# Question\n- {text}" in llm.calls[0][1][-1].content


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
        assert evidence == locate_quote(page, evidence.source_id, evidence.quote)
        assert evidence.quote == page.text[evidence.start : evidence.end]


async def test_short_read_overflow_uses_reader_without_truncating_candidates() -> None:
    page = capture(*((BlockKind.PARAGRAPH, f"Candidate {i}") for i in range(MAX_CHOICE_OPTIONS)))
    requirement = Requirement(id="r", text="Find the price", kind=RequirementKind.INFORMATION)
    jev, llm = _ReadJev({}), ScriptedLLM([{"claims": [], "answered": False}])
    await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    assert jev.requests == [] and len(llm.calls) == 1


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
    jev = _ReadJev({"r": _choice("none")})
    response: JsonValue = {"claims": [], "answered": False}
    llm = ScriptedLLM([response, response])
    await read(llm, page, requirement.text, ["r"], Notes(), jev=jev, requirements=(requirement,))
    if repetitions == 1:
        assert passage in str(jev.requests[0][0])
    else:
        assert jev.requests == [] and len(llm.calls) == 2


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


async def test_compose_drops_uncited_and_unknown_claims_including_answer_text() -> None:
    page = capture((BlockKind.PARAGRAPH, "Price is $12"))
    evidence = locate_quote(page, "s0", "Price is $12")
    assert evidence is not None
    notes = Notes((Fact(requirement_id="r1", text="Price is $12", evidence=evidence),))
    key = evidence_id(evidence)
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"text": "It is $12.", "evidence_ids": [key]},
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
    result = await compose(llm, "Find price and shipping", plan, notes)
    assert result.data.answer == "It is $12."
    assert result.data.dropped_claims == 2 and len(result.data.claims) == 1
    assert result.cost.dollars == 0.001
    questions = claim_check_questions(result.data, notes)
    assert set(questions) == {"unsupported_0", "contradicted_0", "requirement_omitted"}
    assert "Price is $12" in questions["unsupported_0"].instructions
    assert "Find shipping" in questions["requirement_omitted"].instructions
    assert all(question.true is not None and question.true.startswith("Yes,") for question in questions.values())


async def test_compose_cannot_return_uncited_free_text_without_claims() -> None:
    llm = ScriptedLLM([{"claims": []}])
    result = await compose(llm, "Do it", Plan(requirements=(), answer_expected=True), Notes())
    assert result.data.answer == "" and result.data.claims == ()


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


async def test_text_fields_are_kept_only_when_quoted_verbatim_from_the_page() -> None:
    page = capture((BlockKind.HEADING, "httpx 0.28.1"), (BlockKind.PARAGRAPH, "License: BSD"))
    llm = ScriptedLLM(
        [
            {
                "fields": [
                    {"field": "label", "value": "0.28.1", "source_id": "s0", "quote": "httpx 0.28.1"},
                    {"field": "license", "value": "MIT", "source_id": "s1", "quote": "License: BSD"},
                    {"field": "owner", "value": "encode", "source_id": "s1", "quote": "Owner: encode"},
                ]
            }
        ]
    )
    fields = {name: Fields.model_fields["label"] for name in ("label", "license", "owner")}
    found, _ = await propose_text_fields(llm, "Get the version", page, fields)
    assert found.keys() == {"label"}
    value, evidence = found["label"]
    assert value == "0.28.1" and evidence.quote == "httpx 0.28.1"


async def test_a_text_field_off_the_final_page_is_taken_from_a_note_that_quotes_it() -> None:
    earlier = capture((BlockKind.PARAGRAPH, "requests 2.33.0 released May 14, 2026"))
    quote = locate_quote(earlier, "s0", "requests 2.33.0 released May 14, 2026")
    assert quote is not None
    notes = Notes((Fact(text="requests was released on May 14, 2026", evidence=quote),))
    key = next(iter(notes.evidence))
    llm = ScriptedLLM(
        [
            {
                "fields": [
                    {"field": "label", "value": "requests", "source_id": key, "quote": quote.quote},
                    {"field": "license", "value": "httpx", "source_id": key, "quote": quote.quote},
                ]
            }
        ]
    )
    fields = {name: Fields.model_fields["label"] for name in ("label", "license")}
    found = await propose_text_fields_from_notes(llm, "Which is newer?", notes, fields)
    # A value the cited quote does not contain is not taken, whatever the model says.
    assert found == {"label": ("requests", quote)}


async def test_a_name_the_task_gives_can_be_chosen_on_a_note_that_quotes_only_a_date() -> None:
    earlier = capture((BlockKind.PARAGRAPH, "May 14, 2026"))
    quote = locate_quote(earlier, "s0", "May 14, 2026")
    assert quote is not None
    notes = Notes((Fact(text="requests: May 14, 2026", evidence=quote),))
    key = next(iter(notes.evidence))
    proposal: dict[str, JsonValue] = {"field": "label", "value": "requests", "source_id": key, "quote": quote.quote}
    fields = {"label": Fields.model_fields["label"]}
    task = "Which has the more recent release, httpx or requests?"
    assert await propose_text_fields_from_notes(ScriptedLLM([{"fields": [proposal]}]), task, notes, fields) == {
        "label": ("requests", quote)
    }
    # Part of a word the task uses is not a name it gives.
    partial: dict[str, JsonValue] = {**proposal, "value": "request"}
    assert not await propose_text_fields_from_notes(ScriptedLLM([{"fields": [partial]}]), task, notes, fields)


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
    evidence = locate_quote(page, "s0", "Price is $12")
    assert evidence is not None
    notes = Notes((Fact(requirement_id="r1", text="The price is $12.", evidence=evidence),))
    plan = Plan(
        requirements=(Requirement(id="r1", text="Find price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    draft = draft_answer(plan, notes)
    assert draft is not None and draft.answer == "The price is $12."
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
    composed = ComposedAnswer(answer=answer, claims=(), dropped_claims=dropped)
    assert (await check_claims(jev, composed, Notes(), Thresholds()) is not None) is expected
    jev.evaluate.assert_not_called()


@pytest.mark.parametrize("omitted_after", [0.1, 0.9])
async def test_a_doubted_claim_is_dropped_only_if_the_rest_still_answers(omitted_after: float) -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.models import CostBasis, CostComponent, CostLine
    from fastbrowse.retrieval import Claim, ComposedAnswer
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
    claims = (
        Claim(text="It says you are logged in.", evidence_ids=("e1",)),
        Claim(text="It has a Log out button.", evidence_ids=("e1",)),
    )
    composed = ComposedAnswer(answer="unused", claims=claims, requirements=(requirement,))
    held = await check_claims(Jev(), composed, Notes(), Thresholds())
    if omitted_after > 0.5:
        assert held is None
    else:
        assert held is not None and held.answer == "It says you are logged in."


async def test_pruning_the_only_claim_for_a_requirement_is_an_omission() -> None:
    from fastbrowse.jev import Evaluation, NoulAnswer
    from fastbrowse.models import CostBasis, CostComponent, CostLine, Evidence
    from fastbrowse.retrieval import Claim, ComposedAnswer
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
        return Fact(requirement_id=requirement, text=quote, evidence=evidence)

    notes = Notes((fact("r1", 0, "A Year in Provence"), fact("r2", 40, "£56.88")))
    requirements = (
        Requirement(id="r1", text="Which book is the most expensive?", kind=RequirementKind.INFORMATION),
        Requirement(id="r2", text="What does it cost?", kind=RequirementKind.INFORMATION),
    )
    claims = (
        Claim(text="The most expensive is A Year in Provence.", evidence_ids=("c:0:18",)),
        Claim(text="It costs £56.88.", evidence_ids=("c:40:46",)),
    )
    composed = ComposedAnswer(answer="unused", claims=claims, requirements=requirements)
    assert await check_claims(Jev(), composed, notes, Thresholds()) is None


async def test_a_winner_from_part_of_a_list_is_kept_but_does_not_answer() -> None:
    page = capture((BlockKind.PARAGRAPH, "Sharp Objects £47.82"), (BlockKind.PARAGRAPH, "Page 1 of 2"))
    notes = Notes()
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"requirement_id": "r1", "text": "cheapest", "source_id": "s0", "quote": "Sharp Objects £47.82"}
                ],
                "answered": True,
                "continues": ["r1", "not-asked"],
            }
        ]
    )
    outcome = await read(llm, page, "Cheapest mystery?", ["r1"], notes, notice="This page has a next-page control.")
    assert outcome.continues == ("r1",)
    assert len(notes.facts) == 1 and not notes.evidenced("r1")
    assert "This page has a next-page control." in llm.calls[0][1][-1].content


async def test_the_choice_shortcut_is_told_the_list_continues() -> None:
    from tests.test_policy import ScriptedJev

    page = capture((BlockKind.PARAGRAPH, "Sharp Objects £47.82"))
    jev = ScriptedJev({"r1": "none"})
    requirement = Requirement(id="r1", text="The cheapest book", kind=RequirementKind.INFORMATION)
    llm = ScriptedLLM([{"claims": [], "answered": False, "continues": ["r1"]}])
    await read(
        llm,
        page,
        "Cheapest?",
        ["r1"],
        Notes(),
        jev=jev,
        requirements=[requirement],
        notice="This page has a next-page control ('next').",
    )
    asked = jev.requests[0]["r1"]
    assert isinstance(asked, ChoiceQuestion)
    assert "next-page control" in asked.instructions


async def test_a_later_chunk_saying_the_list_goes_on_reopens_an_earlier_chunks_claim() -> None:
    # The pager sits at the foot of a long listing, so the chunk that names it is read after the winner.
    page = capture(
        (BlockKind.PARAGRAPH, "Sharp Objects £47.82"),
        (BlockKind.PARAGRAPH, "a" * 13000),
        (BlockKind.PARAGRAPH, "Page 1 of 2"),
    )
    notes = Notes()
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {"requirement_id": "r1", "text": "cheapest", "source_id": "s0", "quote": "Sharp Objects £47.82"}
                ],
                "answered": True,
            },
            {"claims": [], "answered": False, "continues": ["r1"]},
            {"claims": [], "answered": False, "continues": ["r1"]},
        ]
    )
    outcome = await read(llm, page, "Cheapest?", ["r1"], notes, notice="This page has a next-page control.")
    assert len(llm.calls) > 1, "a chunk claiming to have answered cannot end a read of a list that goes on"
    assert outcome.continues == ("r1",)
    assert len(notes.facts) == 1 and not notes.evidenced("r1")


async def test_a_later_chunk_is_read_against_what_earlier_chunks_of_the_page_found() -> None:
    """The notes are written once the page is read, so the read carries its own findings between chunks."""
    page = capture(
        (BlockKind.PARAGRAPH, "Einstein: the world as we have created it"),
        (BlockKind.PARAGRAPH, "b" * 13000),
        (BlockKind.PARAGRAPH, "Einstein: there are two ways to live"),
    )
    llm = ScriptedLLM(
        [
            {
                "claims": [
                    {
                        "requirement_id": None,
                        "text": "an Einstein quote",
                        "source_id": "s0",
                        "quote": "Einstein: the world as we have created it",
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
