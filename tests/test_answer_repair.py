"""An answer repair receives the failed checks, retains quotes, and goes through the same checks again."""

from unittest.mock import AsyncMock, Mock

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent
from fastbrowse.config import TokenBudget
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer
from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import CostBasis, CostComponent, CostLine, FactReader, LLMPurpose
from fastbrowse.page import BlockKind, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.retrieval import OUTPUT_MISSING, Claim, FieldAnswer, assemble_answer, compose
from fastbrowse.verification import check_answer_outputs
from tests.test_agent import run_state
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


class RoutingJev:
    async def evaluate(self, state, questions):
        return Evaluation(
            model="test",
            answers={
                key: ChoiceAnswer(choice="claim_0", probabilities={"claim_0": 1}, confidence=1)
                if isinstance(question, ChoiceQuestion)
                else NoulAnswer(probability=0)
                for key, question in questions.items()
            },
            input_tokens=1,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0),
        )


@pytest.mark.parametrize("retained_report", [True, False])
async def test_missing_player_reports_are_repaired_from_notes_or_remain_unverified(retained_report: bool) -> None:
    state = await run_state()
    state.task = "Report player bugs and author replies for Project Quartz."
    state.ready_plan = Plan(
        requirements=(Requirement(id="project", text=state.task, kind=RequirementKind.INFORMATION),),
        answer_checks=("Player bug reports for Project Quartz",),
        answer_expected=True,
    )
    page = capture(
        (BlockKind.PARAGRAPH, "Project Quartz: Player reports a controller crash."),
        (BlockKind.PARAGRAPH, "Project Quartz: Author says the fix will ship next release."),
    )
    report = Fact(text=page.text[: page.blocks[0].end], evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    reply = Fact(
        text="Project Quartz: Author says the fix will ship next release.",
        requirement_id="project",
        evidence=block_evidence(page, "s1"),
        reader=FactReader.LLM,
    )
    state.notes = Notes((report, reply) if retained_report else (reply,))
    initial: JsonValue = {"claims": [{"text": reply.text, "evidence_ids": [fact_id(reply)]}]}
    repaired: JsonValue = {"claims": [{"text": page.text, "evidence_ids": [fact_id(report), fact_id(reply)]}]}
    llm = ScriptedLLM([initial, repaired if retained_report else initial])

    class ReportJev(RoutingJev):
        """Says the player-reports output is unstated until the answer names the crash."""

        def __init__(self) -> None:
            self.asked: list[str] = []

        async def evaluate(self, state, questions):
            self.asked.extend(questions)
            result = await super().evaluate(state, questions)
            unstated = float("controller crash" not in state["answer"])
            return result.model_copy(
                update={
                    "answers": {
                        key: NoulAnswer(probability=unstated) if key == f"{OUTPUT_MISSING}0" else answer
                        for key, answer in result.answers.items()
                    }
                }
            )

    jev = ReportJev()
    answer, verified = await Agent(Mock(spec=Page), jev, llm)._answer(state, None)
    assert verified is retained_report
    assert f"{OUTPUT_MISSING}0" in jev.asked and "requirement_omitted" not in jev.asked
    composing = [messages[-1].content for purpose, messages in llm.calls if purpose is LLMPurpose.COMPOSE]
    assert len(composing) == 2 and "# Answer corrections" in composing[1]
    assert "Player bug reports for Project Quartz" in composing[1]
    if retained_report:
        assert "controller crash" in answer.answer
        assert {citation.quote for citation in answer.citations} == {report.text, reply.text}
    else:
        assert "controller crash" not in answer.answer
        assert state.missing_answer_outputs == ("Player bug reports for Project Quartz",)


def price_notes():
    """A price read twice: the first reading no longer evidences anything, the second is current."""
    page = capture((BlockKind.PARAGRAPH, "Price 12"), (BlockKind.PARAGRAPH, "Member price 12"))
    stale = Fact(text="Price 12", evidence=block_evidence(page, "s0"), reader=FactReader.LLM, requirement_id="old")
    fresh = Fact(text="Member price 12", evidence=block_evidence(page, "s1"), reader=FactReader.LLM, requirement_id="r")
    notes = Notes((stale, fresh))
    notes.unevidence(("old",))
    return notes, stale, fresh


async def test_failed_reader_fallback_keeps_the_composed_answers_missing_outputs() -> None:
    state = await run_state()
    state.task = "Report player feedback across projects."
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text=state.task, kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    page = capture((BlockKind.PARAGRAPH, "Project Quartz: Player reported a crash. Author plans a fix."))
    fact = Fact(
        text="Project Quartz: Author plans a fix.",
        evidence=block_evidence(page, "s0"),
        requirement_id="r",
        reader=FactReader.LLM,
    )
    state.notes = Notes((fact,))
    llm = ScriptedLLM([{"claims": [{"text": page.text, "evidence_ids": [fact_id(fact)]}]}])
    agent = Agent(Mock(spec=Page), RoutingJev(), llm)

    async def reject(state, answer):
        missing = (
            "Player feedback for Project Garnet" if "crash" in answer.answer else "Player feedback for Project Quartz"
        )
        state.missing_answer_outputs = (missing,)
        state.open_answer_outputs = (missing,)
        return None

    agent._holds = AsyncMock(side_effect=reject)
    answer, verified = await agent._answer(state, None)
    assert not verified and answer.answer == page.text
    assert agent._holds.await_count == 2
    assert state.missing_answer_outputs == state.open_answer_outputs == ("Player feedback for Project Garnet",)


@pytest.mark.parametrize("failure", ["stale", "unclaimed"])
async def test_code_check_failures_produce_untrusted_advisory_corrections(failure: str) -> None:
    notes, stale, fresh = price_notes()
    claim = Claim(text="Price 12", evidence_ids=(fact_id(stale if failure == "stale" else fresh),))
    answer = assemble_answer((claim,), notes, ())
    if failure == "unclaimed":
        answer = FieldAnswer(**answer.model_dump(), output_claims={})
    corrections = []
    missing = []
    assert not check_answer_outputs(
        answer,
        notes,
        ("Report the price.",),
        corrections=corrections,
        missing_outputs=missing,
        independent_fields=False,
    )
    assert missing == ["Report the price."]
    assert len(corrections) == 1
    assert corrections[0].stage == "source"
    assert corrections[0].claims == (claim,)
    assert corrections[0].criterion == "Report the price."
    assert ("no longer current" in corrections[0].reason) is (failure == "stale")


@pytest.mark.parametrize("repair_succeeds", [True, False])
async def test_answer_repairs_once_and_rechecks_instead_of_accepting_a_stale_citation(repair_succeeds: bool) -> None:
    state = await run_state()
    state.task = "Report the price."
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text=state.task, kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=(state.task,),
    )
    state.notes, stale, fresh = price_notes()
    first: JsonValue = {"claims": [{"text": "Price 12", "evidence_ids": [fact_id(stale)]}]}
    repaired: JsonValue = {"claims": [{"text": "Member price 12", "evidence_ids": [fact_id(fresh)]}]}
    llm = ScriptedLLM([first, repaired if repair_succeeds else first])
    agent = Agent(Mock(spec=Page), RoutingJev(), llm)
    answer, verified = await agent._answer(state, None)
    composing = [messages[-1].content for purpose, messages in llm.calls if purpose is LLMPurpose.COMPOSE]
    assert len(composing) == 2
    assert "# Answer corrections" in composing[1] and "no longer current" in composing[1]
    # A repair that cites the stale reading again is not accepted; the reader's current fact is offered instead.
    assert verified
    assert answer.answer == "Member price 12"
    assert [citation.quote for citation in answer.citations] == ["Member price 12"]
    assert state.ledger.llm_calls == len(llm.calls)


