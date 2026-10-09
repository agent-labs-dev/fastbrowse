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


def _quoted(quote: str, sha: str, url: str = "https://example.test") -> Fact:
    span = evidence(sha=sha).model_copy(update={"quote": quote, "url": url, "end": len(quote)})
    return Fact(requirement_id="r1", text=quote, evidence=span, reader=FactReader.LLM)


@pytest.mark.parametrize(
    ("shown", "url", "kept"),
    [
        ("SelectedDate: 28/11/2026", "https://example.test", ["SelectedDate: 28/11/2026"]),
        (
            "SelectedDate: 28/11/2026 was SelectedDate: 31/10/2026",
            "https://example.test",
            ["SelectedDate: 31/10/2026", "SelectedDate: 28/11/2026"],
        ),
        (
            "SelectedDate: 28/11/2026",
            "https://example.test/2",
            ["SelectedDate: 31/10/2026", "SelectedDate: 28/11/2026"],
        ),
    ],
    ids=["gone from the page", "still shown", "another address"],
)
def test_a_new_read_of_an_address_retires_quotes_it_no_longer_shows(shown: str, url: str, kept: list[str]) -> None:
    notes = Notes([_quoted("SelectedDate: 31/10/2026", "before"), _quoted("SelectedDate: 28/11/2026", "after", url)])
    notes.supersede("r1", url, "after", shown)
    assert [fact.text for _, fact in notes.supporting("r1")] == kept
    assert len(notes.facts) == 2


def test_a_quote_inside_a_longer_word_is_not_still_shown() -> None:
    notes = Notes([_quoted("Priya Sharma", "before"), _quoted("Priya Sharman", "after")])
    notes.supersede("r1", "https://example.test", "after", "Name: Priya Sharman")
    assert [fact.text for _, fact in notes.supporting("r1")] == ["Priya Sharman"]
    assert [e.quote for e in notes.read_for("r1")] == ["Priya Sharma", "Priya Sharman"]


def test_a_read_that_evidenced_nothing_retires_nothing() -> None:
    notes = Notes([_quoted("SelectedDate: 31/10/2026", "before")])
    notes.supersede("r1", "https://example.test", "after", "SelectedDate: 28/11/2026")
    assert notes.evidenced("r1")


@pytest.mark.parametrize("still_shown", [False, True])
def test_updated_page_retires_vanished_context_quotes(still_shown: bool) -> None:
    old = _quoted("Subtotal: GBP 34.50", "before").model_copy(update={"requirement_id": None})
    fresh = _quoted("Subtotal: GBP 43.25", "after")
    notes = Notes((old, fresh))
    shown = fresh.text + ("\n" + old.text if still_shown else "")

    notes.supersede("r1", "https://example.test", "after", shown)

    assert (fact_id(old) in notes.current_evidence()) is still_shown
    assert fact_id(fresh) in notes.current_evidence()
    assert old in notes.facts
    notes.add(old)
    assert fact_id(old) in notes.current_evidence()


def test_changed_visible_rows_do_not_retire_a_cumulative_tallys_records() -> None:
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import _output_context

    first = _quoted("Order A: 12", "before").model_copy(update={"requirement_id": None})
    last = _quoted("Order B: 7", "after").model_copy(update={"requirement_id": None})
    notes = Notes((first, last))
    tally = notes.add_tally(Tally(requirement_id="r1", key="Orders", records=(fact_id(first), fact_id(last))))
    notes.complete_tallies("r1")

    notes.supersede("r1", "https://example.test", "after", last.text)

    answer = assemble_answer((Claim(text="There are two orders.", evidence_ids=(fact_id(tally),)),), notes, ())
    assert _output_context(answer, notes) is not None


def test_changed_visible_rows_do_not_retire_a_comparisons_records() -> None:
    first = _quoted("Order A: 12", "before").model_copy(update={"requirement_id": None})
    last = _quoted("Order B: 7", "after")
    notes = Notes((first, last))
    notes.add_continuation("r1", fact_id(first))

    notes.supersede("r1", "https://example.test", "after", last.text)

    assert fact_id(first) in notes.current_evidence()


