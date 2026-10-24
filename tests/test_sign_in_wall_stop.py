"""A run that ends at a sign-in wall reports needs_login, not stuck."""

from unittest.mock import AsyncMock, Mock

import pytest
from pydantic import JsonValue

from fastbrowse import agent as agent_module
from fastbrowse.agent import Agent, _code_decision, _Stop
from fastbrowse.config import Config, StallRules
from fastbrowse.models import Operation, Status
from fastbrowse.page import Control, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.safety import ScopedSecrets
from tests.test_agent import _at, _button, _link, run_state
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM

_GIVE_UP: dict[str, JsonValue] = {
    "diagnosis": "The page offers nothing further.",
    "next_subgoal": "Stop.",
    "give_up": True,
}


@pytest.mark.parametrize("ending", ["exhausted", "give_up"])
@pytest.mark.parametrize("moved", [False, True])
async def test_access_inspection_that_stops_at_its_observed_wall_needs_login(
    monkeypatch: pytest.MonkeyPatch, ending: str, moved: bool
) -> None:
    """An inspection task reads a sign-in wall instead of stopping on it. When the run then cannot go on from that
    wall, the wall is what blocked it: an owner-controls audit escalated on an SSO page until recovery ran out and
    reported stuck, which told the caller to narrow the task rather than sign in."""
    wall = _at("https://shop.test/sso/interaction", _button("Sign in"))
    elsewhere = _at("https://shop.test/projects", _button("Projects"))
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    agent = Agent(
        page,
        ScriptedJev({}),
        ScriptedLLM([_GIVE_UP]),
        config=Config(stall=StallRules(max_recoveries=0 if ending == "exhausted" else 1)),
    )
    agent._observe = AsyncMock(side_effect=[wall, elsewhere if moved else wall])
    agent._outwait = AsyncMock(return_value=False)
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text="List the owner controls", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        inspect_access=True,
    )
    walled = _code_decision(Operation.READ, None).model_copy(update={"login_required": 0.99})
    unsure = _code_decision(Operation.ESCALATE, None).model_copy(update={"operation_confidence": 0.81})
    monkeypatch.setattr(agent_module, "decide", AsyncMock(side_effect=[walled, unsure]))
    agent._read_before_interaction = AsyncMock(side_effect=[True, False])
    with pytest.raises(_Stop) as stopped:
        await agent._loop(state, None, None)
    assert stopped.value.status is (Status.STUCK if moved else Status.NEEDS_LOGIN)


@pytest.mark.parametrize("stored", [False, True])
async def test_recovery_giving_up_at_a_sign_in_ends_the_run_needing_login(stored: bool) -> None:
    """Recovery said credentials were required and the run still ended stuck. A held secret makes the sign-in work
    the run can do, so only a site with none ends needing login."""
    obs = _at("https://shop.test/login", _button("Sign in"))
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"")
    recovery: JsonValue = {**_GIVE_UP, "needs_login": True}
    llm = ScriptedLLM([recovery])
    secrets = ScopedSecrets({"PASSWORD": "hunter2"}, "https://shop.test") if stored else None
    agent = Agent(page, ScriptedJev({}), llm, secrets=secrets)
    state = await run_state()
    with pytest.raises(_Stop) as stopped:
        await agent._recover(state, obs, "uncertain next step (0.81)")
    assert stopped.value.status is (Status.STUCK if stored else Status.NEEDS_LOGIN)


@pytest.mark.parametrize(
    "control",
    [
        # A wall whose only affordance is localized still says it wants a sign-in.
        _button("Se connecter"),
        _button("Anmelden"),
        _button("Iniciar sesión"),
        _button("Accedi"),
        _button("ログイン"),
        # A federated sign-in names its provider, and neither half says "sign in" alone.
        _link("sso", "Continue with SSO", "/sso"),
        _link("google", "Continue with Google", "https://accounts.google.com/o/oauth2/v2/auth"),
        # The label can be plain and only the link says what the control does.
        _link("sso", "Continue", "/sso/start"),
    ],
)
async def test_a_localized_or_federated_sign_in_still_ends_the_run_needing_login(control: Control) -> None:
    """A real wall often offers only `Se connecter`, `Anmelden` or `Continue with Google`, so an English-only
    label check would report such a page as stuck and tell the caller to narrow the task instead of signing in."""
    obs = _at("https://shop.test/projects/deep", control)
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"")
    recovery: JsonValue = {**_GIVE_UP, "needs_login": True}
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([recovery]))
    state = await run_state()
    with pytest.raises(_Stop) as stopped:
        await agent._recover(state, obs, "uncertain next step (0.81)")
    assert stopped.value.status is Status.NEEDS_LOGIN


async def test_a_recovery_claim_of_a_sign_in_without_a_wall_stays_stuck() -> None:
    """Absent controls do not establish missing authentication. A page showing neither a credential field nor a
    sign-in to act on is not a wall, however recovery reads it, so the run stays stuck rather than claiming the
    user is signed out."""
    obs = _at("https://shop.test/author/deep", _button("Menu")).model_copy(
        update={"title": "Author", "viewport_text": "Sidebar\n\nAccount\n"}
    )
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"")
    recovery: JsonValue = {**_GIVE_UP, "needs_login": True}
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([recovery]))
    state = await run_state()
    with pytest.raises(_Stop) as stopped:
        await agent._recover(state, obs, "uncertain next step (0.81)")
    assert stopped.value.status is Status.STUCK


async def test_a_page_with_no_sign_in_affordance_is_not_reported_as_a_wall(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A page showing neither credentials nor a sign-in to act on does not end the run needing login just because
    Jev flagged a wall: absent controls are not evidence of missing authentication, so the run acts instead."""
    obs = _at("https://shop.test/author/deep", _button("Menu")).model_copy(
        update={"title": "Author", "viewport_text": "Sidebar"}
    )
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    agent._observe = AsyncMock(return_value=obs)
    agent._outwait = AsyncMock(return_value=False)
    agent._step = AsyncMock(return_value=False)
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r", text="Report the editor URL", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    walled = _code_decision(Operation.CLICK, obs.controls[0]).model_copy(update={"login_required": 0.99})
    monkeypatch.setattr(agent_module, "decide", AsyncMock(side_effect=[walled, _Stop(Status.UNVERIFIED)]))
    with pytest.raises(_Stop) as stopped:
        await agent._loop(state, None, None)
    assert stopped.value.status is Status.UNVERIFIED