async def test_large_advisory_feedback_cannot_displace_required_quotes() -> None:
    from fastbrowse.retrieval import AnswerCorrection

    page = capture((BlockKind.PARAGRAPH, "Source value " * 100))
    fact = Fact(text="Requested value", evidence=block_evidence(page, "s0"), reader=FactReader.LLM, requirement_id="r")
    notes = Notes((fact,))
    plan = Plan(
        requirements=(Requirement(id="r", text="Report the requested value", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    rejected = Claim(text="Incorrect assertion " * 1000, evidence_ids=(fact_id(fact),))
    correction = AnswerCorrection(
        criterion="Report the requested value", claims=(rejected,), reason="Incorrect " * 1000
    )
    llm = ScriptedLLM([{"claims": [{"text": "Requested value", "evidence_ids": ["e0"]}]}])
    await compose(
        llm,
        "Report the requested value",
        plan,
        notes,
        # Room for the composer's prompt and the quote, never for the corrections as well.
        tokens=TokenBudget(state_plus_largest_question=1600),
        corrections=(correction,),
    )
    prompt = llm.calls[0][1][-1].content
    assert page.text in prompt and "# Answer corrections" not in prompt


async def test_small_rejected_finish_reopens_information_and_checks_a_reread() -> None:
    from fastbrowse.models import Status
    from tests.test_partitioned_finish import finish_agent

    agent, state, jev, _ = await finish_agent(size=20)
    original = state.notes.facts
    conclude = agent._conclude

    async def reject_once(state, *args, **kwargs):
        state.missing_answer_outputs = state.open_answer_outputs = (state.plan.answer_checks[0],)
        return agent._result(state, state.ledger, Status.UNVERIFIED)

    agent._conclude = AsyncMock(side_effect=reject_once)
    assert await agent._finish(state, None, None) is None
    assert not any(state.notes.evidenced(r.id) for r in state.plan.requirements)
    assert not state.finish_partitions
    first_checks = len(jev.states)
    for fact in original:
        state.notes.add(fact)
    agent._conclude = conclude
    result = await agent._finish(state, None, None)
    assert result is not None and result.status is Status.COMPLETE
    assert len(jev.states) > first_checks
    assert not state.finish_partitions


@pytest.mark.parametrize("complete", [False, True])
async def test_field_repair_receives_rejection_with_current_collection_coverage(complete: bool) -> None:
    from fastbrowse.retrieval import AnswerCorrection
    from tests.test_finish_fields import comments

    plan, notes = comments(complete)
    rejected = Claim(text="Oak is an author reply.", evidence_ids=(fact_id(notes.facts[2]),))
    correction = AnswerCorrection(
        stage="source",
        criterion=plan.answer_checks[0],
        claims=(rejected,),
        reason="Oak is a player thanking the author, not an author reply.",
    )
    llm = ScriptedLLM(
        [
            {
                "output_0": [
                    {
                        "text": "No author replies occur in Birch's two comments; both comments are from players.",
                        "evidence_ids": ["e3"],
                    }
                ]
            }
        ]
    )
    await compose(
        llm,
        plan.requirements[0].text,
        plan,
        notes,
        corrections=(correction,),
        field_outputs=True,
        preserve_collections=True,
    )
    prompt = llm.calls[0][1][-1].content
    repair = prompt.split("# Answer corrections\n", 1)[1].split("# Notes\n", 1)[0]
    assert correction.reason in repair
    assert ('"complete_collections": [' in repair) is complete
    if complete:
        assert '"evidence_id": "e3"' in repair
        assert "exclude" in repair and "bounded absence" in repair
    assert "Player Elm: Please add a compact view." in prompt
    assert "Player Oak: Thanks for the update." in prompt


async def test_small_reopened_read_can_rebind_the_same_quote_without_novelty() -> None:
    from tests.test_agent import _reading_state
    from tests.test_policy import ScriptedJev, observation

    state = await _reading_state()
    page = capture((BlockKind.PARAGRAPH, "Total: 12"))
    response: JsonValue = {
        "claims": [{"text": "Total: 12", "requirement_id": "r1", "cite": {"first": "s0", "last": "s0"}}],
        "answered": True,
    }
    llm = ScriptedLLM([response, response])
    jev = ScriptedJev({"r1": "synthesis"}, noul=0)
    agent = Agent(Mock(spec=Page), jev, llm)
    await agent._read(state, page, observation(()))
    state.notes.unevidence(("r1",))
    state.open_answer_outputs = ("Report the total.",)
    await agent._read(state, page, observation(()))
    assert state.notes.evidenced("r1")
    assert "novelty" not in jev.requests[-1]
    assert len(llm.calls) == 2


async def test_complete_collection_repair_cannot_silently_drop_its_rejection() -> None:
    from fastbrowse.memory import NotesTooLarge
    from fastbrowse.retrieval import AnswerCorrection
    from tests.test_finish_fields import comments

    plan, notes = comments(True)
    correction = AnswerCorrection(
        criterion=plan.answer_checks[0], claims=(), reason="The member is not an author. " * 400
    )
    llm = ScriptedLLM([])
    with pytest.raises(NotesTooLarge):
        await compose(
            llm,
            plan.requirements[0].text,
            plan,
            notes,
            corrections=(correction,),
            field_outputs=True,
            preserve_collections=True,
            tokens=TokenBudget(state_plus_largest_question=1500),
        )
    assert not llm.calls
