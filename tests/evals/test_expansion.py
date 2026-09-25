import dataclasses
import json
import runpy
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock

import httpx
import pytest

from fastbrowse.evals import live, live_tasks, more_tasks, observe
from fastbrowse.evals.live_tasks import Outcome
from fastbrowse.evals.status import Ending, normalize, status_matches
from fastbrowse.models import Status


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
    "internet-login": ("You logged into a secure area!", None),
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


def test_an_idle_hosted_session_that_succeeded_is_done() -> None:
    assert normalize("idle", hosted=True, hosted_success=True) == Ending.DONE
    assert normalize("idle", hosted=True, hosted_success=False) != Ending.DONE


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


@pytest.mark.parametrize(
    ("raw", "success", "passed"),
    [
        ("stopped", True, True),
        ("stopped", False, False),
        ("stopped", None, False),
        ("done", None, False),
        ("complete", True, False),
        ("running", True, False),
        ("timed_out", True, False),
        ("error", True, False),
    ],
)
async def test_hosted_correct_answer_requires_successful_terminal_status(
    raw: str, success: bool | None, passed: bool, monkeypatch: pytest.MonkeyPatch
) -> None:
    hosted = AsyncMock(
        return_value=(
            Outcome("v1", None, None),
            live.ArmReport(status=raw, task_successful=success, seconds=1, dollars=0.1),
        )
    )
    monkeypatch.setattr(live, "hosted_arm", hosted)
    task = next(t for t in live_tasks.TASKS if t.id == "pypi-version")
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("browser-use", task, "v1", http, Path(), bitwarden=False, record=None)
    assert row.correct and row.passed is passed
    assert row.status == raw
    assert (row.normalized_status == Ending.DONE) is passed


