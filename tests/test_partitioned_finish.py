"""Independent outputs retain their full evidence when the combined finish exceeds a prompt."""

import json
import re
from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer
from fastbrowse.memory import Fact, Notes, NotesTooLarge, fact_id
from fastbrowse.models import CostBasis, CostComponent, CostLine, FactReader, LLMPurpose, Operation, Status, StepOutcome
from fastbrowse.page import BlockKind, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind, partition_plan
from fastbrowse.policy import HistoryEntry
from tests.test_agent import run_state
from tests.test_policy import observation
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


def project_notes(size: int = 24_000) -> tuple[Plan, Notes]:
    requirements = tuple(
        Requirement(id=f"r{i}", text=f"Report the reply for Project {i}.", kind=RequirementKind.INFORMATION)
        for i in range(5)
    )
    notes = Notes()
    for i, requirement in enumerate(requirements):
        text = f"Project {i}: Author says the fix is available."
        page = capture(
            (BlockKind.PARAGRAPH, text + " Quoted context." * (size // 16)), url=f"https://projects.test/{i}"
        )
        notes.remember_capture(page)
        notes.add(
            Fact(requirement_id=requirement.id, text=text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
        )
    return Plan(
        requirements=requirements, answer_expected=True, answer_checks=tuple(r.text for r in requirements)
    ), notes


class FinishJev:
    def __init__(self):
        self.states = []

    async def evaluate(self, state, questions):
        self.states.append(state)
        answers = {
            key: ChoiceAnswer(choice="all", probabilities={"all": 1}, confidence=1)
            if isinstance(question, ChoiceQuestion)
            else NoulAnswer(probability=1 if key in {"complete", "draft_needs_writing"} else 0)
            for key, question in questions.items()
        }
        return Evaluation(
            model="test",
            answers=answers,
            input_tokens=1,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0),
        )


class FinishWriter(ScriptedLLM):
    def __init__(self, unsupported: int | None = None):
        super().__init__([])
        self.unsupported = unsupported
        self.composed = []

    async def generate(self, purpose, messages, schema, **kwargs):
        content = messages[-1].content
        if schema.__name__ == "_OutputBindings":
            payload = json.loads(content)
            self.responses.append(
                {
                    "bindings": [
                        {"check_index": i, "requirement_ids": [f"r{i}"]}
                        for i, _ in enumerate(payload["plan"]["answer_checks"])
                    ]
                }
            )
        elif purpose is LLMPurpose.COMPOSE:
            offered = content.split("# Notes\n", 1)[1]
            projects = re.findall(r'\[(e\d+)\] "(Project (\d+): Author says the fix is available\.)"', offered)
            assert projects
            self.composed.append(tuple(int(project) for _, _, project in projects))
            self.responses.append({"claims": [{"text": text, "evidence_ids": [ref]} for ref, text, _ in projects]})
        elif schema.__name__ != "_OutputIdentities":
            key, field = next(iter(json.loads(content)["criteria"].items()))
            fails = self.unsupported is not None and f"Project {self.unsupported}" in field["criterion"]
            self.responses.append(
                {
                    "judgments": {key: "no" if fails else "yes"},
                    "reason": "Requested reply missing." if fails else "Reply quoted.",
                }
            )
        return await super().generate(purpose, messages, schema, **kwargs)


async def finish_agent(size: int = 24_000, unsupported: int | None = None):
    state = await run_state()
    state.task = "Report the reply for each of five projects."
    state.ready_plan, state.notes = project_notes(size)
    page = Mock(spec=Page)
    page.artifacts = ()
    jev, llm = FinishJev(), FinishWriter(unsupported)
    agent = Agent(page, jev, llm)
    seen = observation(())
    agent._observed = seen
    agent._raw_observation = seen
    agent._observe = AsyncMock(return_value=seen)
    agent._observe_if_changed = AsyncMock(return_value=seen)
    agent._recover = AsyncMock()
    agent._screenshots = AsyncMock(return_value=())

    async def ending(result, **kwargs):
        return result

    agent._ending_frame = AsyncMock(side_effect=ending)
    return agent, state, jev, llm


async def test_oversized_finish_completes_with_all_five_projects_and_bounded_prompts() -> None:
    agent, state, jev, llm = await finish_agent()
    with pytest.raises(NotesTooLarge):
        state.notes.render(48_000, preserve_requirements=True)
    result = await agent._finish(state, None, None)
    assert result is not None and result.status is Status.COMPLETE
    assert {citation.url for citation in result.citations} == {f"https://projects.test/{i}" for i in range(5)}
    assert llm.composed == [(i,) for i in range(5)]
    assert all(len(json.dumps(value)) <= 48_000 for value in jev.states)
    assert all(sum(len(message.content) for message in messages) < 72_000 for _, messages in llm.calls)
    assert state.ledger.breakdown().has_unknown is False
    agent._recover.assert_not_awaited()


async def test_unsupported_output_reopens_only_its_requirement_and_keeps_verified_partial() -> None:
    agent, state, _, llm = await finish_agent(unsupported=3)
    assert await agent._finish(state, None, None) is None
    assert {r.id for r in state.notes.unresolved(state.plan)} == {"r3"}
    assert state.missing_answer_outputs == (state.plan.requirements[3].text,)
    partial = agent._partial_result(state.notes, state, state.ledger, Status.UNVERIFIED, "Missing reply")
    assert partial.status is Status.UNVERIFIED and partial.answer.startswith("Partial answer.")
    assert {citation.url for citation in partial.citations} == {f"https://projects.test/{i}" for i in (0, 1, 2, 4)}
    assert "Project 3: Author says" not in partial.answer
    previous = len(llm.composed)
    assert await agent._finish(state, None, None) is None
    assert len(llm.composed) == previous


async def test_finish_that_fits_keeps_one_compose_and_does_not_bind_outputs() -> None:
    agent, state, _, llm = await finish_agent(size=100)
    result = await agent._finish(state, None, None)
    assert result is not None and result.status is Status.COMPLETE
    assert llm.composed == [(0, 1, 2, 3, 4)]
    assert not getattr(state, "finish_partitions", ())
    assert not any(purpose is LLMPurpose.PLAN for purpose, _ in llm.calls)


@pytest.mark.parametrize("new_source", [True, False])
async def test_recovered_output_evidence_is_checked_before_more_browsing(new_source, monkeypatch) -> None:
    agent, state, _, _ = await finish_agent(size=100)
    state.open_answer_outputs = (state.plan.answer_checks[0],)
    state.history.append(
        HistoryEntry(
            operation=Operation.READ,
            target=None,
            outcome=StepOutcome.EXECUTED,
            page_changed=False,
            read_progress=new_source,
        )
    )
    agent._reread_if_changed = AsyncMock(return_value=False)
    finish = agent._finish = AsyncMock(wraps=agent._finish)
    choosing = Mock(side_effect=RuntimeError("choosing another browser action"))
    monkeypatch.setattr("fastbrowse.agent.decide", choosing)
    if new_source:
        result = await agent._loop(state, None, None)
        assert result.status is Status.COMPLETE
        finish.assert_awaited_once()
        choosing.assert_not_called()
    else:
        with pytest.raises(RuntimeError, match="choosing another browser action"):
            await agent._loop(state, None, None)
        finish.assert_not_awaited()


async def test_one_indivisible_oversized_requirement_stays_unverified() -> None:
    agent, state, _, _ = await finish_agent(size=85_000)
    result = await agent._finish(state, None, None)
    assert result is not None and result.status is Status.UNVERIFIED
    assert "character notes budget" in result.error
    agent._recover.assert_not_awaited()


def test_scoped_notes_keep_shared_ownership_basis_identity_freshness_and_counterevidence() -> None:
    _, notes = project_notes(size=100)
    source = notes.facts[0]
    notes.add(source.model_copy(update={"requirement_id": "shared"}))
    notes.unevidence(("r0", "shared"))
    page = capture(
        (BlockKind.HEADING, "Project 0"), (BlockKind.PARAGRAPH, "Reply withdrawn."), url="https://projects.test/0"
    )
    notes.remember_capture(page)
    context = Fact(text="Reply withdrawn.", evidence=block_evidence(page, "s1"), reader=FactReader.LLM)
    notes.add(context)
    scoped = notes.for_requirements(("shared",))
    assert scoped.facts == (source, context)
    assert not scoped.current(fact_id(source))
    assert scoped.current(fact_id(context))
    assert context.evidence is not None
    remembered = scoped.captured_page(context.evidence)
    assert remembered is not None and remembered.title == page.title
    scoped.add(context.model_copy(update={"requirement_id": "shared"}))
    assert not notes.evidenced("shared")


async def test_binding_keeps_cross_requirement_comparisons_together() -> None:
    plan, _ = project_notes(size=100)
    llm = ScriptedLLM(
        [{"bindings": [{"check_index": i, "requirement_ids": ["r0", "r1"] if i < 2 else [f"r{i}"]} for i in range(5)]}]
    )
    state = await run_state()
    groups = await partition_plan(llm, "Compare two projects, then report three replies.", plan, ledger=state.ledger)
    assert [tuple(r.id for r in group.requirements) for group in groups] == [("r0", "r1"), ("r2",), ("r3",), ("r4",)]
    assert groups[0].answer_checks == plan.answer_checks[:2]


async def test_scoped_finish_keeps_actions_together_and_does_not_invent_action_answers() -> None:
    plan, _ = project_notes(size=100)
    actions = tuple(
        Requirement(id=f"action{i}", text=text, kind=RequirementKind.ACTION)
        for i, text in enumerate(("Enter the first name.", "Go back and correct the name."))
    )
    plan = plan.model_copy(update={"requirements": (*actions, *plan.requirements)})
    state = await run_state()
    groups = await partition_plan(
        FinishWriter(), "Correct the name, then read five replies.", plan, ledger=state.ledger
    )
    assert tuple(r.id for r in groups[0].requirements) == ("action0", "action1")
    assert not groups[0].answer_expected
    assert all(group.answer_expected for group in groups[1:])


async def test_unknown_partition_charge_stops_before_another_call() -> None:
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger

    agent, state, jev, _ = await finish_agent()
    state.ledger = Ledger(Limits(max_dollars=8))

    class UnpricedBinding(FinishWriter):
        async def generate(self, purpose, messages, schema, **kwargs):
            result = await super().generate(purpose, messages, schema, **kwargs)
            return result.model_copy(
                update={
                    "cost": result.cost.model_copy(
                        update={
                            "basis": CostBasis.UNKNOWN,
                            "dollars": None,
                        }
                    )
                }
            )

    llm = UnpricedBinding()
    agent._llm = llm
    with pytest.raises(BudgetExceeded):
        await agent._finish(state, None, None)
    assert state.ledger.breakdown().has_unknown
    assert len(llm.calls) == 1 and not jev.states


async def test_unknown_charge_after_one_verified_group_keeps_a_truthful_partial() -> None:
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger

    agent, state, _, _ = await finish_agent()
    state.ledger = Ledger(Limits(max_dollars=8))

    class UnpricedSecondAnswer(FinishWriter):
        async def generate(self, purpose, messages, schema, **kwargs):
            result = await super().generate(purpose, messages, schema, **kwargs)
            if purpose is LLMPurpose.COMPOSE and len(self.composed) == 2:
                return result.model_copy(
                    update={"cost": result.cost.model_copy(update={"basis": CostBasis.UNKNOWN, "dollars": None})}
                )
            return result

    agent._llm = UnpricedSecondAnswer()
    with pytest.raises(BudgetExceeded):
        await agent._finish(state, None, None)
    result = agent._partial_result(state.notes, state, state.ledger, Status.BUDGET_EXCEEDED, "Unknown charge")
    assert result.status is Status.BUDGET_EXCEEDED and result.cost.has_unknown
    assert {citation.url for citation in result.citations} == {"https://projects.test/0"}
    assert result.answer.startswith("Partial answer. Unverified outputs:")
    assert all(requirement.text in result.answer for requirement in state.plan.requirements[1:])


def test_scoped_comparison_keeps_transitive_records_and_their_identity_quotes() -> None:
    from fastbrowse.memory import Comparison

    _, notes = project_notes(size=100)
    operands = notes.facts[:2]
    derived = Fact(
        requirement_id="winner",
        text="Project 0 replied first.",
        evidence=None,
        basis=tuple(fact_id(fact) for fact in operands),
        reader=FactReader.LLM,
        comparison=Comparison(requirement_id="winner", records=tuple(fact_id(f) for f in operands), complete=True),
    )
    notes.add(derived)
    scoped = notes.for_requirements(("winner",))
    assert scoped.facts == (*operands, derived)
    assert scoped.supporting_evidence("winner") == tuple(f.evidence for f in operands)
    assert scoped.facts[-1].comparison is not None and scoped.facts[-1].comparison.complete
    assert scoped.fact_requirements(fact_id(derived)) == ("winner",)


async def test_new_counterevidence_invalidates_only_its_verified_group() -> None:
    agent, state, _, llm = await finish_agent()
    assert (await agent._finish(state, None, None)).status is Status.COMPLETE
    page = capture((BlockKind.PARAGRAPH, "Project 2: The author withdrew the reply."), url="https://projects.test/2")
    fact = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    state.notes.add(fact)
    llm.unsupported = 2
    assert await agent._finish(state, None, None) is None
    assert llm.composed[5:] and all(group == (2,) for group in llm.composed[5:])
    audits = [
        json.loads(messages[-1].content)
        for purpose, messages in llm.calls
        if purpose is LLMPurpose.VERIFY and '"counterevidence"' in messages[-1].content
    ]
    assert fact.evidence is not None
    assert any(fact.evidence.quote in json.dumps(audit) for audit in audits)
    assert {r.id for r in state.notes.unresolved(state.plan)} == {"r2"}


@pytest.mark.parametrize("addition", ["summary", "unrelated_navigation"])
async def test_repeated_summary_and_unrelated_navigation_keep_verified_outputs(addition: str) -> None:
    agent, state, _, llm = await finish_agent()
    assert (await agent._finish(state, None, None)).status is Status.COMPLETE
    original = state.notes.facts[3]
    if addition == "summary":
        state.notes.add(
            Fact(
                requirement_id="r3",
                text="Project 3: Author says the fix is available.",
                evidence=None,
                basis=(fact_id(original),),
                reader=FactReader.LLM,
            )
        )
    else:
        state.invented.add("https://unrelated.test/search?q=new")
    assert (await agent._finish(state, None, None)).status is Status.COMPLETE
    assert len(llm.composed) == 5


async def test_changed_group_rechecks_its_verified_draft_before_composing_again() -> None:
    agent, state, _, llm = await finish_agent()
    assert (await agent._finish(state, None, None)).status is Status.COMPLETE
    page = capture((BlockKind.PARAGRAPH, "Project 3: Translations are welcome."), url="https://projects.test/3")
    state.notes.add(Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM))
    checking = agent._holds
    agent._holds = AsyncMock(wraps=checking)
    assert (await agent._finish(state, None, None)).status is Status.COMPLETE
    assert len(llm.composed) == 5
    agent._holds.assert_awaited()