@pytest.mark.parametrize("source_url", ["https://example.test", "https://child.test"])
def test_repeated_choice_sources_are_kept_and_retired_when_one_changes(source_url: str) -> None:
    context = _quoted("Availability: 20", "before", source_url).model_copy(update={"requirement_id": None})
    repeated = _quoted("20", "before", source_url).model_copy(update={"requirement_id": None})
    answer = Fact(
        requirement_id="r1",
        text="20",
        evidence=None,
        reader=FactReader.JEV_CHOICE,
        basis=(fact_id(context), fact_id(repeated)),
    )
    current = _quoted("Availability: 22", "after", source_url)
    notes = Notes((context, repeated, answer, current))
    assert context.evidence in notes.read_for("r1")
    notes.supersede("r1", "https://example.test", "after", "20\nAvailability: 22")
    assert fact_id(answer) not in {key for key, _ in notes.supporting("r1")}
    assert notes.evidenced("r1")


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


def test_tallies_keep_identical_rows_in_one_capture_and_deduplicate_recaptures() -> None:
    notes = Notes()
    originals: list[str] = []
    for sha, starts in (("first", (0, 30)), ("reread", (10, 40)), ("overlap", (20,)), ("first", (0, 30))):
        records = []
        for start in starts:
            quote = "Ada: approved" if sha == "first" else "Ada:  approved"
            item = evidence(sha=sha, start=start, end=start + len(quote)).model_copy(update={"quote": quote})
            fact = Fact(text=quote, evidence=item, reader=FactReader.LLM)
            notes.add(fact)
            records.append(fact_id(fact))
        notes.add_tally(Tally(requirement_id="r", key="Ada", records=tuple(records)))
        if not originals:
            originals = records
        assert notes.tallies[0].count == 2
        assert notes.tallies[0].records == tuple(originals)


@pytest.mark.parametrize(
    ("second_url", "count"),
    [
        ("https://example.test/rows?page=2", 2),
        ("https://example.test/other?page=1", 2),
        ("http://example.test/rows?page=1", 2),
        ("https://example.test/rows?page=1#row", 1),
    ],
)
def test_tally_row_identity_is_scoped_to_the_address(second_url: str, count: int) -> None:
    notes = Notes()
    records: list[str] = []
    quote = "Ada | Widget | $10"
    for index, url in enumerate(("https://example.test/rows?page=1", second_url, second_url)):
        item = evidence(sha=f"capture-{index}", end=len(quote)).model_copy(update={"url": url, "quote": quote})
        fact = Fact(text=quote, evidence=item, reader=FactReader.LLM)
        notes.add(fact)
        records.append(fact_id(fact))
        notes.add_tally(Tally(requirement_id="r", key="Ada", records=(fact_id(fact),)))
    assert notes.tallies[0].count == count
    assert notes.tallies[0].records == tuple(records[:count])
    assert not notes.evidenced("r")
    notes.complete_tallies("r")
    assert notes.evidenced("r")
    assert len(notes.supporting_evidence("r")) == count


def test_tally_record_identity_survives_returning_to_an_earlier_capture() -> None:
    notes = Notes()
    for sha, start in (("first", 0), ("second", 10), ("second", 40), ("first", 30)):
        item = evidence(sha=sha, start=start, end=start + 13).model_copy(update={"quote": "Ada: approved"})
        fact = Fact(text=item.quote, evidence=item, reader=FactReader.LLM)
        notes.add(fact)
        notes.add_tally(Tally(requirement_id="r", key="Ada", records=(fact_id(fact),)))
    assert notes.tallies[0].count == 2


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


def test_tally_aliases_keep_other_basis_quotes_and_citation_ids() -> None:
    record = Fact(text="Ada", evidence=evidence(sha="ada"), reader=FactReader.LLM)
    context = Fact(text="All authors", evidence=evidence(sha="context"), reader=FactReader.LLM)
    notes = Notes((record, context))
    tally = notes.add_tally(Tally(requirement_id="r1", key="Ada", records=(fact_id(record),)))
    notes.complete_tallies("r1")
    conclusion = Fact(
        text="Ada leads",
        evidence=None,
        basis=(fact_id(record), fact_id(context)),
        requirement_id="r2",
        reader=FactReader.LLM,
    )
    notes.add(conclusion)
    rendered = notes.render_with_ids(1000, preserve_requirements=True)
    assert f'basis=records(1) + ["{fact_id(context)}"]' in rendered.text
    assert f'[{fact_id(context)}] "All authors"' in rendered.text and 'quote="fact"' in rendered.text
    assert f"[{fact_id(record)}]" not in rendered.text
    assert set(rendered.evidence_ids) == {fact_id(fact) for fact in (record, context, tally, conclusion)}
    assert notes.expand_evidence_ids((fact_id(conclusion),)) == (fact_id(record), fact_id(context), fact_id(conclusion))


