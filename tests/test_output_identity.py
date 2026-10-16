import json

import pytest
from pydantic import JsonValue, ValidationError

from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import FactReader
from fastbrowse.page import BlockKind
from fastbrowse.retrieval import Claim, assemble_answer
from fastbrowse.verification import _QuotedIdentity, check_answer_outputs
from tests.test_answer_repair import RoutingJev
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


class IdentityWriter(ScriptedLLM):
    def __init__(
        self, scope="entities", quote="Adapter Beacon", ref="q1", url_ref="u1", identity_ids=None, extra_identities=()
    ):
        super().__init__([])
        self.scope, self.quote, self.ref, self.url_ref = scope, quote, ref, url_ref
        self.identity_ids = identity_ids
        self.extra_identities = list(extra_identities)

    async def generate(self, purpose, messages, schema, **kwargs):
        payload = json.loads(messages[-1].content)
        if schema.__name__ == "_OutputIdentities":
            response = {
                "identities": [{"id": "i0", "source_ref": self.ref, "quote": self.quote}, *self.extra_identities],
                "bindings": {
                    key: {
                        "scope": self.scope,
                        "identity_ids": (
                            self.identity_ids
                            if self.identity_ids is not None
                            else (["i0"] if self.scope == "entities" else [])
                        ),
                    }
                    for key in payload["criteria"]
                },
            }
            # This seam's identities are explicit test inputs, rather than the default field-audit fixture.
            self.calls.append((purpose, tuple(messages)))
            from fastbrowse.llm import Generation
            from fastbrowse.models import CostBasis, CostComponent, CostLine

            if kwargs.get("ledger") is not None:
                kwargs["ledger"].reserve(CostComponent.LLM)
            return Generation(
                data=schema.model_validate(response),
                cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose),
            )
        field = next(iter(payload["criteria"].values()))
        if self.scope == "entities" and "requested_entities" in field:
            assert field["requested_entities"] == [
                {
                    "reference": "Report Adapter Beacon exact port count.",
                    "url_ref": self.url_ref,
                    "quote": "Adapter Beacon",
                }
            ]
            assert "Actual reported value 999" not in json.dumps(field.get("requested_entities"))
        self.responses.append({"judgments": {key: "yes" for key in payload["criteria"]}, "reason": "Test verdict."})
        return await super().generate(purpose, messages, schema, **kwargs)


def two_entity_answer():
    facts = []
    for i, text in enumerate(("Adapter Atlas has 2 ports.", "Adapter Beacon has 3 ports.")):
        page = capture((BlockKind.PARAGRAPH, text)).model_copy(update={"url": f"https://example.test/{i}"})
        facts.append(Fact(text=text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM))
    notes = Notes(facts)
    answer = assemble_answer(tuple(Claim(text=fact.text, evidence_ids=(fact_id(fact),)) for fact in facts), notes, ())
    return notes, answer


@pytest.mark.parametrize("ref,quote", [("q999", "Adapter Beacon"), ("q1", "Invented Beacon")])
async def test_identity_must_copy_an_offered_literal_source(ref, quote):
    notes, answer = two_entity_answer()
    writer = IdentityWriter(ref=ref, quote=quote)
    corrections = []
    assert not await check_answer_outputs(
        RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",), corrections=corrections
    )
    assert len(writer.calls) == 1
    assert len(corrections) == 1 and corrections[0].stage == "source"
    assert corrections[0].claims == answer.claims


