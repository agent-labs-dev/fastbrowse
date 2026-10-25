"""Answers spanning five sources retain full quotes through output verification."""

import pytest

from fastbrowse.config import Thresholds
from fastbrowse.memory import Fact, Notes, NotesTooLarge
from fastbrowse.models import FactReader
from fastbrowse.page import BlockKind
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.retrieval import OUTPUT_MISSING, compose, partial_answer
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


@pytest.mark.parametrize("with_checks", [True, False])
async def test_five_source_answer_above_notes_budget_keeps_full_quote_checks(with_checks: bool) -> None:
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
    llm = ScriptedLLM(
        [{"claims": [{"text": fact.text, "evidence_ids": [f"e{i}"]} for i, fact in enumerate(notes.facts)]}]
    )
    answer = (await compose(llm, task, plan, notes)).data
    jev = ScriptedJev({}, noul=0)
    if not with_checks:
        with pytest.raises(NotesTooLarge):
            await check_claims(jev, answer, notes, Thresholds())
        return

    held = await check_claims(jev, answer, notes, Thresholds(), answer_checks=checks)
    assert held == answer
    assert notes.facts == original
    assert {citation.url for citation in answer.citations} == {item.url for item in notes.evidence.values()}
    questions = {key: question for request in jev.requests for key, question in request.items()}
    # Each check's omission question is asked of the answer alone, so the notes never need to fit beside it.
    assert "requirement_omitted" not in questions
    for index, check in enumerate(checks):
        assert check in questions[f"{OUTPUT_MISSING}{index}"].instructions
        assert "# Notes" not in questions[f"{OUTPUT_MISSING}{index}"].instructions
    for index, fact in enumerate(notes.facts):
        assert fact.evidence is not None
        for issue in ("unsupported", "contradicted"):
            assert fact.evidence.model_dump_json() in questions[f"{issue}_{index}"].instructions
