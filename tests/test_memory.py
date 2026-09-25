import json
from datetime import UTC, datetime

import pytest

from fastbrowse.memory import Fact, Notes, NotesTooLarge, Tally, fact_id
from fastbrowse.models import Evidence, FactReader
from fastbrowse.planner import Plan, Requirement, RequirementKind


def evidence(*, sha: str = "capture", start: int = 0, end: int = 4) -> Evidence:
    return Evidence(
        source_id="s1",
        url="https://example.test",
        frame_id=None,
        captured_at=datetime(2026, 1, 1, tzinfo=UTC),
        capture_sha256=sha,
        start=start,
        end=end,
        quote="fact",
    )


def test_notes_deduplicate_spans_without_losing_requirement_coverage() -> None:
    notes = Notes()
    assert notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="First fact", evidence=evidence()))
    assert not notes.add(
        Fact(reader=FactReader.LLM, requirement_id="r2", text="Same span, another requirement", evidence=evidence())
    )
    assert notes.evidenced("r1") and notes.evidenced("r2")
    assert len(notes.facts) == 1
    # One block answering two requirements keeps both claims: the draft answers each from this fact's text.
    assert notes.facts[0].text == "First fact\nSame span, another requirement"
    assert not notes.add(Fact(reader=FactReader.LLM, requirement_id="r2", text="First fact", evidence=evidence()))
    assert notes.facts[0].text == "First fact\nSame span, another requirement"
    plan = Plan(
        requirements=tuple(
            Requirement(id=f"r{i}", text=f"Requirement {i}", kind=RequirementKind.INFORMATION) for i in range(1, 4)
        ),
        answer_expected=True,
    )
    assert tuple(requirement.id for requirement in notes.unresolved(plan)) == ("r3",)
    assert notes.add(Fact(reader=FactReader.LLM, text="Another capture", evidence=evidence(sha="different")))
    assert notes.add(Fact(reader=FactReader.LLM, text="Another span", evidence=evidence(start=10, end=14)))
    assert len(notes.facts) == 3
    copy = notes.evidence
    copy.clear()
    assert len(notes.evidence) == 3


def test_render_reports_omissions_and_never_slices_a_citation() -> None:
    first = Fact(reader=FactReader.LLM, text="A long cited fact", evidence=evidence())
    second = Fact(reader=FactReader.LLM, text="A second cited fact", evidence=evidence(sha="second"))
    notes = Notes((first, second))
    complete = notes.render(1000)
    assert fact_id(first) in complete and fact_id(second) in complete
    assert 'quote="fact"' in complete
    one_line = Notes((first,)).render(1000)
    bounded = notes.render(len(one_line) + len("\n[1 facts omitted]"))
    assert bounded == one_line + "\n[1 facts omitted]"
    assert notes.render(20) == "[2 facts omitted]"
    with pytest.raises(ValueError, match="too small"):
        notes.render(1)
    assert Notes().render(0) == ""


def test_requirement_evidence_has_priority_including_reused_spans() -> None:
    context = Fact(reader=FactReader.LLM, text="Context", evidence=evidence(sha="context"))
    early = Fact(reader=FactReader.LLM, requirement_id="r1", text="First answer", evidence=evidence(sha="early"))
    late = Fact(reader=FactReader.LLM, text="Checkout total", evidence=evidence(sha="late"))
    notes = Notes((context, early, late))
    notes.add(late.model_copy(update={"requirement_id": "r2"}))
    required = Notes((early, late.model_copy(update={"requirement_id": "r2"})))
    expected = required.render(1000) + "\n[1 facts omitted]"
    assert notes.render(len(expected), preserve_requirements=True) == expected
    with pytest.raises(NotesTooLarge, match=f"{len(expected) - 1} character notes budget"):
        notes.render(len(expected) - 1, preserve_requirements=True)


def test_reused_span_keeps_the_answer_and_unions_its_basis_in_read_order() -> None:
    records = tuple(Fact(reader=FactReader.LLM, text=name, evidence=evidence(sha=name)) for name in ("A", "B", "C"))
    notes = Notes(records)
    a, b, c = tuple(notes.evidence)
    assert not notes.add(records[0].model_copy(update={"requirement_id": "r", "text": "A wins", "basis": (b,)}))
    assert not notes.add(records[0].model_copy(update={"basis": (c, b, a)}))
    assert notes.facts[0].text == "A wins" and notes.facts[0].requirement_id == "r"
    assert notes.facts[0].basis == (b, c, a)
    assert notes.expand_evidence_ids((a, c, a)) == (a, b, c)


