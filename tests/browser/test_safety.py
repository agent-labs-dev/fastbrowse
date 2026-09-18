"""Safety boundaries exercised through real Chrome, with only model responses scripted."""

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.config import Config, Thresholds
from fastbrowse.models import Attachment, Authorization, Limits, Operation, SecretRef, Status, StepOutcome
from fastbrowse.page import Action
from tests.browser.test_browser import eval_value, find, observe_until, wait_until
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM

PLAN: JsonValue = {"requirements": [], "answer_expected": False}
CONFIG = Config(thresholds=Thresholds(login_required_above=1))


class Secrets:
    def __init__(self, origin: str) -> None:
        self.origin = origin
        self.resolved: list[str] = []

    def available(self) -> tuple[SecretRef, ...]:
        return (SecretRef(name="token", origins=(self.origin,)),)

    async def resolve(self, name: str, origin: str) -> str:
        self.resolved.append(origin)
        return "top-secret-value"


@pytest.mark.parametrize("label", ["Username", "Frame field"])
async def test_window_origin_cannot_spoof_secret_scope(
    page: CdpPage, browser_session: BrowserSession, main_site: str, iframe_site: str, label: str
) -> None:
    await page.navigate(main_site)
    target = find(await observe_until(page, label), label)
    session_id = (
        browser_session.frame_sessions()[target.frame_id] if target.frame_id else browser_session.active_session_id
    )
    assert (
        await eval_value(browser_session, session_id, "window.origin = 'https://bank.example'")
        == "https://bank.example"
    )
    obs = await page.observe()
    target = find(obs, label)
    assert target.frame_origin == (iframe_site if target.frame_id else main_site)

    secrets = Secrets("https://bank.example")
    result = await Agent(
        page,
        ScriptedJev({"operation": "fill", "fill_target": target.id, "pick": "secret:token"}),
        ScriptedLLM([PLAN]),
        config=CONFIG,
        secrets=secrets,
    ).run("Fill the token")
    assert result.status is Status.NEEDS_INPUT and secrets.resolved == []

    obs = await page.observe()
    result = await page.act(
        Action(
            operation=Operation.FILL,
            target_id=target.id,
            text="do-not-leak",
            secret=True,
            secret_origin="https://bank.example",
        ),
        obs,
    )
    assert result.outcome is StepOutcome.FAILED
    assert find(await page.observe(), label).value == ""


