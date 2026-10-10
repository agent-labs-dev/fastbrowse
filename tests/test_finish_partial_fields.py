"""A failed answer field leaves independently checked siblings available for recovery."""

import json
from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent
from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import FactReader, LLMPurpose, Status
from fastbrowse.page import BlockKind, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.retrieval import FieldAnswer, assemble_answer
from tests.test_agent import run_state
from tests.test_answer_repair import RoutingJev
from tests.test_policy import observation
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


class SiblingJudge(ScriptedLLM):
    async def generate(self, purpose, messages, schema, **kwargs):
        if schema.__name__ != "_OutputIdentities":
            key, field = next(iter(json.loads(messages[-1].content)["criteria"].items()))
            self.responses.append(
                {
                    "judgments": {key: "no" if "links" in field["criterion"] else "yes"},
                    "reason": "Only the reply is quoted.",
                }
            )
        return await super().generate(purpose, messages, schema, **kwargs)


@pytest.mark.parametrize("empty", [False, True])
async def test_failed_sibling_retains_only_fully_verified_field(empty):
    from fastbrowse.retrieval import Claim

    state = await run_state()
    state.oversized_answer = True
    requirement = Requirement(id="r", text="Report replies and links for Birch.", kind=RequirementKind.INFORMATION)
    plan = Plan(
        requirements=(requirement,),
        answer_expected=True,
        answer_checks=("Report replies for Birch.", "Report links for Birch."),
    )
    page = capture((BlockKind.RECORD, "Birch author Ash: Fixed in the next release."))
    fact = Fact(requirement_id="r", text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    state.ready_plan, state.notes, state.finish_partitions = plan, Notes((fact,)), (plan,)
    claims = [{"text": fact.text, "evidence_ids": [fact_id(fact)]}]
    if not empty:
        claims.append({"text": "Birch links: source.test/birch", "evidence_ids": [fact_id(fact)]})
    base = assemble_answer(tuple(Claim.model_validate(c) for c in claims), state.notes, plan.requirements)
    answer = FieldAnswer(
        **base.model_dump(), output_claims={plan.answer_checks[0]: (0,), plan.answer_checks[1]: () if empty else (1,)}
    )
    judge = SiblingJudge([])
    agent = Agent(Mock(spec=Page, artifacts=()), RoutingJev(), judge)
    assert await agent._holds(state, answer) is None
    partial = agent._partial_result(state.notes, state, state.ledger, Status.UNVERIFIED, "Missing links")
    assert partial.answer is not None
    assert partial.answer.startswith("Partial answer.")
    assert "Fixed in the next release." in partial.answer
    assert "source.test/birch" not in partial.answer
    audited = [
        json.loads(messages[-1].content)
        for purpose, messages in judge.calls
        if purpose is LLMPurpose.VERIFY and '"reported_claims"' in messages[-1].content
    ]
    assert any("replies for Birch" in json.dumps(item) for item in audited)
    assert state.open_answer_outputs == (plan.answer_checks[1],)


async def test_recovery_reader_requests_only_open_fields(monkeypatch):
    from fastbrowse.retrieval import AnswerCorrection, ReadOutcome

    state = await run_state()
    state.oversized_answer = True
    plans = tuple(
        Plan(
            requirements=(
                Requirement(id=name, text=f"Read replies and links for {name}.", kind=RequirementKind.INFORMATION),
            ),
            answer_expected=True,
            answer_checks=(f"Report replies for {name}.", f"Report links for {name}."),
        )
        for name in ("Birch", "Elm")
    )
    state.ready_plan = Plan(requirements=tuple(p.requirements[0] for p in plans), answer_expected=True)
    state.finish_partitions = plans
    state.open_answer_outputs = (plans[0].answer_checks[1],)
    state.answer_corrections = (
        AnswerCorrection(
            criterion=plans[0].answer_checks[1], claims=(), reason="A menu label does not quote its destination."
        ),
    )
    state.task = "Read replies and links for Birch and Elm."
    page = capture((BlockKind.PARAGRAPH, "Birch source: source.test/birch"))
    read = AsyncMock(return_value=ReadOutcome(facts=(), coverage=(), rejected_claims=0, cost_lines=()))
    monkeypatch.setattr("fastbrowse.agent.read", read)
    agent = Agent(Mock(spec=Page), RoutingJev(), ScriptedLLM([]))
    await agent._read(state, page, observation(()))
    args, kwargs = read.call_args
    assert args[3] == ["Birch"]
    assert [r.text for r in kwargs["requirements"]] == [plans[0].answer_checks[1]]
    assert state.answer_corrections[0].reason in args[2]


@pytest.mark.parametrize("notice", ["", "Next page remains unread"])
async def test_collection_completeness_survives_an_unanswered_neighbor(notice):
    from fastbrowse.memory import CollectionFact
    from fastbrowse.retrieval import read

    page = capture(
        (BlockKind.HEADING, "Birch comments (2)"),
        (BlockKind.RECORD, "Player Elm: Please add a compact view."),
        (BlockKind.RECORD, "Player Oak: Thanks for the update."),
    )
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "collections": [
                    {
                        "requirement_id": "r",
                        "scope": "Birch's two comments",
                        "records": [{"first": f"s{i}", "last": f"s{i}"} for i in range(3)],
                    }
                ],
            }
        ]
    )
    notes = Notes()
    await read(
        llm,
        page,
        "Read comments and links for Birch.",
        ["r"],
        notes,
        preserve_collections=True,
        field_outputs=True,
        notice=notice,
    )
    scoped = notes.for_requirements(("r",))
    collection = max((f for f in scoped.facts if isinstance(f, CollectionFact)), key=lambda f: f.comparison.complete)
    assert collection.comparison.complete is (not notice)
    assert not notes.evidenced("r")
    assert len(collection.comparison.records) == 3
    assert f"complete={str(not notice).lower()}" in scoped.render_with_ids(20000, preserve_collections=True).text
    from fastbrowse.retrieval import Claim
    from fastbrowse.verification import check_answer_outputs
    from tests.test_finish_fields import FieldJudge

    claim = Claim(text="No author replies occur in Birch's two comments.", evidence_ids=(fact_id(collection),))
    base = assemble_answer((claim,), scoped, ())
    answer = FieldAnswer(**base.model_dump(), output_claims={"Report author replies for Birch.": (0,)})
    assert await check_answer_outputs(RoutingJev(), FieldJudge([]), answer, scoped, tuple(answer.output_claims)) is (
        not notice
    )


