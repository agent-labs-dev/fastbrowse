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
async def test_source_and_assertion_failures_produce_untrusted_advisory_corrections(source_missing: bool) -> None:
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
    assert corrections
    assert corrections[0].stage == ("source" if source_missing else "assertion")
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
    assert llm.composes == 2
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


@pytest.mark.parametrize("mode", ["improve", "worse", "omit", "retained-source"])
async def test_answer_repairs_continue_only_while_failed_assertions_decrease(mode: str) -> None:
    state = await run_state()
    state.task = "Report the price and subscription term."
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text=state.task, kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("Report the price.", "Report the subscription term."),
    )
    page = capture((BlockKind.PARAGRAPH, "Member price 12"), (BlockKind.PARAGRAPH, "Trial term 1 year"))
    state.notes = Notes(
        Fact(text=text, evidence=block_evidence(page, key), reader=FactReader.LLM, requirement_id="r")
        for key, text in [("s0", "Price 12"), ("s1", "Term 1 year")]
    )

    class AllClaimsJev(RoutingJev):
        async def evaluate(self, state, questions):
            result = await super().evaluate(state, questions)
            return result.model_copy(
                update={
                    "answers": {
                        key: answer.model_copy(update={"choice": "all"}) if isinstance(answer, ChoiceAnswer) else answer
                        for key, answer in result.answers.items()
                    }
                }
            )

    class ImprovingWriter(ScriptedLLM):
        composes = 0

        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ == "_OutputIdentities":
                return await super().generate(purpose, messages, schema, **kwargs)
            response: JsonValue
            if purpose is LLMPurpose.COMPOSE:
                self.composes += 1
                response = {
                    "claims": [
                        {
                            "text": "Member price 12" if self.composes >= 2 and mode != "worse" else "Price 12",
                            "evidence_ids": ["e0"],
                        },
                        {
                            "text": "Trial term 1 year"
                            if self.composes >= 3 or (mode == "worse" and self.composes == 1)
                            else "Term 1 year",
                            "evidence_ids": ["e1"],
                        },
                    ]
                }
                if mode == "omit" and self.composes >= 2:
                    response["claims"] = response["claims"][:1]
                if mode == "retained-source" and self.composes == 1:
                    response["claims"] = response["claims"][:1]
            else:
                key, field = next(iter(json.loads(messages[-1].content)["criteria"].items()))
                claims = field.get("reported_claims")
                valid = claims is None or all(c["text"] in {"Member price 12", "Trial term 1 year"} for c in claims)
                if claims is None and mode == "retained-source" and "term" in field["criterion"]:
                    valid = any(
                        "Trial term 1 year" in source["quote"]
                        for record in field["sources"]
                        for source in record["cited_sources"]
                    )
                if mode == "omit" and self.composes >= 2 and "term" in field["criterion"]:
                    valid = False
                response = {"judgments": {key: "yes" if valid else "no"}, "reason": "Preserve the source condition."}
            self.responses.append(response)
            return await super().generate(purpose, messages, schema, **kwargs)

    llm = ImprovingWriter([])
    answer, verified = await Agent(Mock(spec=Page), AllClaimsJev(), llm)._answer(state, None)
    assert verified is (mode in {"improve", "retained-source"})
    assert llm.composes == (3 if mode in {"improve", "retained-source"} else 2)
    if verified:
        assert {claim.text for claim in answer.claims} == {"Member price 12", "Trial term 1 year"}
    assert state.ledger.llm_calls == len(llm.calls)
