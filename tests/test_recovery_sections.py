"""Missing output recovery follows observed sections and audits retained evidence."""

from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent, _code_decision
from fastbrowse.memory import CollectionFact, Comparison, Notes, fact_id
from fastbrowse.models import FactReader, Operation, Status, StepOutcome
from fastbrowse.page import ActResult, BlockKind, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from tests.test_agent import _link, open_outputs, run_state
from tests.test_partitioned_finish import finish_agent
from tests.test_policy import ScriptedJev, observation
from tests.test_retrieval import ScriptedLLM, capture


@pytest.mark.parametrize("already_read", [False, True])
async def test_recovery_chooses_unread_target_section_before_model_scroll(already_read):
    state = await run_state()
    open_outputs(state, ("links", "Project Birch"))
    url = "https://example.test/projects/birch"
    seen = observation((_link("description", "Description", url),)).model_copy(
        update={"url": url + "/comments", "title": "Project Birch comments"}
    )
    Agent._note_effect(state, seen)
    if already_read:
        state.read_urls.add(url)
    llm = ScriptedLLM(
        [{"diagnosis": "Look lower.", "next_subgoal": "Scroll.", "operation": "scroll", "give_up": False}]
    )
    agent = Agent(Mock(spec=Page), ScriptedJev({}), llm)
    agent._screenshots = AsyncMock(return_value=())
    await agent._recover(state, seen, "Missing quoted link destination.")
    if already_read:
        assert state.directed == (Operation.SCROLL, None)
    else:
        assert state.directed == (Operation.NAVIGATE, url)
        assert not llm.calls


async def test_grouped_finish_audits_unbound_quotes_without_relaxing_completion():
    agent, state, _, writer = await finish_agent(size=100)
    state.oversized_answer = True
    _unbind_partial_reads(state)
    result = await agent._finish(state, None, None)
    assert writer.composed, "Retained source quotes must reach the answer audit"
    assert result is not None and result.status is Status.COMPLETE


async def test_grouped_recovery_done_reaches_audit_with_unbound_notes():
    agent, state, _, _ = await finish_agent(size=100)
    state.oversized_answer = True
    _unbind_partial_reads(state)
    state.read_here = True
    agent._llm = ScriptedLLM(
        [{"diagnosis": "All sources read.", "next_subgoal": "Audit the answer.", "operation": "done", "give_up": False}]
    )
    await Agent._recover(agent, state, observation(()), "No further action.")
    assert state.directed == (Operation.DONE, None)


def _unbind_partial_reads(state):
    notes = Notes()
    for fact in state.notes.facts:
        context = fact.model_copy(update={"requirement_id": None})
        notes.add(context)
        notes.add(
            CollectionFact(
                text="Read source records",
                evidence=None,
                basis=(fact_id(context),),
                reader=FactReader.LLM,
                scope="Source records",
                comparison=Comparison(requirement_id=fact.requirement_id, records=(fact_id(context),), complete=True),
            )
        )
    notes._pages = dict(state.notes._pages)
    state.notes = notes


@pytest.mark.parametrize("changed_content", [False, True])
@pytest.mark.parametrize("kind", list(RequirementKind))
@pytest.mark.parametrize("open_output", [False, True])
async def test_scrolling_a_read_lookup_credits_new_content_only(
    changed_content: bool, kind: RequirementKind, open_output: bool
) -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text="Inspect project links", kind=kind),),
        answer_expected=True,
    )
    if open_output:
        open_outputs(state, ("links", "Birch"))
    here = observation(()).model_copy(update={"document_key": "doc"})
    captured = capture((BlockKind.PARAGRAPH, "All project comments"))
    state.reads.add((here.document_key, captured.read_key, ("r",), ()))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    page.capture = AsyncMock(return_value=captured)
    page.screenshot = AsyncMock(return_value=b"")
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    agent._recover = AsyncMock()
    agent._settle(state, here)
    for n in range(agent._config.stall.unchanged_actions):
        if changed_content:
            page.capture.return_value = capture((BlockKind.PARAGRAPH, f"Newly loaded comments {n}"))
        await agent._step(state, here, _code_decision(Operation.SCROLL, None))
        there = here.model_copy(update={"viewport_text": f"Viewport {n}"})
        agent._note_effect(state, there)
        agent._settle(state, there)
        here = there
    if changed_content or kind is RequirementKind.ACTION or not open_output:
        agent._recover.assert_not_awaited()
        assert state.unchanged == 0
    else:
        agent._recover.assert_awaited_once()
        assert "no_progress" in agent._recover.call_args.args[2]
        assert "already read" in (state.history[-1].effect or "")
    if kind is RequirementKind.ACTION or not open_output:
        page.capture.assert_not_awaited()