async def test_composer_keeps_verified_sibling_while_writing_only_missing_slot():
    from fastbrowse.retrieval import Claim, compose

    page = capture(
        (BlockKind.RECORD, "Birch author Ash: Fixed in the next release."),
        (BlockKind.RECORD, "Birch source: https://source.test/birch"),
    )
    notes = Notes(
        Fact(text=page.text[b.start : b.end], evidence=block_evidence(page, b.source_id), reader=FactReader.LLM)
        for b in page.blocks
    )
    plan = Plan(
        requirements=(
            Requirement(id="r", text="Report replies and links for Birch.", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=True,
        answer_checks=("Report replies for Birch.", "Report links for Birch."),
    )
    reply = Claim(text="Ash replies that the fix is in the next release.", evidence_ids=(fact_id(notes.facts[0]),))
    partial = FieldAnswer(
        **assemble_answer((reply,), notes, plan.requirements).model_dump(), output_claims={plan.answer_checks[0]: (0,)}
    )
    writer = ScriptedLLM([{"output_1": [{"text": "Birch source: https://source.test/birch", "evidence_ids": ["e1"]}]}])
    answer = (
        await compose(
            writer,
            "Read Birch replies and links.",
            plan,
            notes,
            field_outputs=True,
            preserve_collections=True,
            verified=partial,
        )
    ).data
    assert isinstance(answer, FieldAnswer)
    assert answer.claims[answer.output_claims[plan.answer_checks[0]][0]] == reply
    assert "https://source.test/birch" in answer.claims[answer.output_claims[plan.answer_checks[1]][0]].text


async def test_partitioned_composer_does_not_treat_reader_paraphrases_as_quotes():
    from fastbrowse.retrieval import compose

    page = capture((BlockKind.RECORD, "Birch comments: The tracker crashes."))
    fact = Fact(
        text="User Willow reports that Birch's tracker crashes.",
        evidence=block_evidence(page, "s0"),
        reader=FactReader.LLM,
        requirement_id="r",
    )
    notes = Notes(
        (
            fact,
            Fact(
                text="Willow is the bug reporter.",
                evidence=None,
                basis=(fact_id(fact),),
                reader=FactReader.LLM,
                requirement_id="r",
            ),
        )
    )
    plan = Plan(
        requirements=(Requirement(id="r", text="Report Birch bugs.", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("Report Birch bugs.",),
    )
    writer = ScriptedLLM(
        [{"output_0": [{"text": "A player reports that the tracker crashes.", "evidence_ids": ["e0"]}]}]
    )
    await compose(writer, "Report Birch bugs.", plan, notes, field_outputs=True, preserve_collections=True)
    offered = writer.calls[0][1][-1].content
    assert "Birch comments: The tracker crashes." in offered
    assert "Willow" not in offered


@pytest.mark.parametrize("already_recovered", [False, True])
async def test_new_verified_field_renews_recovery_once(already_recovered):
    from fastbrowse.agent import _partition_proof, _Stop
    from fastbrowse.retrieval import AnswerCorrection, Claim
    from tests.test_finish_fields import comments

    plan, notes = comments(True)
    plan = plan.model_copy(update={"answer_checks": (*plan.answer_checks, "Report links for Birch.")})
    state = await run_state()
    state.oversized_answer = True
    state.ready_plan, state.notes, state.finish_partitions = plan, notes, (plan,)
    claim = Claim(text="No author replies occur in Birch's two comments.", evidence_ids=(fact_id(notes.facts[-1]),))
    partial = FieldAnswer(
        **assemble_answer((claim,), notes, plan.requirements).model_dump(), output_claims={plan.answer_checks[0]: (0,)}
    )
    state.partial_answers[_partition_proof(plan, notes, state.invented)] = partial
    state.open_answer_outputs = (plan.answer_checks[1],)
    state.answer_corrections = (
        AnswerCorrection(
            criterion=plan.answer_checks[1], claims=(), reason="A menu label does not quote its destination."
        ),
    )
    writer = ScriptedLLM(
        [
            {
                "diagnosis": "Read the links.",
                "next_subgoal": "Go back.",
                "operation": "back",
                "give_up": False,
                "needs_input": False,
            }
        ]
    )
    agent = Agent(Mock(spec=Page, artifacts=()), RoutingJev(), writer)
    agent._screenshots = AsyncMock(return_value=())
    agent._record_step = AsyncMock()
    state.recoveries = agent._config.stall.max_recoveries
    if already_recovered:
        state.recoveries = 0
        await agent._recover(state, observation(()), "Missing links", gives_up_as=Status.UNVERIFIED)
        state.recoveries = agent._config.stall.max_recoveries
        with pytest.raises(_Stop):
            await agent._recover(state, observation(()), "Missing links", gives_up_as=Status.UNVERIFIED)
    else:
        await agent._recover(state, observation(()), "Missing links", gives_up_as=Status.UNVERIFIED)
        assert state.recoveries == 1
    assert state.answer_corrections[0].reason in writer.calls[0][1][-1].content


def test_collection_scope_changes_partition_proof():
    from fastbrowse.agent import _partition_proof
    from fastbrowse.memory import CollectionFact, Comparison

    page = capture((BlockKind.RECORD, "Birch has no comments."))
    fact = Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    collection = CollectionFact(
        text="Read collection",
        scope="Birch comments",
        evidence=None,
        basis=(fact_id(fact),),
        reader=FactReader.LLM,
        comparison=Comparison(requirement_id="r", records=(fact_id(fact),), complete=True),
    )
    plan = Plan(requirements=(), answer_expected=True, answer_checks=("Report comments for Birch.",))
    proof = _partition_proof(plan, Notes((fact, collection)), set())
    changed = collection.model_copy(update={"scope": "Birch reviews"})
    assert _partition_proof(plan, Notes((fact, changed)), set()) != proof
