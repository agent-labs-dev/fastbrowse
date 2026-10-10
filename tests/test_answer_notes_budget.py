"""Answers spanning five sources retain full quotes through output verification."""

import json

import pytest

from fastbrowse.config import Thresholds
from fastbrowse.llm import Generation
from fastbrowse.memory import Fact, Notes, NotesTooLarge
from fastbrowse.models import CostBasis, CostComponent, CostLine, FactReader, LLMPurpose
from fastbrowse.page import BlockKind
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.retrieval import compose, partial_answer
from fastbrowse.verification import check_claims
from tests.test_navigation_memory import PROJECTS
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


def test_partial_answer_retains_later_project_findings_within_the_budget() -> None:
    notes = Notes()
    for index, project in enumerate((PROJECTS[0], PROJECTS[4], PROJECTS[3])):
        reports = (
            tuple(f"Report {n}: " + "The map overlaps the tracker. " * 12 for n in range(10))
            if index == 0
            else (f"{project}: no comments yet.",)
        )
        source = capture(
            (BlockKind.HEADING, project),
            *((BlockKind.PARAGRAPH, report) for report in reports),
            url=f"https://projects.test/{index}/comments",
        )
        for block in source.blocks:
            notes.add(
                Fact(
                    text=source.text[block.start : block.end],
                    requirement_id=f"reports-{index}" if index and block.source_id == "s1" else None,
                    evidence=block_evidence(source, block.source_id),
                    reader=FactReader.LLM,
                )
            )
    original = notes.facts
    answer = partial_answer(notes, 1200)
    assert {citation.url for citation in answer.citations} == {
        f"https://projects.test/{index}/comments" for index in range(3)
    }
    assert f"{PROJECTS[3]}: no comments yet." in answer.answer
    assert f"{PROJECTS[4]}: no comments yet." in answer.answer
    assert len(answer.linked_answer) <= 1200
    assert notes.facts == original


@pytest.mark.parametrize("failure", [None, "source", "assertion", "missing", "legacy"])
async def test_five_source_answer_above_notes_budget_keeps_full_quote_checks(failure: str | None) -> None:
    notes = Notes()
    for project, size in zip(PROJECTS, (7000, 29000, 8000, 2000, 5000), strict=True):
        quote = f"{project}: report, author reply and link. " + "Comment context. " * (size // 17)
        page = capture((BlockKind.PARAGRAPH, quote), url=f"https://projects.test/{len(notes.facts)}/comments")
        notes.add(
            Fact(
                requirement_id="reports",
                text=f"{project}: report, author reply and link.",
                evidence=block_evidence(page, "s0"),
                reader=FactReader.LLM,
            )
        )
    original = notes.facts
    task = f"Read reports, author replies and links across {', '.join(PROJECTS)}."
    checks = tuple(f"Report the reports, author replies and links for {project}." for project in PROJECTS)
    plan = Plan(
        requirements=(Requirement(id="reports", text=task, kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=checks,
    )
    with pytest.raises(NotesTooLarge):
        notes.render(48_000, preserve_requirements=True, json_encoded=True)

    class WriterAndAuditor(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            payload = json.loads(messages[-1].content) if purpose is LLMPurpose.VERIFY else None
            if schema.__name__ == "_OutputIdentities":
                assert payload is not None
                self.calls.append((purpose, tuple(messages)))
                sources = payload["sources"]
                return Generation(
                    data=schema.model_validate(
                        {
                            "identities": [
                                {"id": f"i{index}", "source_ref": ref, "quote": project}
                                for index, (ref, project) in enumerate(zip(sources, PROJECTS, strict=True))
                            ],
                            "bindings": {
                                f"output_{index}": {"scope": "entities", "identity_ids": [f"i{index}"]}
                                for index in range(len(PROJECTS))
                            },
                        }
                    ),
                    cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0),
                )
            if payload is not None:
                key, field = next(iter(payload["criteria"].items()))
                stage = "assertion" if "reported_claims" in field else "source"
                self.responses.append(
                    {"judgments": {key: "no" if failure == stage else "yes"}, "reason": "Quoted source check."}
                )
            return await super().generate(purpose, messages, schema, **kwargs)

    llm = WriterAndAuditor(
        [{"claims": [{"text": fact.text, "evidence_ids": [f"e{i}"]} for i, fact in enumerate(notes.facts)]}]
    )
    answer = (await compose(llm, task, plan, notes)).data
    jev = ScriptedJev(
        {f"output_{index}": "none" if failure == "missing" and index == 4 else f"claim_{index}" for index in range(5)},
        noul=0,
    )
    if failure == "legacy":
        with pytest.raises(NotesTooLarge):
            await check_claims(jev, answer, notes, Thresholds())
        return

    held = await check_claims(jev, answer, notes, Thresholds(), answer_checks=checks, llm=llm, task=task)
    assert (held == answer) is (failure is None)
    assert notes.facts == original
    assert {citation.url for citation in answer.citations} == {item.url for item in notes.evidence.values()}
    questions = {key: question for request in jev.requests for key, question in request.items()}
    assert "requirement_omitted" not in questions
    for index, fact in enumerate(notes.facts):
        assert fact.evidence is not None
        for issue in ("unsupported", "contradicted"):
            assert fact.evidence.model_dump_json() in questions[f"{issue}_{index}"].instructions
    if failure != "missing":
        audits = [
            json.loads(messages[-1].content)
            for purpose, messages in llm.calls
            if purpose is LLMPurpose.VERIFY and '"criteria"' in messages[-1].content
        ]
        for index, fact in enumerate(notes.facts):
            assert fact.evidence is not None
            fields = [
                audit["criteria"][f"output_{index}"] for audit in audits if f"output_{index}" in audit["criteria"]
            ]
            sources = [field for field in fields if "sources" in field]
            assert len(sources) == 1
            assert sources[0]["sources"][0]["cited_sources"][0]["quote"] == fact.evidence.quote
            if failure != "source":
                assertions = [field for field in fields if "reported_claims" in field]
                assert len(assertions) == 1
                assert assertions[0]["reported_claims"][0]["cited_sources"][0]["quote"] == fact.evidence.quote