async def test_section_navigation_and_read_precede_another_choice_even_if_click_reported_no_change():
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text="Find links.", kind=RequirementKind.INFORMATION),), answer_expected=True
    )
    state.oversized_answer = True
    open_outputs(state, ("links", "Project Birch"))
    url = "https://example.test/birch"
    source = observation((_link("description", "Description", url),)).model_copy(
        update={"url": url + "/comments", "title": "Project Birch comments"}
    )
    destination = observation(()).model_copy(update={"url": url, "document_key": "destination"})
    Agent._note_effect(state, source)
    agent = Agent(Mock(spec=Page, artifacts=()), ScriptedJev({}), ScriptedLLM([]))
    agent._screenshots = AsyncMock(return_value=())
    agent._llm = ScriptedLLM(
        [{"diagnosis": "Look lower.", "next_subgoal": "Scroll.", "operation": "scroll", "give_up": False}]
    )
    agent._observe = AsyncMock(side_effect=[source, destination, destination])
    agent._finish = AsyncMock(return_value=agent._result(state, state.ledger, Status.UNVERIFIED))
    actions = []

    async def step(state, observed, decision, *args, **kwargs):
        actions.append((decision.operation, observed.url, decision.url))
        if decision.operation is Operation.READ:
            state.read_here = True
            state.read_urls.add(observed.url)
        return False

    agent._step = AsyncMock(side_effect=step)
    await agent._recover(state, source, "Missing links")
    assert state.directed == (Operation.NAVIGATE, url)
    result = await agent._loop(state, None, None)
    assert result.status is Status.UNVERIFIED
    assert actions == [(Operation.NAVIGATE, source.url, url), (Operation.READ, url, None)]


async def test_grouped_unbound_answer_with_failed_field_stays_open():
    agent, state, _, writer = await finish_agent(size=100, unsupported=2)
    state.oversized_answer = True
    _unbind_partial_reads(state)
    assert await agent._finish(state, None, None) is None
    assert writer.composed
    assert state.open_answer_outputs == ("Report the reply for Project 2.",)
    assert not state.notes.evidenced("r2")


@pytest.mark.parametrize("grouped", [False, True])
async def test_only_grouped_composer_requires_quoted_link_destinations(grouped):
    from fastbrowse.retrieval import compose

    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text="Find links.", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("Report links for Birch.",),
    )
    writer = ScriptedLLM([{"output_0": []} if grouped else {"claims": []}])
    await compose(writer, state.task, state.plan, state.notes, field_outputs=grouped)
    prompt = writer.calls[0][1][0].content
    assert ("quoted destination" in prompt) is grouped
    assert ("hostname or hostname/path" in prompt) is grouped