def test_registry_environment_excludes_unrelated_keys_and_uses_fresh_home() -> None:
    env = live.arm_environment(
        "jev-ultrafast",
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
    assert {name for name, spec in live.ARMS.items() if spec.default} == {"fastbrowse", "jev-ultrafast", "browser-use"}
    assert live.ARMS["browser-use-oss"].pin == "browser-use==0.13.10"
    assert live.prompt(live_tasks.TASKS[0]) == f"Start at {live_tasks.TASKS[0].start}. {live_tasks.TASKS[0].task}"


async def test_registry_dispatch_accepts_new_arm_without_branching(monkeypatch: pytest.MonkeyPatch) -> None:
    runner = AsyncMock(return_value=(Outcome("v1", None, None), live.ArmReport(status="done", seconds=3, dollars=None)))
    monkeypatch.setitem(live.ARMS, "test-arm", live.ArmSpec(runner=runner, pin="test", env_allowlist=(), tier="A"))
    task = next(t for t in live_tasks.TASKS if t.id == "pypi-version")
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("test-arm", task, "v1", http, Path(), bitwarden=False, record=None)
    assert row.passed
    runner.assert_awaited_once()


@pytest.mark.parametrize("focused", [True, False])
async def test_observer_chooses_focused_page_or_rejects_ambiguity(
    focused: bool, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def evaluate(*, params: dict, session_id: str) -> dict:
        expr = params["expression"]
        if expr == "document.hasFocus()":
            return {"result": {"value": focused and session_id == "new"}}
        if expr == observe._SNAPSHOT:
            assert session_id == "new"
            return {"result": {"value": {"url": "https://fixture.test/final", "controls": [{"label": "Backpack"}]}}}
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
    monkeypatch.setattr(observe, "CDPClient", lambda _: client)
    result = await observe.observe_browser("ws://fixture.test")
    assert (result.url == "https://fixture.test/final") is focused
    if not focused:
        assert result.error and result.controls is None
    client.stop.assert_awaited_once()


async def test_final_observer_reads_local_chrome_after_driver_disconnects() -> None:
    from cdp_use.client import CDPClient

    from fastbrowse.adapters.local_chrome import find_chrome, local_chrome
    from fastbrowse.evals.local import fixture_server
    from fastbrowse.models import LocalChrome

    if find_chrome(None) is None:
        pytest.skip("Chrome is not installed")  # ty: ignore[too-many-positional-arguments]
    with fixture_server() as (base, _), local_chrome(LocalChrome()) as browser:
        client = CDPClient(browser.cdp_url)
        await client.start()
        try:
            target = await client.send.Target.createTarget(params={"url": "about:blank"})
            attached = await client.send.Target.attachToTarget(params={"targetId": target["targetId"], "flatten": True})
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
        final = await observe.observe_browser(browser.cdp_url)
        assert final.error is None and final.url == base + "/contact.html"
        assert final.controls and any("name" in label.lower() for label, _ in final.controls)


def test_oss_done_requires_success_and_step_exhaustion_is_budget() -> None:
    runner = runpy.run_path(str(live.OSS_RUNNER))
    for done, success, steps, status in [
        (True, True, 1, "done"),
        (True, False, 1, "stopped"),
        (False, None, 50, "budget_exceeded"),
    ]:
        history = SimpleNamespace(
            is_done=lambda done=done: done, is_successful=lambda success=success: success, history=[None] * steps
        )
        assert runner["ending"](history, 50) == status
    schema = runner["output_model"]({"title": "Answer", "properties": {"package": {"type": "string"}}})
    assert schema.model_validate_json('{"package":"test"}').package == "test"


@pytest.mark.parametrize("arm", ["jev-ultrafast", "browser-use-oss"])
async def test_subprocess_uses_shared_prompt_and_harness_evidence(arm: str, monkeypatch: pytest.MonkeyPatch) -> None:
    from fastbrowse.evals.observe import FinalPage

    entered = False

    class Cloud:
        connection = SimpleNamespace(cdp_url="ws://fixture", live_url=None)
        cost = ()

        async def __aenter__(self) -> "Cloud":
            nonlocal entered
            entered = True
            return self

        async def __aexit__(self, *_: object) -> None:
            nonlocal entered
            entered = False

    task = next(t for t in live_tasks.TASKS if t.id == "pypi-open")

    async def invoke(command: tuple, script: Path, request: dict, env: dict, runtime: str) -> bytes:
        assert entered
        assert request["start"] == task.start and request["goal"] == live.prompt(task)
        assert request["max_steps"] == live.MAX_STEPS
        assert "GH_TOKEN" not in env
        return json.dumps(
            {
                "status": "done",
                "error": None,
                "seconds": 1.0,
                "dollars": 0.03,
                "final_url": "https://stale.test/",
                "controls": [["stale", None]],
                "steps": 2,
                "actions": 1,
                "trace": [],
                "jev_dollars": 0.01,
                "text_dollars": 0.02,
                "unmetered_requests": 0,
                "text_model": "test",
            }
        ).encode()

    async def observed(_: str) -> FinalPage:
        assert entered
        return FinalPage(url="https://pypi.org/project/httpx", controls=(("observed", None),))

    monkeypatch.setattr(live, "BrowserUseCloudBrowser", lambda *_, **__: Cloud())
    monkeypatch.setattr(
        live,
        "load_settings",
        lambda: SimpleNamespace(
            browser_key=lambda: "cloud", openrouter_key=lambda: "text", typesafe_api_key=None, ai_gateway_api_key=None
        ),
    )
    monkeypatch.setattr(live, "_invoke", invoke)
    monkeypatch.setattr(live, "observe_browser", observed)
    monkeypatch.setenv("GH_TOKEN", "unrelated")
    async with httpx.AsyncClient() as http:
        runner = live.ultrafast_arm if arm == "jev-ultrafast" else live.oss_arm
        outcome, report = await runner(task, http, record=None)
    assert not entered and report.dollars == 0.03
    assert outcome.final_url == "https://pypi.org/project/httpx" and outcome.controls == (("observed", None),)


async def test_fast_arm_does_not_substitute_reported_or_start_url(monkeypatch: pytest.MonkeyPatch) -> None:
    task = live_tasks.TASKS[0]

    async def run(prompt: str, **kwargs: object) -> SimpleNamespace:
        assert prompt == live.prompt(task) and kwargs["start"] == task.start
        return SimpleNamespace(answer="v1", data=None, final_url="https://unobserved.test", evidence=[])

    monkeypatch.setattr(live, "run_task", run)
    monkeypatch.setattr(live, "load_settings", lambda: SimpleNamespace(browser_key=lambda: "test"))
    async with httpx.AsyncClient() as http:
        outcome, _, _ = await live.fast_arm(task, http, Path(), bitwarden=False, record=None)
    assert outcome.final_url is None and outcome.controls is None


async def test_unsupported_recording_fails_before_any_preparation(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(live, "load_settings", AsyncMock(side_effect=AssertionError("must not load credentials")))
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


async def test_local_rows_keep_unknown_cost_and_raw_time(monkeypatch: pytest.MonkeyPatch) -> None:
    from fastbrowse.evals import runner

    result = SimpleNamespace(
        status=Status.COMPLETE,
        would_fire=(),
        answer="34.50",
        data=None,
        error=None,
        steps=(),
        cost=SimpleNamespace(has_unknown=True, known_dollars=0.01, seconds_by_call=lambda: {}),
    )
    session = SimpleNamespace(__aenter__=AsyncMock(), __aexit__=AsyncMock())

    class Session:
        async def __aenter__(self) -> object:
            return await session.__aenter__()

        async def __aexit__(self, *_: object) -> None:
            await session.__aexit__()

    monkeypatch.setattr(runner, "BrowserSession", lambda *_: Session())
    monkeypatch.setattr(runner, "CdpPage", lambda *_: None)
    monkeypatch.setattr(runner, "Agent", lambda *_, **__: SimpleNamespace(run=AsyncMock(return_value=result)))
    ticks = iter([10.0, 15.0])
    monkeypatch.setattr(runner, "time", SimpleNamespace(monotonic=lambda: next(ticks)))
    monkeypatch.setattr(runner, "transient_seconds", lambda *_: 2.0)
    settings = Mock(jev=lambda _: None, llm=lambda _: None)
    recorder = Mock(clear=lambda: None, snapshot=lambda: {})
    row = await runner.run_task(runner.TASKS[0], "https://fixture.test", recorder, Mock(), Mock(), Mock(), settings)
    assert row["arm"] == "fastbrowse" and row["correct"] is True
    assert row["seconds"] == 5 and row["transient_seconds"] == 2 and row["dollars"] is None


def test_a_hosted_run_never_judged_is_an_outage_not_a_failure() -> None:
    """A dynamic-loading answer was correct, but Browser Use gave no verdict within the wait and it graded failed."""
    assert normalize("stopped", hosted=True, hosted_success=None) == Ending.UNAVAILABLE
    assert normalize("idle", hosted=True, hosted_success=None) == Ending.UNAVAILABLE
    assert normalize("stopped", hosted=True, hosted_success=False) == Ending.STOPPED


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
    (task,) = [t for t in more_tasks.HELDOUT if t.id == "new-window"]
    assert task.check(Outcome("It opens a new window", None, None), None)
    assert task.check(Outcome('The heading is "New Window"', None, None), None) is None