@pytest.mark.parametrize("text", ["Price: GBP25.99", "The charger costs GBP25.99"])
def test_render_keeps_source_quote_once_and_retains_distinct_fact_text(text: str) -> None:
    quote = "Price: GBP25.99"
    source = evidence().model_copy(update={"quote": quote, "end": len(quote)})
    fact = Fact(text=text, evidence=source, reader=FactReader.LLM)
    notes = Notes((fact,))
    rendered = notes.render_with_ids(2000)
    assert rendered.text.count(json.dumps(quote)) == 1
    assert text in rendered.text and 'source="s1"' in rendered.text
    assert rendered.evidence_ids == (fact_id(fact),) and notes.evidence[fact_id(fact)] == source


def test_navigation_writes_a_shared_long_source_url_once_without_losing_product_fields() -> None:
    address = "https://shop.test/product?tracking=" + "x" * 300
    facts = tuple(
        Fact(
            text=text,
            evidence=evidence().model_copy(update={"url": address, "start": nth, "end": nth + 1}),
            reader=FactReader.LLM,
        )
        for nth, text in enumerate(("Title: Charger A", "Price: GBP20", "Power: 65W", "Ports: two USB-C"))
    )
    notes = Notes(facts)
    rendered = notes.render_for_navigation(700)
    assert all(fact.text in rendered for fact in facts)
    assert rendered.count(json.dumps(address[:256])) == 1
    assert "url_prefix=" in rendered
    assert "facts omitted" not in rendered
    assert len(rendered) <= 700
    assert all(fact.evidence is not None and fact.evidence.url == address for fact in notes.facts)


@pytest.mark.parametrize("quote_last", [False, True])
def test_navigation_retains_recent_progress_when_source_quotes_exceed_its_budget(quote_last: bool) -> None:
    quote = "catalogue " * 800
    source = evidence().model_copy(update={"quote": quote, "end": len(quote)})
    basis = Fact(text=quote, evidence=source, reader=FactReader.LLM)
    progress = Fact(
        text="The first charger has been checked; the second charger is still unchecked.",
        evidence=None,
        basis=(fact_id(basis),),
        reader=FactReader.LLM,
    )
    notes = Notes((progress, basis) if quote_last else (basis, progress))
    assert progress.text in notes.render_for_navigation(500)
    assert len(notes.render_for_navigation(500)) <= 500
    assert json.dumps(quote) in notes.render(20_000)


@pytest.mark.parametrize(
    "second_url, expected", [("https://example.test/second", 2), ("https://example.test#section", 1)]
)
def test_equal_spans_keep_distinct_source_records(second_url: str, expected: int) -> None:
    first = _quoted("Row A", "same-text")
    second = _quoted("Row A", "same-text", second_url)
    assert first.evidence is not None
    notes = Notes((first, second))
    tally = notes.add_tally(Tally(requirement_id="r1", key="Rows", records=tuple(notes.evidence)))
    assert tally.tally is not None and tally.tally.count == expected
    assert len(notes.evidence) == expected
    assert {source.url for source in notes.evidence.values()} == (
        {first.evidence.url, second_url} if expected == 2 else {first.evidence.url}
    )


def test_navigation_tracking_addresses_do_not_hide_previously_checked_entities() -> None:
    facts = tuple(
        Fact(
            text=f"Checked item {index}: title, price and specifications collected.",
            evidence=evidence().model_copy(update={"url": f"https://shop.test/item/{index}?tracking=" + "x" * 1500}),
            reader=FactReader.LLM,
        )
        for index in range(3)
    )
    notes = Notes(facts)
    rendered = notes.render_for_navigation(1200)
    assert all(fact.text in rendered for fact in facts)
    assert "facts omitted" not in rendered
    assert len(rendered) <= 1200
    assert len(notes.evidence) == 3
    assert all(fact.evidence and len(fact.evidence.url) > 1500 for fact in notes.facts)


