"""Bounded action context retains partial reads across a multi-project audit."""

from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent, _code_decision, _Stop
from fastbrowse.config import ObservationLimits
from fastbrowse.memory import Fact, Notes
from fastbrowse.models import FactReader, Operation, Status
from fastbrowse.page import BlockKind, Control, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from tests.test_agent import field, run_state
from tests.test_memory import evidence
from tests.test_policy import ScriptedJev, observation
from tests.test_retrieval import ScriptedLLM, block_evidence, capture

PROJECTS = ("Map Paths", "Quest Guide", "Craft Skills", "Old Items", "UI Tweaks")


def _audit_notes(last_step: int) -> Notes:
    # Recapturing comments must retain the earlier projects while a compound requirement remains open.
    reads = (
        (1, 0, 10),
        (4, 0, 5),
        (13, 1, 10),
        (17, 1, 4),
        (24, 1, 7),
        (30, 2, 6),
        (33, 2, 6),
        (38, 2, 7),
        (50, 1, 11),
        (53, 1, 10),
        (56, 1, 11),
        (61, 1, 10),
    )
    notes = Notes()
    for step, project, count in reads:
        if step > last_step:
            break
        for index in range(count):
            quote = f"Report {index}: " + "The tracker overlaps the map after opening the settings. " * 12
            span = evidence(sha=f"step-{step}", start=index * 1000, end=index * 1000 + len(quote)).model_copy(
                update={"url": f"https://projects.test/{project}/comments", "quote": quote}
            )
            notes.add(
                Fact(
                    text=f"{PROJECTS[project]} player report {index}: the tracker overlaps the map.",
                    evidence=span,
                    reader=FactReader.LLM,
                )
            )
    return notes


@pytest.mark.parametrize("last_step", [38, 61])
@pytest.mark.parametrize("consumer", ["navigation", "field", "recovery"])
async def test_multi_project_reads_survive_bounded_action_context(last_step: int, consumer: str) -> None:
    state = await run_state()
    state.task = f"Read player reports and author replies across {', '.join(PROJECTS)}. Read only."
    requirement = Requirement(id="reports", text=state.task, kind=RequirementKind.INFORMATION)
    state.ready_plan = Plan(requirements=(requirement,), answer_expected=True)
    state.notes = _audit_notes(last_step)
    before = state.notes.facts
    target = field()
    obs = observation((target,))
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"png")
    llm = ScriptedLLM(
        [
            {
                "diagnosis": "Continue the audit",
                "next_subgoal": "Search",
                "give_up": False,
                "operation": "fill",
                "control": 0,
            }
        ]
    )
    agent = Agent(page, ScriptedJev({}), llm)

    if consumer == "navigation":
        context = agent._context(state, (), check_login=False, check_bot=False)
        shown = context.notes
        assert context.unread_requirements == (requirement.text,)
    elif consumer == "field":
        field_context = await agent._field_context(state, obs, target)
        shown = field_context["notes"]
    else:
        await agent._recover(state, obs, "uncertain next step (0.35)")
        prompt = llm.calls[0][1][1].content
        shown = prompt.split("## Notes read so far\n", 1)[1].split("\n\n## Recent steps", 1)[0]
        assert f"## Still to find\n- {requirement.text}" in prompt

    assert isinstance(shown, str)
    assert len(shown) <= ObservationLimits().working_notes_chars
    for project, name in enumerate(PROJECTS[:3]):
        assert f"https://projects.test/{project}/comments" in shown
        assert f"{name} player report" in shown
    assert all(name not in shown for name in PROJECTS[3:])
    assert state.notes.unresolved(state.ready_plan) == (requirement,)
    assert state.notes.facts == before


async def test_reading_a_new_project_keeps_recovery_available_on_the_shared_list() -> None:
    state = await run_state()
    state.task = f"Read reports, replies and links for each of {', '.join(PROJECTS)}."
    requirements = tuple(
        Requirement(id=f"reports-{i}", text=f"Read reports for {name}.", kind=RequirementKind.INFORMATION)
        for i, name in enumerate(PROJECTS)
    )
    state.ready_plan = Plan(requirements=requirements, answer_expected=True)
    for index, requirement_id in ((0, None), (4, "reports-4")):
        source = capture(
            (BlockKind.PARAGRAPH, f"{PROJECTS[index]} player report."), url=f"https://projects.test/{index}"
        )
        state.notes.add(
            Fact(
                text=source.text,
                requirement_id=requirement_id,
                evidence=block_evidence(source, "s0"),
                reader=FactReader.LLM,
            )
        )
    links = tuple(
        Control(
            id=f"project-{i}",
            frame_id=None,
            role="link",
            label=name,
            href=f"https://projects.test/{i}",
            operations=frozenset({Operation.CLICK}),
        )
        for i, name in enumerate(PROJECTS)
    )
    listing = observation(links).model_copy(update={"url": "https://projects.test/author"})
    source = capture((BlockKind.PARAGRAPH, "Old Items: no comments yet."), url="https://projects.test/3/comments")
    comments = observation(()).model_copy(update={"url": source.url})
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"png")
    llm = ScriptedLLM(
        [
            {"diagnosis": "Inspect comments", "next_subgoal": "Read", "operation": "read", "give_up": False},
            {
                "claims": [
                    {
                        "text": source.text,
                        "requirement_id": "reports-3",
                        "cite": {"first": "s0", "last": "s0"},
                    }
                ],
                "answered": True,
            },
            {"diagnosis": "Return to the projects", "next_subgoal": "Go back", "operation": "back", "give_up": False},
            {
                "diagnosis": "Quest Guide remains",
                "next_subgoal": "Open Quest Guide",
                "operation": "click",
                "control": 1,
                "give_up": False,
            },
        ]
    )
    agent = Agent(page, ScriptedJev({r.id: "synthesis" for r in requirements}), llm)
    agent._settle(state, listing)
    agent._settle(state, comments)
    await agent._recover(state, comments, "uncertain next step (0.29)")
    await agent._step(state, comments, _code_decision(Operation.READ, None), capture=source)
    await agent._recover(state, comments, "uncertain next step (0.20)")
    agent._settle(state, listing)

    context = agent._context(state, (), check_login=False, check_bot=False)
    assert context.unread_requirements == tuple(r.text for r in requirements[:3])
    await agent._recover(state, listing, "uncertain next step (0.27)")
    assert state.directed == (Operation.CLICK, "project-1")
    prompt = llm.calls[-1][1][-1].content
    assert "## Still to find\n" + "\n".join(f"- {r.text}" for r in requirements[:3]) in prompt

    # Reopening and reading the same requirement cannot renew recovery without end.
    state.notes.unevidence(("reports-3",))
    state.notes.add(
        Fact(
            text=source.text,
            requirement_id="reports-3",
            evidence=block_evidence(source, "s0"),
            reader=FactReader.LLM,
        )
    )
    with pytest.raises(_Stop) as stopped:
        await agent._recover(state, listing, "link still did not open")
    assert stopped.value.status is Status.STUCK
    assert "Still to find:" in str(stopped.value)
    assert all(r.text in str(stopped.value) for r in requirements[:3])
