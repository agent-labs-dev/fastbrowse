from collections.abc import Sequence

import pytest
from pydantic import BaseModel, ValidationError

from fastbrowse.llm import Generation, Message
from fastbrowse.models import (
    CostBasis,
    CostComponent,
    CostLine,
    Limits,
    LLMPurpose,
)
from fastbrowse.planner import Plan, Requirement, RequirementKind, make_plan
from fastbrowse.telemetry import BudgetExceeded, Ledger


def example_plan() -> Plan:
    return Plan(
        requirements=(Requirement(id="r1", text="Find the price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("Find the price",),
    )


class PlannerLLM:
    def __init__(self, plan: Plan | None = None, repaired_checks: tuple[str, ...] = ("Find the price",)) -> None:
        self.calls: list[tuple[LLMPurpose, tuple[Message, ...]]] = []
        self.plan = plan if plan is not None else example_plan()
        self.repaired_checks = repaired_checks
        self.cost = CostLine(
            component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.01, purpose=LLMPurpose.PLAN
        )

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = 2000,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        # Reserve exactly as the real client does, so a test can see a budget stop a request.
        if ledger is not None:
            ledger.reserve(CostComponent.LLM)
        self.calls.append((purpose, tuple(messages)))
        data = (
            schema.model_validate_json(self.plan.model_dump_json())
            if schema is Plan
            else schema.model_validate({"checks": self.repaired_checks})
        )
        return Generation(data=data, cost=self.cost)


async def test_planning_reads_only_the_task_and_start_address_and_preserves_cost() -> None:
    llm = PlannerLLM()
    result = await make_plan(llm, "Find the price", start="https://shop.test/")
    purpose, messages = llm.calls[0]
    prompt = "\n".join(message.content for message in messages)
    assert purpose is LLMPurpose.PLAN
    assert result.data == example_plan() and result.cost.dollars == 0.02
    assert "# Task\nFind the price" in prompt and "# Start page\nhttps://shop.test/" in prompt
    assert "individually checkable" in prompt


def test_plan_rejects_duplicate_ids() -> None:
    with pytest.raises(ValidationError, match="unique"):
        Plan(requirements=example_plan().requirements * 2, answer_expected=True)


async def test_planning_keeps_a_spelled_out_search_out_of_the_requirements() -> None:
    # "Search for X, open that article, and tell me the year" became an action requirement to search in 22 plans
    # of 30. A shortcut that opened the article directly then failed the verifier on it, and recovery typed into
    # the site's search box for up to 70s more. A click the task names is work of its own and stays one.
    llm = PlannerLLM()
    await make_plan(llm, "Search for X, open that article, and tell me the year.")
    prompt = "\n".join(message.content for message in llm.calls[0][1])
    assert "'search for X, open its page and tell me Y' has one requirement, to find Y" in prompt
    assert "a click, entry or submission the task names is still an action requirement" in prompt


@pytest.mark.parametrize("answer_expected", [False, True])
async def test_an_empty_plan_retains_the_requested_outcome(answer_expected: bool) -> None:
    task = "Check whether the document can be opened without signing in."
    llm = PlannerLLM(Plan(requirements=(), answer_expected=answer_expected))
    result = await make_plan(llm, task)
    assert len(result.data.requirements) == 1
    requirement = result.data.requirements[0]
    assert requirement.text == task
    assert requirement.kind is (RequirementKind.INFORMATION if answer_expected else RequirementKind.ACTION)
    assert result.cost.dollars == (0.02 if answer_expected else 0.01)


@pytest.mark.parametrize("checks", [(), (" ",)])
async def test_omitted_answer_checks_are_decomposed_without_replanning_discovery(
    checks: tuple[str, ...],
) -> None:
    plan = Plan(
        requirements=(
            Requirement(id="r", text="Report admission price and opening hours.", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=True,
        answer_checks=checks,
    )
    checks = ("Report admission price.", "Report opening hours.")
    llm = PlannerLLM(plan, repaired_checks=checks)
    result = await make_plan(llm, plan.requirements[0].text)
    assert result.data.answer_checks == checks
    assert result.data.requirements == plan.requirements
    assert len(llm.calls) == 2
    assert result.cost.dollars == 0.02


async def test_action_only_plans_do_not_acquire_answer_checks() -> None:
    plan = Plan(
        requirements=(Requirement(id="r", text="Open the museum page.", kind=RequirementKind.ACTION),),
        answer_expected=True,
        answer_checks=("Report the museum's opening hours.",),
    )
    result = await make_plan(PlannerLLM(plan), plan.requirements[0].text)
    assert result.data.answer_checks == ()


async def test_output_check_repair_keeps_both_receipts_and_respects_call_limits() -> None:
    plan = Plan(requirements=example_plan().requirements, answer_expected=True)
    llm = PlannerLLM(plan, repaired_checks=("Find the price",))
    ledger = Ledger(Limits(max_llm_calls=2))
    result = await make_plan(llm, "Find the price", ledger=ledger)
    ledger.record(result.cost)
    assert ledger.llm_calls == 2 and ledger.lines == [llm.cost, llm.cost]
    capped = Ledger(Limits(max_llm_calls=1))
    with pytest.raises(BudgetExceeded):
        await make_plan(llm, "Find the price", ledger=capped)
    assert capped.llm_calls == 1 and capped.lines == [llm.cost]


async def test_unpriced_plan_repair_retains_unknown_aggregate_dollars() -> None:
    llm = PlannerLLM(Plan(requirements=example_plan().requirements, answer_expected=True))
    llm.cost = llm.cost.model_copy(update={"basis": CostBasis.UNKNOWN, "dollars": None})
    result = await make_plan(llm, "Find the price")
    assert result.cost.basis is CostBasis.UNKNOWN and result.cost.dollars is None
    assert len(llm.calls) == 2


async def test_nonempty_output_checks_retain_every_requested_entity() -> None:
    task = "Compare Adapter Atlas and Adapter Beacon. Report the exact port count of each adapter."
    draft = Plan(
        requirements=(Requirement(id="r", text=task, kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("exact port count",),
    )
    scoped = ("Report Adapter Atlas exact port count.", "Report Adapter Beacon exact port count.")
    llm = PlannerLLM(draft, repaired_checks=scoped)
    result = await make_plan(llm, task)
    assert result.data.answer_checks == scoped
    assert result.data.requirements == draft.requirements
    assert len(llm.calls) == 2
    assert result.cost.dollars == 0.02


async def test_mixed_page_answer_excludes_code_reported_outputs() -> None:
    import json

    from fastbrowse.planner import RunReport

    plan = example_plan().model_copy(update={"run_reports": (RunReport.FINAL_URL,)})
    llm = PlannerLLM(plan)
    result = await make_plan(llm, "Report the price and final URL.")
    prompt = llm.calls[1][1]
    assert json.loads(prompt[-1].content)["run_reports"] == ["final_url"]
    assert "Exclude the supplied run_reports" in prompt[0].content
    assert result.data.answer_checks == ("Find the price",)
    assert result.data.run_reports == (RunReport.FINAL_URL,)


async def test_code_report_only_plan_needs_no_output_normalization() -> None:
    from fastbrowse.planner import RunReport

    plan = Plan(requirements=(), answer_expected=True, run_reports=(RunReport.FINAL_URL,))
    llm = PlannerLLM(plan)
    result = await make_plan(llm, "Report the final URL.")
    assert result.data.answer_checks == ()
    assert result.data.run_reports == plan.run_reports
    assert len(llm.calls) == 1
