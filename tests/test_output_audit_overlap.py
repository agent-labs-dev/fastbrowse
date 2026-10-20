"""The output audit's independent calls run side by side, and a wrong answer still fails whichever audit refuses it."""

import asyncio
import json

import pytest

from fastbrowse.jev import ChoiceQuestion
from fastbrowse.llm import Generation
from fastbrowse.models import CostBasis, CostComponent, CostLine, Limits
from fastbrowse.retrieval import Claim, assemble_answer
from fastbrowse.telemetry import Ledger
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


async def test_under_a_cap_each_audit_call_reserves_after_the_receipts_it_used_to_follow() -> None:
    """Side by side, both audits reserve before either records, so a cap would not count the sibling's spend."""
    notes, answer = _answer("Price 99")
    ledger = Ledger(Limits(max_dollars=1.0))
    llm = AuditLLM(source="no")
    order: list[str] = []

    class OrderedJev(RoutingJev):
        async def evaluate(self, state, questions):
            if any(isinstance(question, ChoiceQuestion) for question in questions.values()):
                # The identity call's receipt is in the ledger, after the Jev tier's, before the selection is sent.
                order.append(f"selection after {ledger.llm_calls} llm receipt(s): {len(ledger.lines)} line(s)")
            return await super().evaluate(state, questions)

    assert not await check_answer_outputs(OrderedJev(), llm, answer, notes, ("Report the price.",), ledger=ledger)
    assert order == ["selection after 1 llm receipt(s): 2 line(s)"]
    # The source refusal decides before the assertion audit is ever sent.
    assert llm.stages == ["source"]


async def test_an_identity_call_that_finished_is_billed_when_the_check_is_cancelled() -> None:
    """The selection beside it was still running when the run's time ran out."""
    notes, answer = _answer()
    ledger = Ledger(Limits())
    identified, selecting = asyncio.Event(), asyncio.Event()

    class IdentityLLM(AuditLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            generated = await super().generate(purpose, messages, schema, **kwargs)
            if schema.__name__ == "_OutputIdentities":
                generated = generated.model_copy(update={"cost": generated.cost.model_copy(update={"dollars": 0.7})})
                identified.set()
            return generated

    class StalledJev(RoutingJev):
        async def evaluate(self, state, questions):
            if not any(isinstance(question, ChoiceQuestion) for question in questions.values()):
                return await super().evaluate(state, questions)
            selecting.set()
            await asyncio.Event().wait()
            raise AssertionError("the selection should have been cancelled")

    checking = asyncio.create_task(
        check_answer_outputs(StalledJev(), IdentityLLM(), answer, notes, ("Report the price.",), ledger=ledger)
    )
    await asyncio.wait_for(asyncio.gather(identified.wait(), selecting.wait()), _TOGETHER_SECONDS)
    await asyncio.sleep(0.01)
    checking.cancel()
    with pytest.raises(asyncio.CancelledError):
        await checking
    assert [line.dollars for line in ledger.lines if line.component is CostComponent.LLM] == [0.7]


async def test_both_audits_progress_together_when_their_fields_exceed_the_cap() -> None:
    """Queued stage by stage, six source fields took all four slots and the assertion audit waited behind them."""
    notes, answer = _answer()
    log: list[tuple[str, str]] = []

    class LoggingLLM(AuditLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ == "_OutputIdentities":
                return await super().generate(purpose, messages, schema, **kwargs)
            stage = _stage(messages)
            log.append(("start", stage))
            await asyncio.sleep(0.01)
            generated = await super().generate(purpose, messages, schema, **kwargs)
            log.append(("end", stage))
            return generated

    checks = tuple(f"Report the price, part {index}." for index in range(6))
    assert await check_answer_outputs(RoutingJev(), LoggingLLM(), answer, notes, checks)
    first_end = log.index(next(entry for entry in log if entry[0] == "end"))
    assert {stage for event, stage in log[:first_end] if event == "start"} == {"source", "assertion"}
    # The cap of eight still holds across both stages.
    in_flight = peak = 0
    for event, _ in log:
        in_flight += 1 if event == "start" else -1
        peak = max(peak, in_flight)
    assert peak == 8


async def test_a_speculative_yes_cannot_pass_an_identity_missing_from_its_sources() -> None:
    """The audits are sent before code checks the identity quotes; their yes stands only once the quotes are found."""
    notes, answer = _answer()

    class UnquotedIdentityLLM(AuditLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ != "_OutputIdentities":
                return await super().generate(purpose, messages, schema, **kwargs)
            payload = scripted_identities(messages)
            payload["identities"][0]["quote"] = "Gamma Corporation"
            cost = CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose)
            return Generation(data=schema.model_validate(payload), cost=cost)

    llm = UnquotedIdentityLLM()
    assert not await check_answer_outputs(RoutingJev(), llm, answer, notes, ("Report the price.",))
    # Both speculative audits ran and agreed, and the answer still failed on its identity.
    assert sorted(llm.stages) == ["assertion", "source"]
