"""The output audit's independent calls run side by side, and a wrong answer still fails whichever audit refuses it."""

import asyncio
import json

import pytest

from fastbrowse.jev import ChoiceQuestion
from fastbrowse.llm import Generation
from fastbrowse.models import CostBasis, CostComponent, CostLine
from fastbrowse.retrieval import Claim, assemble_answer
from fastbrowse.verification import check_answer_outputs
from tests.test_answer_repair import RoutingJev, price_notes
from tests.test_retrieval import scripted_identities

# Long enough for any scheduler, short enough that a serial audit fails the test rather than hanging it.
_TOGETHER_SECONDS = 2.0


def _stage(messages) -> str:
    fields = json.loads(messages[-1].content)["criteria"]
    return "source" if all("sources" in field for field in fields.values()) else "assertion"


class AuditLLM:
    """Answers each audit with its scripted verdict, and can hold every audit call until both audits have begun."""

    def __init__(self, source: str = "yes", assertion: str = "yes", *, together: bool = False) -> None:
        self.verdicts = {"source": source, "assertion": assertion}
        self.started: dict[str, asyncio.Event] = {"source": asyncio.Event(), "assertion": asyncio.Event()}
        self.together = together
        self.selection_started: asyncio.Event | None = None
        self.stages: list[str] = []

    async def generate(self, purpose, messages, schema, **kwargs):
        if kwargs.get("ledger") is not None:
            kwargs["ledger"].reserve(CostComponent.LLM)
        cost = CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose)
        if schema.__name__ == "_OutputIdentities":
            if self.selection_started is not None:
                await asyncio.wait_for(self.selection_started.wait(), _TOGETHER_SECONDS)
            return Generation(data=schema.model_validate(scripted_identities(messages)), cost=cost)
        stage = _stage(messages)
        self.stages.append(stage)
        self.started[stage].set()
        if self.together:
            other = self.started["assertion" if stage == "source" else "source"]
            await asyncio.wait_for(other.wait(), _TOGETHER_SECONDS)
        fields = json.loads(messages[-1].content)["criteria"]
        verdict = {"judgments": {key: self.verdicts[stage] for key in fields}, "reason": f"{stage} reason"}
        return Generation(data=schema.model_validate(verdict), cost=cost)


def _answer(text: str = "Member price 12"):
    notes, fact = price_notes()
    from fastbrowse.memory import fact_id

    return notes, assemble_answer((Claim(text=text, evidence_ids=(fact_id(fact),)),), notes, ())


async def test_source_and_assertion_audits_are_in_flight_together() -> None:
    notes, answer = _answer()
    llm = AuditLLM(together=True)
    assert await check_answer_outputs(RoutingJev(), llm, answer, notes, ("Report the price.",))
    assert sorted(llm.stages) == ["assertion", "source"]


@pytest.mark.parametrize(
    ("source", "assertion", "stage", "reason"),
    [("no", "yes", "source", "source reason"), ("yes", "no", "assertion", "assertion reason")],
)
async def test_a_refusal_from_either_audit_still_fails_the_answer(
    source: str, assertion: str, stage: str, reason: str
) -> None:
    """Run together, the assertion audit's verdict must not stand in for a source refusal, nor its reason for the
    source audit's: a source failure is repaired from the notes, an assertion failure by rewriting the claim."""
    notes, answer = _answer("Price 99")
    missing: list[str] = []
    corrections = []
    llm = AuditLLM(source, assertion, together=True)
    assert not await check_answer_outputs(
        RoutingJev(), llm, answer, notes, ("Report the price.",), missing_outputs=missing, corrections=corrections
    )
    assert missing == ["Report the price."]
    assert [(c.stage, c.reason) for c in corrections] == [(stage, reason)]


async def test_claim_selection_runs_beside_the_identity_call() -> None:
    notes, answer = _answer()
    llm = AuditLLM()
    llm.selection_started = asyncio.Event()

    class SelectingJev(RoutingJev):
        async def evaluate(self, state, questions):
            if any(isinstance(question, ChoiceQuestion) for question in questions.values()):
                assert llm.selection_started is not None
                llm.selection_started.set()
            return await super().evaluate(state, questions)

    assert await check_answer_outputs(SelectingJev(), llm, answer, notes, ("Report the price.",))