async def test_secret_resolution_uses_receiving_origin(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    observed = await observe_until(page, "Frame field")
    target = find(observed, "Frame field")
    secrets = Secrets(main_site)
    agent = Agent(
        page,
        ScriptedJev({"operation": "fill", "fill_target": target.id, "pick": "secret:token"}),
        ScriptedLLM([PLAN]),
        config=CONFIG,
        secrets=secrets,
    )
    result = await agent.run("Fill the frame field with the token")
    assert result.status is Status.NEEDS_INPUT and secrets.resolved == []
    assert find(await page.observe(), "Frame field").value == ""


async def test_page_rechecks_secret_origin_before_insertion(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    obs = await observe_until(page, "Frame field")
    target = find(obs, "Frame field")
    result = await page.act(
        Action(operation=Operation.FILL, target_id=target.id, text="do-not-leak", secret=True, secret_origin=main_site),
        obs,
    )
    assert result.outcome is StepOutcome.FAILED
    assert find(await page.observe(), "Frame field").value == ""


@pytest.mark.parametrize("authorize", [False, True])
async def test_enter_uses_form_semantics_and_focuses_verified_target(
    page: CdpPage, browser_session: BrowserSession, main_site: str, authorize: bool
) -> None:
    await page.navigate(f"{main_site}/safety.html")
    obs = await page.observe()
    field = find(obs, "Account name")
    assert field.submit_semantics and "Delete account" in field.submit_semantics
    await eval_value(browser_session, browser_session.active_session_id, "document.getElementById('other').focus()")
    jev = ScriptedJev({"operation": "enter", "enter_target": field.id})
    result = await Agent(page, jev, ScriptedLLM([PLAN]), config=CONFIG).run(
        "Delete the account", authorization=Authorization(irreversible_actions=authorize), limits=Limits(max_steps=1)
    )
    assert any("irreversible" in request for request in jev.requests) == (not authorize)
    submitted = await eval_value(browser_session, browser_session.active_session_id, "document.body.dataset.submitted")
    if authorize:
        assert submitted == "delete" and result.steps[0].outcome is StepOutcome.EXECUTED
    else:
        assert submitted is None and result.status is Status.NEEDS_CONFIRMATION


@pytest.mark.parametrize("choice,authorize", [("accept", False), ("accept", True), ("dismiss", False)])
async def test_dialog_acceptance_has_its_own_gate(
    page: CdpPage, browser_session: BrowserSession, main_site: str, choice: str, authorize: bool
) -> None:
    await page.navigate(f"{main_site}/safety.html")
    obs = await page.observe()
    await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Continue").id), obs)
    jev = ScriptedJev({"operation": "dialog", "pick": choice})
    result = await Agent(page, jev, ScriptedLLM([PLAN]), config=CONFIG).run(
        "Delete the account", authorization=Authorization(irreversible_actions=authorize), limits=Limits(max_steps=1)
    )
    if choice == "accept" and not authorize:
        assert result.status is Status.NEEDS_CONFIRMATION
        assert browser_session.pending_dialog() is not None
        await browser_session.handle_dialog(False)
    else:
        assert result.steps[0].outcome is StepOutcome.EXECUTED
        assert (
            await eval_value(browser_session, browser_session.active_session_id, "document.body.dataset.accepted")
            == str(choice == "accept").lower()
        )
    assert any("irreversible" in request for request in jev.requests) == (choice == "accept" and not authorize)


@pytest.mark.parametrize("operation", [Operation.SELECT, Operation.UPLOAD])
async def test_covered_controls_are_not_modified(
    page: CdpPage, browser_session: BrowserSession, main_site: str, operation: Operation
) -> None:
    await page.navigate(f"{main_site}/safety.html")
    await eval_value(
        browser_session, browser_session.active_session_id, "document.getElementById('cover').style.display = 'block'"
    )
    obs = await page.observe()
    target = find(obs, "Destination" if operation is Operation.SELECT else "Attachment")
    result = await page.act(
        Action(
            operation=operation,
            target_id=target.id,
            text="Second",
            files=(Attachment(name="data.txt", mime_type="text/plain", content=b"data"),),
        ),
        obs,
    )
    assert result.outcome is StepOutcome.COVERED
    assert await eval_value(
        browser_session,
        browser_session.active_session_id,
        "[document.querySelector('select').value, document.querySelector('[type=file]').files.length]",
    ) == ["First", 0]


@pytest.mark.parametrize("attempt", range(60))
async def test_iframe_focus_hit_testing_and_capture_scope(
    attempt: int, page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/safety.html")

    async def inner_loaded() -> bool:
        return bool(
            await eval_value(
                browser_session,
                browser_session.active_session_id,
                "document.getElementById('same').contentDocument?.querySelector('input') !== null",
            )
        )

    await wait_until(inner_loaded)
    obs = await page.observe()
    field = find(obs, "Inner field")
    assert field.frame_id is not None
    assert (
        await page.act(Action(operation=Operation.FILL, target_id=field.id, text="landed"), obs)
    ).outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Inner field").value == "landed"
    obs = await page.observe()
    button = find(obs, "Inner button")
    assert (await page.act(Action(operation=Operation.CLICK, target_id=button.id), obs)).outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Inner clicked")
    capture = await page.capture()
    assert "Same-origin evidence" in capture.text and "Shadow evidence" in capture.text
    assert capture.inaccessible_frames == 0
    assert all(b.frame_id == field.frame_id for b in capture.blocks if "evidence" in capture.text[b.start : b.end])
    assert any("shadow:" in b.source_id for b in capture.blocks if "Shadow evidence" in capture.text[b.start : b.end])

    await page.navigate(main_site)
    obs = await observe_until(page, "Frame field")
    field = find(obs, "Frame field")
    assert (
        await page.act(Action(operation=Operation.FILL, target_id=field.id, text="frame value"), obs)
    ).outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Frame field").value == "frame value"
    obs = await page.observe()
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "window.__clicks = []; document.addEventListener('mousedown', e => __clicks.push("
        "[e.target.id || e.target.tagName, e.clientX, e.clientY, scrollY]), true); scrollY",
    )
    opened = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Open popup").id), obs)
    assert opened.outcome is StepOutcome.EXECUTED, opened
    try:
        await wait_until(lambda: any("popup.html" in t.url for t in browser_session.tabs()))
    except AssertionError:
        targets = await browser_session.client.send.Target.getTargets(params=None)
        clicks = await eval_value(browser_session, browser_session.active_session_id, "[__clicks, scrollY]")
        raise AssertionError((clicks, opened, browser_session.tabs(), targets)) from None
    popup = next(t for t in browser_session.tabs() if "popup.html" in t.url)
    await browser_session.switch_tab(popup.id)
    assert browser_session.frame_sessions() == {}
    assert all("Frame field" not in c.label for c in (await page.observe()).controls)
    assert "Inside the frame" not in (await page.capture()).text


async def test_resolved_secret_is_masked_in_model_metadata_but_raw_url_is_preserved(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    from collections.abc import Mapping

    from fastbrowse.jev import Evaluation, Question
    from fastbrowse.models import Frozen

    class RecordingJev(ScriptedJev):
        def __init__(self) -> None:
            super().__init__({})
            self.states: list[JsonValue] = []

        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            self.states.append(state)
            return await super().evaluate(state, questions)

    class Output(Frozen):
        count: int

    await page.navigate(f"{main_site}/safety.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.getElementById('other').addEventListener('input', e => { "
        "document.title = e.target.value; history.replaceState({}, '', '#' + e.target.value); });",
    )
    obs = await page.observe()
    field = find(obs, "Other field")
    jev = RecordingJev()
    jev.pick = {"operation": "fill", "fill_target": field.id, "pick": "secret:token"}
    llm = ScriptedLLM([PLAN, PLAN, {"complete": True, "missing": []}, PLAN])
    agent = Agent(page, jev, llm, config=CONFIG, secrets=Secrets(main_site))
    first = await agent.run("Fill the token", limits=Limits(max_steps=1))
    assert first.steps[0].outcome is StepOutcome.EXECUTED
    assert "top-secret-value" in (await page.observe()).url

    seen_urls: list[str] = []

    async def until(url: str) -> bool:
        seen_urls.append(url)
        return True

    jev.pick = {"operation": "done"}
    second = await agent.run("Get the count", output_schema=Output, until=until)
    assert second.status is Status.COMPLETE
    assert seen_urls == [f"{main_site}/safety.html#top-secret-value"]
    obs = await page.observe()
    await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Echo prompt").id), obs)
    jev.pick = {"operation": "dialog", "pick": "dismiss"}
    third = await agent.run("Dismiss the prompt", limits=Limits(max_steps=1))
    assert third.steps[0].outcome is StepOutcome.EXECUTED
    assert all("top-secret-value" not in str(state) for state in jev.states)
    assert all("top-secret-value" not in message.content for _, messages in llm.calls for message in messages)
    assert any("••••" in message.content for _, messages in llm.calls for message in messages)


async def test_fill_does_not_claim_execution_when_page_rejects_value(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/safety.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.getElementById('other').addEventListener('input', e => { e.target.value = ''; });",
    )
    obs = await page.observe()
    result = await page.act(
        Action(operation=Operation.FILL, target_id=find(obs, "Other field").id, text="rejected"), obs
    )
    assert result.outcome is StepOutcome.FAILED
