"""The two accuracy fixes: a stopping run commits an answer, and a browsing step cannot outvote one."""

from unittest.mock import Mock

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent
from fastbrowse.config import Thresholds
from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import FactReader, Status
from fastbrowse.page import BlockKind, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.verification import DoneVerdict, _traversal_only, check_done
from tests.test_agent import run_state
from tests.test_answer_repair import RoutingJev
from tests.test_done_check import _PAGE, _Jev
from tests.test_memory import evidence
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


# A browsing step is how the run reaches the information. Anything that changes state on the site is what the
# caller asked for, so it keeps blocking however good the evidence is.
@pytest.mark.parametrize(
    ("text", "traversal"),
    [
        ("Navigate through every page of the site.", True),
        ("Browse the Sequential Art category, including all of its pages.", True),
        ("Scroll through the listing until every result has been shown.", True),
        ("Sign in with username standard_user and password secret_sauce.", False),
        ("Add both products to the cart and place the order.", False),
        ("Submit the contact form with the prepared message.", False),
        ("Go through every page and submit the last page's form.", False),
        ("Report the country with the 12th-highest GDP.", False),
    ],
)
def test_only_browsing_steps_are_instrumental(text: str, traversal: bool) -> None:
    assert _traversal_only(text) is traversal


def _count_plan() -> Plan:
    return Plan(
        requirements=(
            Requirement(id="r1", text="Navigate through every page of the site.", kind=RequirementKind.ACTION),
            Requirement(id="r2", text="Count the quotes tagged love.", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=True,
    )


def _counted_notes() -> Notes:
    total = "14 quotes are tagged love"
    return Notes(
        [
            Fact(
                reader=FactReader.LLM,
                requirement_id="r2",
                text=total,
                evidence=evidence(sha="love", end=len(total)).model_copy(update={"quote": total}),
            )
        ]
    )


async def test_an_unconfirmed_browsing_step_does_not_hold_an_evidenced_answer() -> None:
    """The tag count was read off the site's own exhaustive listing and the run still looped on "navigate
    through every page" until its recoveries ran out: the step is how the count was reached, not the answer."""
    # Every information requirement is evidenced; only the visit is unconfirmed, and the page is doubted as a whole.
    answers = {"complete": 0.63, "unmet_r1": 0.9, "unmet_r2": 0.1}
    check = await check_done(_Jev(answers), "Count them.", _count_plan(), _PAGE, _counted_notes(), Thresholds())
    assert check.verdict is DoneVerdict.ACCEPT


async def test_the_gate_can_still_be_measured_against_the_old_rule(monkeypatch) -> None:
    """The pre-fix policy stays reachable for a measurement arm: the browsing step blocks as it did before."""
    monkeypatch.setenv("FASTBROWSE_FIX_GATE", "0")
    answers = {"complete": 0.63, "unmet_r1": 0.9, "unmet_r2": 0.1}
    check = await check_done(_Jev(answers), "Count them.", _count_plan(), _PAGE, _counted_notes(), Thresholds())
    assert check.verdict is DoneVerdict.VERIFY


async def test_an_unevidenced_answer_still_keeps_the_run_working() -> None:
    """The traversal exemption is for a run that has read what it was asked for, never for one that has not."""
    check = await check_done(_Jev({"complete": 0.9}), "Count them.", _count_plan(), _PAGE, Notes(), Thresholds())
    assert check.verdict is DoneVerdict.REJECT


async def test_a_stopping_run_commits_an_answer_instead_of_its_notes(monkeypatch) -> None:
    """A table lookup used to arrive as its rows with no sentence naming the country."""
    state = await run_state()
    page = capture((BlockKind.PARAGRAPH, "| Country | IMF (2026) |\n| South Korea | 2,245,847 |"))
    quote = "South Korea 2,245,847"
    evidence_item = block_evidence(page, "s0")
    fact = Fact(requirement_id="r1", text=quote, evidence=evidence_item, reader=FactReader.LLM)
    state.notes, state.ready_plan = (
        Notes((fact,)),
        Plan(
            requirements=(Requirement(id="r1", text="Report the 12th-highest GDP.", kind=RequirementKind.INFORMATION),),
            answer_expected=True,
        ),
    )
    claim: JsonValue = {
        "text": "South Korea has the 12th-highest GDP at 2,245,847 US$ millions.",
        "evidence_ids": [fact_id(fact)],
    }
    agent = Agent(Mock(spec=Page, artifacts=()), RoutingJev(), ScriptedLLM([{"claims": [claim]}]))
    committed = await agent._partial_result(state.notes, state, state.ledger, Status.STUCK, "ran out of recoveries")
    assert committed.status is Status.STUCK
    assert committed.answer is not None and committed.answer.startswith("Answer (unverified)")
    assert "South Korea" in committed.answer and "2,245,847" in committed.answer


async def test_a_committed_answer_keeps_the_citations_it_rests_on() -> None:
    state = await run_state()
    page = capture((BlockKind.PARAGRAPH, "Total: $43.18"))
    evidence_item = block_evidence(page, "s0")
    fact = Fact(requirement_id="r1", text="Total: $43.18", evidence=evidence_item, reader=FactReader.LLM)
    state.notes, state.ready_plan = (
        Notes((fact,)),
        Plan(
            requirements=(Requirement(id="r1", text="Report the total.", kind=RequirementKind.INFORMATION),),
            answer_expected=True,
        ),
    )
    claim: JsonValue = {"text": "The order total is $43.18.", "evidence_ids": [fact_id(fact)]}
    agent = Agent(Mock(spec=Page, artifacts=()), RoutingJev(), ScriptedLLM([{"claims": [claim]}]))
    result = await agent._partial_result(state.notes, state, state.ledger, Status.UNVERIFIED, "unverified")
    assert result.citations and result.citations[0].quote == "Total: $43.18"
    assert "https://" in (result.citations[0].url or "")


async def test_a_failed_commit_falls_back_to_the_notes(monkeypatch) -> None:
    """A stop is the worst moment to trade the evidence already read for an exception."""
    state = await run_state()
    page = capture((BlockKind.PARAGRAPH, "Total: $43.18"))
    evidence_item = block_evidence(page, "s0")
    fact = Fact(requirement_id="r1", text="Total: $43.18", evidence=evidence_item, reader=FactReader.LLM)
    state.notes, state.ready_plan = (
        Notes((fact,)),
        Plan(
            requirements=(Requirement(id="r1", text="Report the total.", kind=RequirementKind.INFORMATION),),
            answer_expected=True,
        ),
    )
    agent = Agent(Mock(spec=Page, artifacts=()), RoutingJev(), ScriptedLLM([{"claims": []}]))
    result = await agent._partial_result(state.notes, state, state.ledger, Status.UNVERIFIED, "unverified")
    assert result.status is Status.UNVERIFIED
    assert result.answer is not None and "43.18" in result.answer


async def test_the_commit_can_still_be_measured_against_the_old_rule(monkeypatch) -> None:
    """With the commit turned off a stopping run hands back its notes as before."""
    monkeypatch.setenv("FASTBROWSE_FIX_COMMIT", "0")
    state = await run_state()
    page = capture((BlockKind.PARAGRAPH, "Total: $43.18"))
    evidence_item = block_evidence(page, "s0")
    fact = Fact(requirement_id="r1", text="Total: $43.18", evidence=evidence_item, reader=FactReader.LLM)
    state.notes, state.ready_plan = (
        Notes((fact,)),
        Plan(
            requirements=(Requirement(id="r1", text="Report the total.", kind=RequirementKind.INFORMATION),),
            answer_expected=True,
        ),
    )
    claim: JsonValue = {"text": "The order total is $43.18.", "evidence_ids": [fact_id(fact)]}
    agent = Agent(Mock(spec=Page, artifacts=()), RoutingJev(), ScriptedLLM([{"claims": [claim]}]))
    result = await agent._partial_result(state.notes, state, state.ledger, Status.UNVERIFIED, "unverified")
    assert result.answer is not None and result.answer.startswith("Partial evidence only")
