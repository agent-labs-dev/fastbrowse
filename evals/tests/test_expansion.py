import dataclasses
import json
import runpy
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast
from unittest.mock import AsyncMock, Mock

import httpx
import pytest
from fastbrowse.evals.status import Ending, normalize, status_matches
from fastbrowse.models import CostBasis, CostComponent, CostLine, Status

from fastbrowse_evals import backend, final_page, live, live_tasks, more_tasks
from fastbrowse_evals.final_page import EVIDENCE, LOCATION, RESTORE, UNHIDE, FinalPage
from fastbrowse_evals.live_tasks import Outcome


def test_missing_final_evidence_fails_every_strict_grader() -> None:
    missing = Outcome("backpack large 43.18 JetBlue $846", None, None)
    assert live_tasks._ended_on(missing, "/post")
    assert live_tasks._ended_under(missing, "/abs")
    assert live_tasks._cart(missing, None)
    assert live_tasks._paused_before_paying(missing, None)
    assert live_tasks._flight_results(missing, one_way_nonstop=False)
    assert more_tasks._submitted(missing, None)
    assert more_tasks._submitted(dataclasses.replace(missing, final_url="https://fixture.test/post"), None) is None


# A correct answer, and the truth it is graded against, for each hosted-agent task whose check reads the page.
_PAGE_GRADED = {
    "wiki-godel": ("1931", "1931"),
    "arxiv-title": ("Attention Is All You Need", "Attention Is All You Need"),
    "saucedemo-checkout": (live_tasks._SAUCE_TOTAL, live_tasks._SAUCE_TOTAL),
    "saucedemo-cart": ("Sauce Labs Backpack", None),
    "expandtesting-login": ("You logged into a secure area!", None),
    "practice-login": ("Logged In Successfully", None),
    "google-flights": ("JetBlue, $846", None),
}


@pytest.mark.parametrize(("task_id", "graded"), _PAGE_GRADED.items())
def test_the_hosted_agent_is_graded_on_its_answer_where_it_cannot_show_a_page(
    task_id: str, graded: tuple[str, str | None]
) -> None:
    """The SDK never reports a final page, so the hosted agent's correct answer passes; any arm whose page the
    harness observes still fails without one."""
    (task,) = (t for t in live_tasks.TASKS if t.id == task_id)
    assert "browser-use" in task.arms
    answer, truth = graded
    hosted = Outcome(answer, None, None, unobservable=True)
    assert task.check(hosted, truth) is None
    assert task.check(dataclasses.replace(hosted, unobservable=False), truth) is not None


def test_an_idle_hosted_session_that_answered_is_done() -> None:
    assert normalize("idle", hosted=True, answered=True) == Ending.DONE
    assert normalize("idle", hosted=True, answered=False) != Ending.DONE


def test_cart_requires_product_in_the_answer_and_on_the_final_cart() -> None:
    """The hosted arm is graded on its answer; fastbrowse answering nothing, or denying it, passed on the page."""
    page = Outcome(
        "The cart holds the Sauce Labs Backpack",
        None,
        "https://www.saucedemo.com/cart.html",
        quotes=(("https://www.saucedemo.com/cart.html", "Sauce Labs Backpack"),),
    )
    assert live_tasks._cart(page, None)
    page = dataclasses.replace(page, controls=(("Sauce Labs Backpack", None),))
    assert live_tasks._cart(page, None) is None
    assert live_tasks._cart(dataclasses.replace(page, answer="No backpack was added"), None)
    assert live_tasks._cart(dataclasses.replace(page, final_url="https://www.saucedemo.com/inventory.html"), None)


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("complete", Ending.DONE),
        ("DONE", Ending.DONE),
        ("stopped", Ending.STOPPED),
        ("needs_login", Ending.STOPPED),
        ("budget_exceeded", Ending.BUDGET),
        ("timed_out", Ending.TIMEOUT),
        ("blocked", Ending.BLOCKED),
        ("unavailable", Ending.UNAVAILABLE),
        (None, Ending.ERROR),
        ("unknown", Ending.ERROR),
    ],
)
def test_status_normalization_fails_closed(raw: str | None, expected: Ending) -> None:
    assert normalize(raw) == expected
    assert not status_matches("fastbrowse", "needs_login", Status.NEEDS_CONFIRMATION)
    assert status_matches("fastbrowse", "needs_confirmation", Status.NEEDS_CONFIRMATION)


