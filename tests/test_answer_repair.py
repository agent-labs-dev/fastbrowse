"""An answer repair receives failed assertions, retains quotes, and goes through the same full audits."""

import json
from unittest.mock import Mock

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent
from fastbrowse.config import TokenBudget
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer
from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import CostBasis, CostComponent, CostLine, FactReader, LLMPurpose
from fastbrowse.page import BlockKind, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.retrieval import Claim, assemble_answer, compose
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


class RepairWriter(ScriptedLLM):
    def __init__(self, *, repair_succeeds: bool = True, source_missing: bool = False):
        super().__init__([])
        self.repair_succeeds = repair_succeeds
        self.source_missing = source_missing
        self.composes = 0

    async def generate(self, purpose, messages, schema, **kwargs):
        response: JsonValue
        if purpose is LLMPurpose.COMPOSE:
            self.composes += 1
            if self.composes == 2:
                assert "# Answer corrections" in messages[-1].content
                assert "conditions omitted or extra detail unsupported" in messages[-1].content
                assert "Member price 12" in messages[-1].content
            text = "Member price 12" if self.composes == 2 and self.repair_succeeds else "Price 12; free shipping"
            response = {"claims": [{"text": text, "evidence_ids": ["e0"]}]}
        else:
            key, field = next(iter(json.loads(messages[-1].content)["criteria"].items()))
            claims = field.get("reported_claims")
            valid = not self.source_missing and (claims is None or all(c["text"] == "Member price 12" for c in claims))
            response = {
                "judgments": {key: "yes" if valid else "no"},
                "reason": "conditions omitted or extra detail unsupported",
            }
        self.responses.append(response)
        return await super().generate(purpose, messages, schema, **kwargs)


def price_notes():
    page = capture((BlockKind.PARAGRAPH, "Member price 12"))
    fact = Fact(text="Price 12", evidence=block_evidence(page, "s0"), reader=FactReader.LLM, requirement_id="r")
    return Notes((fact,)), fact


@pytest.mark.parametrize("source_missing", [False, True])
async def test_only_assertion_failures_produce_advisory_corrections(source_missing: bool) -> None:
    notes, fact = price_notes()
    claim = Claim(text="Price 12; free shipping", evidence_ids=(fact_id(fact),))
    answer = assemble_answer((claim,), notes, ())
    corrections = []
    assert not await check_answer_outputs(
        RoutingJev(),
        RepairWriter(source_missing=source_missing),
        answer,
        notes,
        ("Report the price.",),
        corrections=corrections,
    )
    assert bool(corrections) is not source_missing
    if corrections:
        assert corrections[0].claims == (claim,)
        assert corrections[0].criterion == "Report the price."
        assert corrections[0].reason == "conditions omitted or extra detail unsupported"


@pytest.mark.parametrize("repair_succeeds,source_missing", [(True, False), (False, False), (True, True)])
async def test_answer_repairs_once_and_reaudits_instead_of_accepting_failed_prose(
    repair_succeeds: bool, source_missing: bool
) -> None:
    state = await run_state()
    state.task = "Report the price."
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text=state.task, kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=(state.task,),
    )
    state.notes, _ = price_notes()
    llm = RepairWriter(repair_succeeds=repair_succeeds, source_missing=source_missing)
    agent = Agent(Mock(spec=Page), RoutingJev(), llm)
    answer, verified = await agent._answer(state, None)
    assert verified is (repair_succeeds and not source_missing)
    assert llm.composes == (1 if source_missing else 2)
    if verified:
        assert answer.answer == "Member price 12"
        assert answer.citations[0].quote == "Member price 12"
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
        tokens=TokenBudget(state_plus_largest_question=1500),
        corrections=(correction,),
    )
    prompt = llm.calls[0][1][-1].content
    assert page.text in prompt and "# Answer corrections" not in prompt


async def test_comparison_repair_must_supply_all_operands_in_its_own_citations() -> None:
    state = await run_state()
    state.task = "Recommend the higher-capacity option."
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text=state.task, kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=(state.task,),
    )
    page = capture((BlockKind.PARAGRAPH, "Alpha capacity 10"), (BlockKind.PARAGRAPH, "Beta capacity 7"))
    state.notes = Notes(
        Fact(
            text=block.source_id,
            evidence=block_evidence(page, block.source_id),
            reader=FactReader.LLM,
            requirement_id="r",
        )
        for block in page.blocks
    )
    llm = ScriptedLLM(
        [
            {"claims": [{"text": "Alpha has the highest capacity, 10.", "evidence_ids": ["e0"]}]},
            {"judgments": {"output_0": "yes"}, "reason": "One property supports a preference."},
            {
                "judgments": {"output_0": "no"},
                "reason": "Highest requires the rival's capacity in this claim's citations.",
            },
            {
                "claims": [
                    {"text": "Alpha has the highest capacity, 10 compared with Beta's 7.", "evidence_ids": ["e0", "e1"]}
                ]
            },
            {"judgments": {"output_0": "yes"}, "reason": "Both values are quoted."},
            {"judgments": {"output_0": "yes"}, "reason": "Both operands have own-claim citations."},
        ]
    )
    answer, verified = await Agent(Mock(spec=Page), RoutingJev(), llm)._answer(state, None)
    assert verified and set(answer.claims[0].evidence_ids) == set(state.notes.evidence)
    correction_prompt = llm.calls[3][1][-1].content
    assert "Highest requires the rival" in correction_prompt
    assert "Alpha capacity 10" in correction_prompt and "Beta capacity 7" in correction_prompt