async def test_source_and_assertion_audits_share_literal_identity_without_reported_values():
    notes, answer = two_entity_answer()
    writer = IdentityWriter()
    assert await check_answer_outputs(RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",))
    assert len(writer.calls) == 4


@pytest.mark.parametrize("scope,expected", [("subjectless", True), ("unresolved", False)])
async def test_subjectless_outputs_and_unresolved_subjects_have_distinct_outcomes(scope, expected):
    notes, answer = two_entity_answer()
    assert (
        await check_answer_outputs(
            RoutingJev(), IdentityWriter(scope=scope), answer, notes, ("Use the requested answer format.",)
        )
        is expected
    )


def test_identity_cannot_inject_a_model_written_reference():
    with pytest.raises(ValidationError):
        _QuotedIdentity.model_validate({"source_ref": "q1", "quote": "Adapter Beacon", "reference": "Value is 999"})


async def test_single_compound_claim_does_not_skip_subject_binding():
    notes, _ = two_entity_answer()
    atlas = notes.facts[0]
    answer = assemble_answer(
        (
            Claim(
                text="Adapter Atlas has 2 ports and Adapter Beacon has 3 ports.",
                evidence_ids=(fact_id(atlas),),
            ),
        ),
        notes,
        (),
    )
    writer = IdentityWriter(quote="Adapter Beacon", ref="q0")
    assert not await check_answer_outputs(
        RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",)
    )
    assert len(writer.calls) == 1


@pytest.mark.parametrize(
    "title,expected",
    [("Adapter Beacon", True), ("Adapter Beacon | Adapter Beacon", True), ("Unrelated adapter", False)],
)
async def test_identity_can_copy_only_an_offered_captured_page_title(title, expected):
    page = capture((BlockKind.PARAGRAPH, "3 ports.")).model_copy(update={"title": title})
    fact = Fact(text="3 ports.", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    notes.remember_capture(page)
    answer = assemble_answer((Claim(text="Adapter Beacon has 3 ports.", evidence_ids=(fact_id(fact),)),), notes, ())
    writer = IdentityWriter(ref="q0", url_ref="u0")
    assert (
        await check_answer_outputs(RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",))
        is expected
    )


async def test_repeated_literal_identity_in_bound_body_is_valid():
    page = capture((BlockKind.PARAGRAPH, "Adapter Beacon: 3 ports. Adapter Beacon specifications.")).model_copy(
        update={"title": "Specifications"}
    )
    fact = Fact(text="3 ports.", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    answer = assemble_answer((Claim(text="Adapter Beacon has 3 ports.", evidence_ids=(fact_id(fact),)),), notes, ())
    assert await check_answer_outputs(
        RoutingJev(),
        IdentityWriter(ref="q0", url_ref="u0"),
        answer,
        notes,
        ("Report Adapter Beacon exact port count.",),
    )


@pytest.mark.parametrize("scope,refs", [("entities", ["missing"]), ("entities", []), ("subjectless", ["i0"])])
async def test_shared_identity_references_fail_closed(scope, refs):
    notes, answer = two_entity_answer()
    writer = IdentityWriter(scope=scope, identity_ids=refs)
    assert not await check_answer_outputs(
        RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",)
    )
    assert len(writer.calls) == 1


async def test_duplicate_identity_references_do_not_duplicate_requested_subjects():
    notes, answer = two_entity_answer()
    writer = IdentityWriter(identity_ids=["i0", "i0"])
    assert await check_answer_outputs(RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",))


async def test_an_identity_id_naming_two_quotes_fails_closed():
    notes, answer = two_entity_answer()
    writer = IdentityWriter(extra_identities=[{"id": "i0", "source_ref": "q0", "quote": "Adapter Atlas"}])
    assert not await check_answer_outputs(
        RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",)
    )
    assert len(writer.calls) == 1


async def test_identity_request_lets_a_record_be_named_by_its_requested_identifier():
    # An order row named only by its reference left a strict reading with no legal quote, so every binding came back
    # unresolved and a finished run ended stuck.
    notes, answer = two_entity_answer()
    writer = IdentityWriter(scope="unresolved")
    assert not await check_answer_outputs(
        RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",)
    )
    system = writer.calls[0][1][0].content
    assert "only identifier is itself a requested field" in system


class LiteralIdentities:
    """Binds every criterion to the first offered source holding `subject` word for word, as the identity step
    must, or leaves it unresolved when none does. Each audit after that answers yes, and the source audit asks
    for the run's date when `needs_date` is set."""

    def __init__(self, subject: str, *, needs_date: bool = False) -> None:
        self.subject, self.needs_date = subject, needs_date
        self.calls: list[tuple[str, dict]] = []

    async def generate(self, purpose, messages, schema, **kwargs):
        from fastbrowse.llm import Generation
        from fastbrowse.models import CostBasis, CostComponent, CostLine

        payload = json.loads(messages[-1].content)
        self.calls.append((schema.__name__, payload))
        if schema.__name__ == "_OutputIdentities":
            ref = next((ref for ref, source in payload["sources"].items() if self.subject in source["quote"]), None)
            response = {
                "identities": [] if ref is None else [{"id": "i0", "source_ref": ref, "quote": self.subject}],
                "bindings": {
                    key: {"scope": "unresolved", "identity_ids": []}
                    if ref is None
                    else {"scope": "entities", "identity_ids": ["i0"]}
                    for key in payload["criteria"]
                },
            }
        else:
            verdict = "no" if self.needs_date and "current_date" not in payload else "yes"
            response = {"judgments": {key: verdict for key in payload["criteria"]}, "reason": "Test verdict."}
        return Generation(
            data=schema.model_validate(response),
            cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose),
        )


# Row internal-backend|fastbrowse|quotes-search-lewis|0 of the 0.5.21 benchmark: Jev picked the quote's line of its
# result block, and the "C.S. Lewis (god)" line under it, the only text naming whose quote it was, was left out.
LEWIS_QUOTE = (
    chr(0x201C)
    + "We are not necessarily doubting that God will do the best for us; we are wondering how painful the best will "
    "turn out to be." + chr(0x201D) + " -"
)


@pytest.mark.parametrize(("author", "verified"), [("C.S. Lewis", True), ("Albert Einstein", False)])
async def test_a_chosen_line_keeps_the_record_lines_that_name_its_subject(author: str, verified: bool) -> None:
    from fastbrowse.planner import Requirement, RequirementKind
    from fastbrowse.retrieval import read, read_candidates
    from tests.test_retrieval import _choice, _ReadJev

    page = capture(
        (BlockKind.HEADING, "Quotes to Scrape"),
        (BlockKind.PARAGRAPH, "Author: C.S. Lewis"),
        (BlockKind.PARAGRAPH, "Tag: god"),
        (BlockKind.HEADING, "Results"),
        (BlockKind.PARAGRAPH, f"{LEWIS_QUOTE}\n{author} (god)"),
    ).model_copy(update={"url": "https://quotes.toscrape.com/filter.aspx"})
    requirement = Requirement(
        id="quote_text", text="Find the text of C.S. Lewis's quote tagged god.", kind=RequirementKind.INFORMATION
    )
    chosen = next(candidate.id for candidate in read_candidates(page) if candidate.value == LEWIS_QUOTE)
    notes = Notes()
    await read(
        ScriptedLLM([]),
        page,
        requirement.text,
        [requirement.id],
        notes,
        jev=_ReadJev({requirement.id: _choice(chosen)}),
        requirements=(requirement,),
    )
    value = next(fact for fact in notes.facts if fact.requirement_id == requirement.id)
    assert value.evidence is not None and value.evidence.quote == LEWIS_QUOTE
    answer = assemble_answer((Claim(text=LEWIS_QUOTE, evidence_ids=(fact_id(value),)),), notes, (requirement,))
    # The subject is still bound only to a quote from the page: a record naming another author cannot pass.
    assert (
        await check_answer_outputs(
            RoutingJev(),
            LiteralIdentities("C.S. Lewis"),
            answer,
            notes,
            ("The text of C.S. Lewis's quote tagged god",),
            task="Use the search form to find C.S. Lewis's quote tagged god, and tell me what it says.",
        )
        is verified
    )


# Row internal-backend|fastbrowse-pool|quotes-js-page2|2: the reader cited the first quote's record and narrowed it
# to "by Marilyn Monroe", so nothing offered to the audit said which quote that author wrote.
MONROE_RECORD = (
    chr(0x201C)
    + "This life is what you make it. No matter what, you're going to mess up sometimes, it's a universal truth."
    + chr(0x201D)
    + "\nby Marilyn Monroe\nTags: friends heartbreak inspirational life love sisters"
)
ROWLING_RECORD = (
    chr(0x201C)
    + "It takes a great deal of bravery to stand up to our enemies, but just as much to stand up to our friends."
    + chr(0x201D)
    + "\nby J.K. Rowling\nTags: courage friends"
)


@pytest.mark.parametrize(
    ("cited", "excerpt", "verified"), [("s1", "by Marilyn Monroe", True), ("s2", "by J.K. Rowling", False)]
)
async def test_a_narrowed_excerpt_keeps_its_record_for_the_subject(cited: str, excerpt: str, verified: bool) -> None:
    from fastbrowse.planner import Requirement, RequirementKind
    from fastbrowse.retrieval import read

    page = capture(
        (BlockKind.HEADING, "Quotes to Scrape"),
        (BlockKind.RECORD, MONROE_RECORD),
        (BlockKind.RECORD, ROWLING_RECORD),
    ).model_copy(update={"url": "https://quotes.toscrape.com/js/page/2/"})
    requirement = Requirement(
        id="req_1", text="Find the author of the first quote on the second page.", kind=RequirementKind.INFORMATION
    )
    claim = f"The first quote on the second page is {excerpt}."
    response: JsonValue = {
        "claims": [
            {"cite": {"first": cited, "last": cited}, "excerpt": excerpt, "text": claim, "requirement_id": "req_1"}
        ],
        "answered": True,
    }
    notes = Notes()
    await read(ScriptedLLM([response]), page, requirement.text, [requirement.id], notes, requirements=(requirement,))
    value = next(fact for fact in notes.facts if fact.requirement_id == requirement.id)
    assert value.evidence is not None and value.evidence.quote == excerpt
    answer = assemble_answer((Claim(text=claim, evidence_ids=(fact_id(value),)),), notes, (requirement,))
    assert (
        await check_answer_outputs(
            RoutingJev(),
            LiteralIdentities("This life is what you make it."),
            answer,
            notes,
            ("The author of the first quote on the second page",),
            task="Who wrote the first quote on the second page?",
        )
        is verified
    )


# Rows internal-backend|fastbrowse|stretch-datepicker-last-saturday|2 and internal-fastbrowse|fastbrowse|...|0: the
# audit refused 26/12/2026 as "the last Saturday of the month after next" because no quote states today's date.
DATE_PICKER_TODAY = "2026-10-16 (Friday). In one month: November 2026; in two months: December 2026"


@pytest.mark.parametrize("today", [DATE_PICKER_TODAY, None])
async def test_the_output_audit_judges_a_relative_date_against_the_runs_date(today: str | None) -> None:
    page = capture(
        (BlockKind.PARAGRAPH, "Date Picker 2 (dd/mm/yyyy) :"), (BlockKind.PARAGRAPH, "SelectedDate: 26/12/2026")
    ).model_copy(update={"url": "https://testautomationpractice.blogspot.com/"})
    label = Fact(text="Date Picker 2 (dd/mm/yyyy) :", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    value = Fact(
        requirement_id="req_2",
        text="SelectedDate: 26/12/2026",
        evidence=block_evidence(page, "s1"),
        reader=FactReader.LLM,
    )
    notes = Notes((label, value))
    answer = assemble_answer(
        (Claim(text="The Date Picker 2 field shows 26/12/2026.", evidence_ids=(fact_id(label), fact_id(value))),),
        notes,
        (),
    )
    llm = LiteralIdentities("Date Picker 2", needs_date=True)
    passed = await check_answer_outputs(
        RoutingJev(),
        llm,
        answer,
        notes,
        ("The date displayed in the Date Picker 2 field after selecting the last Saturday of the month after next",),
        task="Using Date Picker 2, the dd/mm/yyyy calendar, select the last Saturday of the month after next. Tell me "
        "the date the field shows.",
        today=today,
    )
    assert passed is (today is not None)
    audits = [payload for schema, payload in llm.calls if schema == "_OutputAssessment"]
    assert audits and all(payload.get("current_date") == today for payload in audits if today is not None)