def test_registry_environment_excludes_unrelated_keys_and_uses_fresh_home() -> None:
    env = live.arm_environment(
        "browser-use-oss",
        "/tmp/test-home",
        {
            "PATH": "/usr/bin",
            "HOME": "/private",
            "GH_TOKEN": "private",
            "AWS_SECRET_ACCESS_KEY": "private",
            "BROWSER_USE_API_KEY": "private",
            "PYTHONPATH": "/private",
            "OPENROUTER_API_KEY": "private",
        },
    )
    assert env["HOME"] == "/tmp/test-home" and env["PATH"] == "/usr/bin"
    assert "private" not in json.dumps(env)
    assert {name for name, spec in live.ARMS.items() if spec.default} == {
        "fastbrowse",
        "browser-use",
        "browser-use-ultrafast",
    }
    assert (
        live.ARMS["browser-use-oss"].pin
        == f"{live.OSS_PACKAGE}; default model bu-fast, default mode (ultrafast on bu-fast)"
    )
    assert live.prompt(live_tasks.TASKS[0]) == f"Start at {live_tasks.TASKS[0].start}. {live_tasks.TASKS[0].task}"


async def test_registry_dispatch_accepts_new_arm_without_branching(monkeypatch: pytest.MonkeyPatch) -> None:
    runner = AsyncMock(return_value=(Outcome("v1", None, None), live.ArmReport(status="done", seconds=3, dollars=None)))
    monkeypatch.setitem(live.ARMS, "test-arm", live.ArmSpec(runner=runner, pin="test", env_allowlist=(), tier="A"))
    task = next(t for t in live_tasks.TASKS if t.id == "pypi-version")
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("test-arm", task, "v1", http, Path(), record=None)
    assert row.passed is None
    runner.assert_awaited_once()


