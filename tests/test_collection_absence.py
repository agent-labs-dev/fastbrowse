"""An absent member needs the identified collection and every record in its scope."""

import json

import pytest

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
        rendered = notes.render(10_000)
        assert "collection_for=r complete=true" in rendered


@pytest.mark.parametrize("retired_source", [False, True])
async def test_new_conclusion_uses_sources_of_an_inactive_summary_without_reviving_stale_quotes(retired_source) -> None:
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
    )
    fresh = notes.supporting("r")[-1][1]
    assert fresh.basis == (fact_id(source),)
    answer = assemble_answer((Claim(text=fresh.text, evidence_ids=(fact_id(fresh),)),), notes, ())
    assert (_output_context(answer, notes) is not None) is (not retired_source)


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