def test_recaptured_text_does_not_replace_the_title_of_older_quote_evidence() -> None:
    from datetime import timedelta

    from fastbrowse.page import BlockKind
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import _output_context
    from tests.test_retrieval import block_evidence, capture

    first = capture((BlockKind.PARAGRAPH, "Price: 12"))
    first = first.model_copy(update={"title": "Atlas"})
    second = first.model_copy(update={"title": "Beacon", "captured_at": first.captured_at + timedelta(seconds=1)})
    fact = Fact(text="Price: 12", evidence=block_evidence(first, "s0"), reader=FactReader.JEV_CHOICE)
    notes = Notes((fact,))
    notes.remember_capture(first)
    notes.remember_capture(second)
    answer = assemble_answer((Claim(text=fact.text, evidence_ids=(fact_id(fact),)),), notes, ())
    context = _output_context(answer, notes)
    assert context and context.claims[0].cited_sources[0].page_title == "Atlas"
    fresh_page = notes.captured_page(block_evidence(second, "s0"))
    assert fresh_page and fresh_page.title == "Beacon"


@pytest.mark.parametrize("other_address", [False, True])
def test_output_audit_keeps_other_pages_but_rejects_retired_values(other_address: bool) -> None:
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import _output_context

    old = _quoted("SelectedDate: 03/04/2026", "before")
    url = "https://example.test/another" if other_address else "https://example.test"
    fresh = _quoted("SelectedDate: 09/04/2026", "after", url)
    notes = Notes((old, fresh))
    notes.supersede("r1", url, "after", fresh.text)
    answer = assemble_answer((Claim(text=old.text, evidence_ids=(fact_id(old),)),), notes, ())
    assert (_output_context(answer, notes) is not None) is other_address


def test_output_audit_rejects_retired_choice_with_unassigned_basis_quotes() -> None:
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import _output_context

    basis = _quoted("Availability: 18", "before").model_copy(update={"requirement_id": None})
    old = Fact(requirement_id="r1", text="18", evidence=None, reader=FactReader.JEV_CHOICE, basis=(fact_id(basis),))
    fresh = _quoted("Availability: 23", "after")
    notes = Notes((basis, old, fresh))
    assert fresh.evidence is not None
    notes.supersede("r1", fresh.evidence.url, "after", fresh.text)
    answer = assemble_answer((Claim(text=old.text, evidence_ids=(fact_id(old),)),), notes, ())
    assert _output_context(answer, notes) is None


@pytest.mark.parametrize("other_address", [False, True])
@pytest.mark.parametrize("derived", [False, True])
def test_replaced_shared_quote_cannot_survive_through_another_requirement(other_address: bool, derived: bool) -> None:
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import _output_context

    old = _quoted("SelectedDate: 03/04/2026", "before")
    context = old.model_copy(update={"requirement_id": None})
    if derived:
        old = Fact(
            requirement_id="r1", text=old.text, evidence=None, reader=FactReader.JEV_CHOICE, basis=(fact_id(context),)
        )
    shared = old.model_copy(update={"requirement_id": "r2"})
    url = "https://example.test/another" if other_address else "https://example.test"
    fresh = _quoted("SelectedDate: 09/04/2026", "after", url)
    notes = Notes((context, old, shared, fresh))
    notes.supersede("r1", url, "after", fresh.text)
    answer = assemble_answer((Claim(text=old.text, evidence_ids=(fact_id(old),)),), notes, ())
    assert (_output_context(answer, notes) is not None) is other_address
    assert notes.evidenced("r2") is other_address