@pytest.mark.parametrize("focused", [True, False])
async def test_observer_chooses_focused_page_or_rejects_ambiguity(
    focused: bool, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def evaluate(*, params: dict, session_id: str) -> dict:
        expr = params["expression"]
        if expr == "document.hasFocus()":
            return {"result": {"value": focused and session_id == "new"}}
        if expr == LOCATION:
            return {"result": {"value": f"https://fixture.test/{session_id}"}}
        if expr == EVIDENCE:
            return {"result": {"value": {"inner_width": 1120}}}
        assert expr in (UNHIDE, RESTORE) and session_id == "new"
        return {"result": {}}

    client = SimpleNamespace(
        start=AsyncMock(),
        stop=AsyncMock(),
        send=SimpleNamespace(
            Target=SimpleNamespace(
                getTargets=AsyncMock(
                    return_value={
                        "targetInfos": [
                            {"type": "page", "url": "https://fixture.test/old", "targetId": "old"},
                            {"type": "page", "url": "https://fixture.test/new", "targetId": "new"},
                        ]
                    }
                ),
                attachToTarget=AsyncMock(side_effect=lambda params: {"sessionId": params["targetId"]}),
            ),
            Runtime=SimpleNamespace(evaluate=AsyncMock(side_effect=evaluate)),
        ),
    )

    @asynccontextmanager
    async def connect(*, cdp_url: str, target_match: str, config: object):
        assert target_match == "https://fixture.test/new"
        controls = [SimpleNamespace(label="Backpack", value=None)]
        yield SimpleNamespace(observe=AsyncMock(return_value=SimpleNamespace(url=target_match, controls=controls)))

    monkeypatch.setattr(final_page, "CDPClient", lambda _: client)
    monkeypatch.setattr(final_page, "connect_cdp", connect)
    result = await final_page.observe_browser("ws://fixture.test")
    assert (result.url == "https://fixture.test/new") is focused
    if focused:
        assert result.controls == (("Backpack", None),) and result.evidence and result.evidence.inner_width == 1120
    else:
        assert result.error and result.controls is None
    client.stop.assert_awaited_once()


async def test_final_observer_reads_local_chrome_at_the_held_viewport() -> None:
    """The harness's own session holds the viewport through the run and the reading, and the observer reads the tab
    another client drove, after that client has gone."""
    from cdp_use.client import CDPClient
    from fastbrowse.evals.harness import find_chrome
    from fastbrowse.evals.local import fixture_server

    from fastbrowse_evals.browsers import local_browser

    if (binary := find_chrome(None)) is None:
        pytest.skip("Chrome is not installed")
    with fixture_server() as (base, _):
        async with local_browser(Path(binary)) as browser, final_page.prepared_tab(browser.cdp_url, viewport=True):
            client = CDPClient(browser.cdp_url)
            await client.start()
            try:
                (page,) = [t for t in (await client.send.Target.getTargets())["targetInfos"] if t["type"] == "page"]
                attached = await client.send.Target.attachToTarget(
                    params={"targetId": page["targetId"], "flatten": True}
                )
                session = attached["sessionId"]
                await client.send.Page.navigate(params={"url": base + "/contact.html"}, session_id=session)
                await client.send.Runtime.evaluate(
                    params={
                        "expression": "new Promise(r => document.readyState === 'complete' ? r() : "
                        "window.addEventListener('load', r, {once: true}))",
                        "awaitPromise": True,
                    },
                    session_id=session,
                )
            finally:
                await client.stop()
            final = await final_page.observe_browser(browser.cdp_url)
    assert final.error is None and final.url == base + "/contact.html"
    assert final.evidence and final.evidence.inner_width == final_page.VIEWPORT["width"]
    assert final.controls and any("name" in label.lower() for label, _ in final.controls)


def test_oss_ending_reads_status_and_verdict() -> None:
    runner = runpy.run_path(str(live.OSS_RUNNER))
    for status, success, outage, ending in [
        ("completed", True, None, "done"),
        ("completed", None, None, "done"),
        ("completed", False, None, "stopped"),
        ("max_steps", None, None, "budget_exceeded"),
        ("timeout", None, None, "budget_exceeded"),
        ("error", None, None, "error"),
        ("error", None, "Model provider returned HTTP 503", "unavailable"),
    ]:
        assert runner["ending"](SimpleNamespace(status=status, success=success), outage) == ending
    schema = runner["output_model"]({"title": "Answer", "properties": {"package": {"type": "string"}}})
    assert schema.model_validate_json('{"package":"test"}').package == "test"


def test_oss_outage_reads_the_error_record() -> None:
    outage = runpy.run_path(str(live.OSS_RUNNER))["provider_outage"]
    transient = frozenset({"ModelServerError", "ModelConnectionError"})
    assert (
        outage("ModelServerError: status_code: 503, body: {}", transient) == "Model provider failed (ModelServerError)"
    )
    assert outage("ModelConnectionError: reset", transient) == "Model provider failed (ModelConnectionError)"
    assert outage("ModelHTTPError: status_code: 400", transient) is None
    assert outage("ValueError: ModelServerError", transient) is None
    assert outage(None, transient) is None


def test_oss_step_log_pairs_calls_with_returns() -> None:
    log = runpy.run_path(str(live.OSS_RUNNER))["step_log"]
    call = SimpleNamespace(part_kind="tool-call", tool_name="click", args={"index": 3}, tool_call_id="a")
    back = SimpleNamespace(part_kind="tool-return", content="x" * 5000, tool_call_id="a")
    steps = log([SimpleNamespace(parts=[call]), SimpleNamespace(parts=[back])])
    assert steps == [{"index": 0, "operation": "click", "target": {"index": 3}, "url": None, "observation": "x" * 2000}]


@pytest.mark.parametrize("stopped", [True, False])
async def test_oss_arm_runs_on_its_own_browser_and_the_harness_clock(stopped: bool, monkeypatch) -> None:
    """browser-use starts and stops its own cloud browser; the harness times ready to answer, reads the final page
    before the stop, prices the browser from Browser Use's meter and stops a browser the runner could not."""
    task = next(t for t in live_tasks.TASKS if t.id == "pypi-open")
    log: list[str] = []
    clock = iter([10.0, 14.5])
    monkeypatch.setattr(live, "session_ready", lambda: next(clock))
    monkeypatch.setattr(live, "session_answered", lambda: next(clock))

    @asynccontextmanager
    async def conversation(command: tuple[str, ...], script: Path, env: dict[str, str], cwd: str):
        assert env["BROWSER_USE_API_KEY"] == "cloud" and "text" not in env.values()
        events = iter(
            [
                {
                    "event": "ready",
                    "cdp_url": "https://b.test",
                    "live_url": "https://live.test",
                    "browser_id": "b1",
                    "open_seconds": 2.0,
                },
                {
                    "event": "result",
                    "status": "done",
                    "answer": "httpx",
                    "steps": 2,
                    "dollars": 0.01,
                    "step_ceiling": 1_000_000,
                    "timeout_ceiling": 86_400.0,
                },
                {"event": "stopped", "browser_id": "b1", "stopped": stopped},
            ]
        )

        async def send(message: dict[str, Any]) -> None:
            log.append("stop" if message == {} else "request")
            if message != {}:
                assert message["goal"] == live.prompt(task) and message["max_steps"] is None
                assert message["browser_minutes"] == live.CLOUD_BROWSER_MINUTES and "cdp_ws" not in message

        async def receive(event: str) -> dict[str, Any]:
            message = next(events)
            assert message["event"] == event
            log.append(event)
            return message

        yield send, receive

    async def observed(cdp: str, *, secrets: object = None) -> FinalPage:
        log.append("observe")
        return FinalPage(url="https://pypi.org/project/httpx", controls=())

    async def stop(browser_id: str) -> None:
        log.append(f"api-stop {browser_id}")

    async def websocket(_: object, cdp_url: str) -> str:
        return "ws://b.test"

    async def metered(browser_id: str) -> tuple[CostLine, ...]:
        return (CostLine(component=CostComponent.BROWSER, basis=CostBasis.METERED, dollars=0.002),)

    monkeypatch.setattr(live, "_conversation", conversation)
    monkeypatch.setattr(live, "observe_browser", observed)
    monkeypatch.setattr(live, "devtools_websocket", websocket)
    monkeypatch.setattr(live, "metered_cost", metered)
    monkeypatch.setattr(live, "stop_cloud_browser", stop)
    monkeypatch.setattr(backend, "environment", lambda: {"BROWSER_USE_API_KEY": "cloud", "OPENROUTER_API_KEY": "text"})
    async with httpx.AsyncClient() as http:
        outcome, report = await live.oss_arm(task, http, record=None)
    assert log == ["request", "ready", "result", "observe", "stop", "stopped"] + ([] if stopped else ["api-stop b1"])
    assert report.seconds == 4.5 and report.session_open_seconds == 2.0 and report.timing_note == live.OSS_SETUP
    assert report.dollars == pytest.approx(0.012) and report.cost_by_component == {"browser": 0.002, "llm": 0.01}
    assert report.provenance["llm_cost_basis"] == live.OSS_LLM_COST
    assert outcome.final_url == "https://pypi.org/project/httpx" and outcome.answer == "httpx"


async def test_oss_arm_whose_browser_did_not_start_never_started(monkeypatch) -> None:
    @asynccontextmanager
    async def conversation(*_: object):
        async def send(_: dict[str, Any]) -> None: ...

        async def receive(event: str) -> dict[str, Any]:
            raise live._OssEvent({"event": "result", "status": "error", "error": "RuntimeError: 402"})

        yield send, receive

    stops: list[str] = []
    monkeypatch.setattr(live, "_conversation", conversation)
    monkeypatch.setattr(live, "stop_cloud_browser", AsyncMock(side_effect=stops.append))
    monkeypatch.setattr(backend, "environment", lambda: {"BROWSER_USE_API_KEY": "cloud"})
    task = next(t for t in live_tasks.TASKS if t.id == "pypi-open")
    async with httpx.AsyncClient() as http:
        with pytest.raises(live.NotStarted, match="402"):
            await live.oss_arm(task, http, record=None)
    assert stops == []  # no browser was reported, so none is left to stop


@pytest.mark.parametrize("secret_usage", ["none", "declared", "stored"])
async def test_fast_arm_grades_the_page_the_harness_read_not_the_reported_url(
    monkeypatch: pytest.MonkeyPatch, secret_usage: str
) -> None:
    task = dataclasses.replace(
        live_tasks.TASKS[0],
        secrets={"password": "hidden"} if secret_usage == "declared" else {},
        bitwarden_item="vault-item" if secret_usage == "stored" else None,
    )
    held: list[bool] = []

    async def run(prompt: str, **kwargs: object) -> SimpleNamespace:
        assert prompt == live.prompt(task) and kwargs["start"] == task.start
        assert await cast(Any, kwargs["open_browser"])() == "ws://pool"
        assert held == [False], "fastbrowse runs inside the harness's prepared tab"
        return SimpleNamespace(answer="v1", data=None, final_url="https://unobserved.test", evidence=[])

    @asynccontextmanager
    async def tab(cdp_url: str, *, viewport: bool):
        held.append(viewport)
        yield

    async def observed(cdp_url: str, *, url: str | None = None, secrets: object = None) -> FinalPage:
        assert url == "https://unobserved.test"
        assert secrets == (None if secret_usage == "stored" else task.secrets)
        return FinalPage(error="expected one final tab, found 2")

    monkeypatch.setattr(live, "run_task", run)
    monkeypatch.setattr(live, "prepared_tab", tab)
    monkeypatch.setattr(live, "observe_browser", observed)
    monkeypatch.setattr(backend, "environment", lambda: {"OPENROUTER_API_KEY": "k"})
    async with httpx.AsyncClient() as http:
        outcome, _, seen = await live.fast_arm(task, http, Path(), record=None, cdp_url="ws://pool")
    assert outcome.final_url is None and outcome.controls is None
    assert seen.final.error == "expected one final tab, found 2"


async def test_unsupported_recording_fails_before_any_preparation(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(backend, "environment", Mock(side_effect=AssertionError("must not load credentials")))
    with pytest.raises(SystemExit):
        await live.main(["--arms", "browser-use-oss", "--record", "/tmp/unused"])


async def test_unavailable_retries_are_bounded_and_recorded(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(live, "_truth", AsyncMock(return_value="v1"))
    monkeypatch.setattr(
        live,
        "hosted_arm",
        AsyncMock(
            return_value=(Outcome(None, None, None), live.ArmReport(status="unavailable", seconds=1, dollars=None))
        ),
    )
    sleep = AsyncMock()
    monkeypatch.setattr(live.asyncio, "sleep", sleep)
    target = tmp_path / "rows.jsonl"
    await live.main(["--arms", "browser-use", "--only", "pypi-version", "--out", str(target)])
    row = live.EvalRow.model_validate_json(target.read_text())
    assert row.retries == live.OUTAGE_RETRIES and row.normalized_status == Ending.UNAVAILABLE and not row.passed
    outage_waits = [c.args[0] for c in sleep.await_args_list if c.args and c.args[0] >= 60]
    assert outage_waits == [60, 120, 240, 480, 600]


def test_a_hosted_run_is_done_on_its_own_answer_not_browser_uses_later_verdict() -> None:
    """0.5.7: `is_task_successful` failed six correct answers whose sessions showed no sign of failing; fastbrowse
    is held only to its own completion, so the hosted agent is held to its own `done`."""
    assert normalize("stopped", hosted=True, answered=True) == Ending.DONE
    assert normalize("stopped", hosted=True, answered=False) == Ending.STOPPED


async def test_a_run_failed_on_a_site_serving_errors_is_an_outage() -> None:
    """the-internet.herokuapp.com served Heroku's Application Error (a 503) and every arm failed its tasks."""
    task = live_tasks.TASKS[0]
    responses = iter([httpx.Response(503), httpx.Response(403)])
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: next(responses))) as http:
        assert "answered HTTP 503" in (await live._down(task, http) or "")
        # A site that answers, even to refuse a bot, is up: the run's failure stands.
        assert await live._down(task, http) is None

    def unreachable(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("refused", request=request)

    async with httpx.AsyncClient(transport=httpx.MockTransport(unreachable)) as http:
        assert "unreachable" in (await live._down(task, http) or "")


def test_new_window_needs_the_heading_not_the_prompt_echoed() -> None:
    (task,) = [t for t in more_tasks.DEV if t.id == "new-window"]
    assert task.check(Outcome("It opens a new window", None, None), None)
    heading = 'The heading is "Example of a new window page for Automation Testing Practice"'
    assert task.check(Outcome(heading, None, None), None) is None