async def test_output_read_keeps_context_and_its_old_basis_in_the_target_audit(monkeypatch):
    from fastbrowse.memory import Fact
    from fastbrowse.retrieval import ReadOutcome
    from tests.test_retrieval import block_evidence

    state = await run_state()
    requirement = Requirement(id="r", text="Report replies and links for Birch.", kind=RequirementKind.INFORMATION)
    state.ready_plan = Plan(
        requirements=(requirement,), answer_expected=True, answer_checks=("Report links for Birch.",)
    )
    state.finish_partitions = (state.plan,)
    state.oversized_answer = True
    state.open_answer_outputs = state.plan.answer_checks
    source = capture((BlockKind.PARAGRAPH, "Birch source: https://code.test/birch"), url="https://example.test/birch")
    quoted = Fact(text=source.text, evidence=block_evidence(source, "s0"), reader=FactReader.LLM)
    state.notes.add(quoted)
    derived = Fact(text=source.text, evidence=None, basis=(fact_id(quoted),), reader=FactReader.LLM)

    async def read(*args, **kwargs):
        state.notes.add(derived)
        return ReadOutcome(facts=(derived,), coverage=(), rejected_claims=0, cost_lines=())

    monkeypatch.setattr("fastbrowse.agent.read", read)
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    state.rejected_answer_evidence = frozenset()
    progressed, _ = await agent._read(state, source)
    assert progressed and state.rejected_answer_evidence is None
    progressed, _ = await agent._read(state, source)
    assert not progressed
    scoped = state.notes.for_requirements(["r"])
    assert fact_id(quoted) in scoped.current_evidence()
    assert not scoped.evidenced("r"), "Source ownership does not verify the output"
    assert not state.notes.for_requirements(["another-target"]).facts


async def test_file_records_do_not_become_output_sections():
    from fastbrowse.agent import _unread_output_section

    state = await run_state()
    open_outputs(state, ("links", "birch"))
    main = "https://example.test/projects/birch"
    files = main + "/files"
    seen = observation((_link("file", "version 1.2", files + "/123"),)).model_copy(
        update={"url": files, "title": "Birch files"}
    )
    Agent._note_effect(state, seen)
    state.read_urls.update((main, files))
    assert _unread_output_section(state) is None


async def test_grouping_retains_unbound_quotes_from_the_named_target_before_recovery():
    agent, state, _, writer = await finish_agent(size=100)
    state.oversized_answer = True
    notes = Notes(fact.model_copy(update={"requirement_id": None}) for fact in state.notes.facts)
    notes._pages = {
        key: page.model_copy(update={"title": "Project " + page.url.rsplit("/", 1)[-1]})
        for key, page in state.notes._pages.items()
    }
    state.notes = notes
    assert not notes.for_requirements(["r0"]).facts
    result = await agent._finish(state, None, None)
    assert result is not None and result.status is Status.COMPLETE
    assert writer.composed


async def test_directory_and_other_targets_are_not_sections_of_this_target():
    from fastbrowse.agent import _unread_output_section

    state = await run_state()
    open_outputs(state, ("links", "Project Birch"))
    root = "https://example.test/projects/birch"
    seen = observation((_link("list", "Projects", "/projects"), _link("other", "Elm", "/projects/elm"))).model_copy(
        update={"url": root, "title": "Project Birch"}
    )
    Agent._note_effect(state, seen)
    state.read_urls.add(root)
    assert _unread_output_section(state) is None


async def test_an_exhausted_output_does_not_hide_another_targets_unread_section():
    from fastbrowse.agent import _unread_output_section

    state = await run_state()
    open_outputs(state, ("links", "Elm"), ("links", "Birch"))
    root = "https://example.test/projects/birch"
    seen = observation((_link("description", "Description", root),)).model_copy(
        update={"url": root + "/comments", "title": "Birch"}
    )
    Agent._note_effect(state, seen)
    assert _unread_output_section(state) == root


async def test_a_target_name_holding_for_keeps_its_whole_name():
    """Split on its last " for ", "Leatrix Plus for Classic" became "Classic" and recovery chose that site section."""
    from fastbrowse.agent import _unread_output_section

    state = await run_state()
    open_outputs(state, ("links", "Leatrix Plus for Classic"))
    root = "https://example.test/addons/leatrix-plus-for-classic"
    seen = observation(
        (
            _link("classic", "Classic", "/classic-era"),
            _link("news", "News", "/classic-era/news"),
            _link("description", "Description", root + "/description"),
        )
    ).model_copy(update={"url": root, "title": "Leatrix Plus for Classic"})
    Agent._note_effect(state, seen)
    state.read_urls.add(root)
    assert _unread_output_section(state) == root + "/description"
