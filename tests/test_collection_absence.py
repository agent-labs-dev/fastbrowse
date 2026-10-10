"""An absent member needs the identified collection and every record in its scope."""

import json

import pytest
from pydantic import JsonValue

from fastbrowse.memory import Comparison, Fact, Notes, fact_id
from fastbrowse.models import FactReader
from fastbrowse.page import BlockKind
from fastbrowse.planner import Requirement, RequirementKind
from fastbrowse.retrieval import Claim, assemble_answer, read
from fastbrowse.verification import _output_context, check_answer_outputs
from tests.test_answer_repair import RoutingJev
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


@pytest.mark.parametrize("gap", [None, "pager", "lost_record", "unread_chunk"])
async def test_absence_keeps_identified_collection_scope_through_read_and_audit(gap: str | None) -> None:
    page = capture(
        (BlockKind.HEADING, "Project Birch comments, 2 comments. Author: Ash."),
        (BlockKind.RECORD, "Player Elm: Please add a compact view."),
        (BlockKind.RECORD, "Player Oak: Thanks for the update."),
        *([(BlockKind.RECORD, "Author Ash: A compact view is available.")] if gap == "unread_chunk" else []),
    )
    requirement = Requirement(
        id="r", text="Report author replies in Project Birch's comments.", kind=RequirementKind.INFORMATION
    )
    reader = ScriptedLLM(
        [
            {
                "answered": True,
                "claims": [
                    {
                        "requirement_id": "r",
                        "text": "No author replies occur in Project Birch's two comments.",
                        "cite": None,
                        "records": [{"first": f"s{i}", "last": f"s{i}"} for i in range(3)],
                    }
                ],
                "continues": [],
            }
        ]
    )
    notes = Notes()
    await read(
        reader,
        page,
        requirement.text,
        ["r"],
        notes,
        notice="Next page is available." if gap == "pager" else "",
        incomplete=("r",) if gap == "lost_record" else (),
        max_chars=130 if gap == "unread_chunk" else None,
    )
    finding = next(fact for fact in notes.facts if fact.comparison is not None)
    answer = assemble_answer((Claim(text=finding.text, evidence_ids=(fact_id(finding),)),), notes, (requirement,))
    context = _output_context(answer, notes)
    assert context is not None
    scope = context.claims[0].compared_records[0]
    assert scope.complete is (gap is None)
    assert len(scope.record_indices) == 3
    assert context.claims[0].cited_sources[0].quote == page.text.split("\n\n")[0]

    class ScopeJudge(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ != "_OutputIdentities":
                field = json.loads(messages[-1].content)["criteria"]["output_0"]
                claims = field.get("sources", field.get("reported_claims"))
                complete = claims[0]["compared_records"][0]["complete"]
                self.responses.append(
                    {
                        "judgments": {"output_0": "yes" if complete else "no"},
                        "reason": "Absence requires the complete identified collection.",
                    }
                )
            return await super().generate(purpose, messages, schema, **kwargs)

    assert bool(await check_answer_outputs(RoutingJev(), ScopeJudge([]), answer, notes, (requirement.text,))) is (
        gap is None
    )
    if gap is None:
        assert "collection_for=" not in notes.render(10_000)
        rendered = notes.render_with_ids(10_000, preserve_collections=True).text
        assert "collection_for=r complete=true" in rendered


@pytest.mark.parametrize("retired_source", [False, True])
@pytest.mark.parametrize("preserve_collections", [False, True])
async def test_new_conclusion_uses_sources_of_an_inactive_summary_without_reviving_stale_quotes(
    retired_source: bool, preserve_collections: bool
) -> None:
    page = capture((BlockKind.RECORD, "Project Birch: Player Oak says thanks."))
    source = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    old = Fact(
        requirement_id="r",
        text="Player Oak thanked Project Birch.",
        evidence=None,
        basis=(fact_id(source),),
        reader=FactReader.LLM,
    )
    notes = Notes((source, old))
    notes.unevidence(("r",))
    if retired_source:
        notes._retired.add(fact_id(source))
    current = capture((BlockKind.PARAGRAPH, "Other project"), url="https://example.test/other")
    await read(
        ScriptedLLM(
            [
                {
                    "answered": True,
                    "claims": [
                        {
                            "requirement_id": "r",
                            "cite": None,
                            "draws_on": [fact_id(old)],
                            "text": "Project Birch received thanks from Player Oak.",
                        }
                    ],
                    "continues": [],
                }
            ]
        ),
        current,
        "Report Project Birch feedback.",
        ["r"],
        notes,
        preserve_collections=preserve_collections,
    )
    fresh = notes.supporting("r")[-1][1]
    assert fresh.basis == (fact_id(source if preserve_collections else old),)
    answer = assemble_answer((Claim(text=fresh.text, evidence_ids=(fact_id(fresh),)),), notes, ())
    assert (_output_context(answer, notes) is not None) is (preserve_collections and not retired_source)


async def test_summary_reuse_preserves_incomplete_collection_metadata() -> None:
    page = capture((BlockKind.RECORD, "Project Birch: Player Oak says thanks."))
    source = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    collection = Fact(
        requirement_id="r",
        text="No replies among comments read so far.",
        evidence=None,
        basis=(fact_id(source),),
        reader=FactReader.LLM,
        comparison=Comparison(requirement_id="r", records=(fact_id(source),), complete=False),
    )
    old = Fact(text="Feedback so far.", evidence=None, basis=(fact_id(collection),), reader=FactReader.LLM)
    notes = Notes((source, collection, old))
    await read(
        ScriptedLLM(
            [
                {
                    "answered": True,
                    "claims": [
                        {"requirement_id": "r", "cite": None, "draws_on": [fact_id(old)], "text": "No replies."}
                    ],
                    "continues": [],
                }
            ]
        ),
        page,
        "Report replies.",
        ["r"],
        notes,
    )
    fresh = notes.supporting("r")[-1][1]
    answer = assemble_answer((Claim(text=fresh.text, evidence_ids=(fact_id(fresh),)),), notes, ())
    context = _output_context(answer, notes)
    assert context is not None and not context.claims[0].compared_records[0].complete


async def test_retired_known_sources_report_outputs_for_repair() -> None:
    page = capture((BlockKind.RECORD, "Project Birch: Player Oak says thanks."))
    source = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((source,))
    notes._retired.add(fact_id(source))
    answer = assemble_answer((Claim(text=source.text, evidence_ids=(fact_id(source),)),), notes, ())
    missing = []
    assert not await check_answer_outputs(
        RoutingJev(), ScriptedLLM([]), answer, notes, ("Report feedback.",), missing_outputs=missing
    )
    assert missing == ["Report feedback."]


@pytest.mark.parametrize("preserve_collections", [False, True])
async def test_collection_instructions_are_scoped_to_partitioned_reads(preserve_collections) -> None:
    reader = ScriptedLLM([{"answered": False, "claims": [], "continues": []}])
    await read(
        reader,
        capture((BlockKind.PARAGRAPH, "Account code: 123456")),
        "Keep the code needed for sign-in.",
        ["r"],
        Notes(),
        preserve_collections=preserve_collections,
    )
    prompt = reader.calls[0][1][0].content
    assert ("# Collection evidence" in prompt) is preserve_collections
    assert "For a comparison, put every compared record" in prompt


@pytest.mark.parametrize("incomplete", [False, True])
async def test_partitioned_read_keeps_records_from_context_in_derived_answer(incomplete: bool) -> None:
    page = capture(
        (BlockKind.HEADING, "Birch: two comments, author Ash."),
        (BlockKind.RECORD, "Player Elm: Please add a compact view."),
        (BlockKind.RECORD, "Player Oak: Thanks for the update."),
    )
    reader = ScriptedLLM(
        [
            {
                "answered": True,
                "claims": [
                    {
                        "cite": {"first": "s0", "last": "s2"},
                        "records": [{"first": f"s{i}", "last": f"s{i}"} for i in range(3)],
                        "text": "Birch has two player comments.",
                    },
                    {
                        "cite": None,
                        "draws_on": ["claim:0"],
                        "requirement_id": "r",
                        "text": "Neither of Birch's two comments is an author reply.",
                    },
                ],
            }
        ]
    )
    notes = Notes()
    await read(
        reader,
        page,
        "Report author replies for Birch.",
        ["r"],
        notes,
        preserve_collections=True,
        notice="Next page available." if incomplete else "",
    )
    conclusion = next(fact for fact in notes.facts if fact.comparison is not None)
    assert conclusion.comparison is not None
    assert conclusion.comparison.complete is (not incomplete)
    assert len(conclusion.comparison.records) == 3
    assert {notes.evidence[key].quote for key in conclusion.comparison.records} == set(page.text.split("\n\n"))


@pytest.mark.parametrize("retired_quote", [False, True])
async def test_partition_composer_uses_quotes_of_inactive_plain_summary(retired_quote) -> None:
    from fastbrowse.planner import Plan
    from fastbrowse.retrieval import compose

    page = capture((BlockKind.RECORD, "Birch source: https://birch.test/code"))
    source = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    old = Fact(
        requirement_id="r",
        text="Birch has a code link.",
        evidence=None,
        basis=(fact_id(source),),
        reader=FactReader.LLM,
    )
    notes = Notes((source, old))
    notes.unevidence(("r",))
    if retired_quote:
        notes._retired.add(fact_id(source))
    plan = Plan(
        requirements=(Requirement(id="r", text="Report Birch's code link.", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    llm = ScriptedLLM([{"claims": [{"text": page.text, "evidence_ids": ["e1"]}]}])
    answer = await compose(llm, plan.requirements[0].text, plan, notes, preserve_collections=True)
    assert answer.data.claims[0].evidence_ids == (fact_id(source),)
    assert (_output_context(answer.data, notes) is not None) is (not retired_quote)


@pytest.mark.parametrize("coverage", ["complete", "incomplete", "missing_record", "inactive"])
async def test_partition_composer_keeps_collection_metadata_for_its_selected_records(coverage: str) -> None:
    from fastbrowse.planner import Plan
    from fastbrowse.retrieval import compose

    page = capture(
        (BlockKind.HEADING, "Birch: two player comments."),
        (BlockKind.RECORD, "Elm: Nice work."),
        (BlockKind.RECORD, "Oak: Thanks."),
    )
    records = tuple(
        Fact(text=page.text[b.start : b.end], evidence=block_evidence(page, b.source_id), reader=FactReader.LLM)
        for b in page.blocks
    )
    collection = Fact(
        requirement_id="r",
        text="The two comments on Birch are praise.",
        evidence=None,
        basis=tuple(fact_id(f) for f in records),
        reader=FactReader.LLM,
        comparison=Comparison(
            requirement_id="r", records=tuple(fact_id(f) for f in records), complete=coverage != "incomplete"
        ),
    )
    notes = Notes((*records, collection))
    if coverage == "inactive":
        notes.unevidence(("r",))
    plan = Plan(
        requirements=(Requirement(id="r", text="Report Birch feature requests.", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    citations: list[JsonValue] = ["e0", "e1"] if coverage == "missing_record" else ["e0", "e1", "e2"]
    llm = ScriptedLLM(
        [{"claims": [{"text": "Neither of Birch's two comments requests a feature.", "evidence_ids": citations}]}]
    )
    answer = (await compose(llm, plan.requirements[0].text, plan, notes, preserve_collections=True)).data
    context = _output_context(answer, notes)
    assert context is not None
    compared = context.claims[0].compared_records
    if coverage in {"missing_record", "inactive"}:
        assert not compared
    else:
        assert len(compared) == 1
        assert compared[0].complete is (coverage == "complete")


async def test_output_recovery_reads_collection_scope_with_current_requirement_sources() -> None:
    from unittest.mock import Mock

    from fastbrowse.agent import Agent
    from fastbrowse.page import Page
    from fastbrowse.planner import Plan
    from tests.test_agent import run_state
    from tests.test_policy import ScriptedJev

    page = capture(
        (BlockKind.HEADING, "Birch: two player comments."),
        (BlockKind.RECORD, "Elm: Nice work."),
        (BlockKind.RECORD, "Oak: Thanks."),
    )
    heading = Fact(
        requirement_id="r",
        text=page.text[: page.blocks[0].end],
        evidence=block_evidence(page, "s0"),
        reader=FactReader.LLM,
    )
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text="Report author replies for Birch.", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    state.notes = Notes((heading,))
    state.owes_read = False
    state.open_answer_outputs = (state.plan.requirements[0].text,)
    llm = ScriptedLLM(
        [
            {
                "answered": True,
                "claims": [
                    {
                        "requirement_id": "r",
                        "text": "Neither of Birch's two player comments is an author reply.",
                        "cite": None,
                        "records": [{"first": "s0", "last": "s2"}],
                    }
                ],
                "continues": [],
            }
        ]
    )
    agent = Agent(Mock(spec=Page), ScriptedJev({"r": "synthesis"}, noul=0.99), llm)
    await agent._read(state, page)
    assert state.notes.current(fact_id(heading))
    collected = [fact for fact in state.notes.facts if fact.comparison is not None]
    assert collected and collected[-1].comparison is not None and collected[-1].comparison.complete
