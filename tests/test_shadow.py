"""Shadow counts belong to each run even when runs overlap and INFO logging is disabled."""

import asyncio
from collections import Counter
from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent
from fastbrowse.config import Config, StallRules
from fastbrowse.models import Limits, Status, StepOutcome, Tripwire, TripwireMode
from fastbrowse.page import ActResult, Observation, Page
from tests.test_agent import field
from tests.test_policy import ScriptedJev, observation
from tests.test_retrieval import ScriptedLLM


@pytest.mark.parametrize("mode", list(TripwireMode))
async def test_concurrent_runs_report_only_their_own_shadow_tripwires(
    mode: TripwireMode, caplog: pytest.LogCaptureFixture
) -> None:
    async def observe() -> Observation:
        await asyncio.sleep(0)
        return observation((field(),))

    runs = []
    for repeated, stagnant in ((2, 10), (10, 2)):
        page = Mock(spec=Page)
        page.observe = AsyncMock(side_effect=observe)
        page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=False))
        page.artifacts = ()
        llm = ScriptedLLM(
            [{"requirements": [{"id": "r", "text": "Submit the form", "kind": "action"}], "answer_expected": False}]
        )
        agent = Agent(
            page,
            ScriptedJev({"operation": "fill", "fill_target": "field", "pick": "input:value"}, noul=0.0),
            llm,
            config=Config(
                stall=StallRules(
                    unchanged_actions=10,
                    repeated_actions=repeated,
                    stagnant_plan_steps=stagnant,
                    max_recoveries=0,
                    tripwires=mode,
                )
            ),
        )
        runs.append(agent.run("Fill the form", inputs={"value": "Bath"}, limits=Limits(max_steps=4)))
    with caplog.at_level("WARNING", logger="fastbrowse"):
        first, second = await asyncio.gather(*runs)
    if mode is TripwireMode.SHADOW:
        assert first.status is second.status is Status.BUDGET_EXCEEDED
        assert Counter(first.would_fire) == {Tripwire.ACTION_REPETITION: 2}
        assert Counter(second.would_fire) == {Tripwire.PLAN_STAGNATION: 3}
    else:
        assert first.status is second.status is Status.STUCK
        assert not first.would_fire and not second.would_fire