@pytest.mark.parametrize("other_address", [False, True])
def test_unchanged_value_does_not_keep_replaced_subject_context_current(other_address: bool) -> None:
    from fastbrowse.page import BlockKind
    from fastbrowse.retrieval import Claim, assemble_answer
    from fastbrowse.verification import _output_context
    from tests.test_retrieval import block_evidence, capture

    old_page = capture(
        (BlockKind.PARAGRAPH, "Participants: Atlas and Beacon"), (BlockKind.PARAGRAPH, "Total mass: 20 kg")
    )
    new_page = capture(
        (BlockKind.PARAGRAPH, "Participants: Atlas and Cobalt"),
        (BlockKind.PARAGRAPH, "Total mass: 20 kg"),
        url="https://example.test/another" if other_address else old_page.url,
    )
    context = Fact(text="Atlas and Beacon", evidence=block_evidence(old_page, "s0"), reader=FactReader.LLM)
    old = Fact(
        requirement_id="r",
        text="Atlas and Beacon total 20 kg",
        evidence=block_evidence(old_page, "s1"),
        basis=(fact_id(context),),
        reader=FactReader.LLM,
    )
    fresh = Fact(
        requirement_id="r",
        text="Atlas and Cobalt total 20 kg",
        evidence=block_evidence(new_page, "s1"),
        reader=FactReader.LLM,
    )
    notes = Notes((context, old, fresh))
    notes.supersede("r", new_page.url, new_page.sha256, new_page.text)
    answer = assemble_answer((Claim(text=old.text, evidence_ids=(fact_id(old),)),), notes, ())
    assert (_output_context(answer, notes) is not None) is other_address


def test_equal_quoted_values_from_different_controls_keep_separate_evidence() -> None:
    from fastbrowse.models import SourceControl

    notes = Notes()
    for position in ("1 of 2", "2 of 2"):
        source = evidence().model_copy(
            update={"control_context": SourceControl(role="figure", label="Avatar", context=position)}
        )
        assert notes.add(Fact(text="fact", evidence=source, reader=FactReader.LLM))
    assert len(notes.facts) == 2
    assert len({fact_id(fact) for fact in notes.facts}) == 2


@pytest.mark.parametrize("json_encoded", [False, True])
def test_shared_source_metadata_fits_without_dropping_required_quotes(json_encoded: bool) -> None:
    url = "https://example.test/item?ref=" + "referral" * 180
    facts = tuple(
        _quoted(f"Field {index}: value {index}", str(index), url).model_copy(update={"requirement_id": f"r{index}"})
        for index in range(8)
    )
    notes = Notes(facts)
    rendered = notes.render_with_ids(4000, preserve_requirements=True, json_encoded=json_encoded)
    assert set(rendered.evidence_ids) == {fact_id(fact) for fact in facts}
    assert rendered.text.count(url) == 1
    assert all(json.dumps(fact.evidence.quote) in rendered.text for fact in facts if fact.evidence is not None)
    assert all(f"requirements=r{index}" in rendered.text for index in range(8))
    assert notes.facts == facts
    size = len(json.dumps(rendered.text)) - 2 if json_encoded else len(rendered.text)
    assert size <= 4000
    assert notes.render_with_ids(size, preserve_requirements=True, json_encoded=json_encoded) == rendered
    with pytest.raises(NotesTooLarge):
        notes.render_with_ids(size - 1, preserve_requirements=True, json_encoded=json_encoded)


def test_shared_url_groups_preserve_read_order_and_derived_basis() -> None:
    first_url = "https://first.test/?ref=" + 'quote"\\' * 100
    second_url = "https://second.test/?ref=" + "context" * 100
    first = _quoted("First price: 10", "first", first_url)
    second = _quoted("First availability: yes", "second", first_url)
    other = _quoted("Other price: 20", "other", second_url)
    last = _quoted("Other availability: no", "last", second_url)
    derived = Fact(
        requirement_id="comparison",
        text="The first price is lower",
        evidence=None,
        basis=(fact_id(first), fact_id(other)),
        reader=FactReader.LLM,
    )
    facts = (first, second, derived, other, last)
    notes = Notes(facts)
    rendered = notes.render_with_ids(10000, preserve_requirements=True, json_encoded=True)
    assert rendered.evidence_ids == tuple(fact_id(fact) for fact in facts)
    positions = [rendered.text.index(f"[{fact_id(fact)}]") for fact in facts]
    assert positions == sorted(positions)
    assert rendered.text.count("# Source URL:") == 2
    assert "# Other evidence\n" in rendered.text
    assert rendered.text.count(json.dumps(first_url)) == 1
    assert rendered.text.count(json.dumps(second_url)) == 1
    assert "requirements=comparison derived basis=" in rendered.text
    assert json.dumps(list(derived.basis)) in rendered.text
    assert notes.facts == facts