@pytest.mark.parametrize("json_encoded", [False, True])
def test_budget_keeps_transitive_basis_with_the_requirement_or_fails(json_encoded: bool) -> None:
    record = Fact(reader=FactReader.LLM, text="Compared record", evidence=evidence(sha="record"))
    subtotal = Fact(reader=FactReader.LLM, text="Subtotal", evidence=evidence(sha="subtotal"), basis=(fact_id(record),))
    total = Fact(
        reader=FactReader.LLM,
        requirement_id="r",
        text="Total",
        evidence=evidence(sha="total"),
        basis=(fact_id(subtotal),),
    )
    context = Fact(reader=FactReader.LLM, text="Unrelated " * 100, evidence=evidence(sha="context"))
    # A reused early span can acquire a basis read later, so a prefix alone need not preserve the comparison.
    required = Notes((total, record, subtotal))
    notes = Notes((context, *required.facts))
    expected = required.render(10000) + "\n[1 facts omitted]"
    budget = len(json.dumps(expected)) - 2 if json_encoded else len(expected)
    rendered = notes.render_with_ids(budget, preserve_requirements=True, json_encoded=json_encoded)
    assert rendered.text == expected
    assert rendered.evidence_ids == tuple(required.evidence)
    with pytest.raises(NotesTooLarge):
        notes.render_with_ids(budget - 1, preserve_requirements=True, json_encoded=json_encoded)
    shortened = notes.render_with_ids(budget - 1, json_encoded=json_encoded)
    assert fact_id(total) not in shortened.evidence_ids


def test_tallies_deduplicate_records_and_render_without_losing_basis() -> None:
    notes = Notes()
    ids = []
    for page in range(10):
        for number in range(10):
            quote = f"Record {page * 10 + number} by Ada: " + "A long quotation. " * 30
            item = evidence(sha=f"page-{page}", start=number * 1000, end=number * 1000 + len(quote)).model_copy(
                update={"quote": quote, "url": f"https://example.test/page/{page}"}
            )
            fact = Fact(text=quote, evidence=item, reader=FactReader.LLM)
            notes.add(fact)
            ids.append(fact_id(fact))
            notes.add_tally(Tally(requirement_id="r", key="Ada", records=(fact_id(fact),)))
    original = notes.facts[0].evidence
    assert original is not None
    duplicate = notes.facts[0].model_copy(update={"evidence": original.model_copy(update={"capture_sha256": "reread"})})
    notes.add(duplicate)
    notes.add_tally(Tally(requirement_id="r", key="Ada", records=(fact_id(duplicate),)))
    assert notes.tallies[0].count == 100
    assert not notes.evidenced("r")
    notes.complete_tallies("r")
    rendered = notes.render_with_ids(3000, preserve_requirements=True)
    assert "Ada: 100" in rendered.text
    assert "100 distinct records" in rendered.text
    assert "A long quotation" not in rendered.text
    assert set(ids) <= set(notes.expand_evidence_ids(rendered.evidence_ids))
    assert len(notes.evidence) >= 100
    assert notes.evidenced("r")


@pytest.mark.parametrize("json_encoded", [False, True])
def test_compact_tallies_do_not_hide_an_unrelated_required_basis(json_encoded: bool) -> None:
    record = Fact(text="Ada", evidence=evidence(sha="ada"), reader=FactReader.LLM)
    basis = Fact(text="Required context " * 100, evidence=evidence(sha="context"), reader=FactReader.LLM)
    conclusion = Fact(
        text="A conclusion", evidence=None, basis=(fact_id(basis),), requirement_id="r2", reader=FactReader.LLM
    )
    notes = Notes((record, basis, conclusion))
    notes.add_tally(Tally(requirement_id="r1", key="Ada", records=(fact_id(record),)))
    notes.complete_tallies("r1")
    with pytest.raises(NotesTooLarge):
        notes.render_with_ids(800, preserve_requirements=True, json_encoded=json_encoded)
    rendered = notes.render_with_ids(800, json_encoded=json_encoded)
    assert fact_id(conclusion) not in rendered.evidence_ids
