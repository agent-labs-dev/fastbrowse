import asyncio
import dataclasses
import json
import os
import runpy
import signal
import sys
import time
import types
from base64 import urlsafe_b64decode, urlsafe_b64encode
from contextlib import asynccontextmanager
from contextvars import ContextVar
from datetime import UTC, date, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast
from unittest.mock import AsyncMock

import httpx
import pytest
from fastbrowse.evals.status import Ending
from fastbrowse.models import CostBreakdown, Unavailable

from fastbrowse_evals import backend, final_page, judging, live, live_tasks, more_tasks
from fastbrowse_evals.attempt_clock import TIMING_BASIS, AttemptClock
from fastbrowse_evals.backend import RETRYABLE_STATUS, TRANSIENT_TRANSPORT
from fastbrowse_evals.browsers import Browser
from fastbrowse_evals.final_page import FinalPage
from fastbrowse_evals.live_tasks import TASKS, LiveTask, Outcome, PageEvidence, page_defect
from fastbrowse_evals.process import CommandError

RUNNER: dict[str, Any] = runpy.run_path(str(live.ULTRAFAST_RUNNER))


async def test_cancelled_competitor_stops_child_after_launcher_exits(tmp_path: Path) -> None:
    pid_file = tmp_path / "child"
    script = tmp_path / "launch.py"
    script.write_text(
        "import subprocess, sys\n"
        "from pathlib import Path\n"
        "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n"
        f"Path({str(pid_file)!r}).write_text(str(child.pid))\n"
    )
    attempt = asyncio.create_task(live._invoke((sys.executable,), script, {}, {}, str(tmp_path)))
    try:
        for _ in range(500):
            if pid_file.exists():
                break
            await asyncio.sleep(0.01)
        else:
            pytest.fail("competitor did not start its child")
        attempt.cancel()
        with pytest.raises(asyncio.CancelledError):
            await attempt
        async with asyncio.timeout(5):
            while True:
                try:
                    os.kill(int(pid_file.read_text()), 0)
                except ProcessLookupError:
                    break
                await asyncio.sleep(0.05)
    finally:
        attempt.cancel()
        await asyncio.gather(attempt, return_exceptions=True)


def task(task_id: str) -> LiveTask:
    return next(t for t in TASKS if t.id == task_id)


def test_videos_count_up_past_names_already_claimed(tmp_path: Path) -> None:
    # Repeats are allocated before any of them records, so a name must be taken the moment it is handed out.
    first = live.video_path(tmp_path, "fastbrowse", task("hn-top"))
    assert first == tmp_path.resolve() / "fastbrowse" / "hn-top-1.mp4"
    assert live.video_path(tmp_path, "fastbrowse", task("hn-top")).name == "hn-top-2.mp4"
    assert live.video_path(tmp_path, "jev-ultrafast", task("hn-top")).name == "hn-top-1.mp4"


@pytest.mark.parametrize(
    "telemetry",
    [
        {"events": [{"event": "request_slow", "call": "jev", "seconds": 7.25}]},
        {"transient_seconds": 4.5},
    ],
)
@pytest.mark.parametrize(
    ("status", "answer", "passed"), [("complete", "Attention Is All You Need", True), ("stuck", "wrong answer", False)]
)
async def test_recovered_provider_delays_preserve_the_attempt(
    monkeypatch: pytest.MonkeyPatch, telemetry: dict[str, Any], status: str, answer: str, passed: bool
) -> None:
    async def fast_report(*_: Any, **__: Any) -> tuple[Outcome, live.ArmReport]:
        outcome = Outcome(answer, None, "https://arxiv.org/abs/1706.03762", controls=(("Search", None),))
        return outcome, live.ArmReport(status=status, dollars=0.001, seconds=12.0, step_log=TRIED, **telemetry)

    monkeypatch.setattr(live, "_fast_report", fast_report)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm(
            "fastbrowse", task("arxiv-title"), "Attention Is All You Need", http, Path(), record=None
        )
    assert row.normalized_status == (Ending.DONE if passed else Ending.STOPPED)
    assert row.passed is None
    assert row.correct is None
    assert row.seconds == 12.0
    assert row.dollars == 0.001
    assert row.final_controls == [("Search", None)]
    assert row.failure is not None


def test_no_arm_has_any_limit() -> None:
    assert all(value is None for value in live.AGENT_LIMITS.model_dump().values())
    assert live.crash_status(TimeoutError("socket")) == ("TimeoutError: socket", None)


@pytest.mark.parametrize("site", [200, 503])
@pytest.mark.parametrize("arm", ["fastbrowse", "browser-use"])
async def test_an_ending_the_arm_found_unavailable_stands_whatever_the_site_probe_sees(arm: str, site: int) -> None:
    """The harness's fetch says nothing of the arm's own browser or provider, so it neither excuses nor convicts."""
    row = live._crashed(arm, task("pypi-newer"), "Unavailable", at=0.0, seconds=1.0, status="unavailable", record=None)
    transport = httpx.MockTransport(lambda _: httpx.Response(site))
    async with httpx.AsyncClient(transport=transport) as http:
        checked = await live._site_checked(row, task("pypi-newer"), http)
    assert checked == row


async def test_an_answer_key_that_will_not_come_back_fails_only_its_task(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A 403 from a rate-limited answer-key API is not worth retrying, and it used to end the whole suite."""

    async def truth(task: LiveTask, _: httpx.AsyncClient) -> object:
        if task.id == "wiki-open":
            raise httpx.HTTPStatusError(
                "rate limited", request=httpx.Request("GET", "https://api.test"), response=httpx.Response(403)
            )
        return None

    async def ultrafast_arm(
        _: LiveTask, __: httpx.AsyncClient, *, record: Path | None
    ) -> tuple[Outcome, live.ArmReport]:
        outcome = Outcome("Done.", None, "https://pypi.org/project/httpx/", evidence=_evidence())
        return outcome, live.ArmReport(status="done", dollars=0.001, seconds=3.0)

    monkeypatch.setattr(live, "_truth", truth)
    monkeypatch.setattr(live, "ultrafast_arm", ultrafast_arm)
    monkeypatch.setattr(live, "prepare_ultrafast", AsyncMock())
    out = tmp_path / "live.jsonl"
    argv = ["--only", "wiki-open", "pypi-open", "--arms", "jev-ultrafast", "--out", str(out)]
    assert await live.main(argv) == 0
    rows = {row.task: row for row in map(live.EvalRow.model_validate_json, out.read_text().splitlines())}
    assert rows["pypi-open"].passed is None
    assert rows["wiki-open"].passed is None
    assert rows["wiki-open"].failure is not None
    assert rows["wiki-open"].failure.startswith("truth raised HTTPStatusError")


def test_gateway_answers_take_the_direct_api_shape() -> None:
    payload = {
        "answers": {"operation": {"type": "choice", "choice": "CLICK", "probabilities": {"CLICK": 0.50, "DONE": 0.51}}},
        "usage": {"inputTokens": 900, "outputTokens": 3},
        "providerMetadata": {"typesafe": {"confidence": {"operation": 0.7}}, "gateway": {"cost": "0.0004"}},
    }
    answer, cost = RUNNER["systemone_answer"](payload)
    operation = answer["answers"]["operation"]
    assert operation["confidence"] == 0.7
    # A near-tie the gateway's rounding put a hundredth the wrong way round is Jev's choice, as fastbrowse reads it.
    assert operation["probabilities"] == {"CLICK": 0.51, "DONE": 0.51}
    assert cost == 0.0004


def test_a_real_gap_in_probabilities_is_left_alone() -> None:
    payload = {"answers": {"op": {"type": "choice", "choice": "A", "probabilities": {"A": 0.3, "B": 0.7}}}}
    answer, cost = RUNNER["systemone_answer"](payload)
    assert answer["answers"]["op"]["probabilities"] == {"A": 0.3, "B": 0.7}
    assert cost is None


def test_the_runner_answers_a_choice_of_one_option_without_asking_jev(monkeypatch: pytest.MonkeyPatch) -> None:
    sent: list[dict[str, Any]] = []

    def post(_model: Any, _url: str, _headers: dict[str, str], body: dict[str, Any]) -> dict[str, Any]:
        sent.append(body)
        return {"answers": {"operation": {"choice": "TYPE_TEXT"}}, "usage": {"cost": 0.0001}}

    monkeypatch.setenv("TYPESAFE_API_KEY", "key")
    monkeypatch.setitem(RUNNER["patch_transport"].__globals__, "_post", post)
    model = SimpleNamespace()
    RUNNER["patch_transport"](model, RUNNER["Meter"]())
    questions = {
        "operation": {"type": "choice", "criteria": {"TYPE_TEXT": "", "DONE": "", "BLOCKED": ""}},
        "type_text_target": {"type": "choice", "criteria": {"search": "the search box"}},
    }
    result = model.post_json("https://api.typesafe.ai/v1/evaluate", "key", {"state": {}, "questions": questions})
    assert list(sent[0]["questions"]) == ["operation"]
    assert result["answers"]["type_text_target"]["choice"] == "search"
    assert result["answers"]["operation"]["choice"] == "TYPE_TEXT"


@pytest.mark.parametrize(
    ("source", "url", "key"),
    [
        (None, "https://openrouter.ai/api/v1/systemone", "or"),
        ("openrouter", "https://openrouter.ai/api/v1/systemone", "or"),
        ("typesafe", "https://api.typesafe.ai/v1/systemone", "ts"),
        ("gateway", "https://ai-gateway.vercel.sh/v4/ai/evaluation-model", "gw"),
    ],
)
@pytest.mark.parametrize("pinned", [False, True])
def test_ultrafast_uses_the_selected_jev_route_and_meters_its_cost(
    monkeypatch: pytest.MonkeyPatch, source: str | None, url: str, key: str, pinned: bool
) -> None:
    configured = {"OPENROUTER_API_KEY": "or", "TYPESAFE_API_KEY": "ts", "AI_GATEWAY_API_KEY": "gw"}
    if source is not None:
        configured["FASTBROWSE_DECISION_SOURCE"] = source
    if pinned:
        configured |= {"FASTBROWSE_DECISION_BASE_URL": "https://proxy.test/", "FASTBROWSE_DECISION_MODEL": "pinned-jev"}
    monkeypatch.setattr(backend, "environment", lambda: configured)
    env = live._ultrafast_env("wss://browser.test", "/tmp/ultrafast-test")
    for name in ("TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY", "FASTBROWSE_JEV_BASE_URL", "FASTBROWSE_JEV_MODEL"):
        monkeypatch.delenv(name, raising=False)
        if name in env:
            monkeypatch.setenv(name, env[name])

    def post(_model: Any, endpoint: str, headers: dict[str, str], body: dict[str, Any]) -> dict[str, Any]:
        path = "/v4/ai/evaluation-model" if source == "gateway" else "/v1/systemone"
        assert endpoint == (f"https://proxy.test{path}" if pinned else url)
        assert headers["Authorization"] == f"Bearer {key}"
        if source == "gateway":
            assert "model" not in body
            assert headers["ai-model-id"] == ("pinned-jev" if pinned else backend.jev_models()["gateway"])
            return {"answers": {}, "usage": {"inputTokens": 100}, "providerMetadata": {"gateway": {"cost": "0.001"}}}
        # The Jev release this fastbrowse build's Rust core sends for the source, never a copy of it.
        assert body["model"] == ("pinned-jev" if pinned else backend.jev_models()[source or "openrouter"])
        return {"answers": {}, "usage": {"input_tokens": 100, "cost": 0.001}}

    monkeypatch.setitem(RUNNER["patch_transport"].__globals__, "_post", post)
    model, meter = SimpleNamespace(), RUNNER["Meter"]()
    RUNNER["patch_transport"](model, meter)
    model.post_json(
        "https://api.typesafe.ai/v1/systemone", os.environ["TYPESAFE_API_KEY"], {"state": {}, "questions": {}}
    )
    assert meter.jev == 0.001 and meter.text == 0 and meter.unmetered == 0


@pytest.mark.parametrize("content", ['{"text": "httpx"}\n```', '```json\n{"text": "httpx"}\n```', '{"text": "httpx"}'])
def test_the_runner_reads_the_text_helpers_json_past_a_stray_fence(content: str) -> None:
    assert json.loads(RUNNER["unfenced"](content)) == {"text": "httpx"}


def test_each_task_runs_only_where_it_grades_on_equal_terms() -> None:
    for live_task in TASKS:
        if "jev-ultrafast" in live_task.arms:
            # jev-ultrafast has no answer, so its tasks must be graded on the page alone.
            assert live_task.output_schema is None
            # The hosted agent is graded on its final page, read through its kept-alive browser, so it runs these too.
            assert "browser-use" in live_task.arms
    assert {t.id for t in TASKS if "jev-ultrafast" in t.arms} >= {"wiki-open", "flights-search"}


@pytest.mark.parametrize(
    ("final_url", "passed"),
    [
        ("https://en.wikipedia.org/wiki/G%C3%B6del%27s_incompleteness_theorems", True),
        ("https://en.wikipedia.org/wiki/Kurt_G%C3%B6del", False),
        (None, False),
    ],
)
def test_a_navigation_task_is_graded_on_the_page_alone(final_url: str | None, passed: bool) -> None:
    outcome = Outcome(None, None, final_url)
    assert (task("wiki-open").check(outcome, None) is None) is passed


def test_hn_comments_accepts_any_leading_story() -> None:
    check = task("hn-comments").check
    assert check(Outcome(None, None, "https://news.ycombinator.com/item?id=2"), ["1", "2"]) is None
    assert check(Outcome(None, None, "https://news.ycombinator.com/item?id=9"), ["1", "2"]) is not None


def _flights_page(day: date, *, trip: str = "One way", nonstop: bool = True) -> tuple[tuple[str, str | None], ...]:
    """The controls a Google Flights results page rendered for a search, as the harness observes them."""
    return (
        (f"Change ticket type. {trip}", trip),
        ("Where from?", "London"),
        ("Where to?", "New York"),
        ("Departure", f"{day:%a, %b} {day.day}"),
        *((("Nonstop, Stops, Selected", ""),) if nonstop else ()),
        (
            f"From 846 US dollars. Nonstop flight with JetBlue. Leaves Heathrow at 8:15 AM on {day:%A, %B} {day.day}",
            None,
        ),
    )


def test_a_flights_search_is_graded_on_the_form_google_rendered_not_the_url() -> None:
    check = task("flights-search").check
    day = date.today() + timedelta(days=28)
    query = "https://www.google.com/travel/flights?q=flights%20from%20London%20to%20New%20York"
    assert check(Outcome(None, None, query, controls=_flights_page(day)), None) is None
    assert check(Outcome(None, None, query, controls=_flights_page(day, trip="Round trip")), None) is not None
    assert check(Outcome(None, None, query, controls=_flights_page(day, nonstop=False)), None) is not None
    assert check(Outcome(None, None, query, controls=_flights_page(day + timedelta(days=1))), None) is not None
    assert check(Outcome(None, None, query), None) is not None


def test_a_filled_form_without_results_is_not_a_search() -> None:
    day = date.today() + timedelta(days=28)
    unsent = (
        *_flights_page(day)[:-1],
        (f"{day:%A, %B} {day.day}, {day.year} , 517 US dollars, Cheapest price", None),
    )
    assert task("flights-search").check(
        Outcome(None, None, "https://www.google.com/travel/flights", controls=unsent), None
    )


def test_the_flights_answer_task_needs_the_search_and_a_price() -> None:
    check = task("google-flights").check
    page = _flights_page(date.today() + timedelta(days=28), trip="Round trip", nonstop=False)
    assert check(Outcome("JetBlue, $846", None, "https://www.google.com/travel/flights", controls=page), None) is None
    assert check(Outcome("JetBlue", None, "https://www.google.com/travel/flights", controls=page), None) is not None
    # Missing page evidence cannot prove a search, even when the answer includes a price.
    assert check(Outcome("JetBlue, $846", None, None), None) is not None


def test_a_repeated_label_passes_when_any_control_holds_the_value() -> None:
    day = date.today() + timedelta(days=28)
    page = (("Where from?", ""), *_flights_page(day))
    assert (
        task("flights-search").check(Outcome(None, None, "https://www.google.com/travel/flights", controls=page), None)
        is None
    )


_FLIGHT_DATE = date(2026, 10, 19)
# Recorded after Search and the Nonstop filter, with city entities rather than airport codes.
_FLIGHT_TFS = "CBwQAhorEgoyMDI2LTEwLTE5KABqDAgDEggvbS8wNGpwbHINCAMSCS9tLzAyXzI4NkABSAFwAYIBCwj___________8BmAEC"
_FLIGHT_LEG = b"\x12\x0a2026-10-19\x28\x00\x6a\x0c\x08\x03\x12\x08/m/04jpl\x72\x0d\x08\x03\x12\x09/m/02_286"


def _flight_payload(*legs: bytes, trip: int = 2) -> bytes:
    return b"".join(bytes((26, len(leg))) + leg for leg in legs) + b"\x98\x01" + bytes((trip,))


@pytest.mark.parametrize(
    ("payload", "form", "failure"),
    [
        pytest.param(urlsafe_b64decode(_FLIGHT_TFS), False, None, id="recorded-collapsed-header"),
        pytest.param(_flight_payload(_FLIGHT_LEG), False, None, id="collapsed-header"),
        pytest.param(
            _flight_payload(_FLIGHT_LEG.replace(b"\x6a", b"\x72", 1).replace(b"\x72\x0d", b"\x6a\x0d")),
            True,
            "encoded search",
            id="reversed-route-overrides-form",
        ),
        pytest.param(
            _flight_payload(_FLIGHT_LEG.replace(b"2026-10-19", b"2026-10-20")),
            True,
            "encoded search",
            id="wrong-date-overrides-form",
        ),
        pytest.param(
            _flight_payload(_FLIGHT_LEG.replace(b"2026-10-19", b"2027-10-19")),
            True,
            "encoded search",
            id="wrong-year-overrides-form",
        ),
        pytest.param(
            _flight_payload(_FLIGHT_LEG.replace(b"2026-10-19", b"2026-10-20"), _FLIGHT_LEG),
            True,
            "encoded search",
            id="date-only-in-later-leg",
        ),
        pytest.param(
            _flight_payload(_FLIGHT_LEG.replace(b"\x12\x09/m/02_286", b"\x1a\x09/m/02_286")),
            True,
            "encoded search",
            id="destination-in-wrong-field",
        ),
        pytest.param(_flight_payload(_FLIGHT_LEG, trip=1), True, "ticket type", id="round-trip-overrides-form"),
        pytest.param(_flight_payload(_FLIGHT_LEG)[:-3], True, None, id="trip-type-from-control"),
        pytest.param(_flight_payload(_FLIGHT_LEG)[:-3], False, "ticket type", id="trip-type-unavailable"),
        pytest.param(
            _flight_payload(_FLIGHT_LEG, _FLIGHT_LEG)[:-3], True, "ticket type", id="multiple-legs-override-form"
        ),
    ],
)
def test_flights_grades_the_encoded_outbound_leg(
    monkeypatch: pytest.MonkeyPatch, payload: bytes, form: bool, failure: str | None
) -> None:
    monkeypatch.setattr(live_tasks, "_FLIGHT_DAY", _FLIGHT_DATE)
    encoded = urlsafe_b64encode(payload).decode().rstrip("=")
    page = _flights_page(_FLIGHT_DATE)
    outcome = Outcome(
        "JetBlue, $846",
        None,
        f"https://www.google.com/travel/flights/search?tfs={encoded}",
        controls=page if form else page[4:],
    )
    actual = task("flights-search").check(outcome, None)
    assert actual is None if failure is None else actual is not None and failure in actual


@pytest.mark.parametrize("encoded", ["!", "A", "", "GoAB", "GoCAgICAgICAgIAC", "GgESAQ", "AA"])
@pytest.mark.parametrize("form", [False, True])
def test_unreadable_flight_urls_require_rendered_fields(
    monkeypatch: pytest.MonkeyPatch, encoded: str, form: bool
) -> None:
    monkeypatch.setattr(live_tasks, "_FLIGHT_DAY", _FLIGHT_DATE)
    page = _flights_page(_FLIGHT_DATE)
    outcome = Outcome(
        None,
        None,
        f"https://www.google.com/travel/flights/search?tfs={encoded}",
        controls=page if form else page[4:],
    )
    assert (task("flights-search").check(outcome, None) is None) is form


@pytest.mark.parametrize(
    ("task_id", "rows", "nonstop", "answer", "failure"),
    [
        ("google-flights", True, False, "JetBlue, $846", None),
        ("google-flights", True, False, "JetBlue", "no price"),
        ("google-flights", False, False, "JetBlue, $846", "no results"),
        ("flights-search", False, True, None, "no results"),
        ("flights-search", True, False, None, "no nonstop filter"),
    ],
)
def test_encoded_flight_search_still_needs_results_filters_and_price(
    monkeypatch: pytest.MonkeyPatch,
    task_id: str,
    rows: bool,
    nonstop: bool,
    answer: str | None,
    failure: str | None,
) -> None:
    monkeypatch.setattr(live_tasks, "_FLIGHT_DAY", _FLIGHT_DATE)
    page = _flights_page(_FLIGHT_DATE, nonstop=nonstop)[4:]
    if not rows:
        page = (*page[:-1], (f"{_FLIGHT_DATE:%A, %B} {_FLIGHT_DATE.day}, {_FLIGHT_DATE.year}, $517", None))
    outcome = Outcome(answer, None, f"https://www.google.com/travel/flights/search?tfs={_FLIGHT_TFS}", controls=page)
    actual = task(task_id).check(outcome, None)
    assert actual is None if failure is None else actual is not None and failure in actual


V4_START = datetime(2026, 11, 9, 12, 0, tzinfo=UTC)


def v4_event(kind: str, after: float, **data: Any) -> SimpleNamespace:
    return SimpleNamespace(type=kind, ts=V4_START + timedelta(seconds=after), data=data)


def v4_tool(after: float, call: str, tool: str, status: str, **state: Any) -> SimpleNamespace:
    part = {"type": "tool", "tool": tool, "callID": call, "state": {"status": status, **state}}
    return v4_event("core.event", after, type="tool_use", part=part)


class V4Client:
    """Browser Use's V4 client as `hosted_arm` uses it: a run that ends after its events, a browser kept alive."""

    def __init__(self, events: list[SimpleNamespace], *, status: str = "completed", **run: Any) -> None:
        self.created: dict[str, Any] = {}
        self.calls: list[str] = []
        self.run = SimpleNamespace(
            status=SimpleNamespace(value=status),
            created_at=V4_START,
            session_id="sess-1",
            model="gpt-6-luna",
            model_params={"reasoning": "high"},
            result=None,
            output=None,
            error=None,
            total_input_tokens=100,
            total_output_tokens=20,
        )
        vars(self.run).update(run)
        self.cost = SimpleNamespace(
            llm_cost_usd="0.01", search_cost_usd="0", browser_cost_usd="0.002", proxy_cost_usd="0.001",
            total_cost_usd="0.013",
        )  # fmt: skip
        self.events = events
        self.runs = SimpleNamespace(
            create=self._create,
            status=AsyncMock(return_value=SimpleNamespace(status=SimpleNamespace(value=status))),
            events=self._events,
            get=AsyncMock(return_value=self.run),
            cancel=AsyncMock(side_effect=lambda *_: self.calls.append("cancel")),
        )
        self.browsers = SimpleNamespace(
            get=AsyncMock(return_value=SimpleNamespace(cdp_url=None, recording_url=None, recording_available=False)),
            stop=AsyncMock(side_effect=lambda *_: self.calls.append("stop")),
        )
        self.sessions = SimpleNamespace(cost=AsyncMock(side_effect=self._cost))
        self.close = AsyncMock()

    async def _create(self, task: str, **kwargs: Any) -> SimpleNamespace:
        self.created = {"task": task, **kwargs}
        return SimpleNamespace(id="run-1")

    async def _events(self, _run: str, *, after: str | None = None) -> SimpleNamespace:
        # Two pages, so the second must be asked for with the cursor the first returned.
        half = len(self.events) // 2
        page = self.events[half:] if after == "c1" else self.events[:half]
        return SimpleNamespace(events=page, next_after=None if after else "c1", has_more=after is None)

    async def _cost(self, _session: str) -> SimpleNamespace:
        self.calls.append("cost")
        return self.cost


def finished_run() -> list[SimpleNamespace]:
    return [
        v4_event("run.created", 0),
        v4_event("browser.ready", 2, live_view_url="https://live.browser-use.com/b1", browser_session_id="b1"),
        v4_event("core.event", 3, type="step_start"),
        v4_tool(3, "c1", "browser_navigate", "running", input={"url": "https://pypi.org/project/httpx/"}),
        v4_tool(4.5, "c1", "browser_navigate", "completed", input={}, output="key bu_secret_key on the page"),
        v4_tool(4.6, "c2", "javascript", "completed", input={}, output="1"),
        v4_event("llm.response", 5, model="gpt-6-luna", cost_usd=0.001),
        v4_event("core.event", 6, type="text", part={"type": "text", "text": "0.28.1"}),
        v4_event("run.completed", 8),
        v4_event("browser.released", 9, status="kept_alive"),
    ]


def use_v4(monkeypatch: pytest.MonkeyPatch, client: V4Client) -> None:
    import browser_use_sdk.v4  # an optional extra

    monkeypatch.setattr(browser_use_sdk.v4, "AsyncBrowserUse", lambda **_: client)
    monkeypatch.setattr(live, "HOSTED_POLL_SECONDS", 0)
    monkeypatch.setattr(
        backend, "environment", lambda: {"BROWSER_USE_API_KEY": "bu_secret_key", "OPENROUTER_API_KEY": "or"}
    )


@pytest.mark.parametrize(("arm", "model"), [("browser-use", None), ("browser-use-ultrafast", "bu-ultrafast")])
def test_each_hosted_arm_asks_v4_for_the_same_run_on_its_own_model(arm: str, model: str | None) -> None:
    request = live.hosted_request(task("pypi-newer"), arm, record=True)
    # The default model is Browser Use's own choice, so it is omitted; neither arm sends model parameters.
    assert request.get("model") == model and "model_params" not in request
    # Always explicit: an omitted max_cost_usd would let Browser Use apply its own $25 default.
    assert request["max_cost_usd"] == live.HOSTED_UNCAPPED_USD == "1000"
    assert request["keep_alive"] == live.HOSTED_KEEP_ALIVE_SECONDS and 120 <= request["keep_alive"] <= 14400
    schema = task("pypi-newer").output_schema
    assert schema is not None and request["output_schema"] == schema.model_json_schema()
    assert request["browser_settings"] == {"proxyCountryCode": "us", "record": True}
    assert request["secret_bindings"] is None
    navigate = next(t for t in TASKS if t.category is live_tasks.Category.NAVIGATE)
    assert {"screenWidth", "screenHeight"} <= live.hosted_request(navigate, arm, record=False)[
        "browser_settings"
    ].keys()


def test_a_login_reaches_the_hosted_agent_as_bindings_typed_only_on_its_start_host() -> None:
    bindings = live.hosted_request(task("saucedemo-checkout"), "browser-use-ultrafast", record=False)["secret_bindings"]
    assert bindings == [
        {"alias": name, "source": {"type": "inline", "value": value}, "allowedDomains": ["www.saucedemo.com"]}
        for name, value in task("saucedemo-checkout").secrets.items()
    ]


@pytest.mark.parametrize("arm", ["browser-use", "browser-use-ultrafast"])
async def test_a_hosted_v4_run_is_read_stopped_then_costed(monkeypatch: pytest.MonkeyPatch, arm: str) -> None:
    client = V4Client(finished_run(), output={"version": "0.28.1"}, model=live.HOSTED_MODELS[arm] or "gpt-6-luna")
    use_v4(monkeypatch, client)
    timed: list[tuple[Any, ...]] = []
    timing = live.hosted_timing
    monkeypatch.setattr(live, "hosted_timing", lambda *args: timed.append(args) or timing(*args))
    outcome, report = await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None, arm=arm)
    assert client.created.get("model") == live.HOSTED_MODELS[arm]
    assert outcome.data == {"version": "0.28.1"} and json.loads(str(outcome.answer)) == {"version": "0.28.1"}
    assert report.status == "completed" and report.answered is True
    assert live.ending_of(arm, report) is Ending.DONE
    assert report.model == client.run.model and report.provenance["browser_session_id"] == "b1"
    # The browser is stopped once, before the session's cost is read, so the cost includes all of its time.
    client.browsers.stop.assert_awaited_once_with("b1")
    assert client.calls == ["stop", "cost"]
    assert report.dollars is None and report.unknown_cost is True
    assert report.cost_by_component == {}
    receipt = report.provisional_hosted_cost
    assert receipt is not None and receipt.total_cost_usd == "0.013"
    assert receipt.proxy_cost_usd == "0.001" and receipt.observed_at.tzinfo is not None
    assert receipt.session_id == "sess-1" and receipt.run_id == "run-1"
    # Timed from the browser being ready to the run completing, by Browser Use's own clock.
    assert timed[0][2:4] == (V4_START + timedelta(seconds=2), V4_START + timedelta(seconds=8))
    assert report.steps == 1 and report.llm_calls == 1 and report.llm_calls_basis == "physical_requests"
    assert [s["operation"] for s in report.step_log] == ["browser_navigate", "javascript", "reply"]
    # A call seen only once it had settled has no opening to time it from; V4 names no browser tools to sum.
    assert report.step_log[0]["duration_ms"] == 1500 and "duration_ms" not in report.step_log[1]
    assert report.browser_seconds is None
    assert "bu_secret_key" not in report.model_dump_json()


async def test_a_new_hosted_run_not_yet_visible_is_followed_not_abandoned(monkeypatch: pytest.MonkeyPatch) -> None:
    import browser_use_sdk.v4  # an optional extra

    client = V4Client(finished_run(), result="0.28.1")
    visible = client.runs.status.return_value
    client.runs.status.side_effect = [browser_use_sdk.v4.BrowserUseError(404, "not found"), visible, visible]
    use_v4(monkeypatch, client)
    monkeypatch.setattr(live, "HOSTED_VISIBILITY_RETRY_SECONDS", 0.0)
    outcome, report = await live.hosted_arm(task("hn-top"), httpx.AsyncClient(), record=None)
    assert outcome.answer == "0.28.1" and report.status == "completed"
    client.runs.cancel.assert_not_awaited()


async def test_a_hosted_browser_the_stop_did_not_reach_leaves_its_cost_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    client = V4Client(finished_run(), result="0.28.1")
    client.browsers.stop.side_effect = httpx.ConnectError("down")
    use_v4(monkeypatch, client)
    outcome, report = await live.hosted_arm(task("hn-top"), httpx.AsyncClient(), record=None)
    assert outcome.answer == "0.28.1" and outcome.data is None
    assert report.dollars is None and report.unknown_cost is True and "cost" not in client.calls
    assert client.browsers.stop.await_count == 2  # retried on the way out: a kept-alive browser bills until stopped


async def test_a_failed_v4_run_is_its_agents_failure_and_a_dispatch_failure_an_outage(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = V4Client(finished_run()[:3], status="failed", error="agent crashed")
    use_v4(monkeypatch, client)
    _, report = await live.hosted_arm(task("hn-top"), httpx.AsyncClient(), record=None)
    assert report.status == "failed" and report.answered is False
    assert live.ending_of("browser-use", report) is Ending.ERROR

    client = V4Client([v4_event("run.dispatch_failed", 1)], status="failed")
    use_v4(monkeypatch, client)
    with pytest.raises(Unavailable):
        await live.hosted_arm(task("hn-top"), httpx.AsyncClient(), record=None)
    client.runs.cancel.assert_awaited_once_with("run-1")


@pytest.mark.parametrize("stop_failed", [False, True])
async def test_dispatch_outages_keep_the_provider_identity_and_provisional_receipt(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, stop_failed: bool
) -> None:
    client = V4Client(
        [
            v4_event("browser.ready", 1, browser_session_id="b1"),
            v4_event("run.dispatch_failed", 2),
        ],
        status="failed",
        error="scheduler unavailable bu_secret_key",
    )
    use_v4(monkeypatch, client)
    if stop_failed:
        client.browsers.stop.side_effect = httpx.ConnectError("down")
    row = await live.run_arm("browser-use", task("hn-top"), None, httpx.AsyncClient(), tmp_path, record=None)
    assert row.normalized_status is Ending.UNAVAILABLE and row.passed is None
    assert any(event["type"] == "run.dispatch_failed" for event in row.events)
    assert row.session_id == "sess-1" and row.provenance["run_id"] == "run-1"
    assert row.provenance["browser_session_id"] == "b1"
    assert row.provenance["provider_status"] == "failed"
    assert row.error is not None and "scheduler unavailable" in row.error
    assert "bu_secret_key" not in row.model_dump_json()
    assert row.dollars is None and row.unknown_cost is True
    assert row.cost_by_component == {}
    receipt = row.provisional_hosted_cost
    if stop_failed:
        assert receipt is None
    else:
        assert receipt is not None and receipt.total_cost_usd == "0.013"
        assert receipt.proxy_cost_usd == "0.001" and receipt.observed_at.tzinfo is not None
        assert receipt.session_id == "sess-1" and receipt.run_id == "run-1"
    assert ("cost" in client.calls) is (not stop_failed)
    if not stop_failed:
        assert client.calls.index("stop") < client.calls.index("cost")


async def test_a_hosted_run_the_attempt_cancels_is_cancelled_and_its_browser_stopped(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = V4Client(finished_run()[:2], status="running")
    use_v4(monkeypatch, client)
    monkeypatch.setattr(live, "HOSTED_POLL_SECONDS", 0.01)
    with pytest.raises(TimeoutError):
        async with asyncio.timeout(0.1):
            await live.hosted_arm(task("hn-top"), httpx.AsyncClient(), record=None)
    client.runs.cancel.assert_awaited_once_with("run-1")
    client.browsers.stop.assert_awaited_once_with("b1")


async def test_a_hosted_outage_retries_without_quoting_the_key(monkeypatch: pytest.MonkeyPatch) -> None:
    import browser_use_sdk.v4  # an optional extra

    client = V4Client([])
    client.runs.create = AsyncMock(side_effect=browser_use_sdk.v4.BrowserUseError(503, "upstream echoed bu_secret_key"))
    use_v4(monkeypatch, client)
    with pytest.raises(Unavailable) as error:
        await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None)
    assert "bu_secret_key" not in str(error.value) and "HTTP 503" in str(error.value)
    client.close.assert_awaited_once()


def test_hosted_timing_runs_from_the_browser_starting_to_the_answer() -> None:
    created = datetime(2026, 9, 25, 22, 0, tzinfo=UTC)
    run = SimpleNamespace(created_at=created)
    answered, started = created + timedelta(seconds=7), created + timedelta(seconds=1)
    # Half a second until the run existed and a second until its browser started, then 6s to the answer by the
    # server's own clock; never past the run's wall time, less the opening.
    timing = live.hosted_timing(0.5, run, started, answered, 120.0)
    assert timing == {"seconds": 6.0, "timing_basis": TIMING_BASIS, "session_open_seconds": 1.5}
    assert live.hosted_timing(0.5, run, started, answered, 3.0)["seconds"] == 1.5
    assert live.hosted_timing(0.5, run, started, None, 120.0)["seconds"] == 118.5
    # No browser: none opened, so the agent's time runs from the run existing.
    timing = live.hosted_timing(0.5, run, None, answered, 120.0)
    assert timing == {"seconds": 7.0, "timing_basis": TIMING_BASIS, "session_open_seconds": 0.5}


@pytest.mark.parametrize(
    ("url", "sent"),
    [("https://api.github.com/repos/encode/httpx", "Bearer t0k"), ("https://pypi.org/pypi/httpx/json", None)],
)
async def test_the_github_token_goes_only_to_the_github_api(
    url: str, sent: str | None, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A retried row refetches its answer key, which ran the anonymous GitHub limit out during a Jev outage."""
    monkeypatch.setenv("GITHUB_TOKEN", "t0k")
    request = httpx.Request("GET", url)

    await live._github_token(request)

    assert request.headers.get("Authorization") == sent


def test_pairs_take_a_value_named_before_its_item_only_when_it_says_so() -> None:
    check = more_tasks._pairs(("Agnostic", "12.51"), ("Mother", "16.89"))
    assert check(Outcome("£12.51 for Agnostic and £16.89 for 'Mother'.", None, None), None) is None
    assert check(Outcome("Agnostic £12.51, Mother £16.89.", None, None), None) is None
    assert check(Outcome("Mother 12.51, Agnostic 16.89.", None, None), None) is not None


def test_a_date_stated_in_iso_form_is_stated() -> None:
    truth = {"nights": "9", "start": "2026-09-28", "end": "2026-10-07"}
    iso = "Start 2026-09-28 (Monday), end 2026-10-07. The page says: You selected a range of 9 days."
    assert more_tasks._date_range_check(Outcome(iso, None, None), truth) is None
    assert more_tasks._date_range_check(Outcome(iso.replace("2026-10-07", "2026-10-08"), None, None), truth)
    friday = {"date": "2026-10-02", "mdy": "10/02/2026"}
    assert more_tasks._first_friday_check(Outcome("Friday, 2026-10-02", None, None), friday) is None
    assert more_tasks._first_friday_check(Outcome("2026-10-02", None, None), friday)


def test_the_first_friday_grader_takes_the_date_as_the_page_shows_it() -> None:
    truth = {"date": "2026-10-02", "mdy": "10/02/2026"}
    shown = "The date shown is 10/02/2026. The day shown is Friday. The month shown is October."
    assert more_tasks._first_friday_check(Outcome(shown, None, None), truth) is None
    assert more_tasks._first_friday_check(Outcome("Friday 2 October 2026", None, None), truth) is None
    assert more_tasks._first_friday_check(Outcome("It shows 10/09/2026, Friday, October.", None, None), truth)
    assert more_tasks._first_friday_check(Outcome("It shows 10/02/2026.", None, None), truth)


def test_the_runner_prices_jev_at_list_when_the_gateway_meters_it_free() -> None:
    """Every 0.5.6 Jev request was metered at $0 by the gateway, so both Jev-backed arms read as free of it."""
    meter = RUNNER["Meter"](0.042 / 1_000_000)
    RUNNER["_meter"](meter, "jev", 0.0, 1_000_000)
    RUNNER["_meter"](meter, "jev", "0.0005", 1_000_000)
    RUNNER["_meter"](meter, "text", 0.0, 1_000_000)
    assert meter.jev == pytest.approx(0.042 + 0.0005) and meter.text == 0.0


def test_an_error_sent_with_http_200_is_not_an_answer() -> None:
    """OpenRouter held a wiki-open request open, then sent its error in a 200; upstream failed the run on it."""
    for code, raised in ((503, RUNNER["Unavailable"]), (400, RuntimeError)):
        response = SimpleNamespace(status_code=200, is_error=False, json=lambda code=code: {"error": {"code": code}})
        model = SimpleNamespace(CLIENT=SimpleNamespace(post=lambda *_, response=response, **__: response))
        with pytest.raises(raised, match=f"error {code} in HTTP 200"):
            RUNNER["_post"](model, "https://openrouter.ai/api/v1/chat/completions", {}, {})


async def test_an_attempt_its_site_stalled_during_is_an_outage_for_any_arm(monkeypatch: pytest.MonkeyPatch) -> None:
    """One day's the-internet.herokuapp.com held requests 30s at a time: nested-frames took 36s, and 7s between."""
    served = iter([httpx.Response(200), httpx.Response(503)])
    http = httpx.AsyncClient(transport=httpx.MockTransport(lambda _: next(served, httpx.Response(200))))
    monkeypatch.setattr(live, "SITE_PROBE_SECONDS", 0.01)
    frames = task("expandtesting-login")
    watch = live.SiteWatch(http, [frames])
    start = time.time()
    watching = asyncio.create_task(watch.run())
    await asyncio.sleep(0.1)
    watching.cancel()
    assert await watch.stalled(frames, start, time.time()) == f"site stalled: {frames.start} answered HTTP 503"
    assert await watch.stalled(frames, start - 10, start - 5) is None
    assert await watch.stalled(task("pypi-newer"), start, time.time()) is None


async def test_grading_waits_for_a_probe_overlapping_the_attempt(monkeypatch: pytest.MonkeyPatch) -> None:
    started = asyncio.Event()

    async def slow(request: httpx.Request) -> httpx.Response:
        started.set()
        await asyncio.sleep(0.06)
        return httpx.Response(200)

    monkeypatch.setattr(live, "SITE_STALL_SECONDS", 0.01)
    frames = task("expandtesting-login")
    async with httpx.AsyncClient(transport=httpx.MockTransport(slow)) as http:
        watch = live.SiteWatch(http, [frames])
        start = time.time()
        watching = asyncio.create_task(watch.run())
        try:
            await started.wait()
            assert await watch.stalled(frames, start, time.time()) is not None
        finally:
            watching.cancel()
            await asyncio.gather(watching, return_exceptions=True)


async def test_grading_does_not_wait_for_an_unrelated_or_later_probe() -> None:
    started, release = asyncio.Event(), asyncio.Event()

    async def pending(request: httpx.Request) -> httpx.Response:
        started.set()
        await release.wait()
        return httpx.Response(200)

    frames = task("expandtesting-login")
    async with httpx.AsyncClient(transport=httpx.MockTransport(pending)) as http:
        watch = live.SiteWatch(http, [frames])
        start = time.time()
        watching = asyncio.create_task(watch.run())
        try:
            await started.wait()
            async with asyncio.timeout(1):
                assert await watch.stalled(frames, start - 10, start - 5) is None
                assert await watch.stalled(task("pypi-newer"), start, time.time()) is None
        finally:
            watching.cancel()
            await asyncio.gather(watching, return_exceptions=True)


async def test_cancelling_one_grade_leaves_the_overlapping_probe_for_another() -> None:
    started, release = asyncio.Event(), asyncio.Event()

    async def pending(request: httpx.Request) -> httpx.Response:
        started.set()
        await release.wait()
        return httpx.Response(503)

    frames = task("expandtesting-login")
    async with httpx.AsyncClient(transport=httpx.MockTransport(pending)) as http:
        watch = live.SiteWatch(http, [frames])
        start = time.time()
        watching = asyncio.create_task(watch.run())
        grading: list[asyncio.Task[str | None]] = []
        try:
            await started.wait()
            grading = [asyncio.create_task(watch.stalled(frames, start, time.time())) for _ in range(2)]
            await asyncio.sleep(0)
            assert not any(grade.done() for grade in grading)
            grading[0].cancel()
            await asyncio.gather(grading[0], return_exceptions=True)
            release.set()
            assert await grading[1] == f"site stalled: {frames.start} answered HTTP 503"
        finally:
            for pending_task in [watching, *grading]:
                pending_task.cancel()
            await asyncio.gather(watching, *grading, return_exceptions=True)


@pytest.mark.parametrize(("flag", "value"), [("--concurrency", "0"), ("--concurrency", "-1"), ("--repeat", "0")])
async def test_invalid_run_counts_are_rejected_before_task_selection(
    flag: str, value: str, capsys: pytest.CaptureFixture[str]
) -> None:
    with pytest.raises(SystemExit) as error:
        await live.main([flag, value, "--only", "not-a-task"])
    assert error.value.code == 2
    assert f"{flag} must be positive" in capsys.readouterr().err


@pytest.mark.parametrize("exhausted", [False, True])
async def test_outage_retries_retain_attempts_and_distinct_recordings(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, exhausted: bool
) -> None:
    records: list[Path | None] = []

    async def run(arm: str, task: LiveTask, *_: Any, record: Path | None, **__: Any) -> live.EvalRow:
        records.append(record)
        unavailable = exhausted or len(records) == 1
        return live.EvalRow(
            arm=arm,
            task=task.id,
            category=task.category.value,
            at=time.time(),
            seconds=10.0,
            dollars=0.002,
            status="unavailable" if unavailable else "done",
            normalized_status=Ending.UNAVAILABLE if unavailable else Ending.DONE,
            correct=not unavailable,
            passed=not unavailable,
            failure="provider timeout" if unavailable else None,
            video=str(record),
        )

    monkeypatch.setattr(live, "OUTAGE_RETRIES", 1)
    monkeypatch.setattr(live, "run_arm", run)
    monkeypatch.setattr(live, "prepare_ultrafast", AsyncMock())
    monkeypatch.setattr(live, "_truth", AsyncMock(return_value=None))
    monkeypatch.setattr(live, "_site_checked", AsyncMock(side_effect=lambda row, *_: row))
    monkeypatch.setattr(live.SiteWatch, "run", AsyncMock())
    monkeypatch.setattr(live.SiteWatch, "stalled", AsyncMock(return_value=None))
    sleep = AsyncMock()
    monkeypatch.setattr(live.asyncio, "sleep", sleep)
    out = tmp_path / "run.jsonl"
    assert (
        await live.main(
            [
                "--only",
                "wiki-open",
                "--arms",
                "jev-ultrafast",
                "--out",
                str(out),
                "--record",
                str(tmp_path / "videos"),
            ]
        )
        == 0
    )
    rows = [json.loads(line) for line in out.read_text().splitlines()]
    ledger = [json.loads(line) for line in out.with_suffix(".attempts.jsonl").read_text().splitlines()]
    assert len(rows) == 1 and rows[0]["retries"] == 1
    assert len(ledger) == 2
    assert [row["retries"] for row in ledger] == [0, 1]
    assert [row["selected"] for row in ledger] == [False, True]
    assert [row["retry_wait_seconds"] for row in ledger] == [60, 0]
    assert sum(row["seconds"] for row in ledger) == 20
    assert sum(row["dollars"] for row in ledger) == pytest.approx(0.004)
    assert all(row["run"] == rows[0]["run"] for row in ledger)
    assert all(row["repeat"] == 0 for row in ledger)
    assert records[0] != records[1]
    assert rows[0]["passed"] is not exhausted
    sleep.assert_awaited_once_with(60)


def _live_fakes(monkeypatch: pytest.MonkeyPatch, run: Any, *, truth: Any = None) -> None:
    """The seams `main` calls around a run: no provider, no browser, no answer key, no site probe."""
    monkeypatch.setattr(live, "run_arm", run)
    monkeypatch.setattr(live, "prepare_ultrafast", AsyncMock())
    monkeypatch.setattr(live, "_truth", truth if truth is not None else AsyncMock(return_value=None))
    monkeypatch.setattr(live, "_site_checked", AsyncMock(side_effect=lambda row, *_: row))
    monkeypatch.setattr(live.SiteWatch, "run", AsyncMock())
    monkeypatch.setattr(live.SiteWatch, "stalled", AsyncMock(return_value=None))


async def _await_started(started: asyncio.Event, runner: "asyncio.Task[Any]") -> None:
    """Wait for the first run to start, failing with the runner's own error if it stopped first.

    `main` rejects bad arguments with `parser.error` (a `SystemExit`) and a catalog that does not match
    the pinned Fastbrowse with a `ValueError`. Waiting on the event alone left the suite waiting for a
    signal nothing would ever send: the shard burned its whole timeout instead of naming the cause.
    """
    waiter = asyncio.create_task(started.wait())
    done, _ = await asyncio.wait({waiter, runner}, return_when=asyncio.FIRST_COMPLETED)
    waiter.cancel()
    if started.is_set():
        return
    exc = runner.exception()
    if exc is not None:
        raise exc
    raise AssertionError("the run finished without starting a task")


def _ledger(out: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in out.with_suffix(".attempts.jsonl").read_text().splitlines()]


async def test_a_cancelled_in_flight_attempt_is_persisted_with_its_cost_unknown(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A cancelled run already paid for the attempt: the ledger keeps how long it ran and that its cost is unknown,
    so a suite stopped mid-flight cannot read as a cheap one that never spent."""
    started = asyncio.Event()
    wall_time = [1000.0]
    monkeypatch.setattr(live.time, "time", lambda: wall_time[0])

    async def run(arm: str, task: LiveTask, *_: Any, record: Path | None, **__: Any) -> None:
        await asyncio.sleep(0.2)
        wall_time[0] = 1020.0
        started.set()
        await asyncio.Event().wait()

    _live_fakes(monkeypatch, run)
    out = tmp_path / "run.jsonl"
    runner = asyncio.create_task(live.main(["--only", "wiki-open", "--arms", "jev-ultrafast", "--out", str(out)]))
    await _await_started(started, runner)
    runner.cancel()
    await asyncio.gather(runner, return_exceptions=True)
    [attempt] = _ledger(out)
    assert attempt["task"] == "wiki-open"
    assert attempt["dollars"] is None
    assert attempt["seconds"] > 0
    assert attempt["at"] == 1000.0
    assert attempt["selected"] is False
    assert attempt["repeat"] == 0
    assert attempt["retries"] == 0
    assert attempt["run"]["run_id"]


async def test_cancelling_the_site_probe_keeps_the_finished_report_price(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    probing = asyncio.Event()
    report = live.EvalRow(
        arm="jev-ultrafast",
        task="wiki-open",
        category="navigate",
        at=time.time(),
        seconds=10.0,
        dollars=0.002,
        status="done",
        normalized_status=Ending.DONE,
        correct=True,
        passed=True,
        failure=None,
    )

    async def probe(row: live.EvalRow, *_: Any) -> None:
        probing.set()
        await asyncio.Event().wait()

    _live_fakes(monkeypatch, AsyncMock(return_value=report))
    monkeypatch.setattr(live, "_site_checked", probe)
    out = tmp_path / "run.jsonl"
    runner = asyncio.create_task(live.main(["--only", "wiki-open", "--arms", "jev-ultrafast", "--out", str(out)]))
    await probing.wait()
    runner.cancel()
    await asyncio.gather(runner, return_exceptions=True)
    [attempt] = _ledger(out)
    assert attempt["dollars"] == 0.002
    assert attempt["seconds"] == 10.0
    assert attempt["selected"] is False
    assert out.read_text() == ""


async def test_a_cancelled_queued_attempt_writes_no_ledger_row(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """A run still waiting for its slot never started a browser, so cancelling it leaves no attempt to keep: only
    the one in flight is billed."""
    started = asyncio.Event()

    async def run(arm: str, task: LiveTask, *_: Any, record: Path | None, **__: Any) -> None:
        started.set()
        await asyncio.Event().wait()

    _live_fakes(monkeypatch, run)
    out = tmp_path / "run.jsonl"
    first = next(t.id for t in TASKS if t.id in {"wiki-open", "pypi-open"})
    runner = asyncio.create_task(
        live.main(
            ["--only", "wiki-open", "pypi-open", "--arms", "jev-ultrafast", "--concurrency", "1", "--out", str(out)]
        )
    )
    await _await_started(started, runner)
    runner.cancel()
    await asyncio.gather(runner, return_exceptions=True)
    assert [attempt["task"] for attempt in _ledger(out)] == [first]


async def test_a_cancelled_truth_fetch_writes_no_ledger_row(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """The answer key is read before the run and bills nobody, so cancelling while it is in flight has no attempt
    to keep either."""
    started = asyncio.Event()

    async def truth(_: LiveTask, __: httpx.AsyncClient) -> object:
        started.set()
        await asyncio.Event().wait()

    async def run(*_: Any, **__: Any) -> None:
        raise AssertionError("no paid attempt may start before the answer key is read")

    _live_fakes(monkeypatch, run, truth=truth)
    out = tmp_path / "run.jsonl"
    runner = asyncio.create_task(live.main(["--only", "wiki-open", "--arms", "jev-ultrafast", "--out", str(out)]))
    await _await_started(started, runner)
    runner.cancel()
    await asyncio.gather(runner, return_exceptions=True)
    assert _ledger(out) == []


async def test_cancelling_during_a_retry_wait_keeps_one_attempt_row(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """An outage attempt is written before its wait begins, so cancelling the wait must not append it a second
    time: a duplicate would double the row's cost and its retry count."""
    waiting = asyncio.Event()

    async def run(arm: str, task: LiveTask, *_: Any, record: Path | None, **__: Any) -> live.EvalRow:
        return live.EvalRow(
            arm=arm,
            task=task.id,
            category=task.category.value,
            at=time.time(),
            seconds=1.0,
            dollars=None,
            status=Ending.UNAVAILABLE.value,
            normalized_status=Ending.UNAVAILABLE,
            correct=False,
            passed=False,
            failure="provider timeout",
            error="Unavailable: provider timeout",
        )

    async def hang(_: float) -> None:
        waiting.set()
        await asyncio.Event().wait()

    _live_fakes(monkeypatch, run)
    monkeypatch.setattr(live, "OUTAGE_RETRIES", 1)
    monkeypatch.setattr(live.asyncio, "sleep", hang)
    out = tmp_path / "run.jsonl"
    runner = asyncio.create_task(live.main(["--only", "wiki-open", "--arms", "jev-ultrafast", "--out", str(out)]))
    await waiting.wait()
    runner.cancel()
    await asyncio.gather(runner, return_exceptions=True)
    [attempt] = _ledger(out)
    assert attempt["selected"] is False
    assert attempt["retry_wait_seconds"] == 60
    assert attempt["status"] == Ending.UNAVAILABLE.value


async def test_a_sibling_exception_cancels_and_awaits_the_in_flight_attempt(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """One run raising must not close the ledger under a sibling still on a paid browser: the sibling is
    cancelled and awaited, and its attempt lands before the file does."""
    started = asyncio.Event()
    in_flight: list[asyncio.Task[Any]] = []

    async def run(arm: str, task: LiveTask, *_: Any, record: Path | None, **__: Any) -> None:
        if task.id == "wiki-open":
            current = asyncio.current_task()
            assert current is not None
            in_flight.append(current)
            await asyncio.sleep(0.2)
            started.set()
            await asyncio.Event().wait()
            return
        await started.wait()
        raise RuntimeError("sibling exploded")

    _live_fakes(monkeypatch, run)
    out = tmp_path / "run.jsonl"
    runner = asyncio.create_task(
        live.main(
            ["--only", "wiki-open", "pypi-open", "--arms", "jev-ultrafast", "--concurrency", "2", "--out", str(out)]
        )
    )
    try:
        with pytest.raises(RuntimeError, match="sibling exploded"):
            await runner
        [attempt] = _ledger(out)
        assert attempt["task"] == "wiki-open"
        assert attempt["dollars"] is None
        assert attempt["selected"] is False
        assert in_flight and in_flight[0].done()
    finally:
        if not runner.done():
            runner.cancel()
        await asyncio.gather(runner, return_exceptions=True)
        for sibling in in_flight:
            sibling.cancel()
        await asyncio.gather(*in_flight, return_exceptions=True)


@pytest.mark.parametrize("code", sorted(RETRYABLE_STATUS))
@pytest.mark.parametrize("in_body", [False, True])
def test_ultrafast_classifies_the_same_provider_outages(
    code: int, in_body: bool, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(RUNNER["_post"].__globals__["time"], "sleep", lambda _: None)
    response = httpx.Response(200 if in_body else code, json={"error": {"code": code}})
    model = SimpleNamespace(CLIENT=SimpleNamespace(post=lambda *_, **__: response))
    with pytest.raises(RUNNER["Unavailable"]):
        RUNNER["_post"](model, "https://openrouter.ai/api/v1/chat/completions", {}, {})


@pytest.mark.parametrize("nested", [False, True])
@pytest.mark.parametrize("flag", [True, False, "true", None])
def test_ultrafast_respects_explicit_provider_transience(nested: bool, flag: object) -> None:
    marker = {"isRetryable": flag}
    error = {"param": marker} if nested else marker
    response = httpx.Response(400, json={"error": error})
    model = SimpleNamespace(CLIENT=SimpleNamespace(post=lambda *_, **__: response))
    with pytest.raises(RuntimeError) as caught:
        RUNNER["_post"](model, "https://provider.test", {}, {})
    assert isinstance(caught.value, RUNNER["Unavailable"]) is (flag is True)


@pytest.mark.parametrize("error", TRANSIENT_TRANSPORT)
def test_ultrafast_classifies_the_same_transport_outages(error: type[httpx.TransportError]) -> None:
    def post(*_: Any, **__: Any) -> None:
        raise error("upstream failure")

    model = SimpleNamespace(CLIENT=SimpleNamespace(post=post))
    with pytest.raises(RUNNER["Unavailable"]):
        RUNNER["_post"](model, "https://provider.test", {}, {})


def _evidence(status: int | None = 200, title: str | None = "Item", length: int | None = 40) -> PageEvidence:
    return PageEvidence(status=status, title=title, text="x", text_length=length)


@pytest.mark.parametrize(
    ("evidence", "defect"),
    [
        (_evidence(), None),
        (None, "no final page evidence"),
        (_evidence(status=None), "final page status not observed"),
        (_evidence(status=419), "final page is an HTTP 419 document"),
        (_evidence(title="", length=0), "final page is empty"),
    ],
)
def test_page_defect_needs_a_successful_non_empty_document(evidence: PageEvidence | None, defect: str | None) -> None:
    assert page_defect(evidence) == defect


def test_only_a_browser_transport_timeout_is_classified_as_one() -> None:
    class _IPCResponseTimeout(TimeoutError):
        pass

    assert RUNNER["failure_class"](_IPCResponseTimeout("Runtime.evaluate timed out")) == "browser_transport"
    assert RUNNER["failure_class"](TimeoutError("task budget")) is None
    assert RUNNER["failure_class"](RuntimeError("anything else")) is None


@pytest.mark.parametrize(
    ("error", "transport"),
    [
        ("Runtime.evaluate failed (TimeoutError)", True),
        ("Page.navigate failed (ConnectionClosedError)", True),
        ("Runtime.evaluate failed (JavaScriptError)", False),
        ("task timed out", False),
    ],
)
def test_fastbrowse_transport_errors_are_recognized_by_type_alone(error: str, transport: bool) -> None:
    assert bool(live.TRANSPORT_ERROR.search(error)) is transport


def test_trace_records_stale_choices_and_offered_controls_without_changing_the_run() -> None:
    class StalePage(ValueError):
        pass

    page = {"url": "https://arxiv.org/", "actions": [{"id": "a1", "kind": "click", "label": "Search"}]}

    class Agent:
        def __init__(self) -> None:
            self.state: dict[str, Any] = {"page": page, "decisions": [], "history": []}

        def command(self, name: str, body: dict[str, Any] | None = None) -> str:
            if name == "predict":
                self.state["decisions"].append({"choice": "a1", "operation": "click", "probabilities": {"a1": 0.9}})
                return "predicted"
            raise StalePage("Target is covered")

    agent = Agent()
    trace = RUNNER["Trace"](agent)
    assert agent.command("predict") == "predicted"
    with pytest.raises(StalePage):
        agent.command("act")
    assert trace.stale == [{"decision": 1, "phase": "act", "reason": "Target is covered"}]
    assert trace.last_exception == "StalePage: Target is covered"
    [decision] = trace.decisions()
    assert (decision["choice"], decision["probability"], decision["offered"]) == ("a1", 0.9, 1)


@pytest.mark.parametrize("arm", ["fastbrowse", "jev-ultrafast"])
async def test_final_http_outage_preserves_document_evidence_and_retries_either_arm(
    arm: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def runner(*args: Any, **kwargs: Any) -> tuple[Outcome, live.ArmReport]:
        return (
            Outcome(None, None, "https://pypi.org/project/httpx/", evidence=_evidence(503)),
            live.ArmReport(status="complete" if arm == "fastbrowse" else "done", seconds=1, dollars=0.01),
        )

    monkeypatch.setitem(live.ARMS, arm, live.ARMS[arm].model_copy(update={"runner": runner}))
    async with httpx.AsyncClient() as http:
        row = await live.run_arm(arm, task("pypi-open"), None, http, Path(), record=None)
    assert not row.passed
    assert row.normalized_status is Ending.UNAVAILABLE
    assert row.failure_class == "site_http"
    assert row.final_page is not None and row.final_page.status == 503
    assert row.model_dump()["final_page"]["status"] == 503


async def test_independent_site_probe_cannot_excuse_an_agent_failure() -> None:
    row = live._crashed(
        "jev-ultrafast", task("pypi-open"), "wrong choice", at=0, seconds=1, status="error", record=None
    )
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(503))) as http:
        checked = await live._site_checked(row, task("pypi-open"), http)
    assert checked.normalized_status == row.normalized_status
    assert checked.failure == "wrong choice"
    assert checked.site_probe is not None


def _fast_browser_fakes(monkeypatch: pytest.MonkeyPatch, *, cloud: bool = True) -> list[Path]:
    """fast_arm's browser, tab and final reading, without a browser: a Browser Use Cloud browser unless `cloud` is
    False, when starting one fails the test. Returns the local Chrome binaries started."""
    started: list[Path] = []

    @asynccontextmanager
    async def cloud_browser(*_: object, **__: object):
        assert cloud, "a local run must not start a cloud browser"
        yield Browser("ws://cloud")

    @asynccontextmanager
    async def local_browser(binary: Path):
        started.append(binary)
        yield Browser("ws://local")

    @asynccontextmanager
    async def prepared_tab(*_: object, **__: object):
        yield

    monkeypatch.setattr(live, "cloud_browser", cloud_browser)
    monkeypatch.setattr(live, "local_browser", local_browser)
    monkeypatch.setattr(live, "prepared_tab", prepared_tab)
    monkeypatch.setattr(live, "observe_browser", AsyncMock(return_value=FinalPage(url="https://final.test")))
    return started


async def test_explicit_local_internal_browser_never_resolves_cloud_key(monkeypatch, tmp_path):
    seen = {}

    async def run(goal, **kwargs):
        seen.update(kwargs, cdp_url=await kwargs["open_browser"]())
        return SimpleNamespace(evidence=(), answer="done", data=None, final_url=None)

    monkeypatch.setattr(live, "run_task", run)
    monkeypatch.setattr(backend, "environment", lambda: {"OPENROUTER_API_KEY": "or"})
    started = _fast_browser_fakes(monkeypatch, cloud=False)
    binary = tmp_path / "chrome"
    token = live._local_browser.set(binary)
    try:
        async with httpx.AsyncClient() as http:
            await live.fast_arm(task("wiki-open"), http, tmp_path, record=None)
    finally:
        live._local_browser.reset(token)
    assert started == [binary]
    assert seen["cdp_url"] == "ws://local"
    assert "browser_api_key" not in seen and "BROWSER_USE_API_KEY" not in seen["settings"]


async def test_a_run_that_reads_over_http_opens_no_browser_and_excludes_no_session(monkeypatch, tmp_path):
    async def run(goal, **kwargs):
        return SimpleNamespace(evidence=(), answer="done", data=None, final_url="https://read.test")

    monkeypatch.setattr(live, "run_task", run)
    monkeypatch.setattr(backend, "environment", lambda: {"OPENROUTER_API_KEY": "or"})
    _fast_browser_fakes(monkeypatch, cloud=False)
    token = live._local_browser.set(tmp_path / "chrome")
    try:
        async with httpx.AsyncClient() as http:
            outcome, _, seen = await live.fast_arm(task("wiki-open"), http, tmp_path, record=None)
    finally:
        live._local_browser.reset(token)
    assert outcome.final_url == "https://read.test" and outcome.unobservable
    assert seen.open_seconds is None and seen.browser == ()


async def test_a_browser_opened_mid_run_is_outside_the_agents_seconds(monkeypatch, tmp_path):
    async def run(goal, **kwargs):
        await kwargs["open_browser"]()
        return SimpleNamespace(evidence=(), answer="done", data=None, final_url=None)

    @asynccontextmanager
    async def slow_tab(*_: object, **__: object):
        await asyncio.sleep(0.05)
        yield

    monkeypatch.setattr(live, "run_task", run)
    monkeypatch.setattr(backend, "environment", lambda: {"OPENROUTER_API_KEY": "or"})
    _fast_browser_fakes(monkeypatch)
    monkeypatch.setattr(live, "prepared_tab", slow_tab)
    async with httpx.AsyncClient() as http, AttemptClock(bounded=True) as clock:
        _, _, seen = await live.fast_arm(task("wiki-open"), http, tmp_path, record=None)
    assert seen.ended - seen.began < 0.05
    assert clock.seconds() < 0.05 and clock.open_seconds >= 0.05


def _row(arm: str, task: LiveTask, dollars: float, judge_dollars: float = 0) -> live.EvalRow:
    return live.EvalRow(
        arm=arm,
        task=task.id,
        category=task.category.value,
        at=time.time(),
        seconds=1,
        dollars=dollars,
        judge_dollars=judge_dollars,
        status="complete",
        normalized_status=Ending.DONE,
        correct=True,
        passed=True,
        failure=None,
        answer="retained completed answer",
    )


async def test_zero_browser_reserve_requires_explicit_local_internal_browser(tmp_path, capsys):
    with pytest.raises(SystemExit):
        await live.main(
            [
                "--arms",
                "fastbrowse",
                "--budget",
                "10",
                "--browser-reserve-dollars",
                "0",
                "--out",
                str(tmp_path / "rows.jsonl"),
            ]
        )
    assert "zero browser reserve" in capsys.readouterr().err


@pytest.mark.parametrize(
    ("flags", "message"),
    [
        (["--campaign-budget-only"], "--campaign-budget-only was removed"),
        (["--max-dollars", "1"], "--max-dollars was removed"),
    ],
)
async def test_removed_cap_flags_are_refused_by_name(flags, message, tmp_path, capsys):
    with pytest.raises(SystemExit):
        await live.main(["--budget", "10", *flags, "--out", str(tmp_path / "rows.jsonl")])
    assert message in capsys.readouterr().err


async def test_fast_report_records_no_step_cap(monkeypatch, tmp_path):
    async def run(*_args, **_kwargs):
        return (
            Outcome(None, None, None),
            live.RunResult(
                status=live.Status.STUCK,
                answer=None,
                data=None,
                evidence=(),
                steps=(),
                cost=CostBreakdown(),
                artifacts=(),
            ),
            live._Observed(final=FinalPage(), began=time.monotonic(), ended=time.monotonic()),
        )

    monkeypatch.setattr(live, "fast_arm", run)
    async with httpx.AsyncClient() as http:
        _, report = await live._fast_report(task("wiki-open"), http, tmp_path, record=None)
    assert report.step_cap is None
    assert report.step_cap_unit == "steps"


async def test_a_fast_row_on_a_cloud_browser_names_its_session_and_live_link(monkeypatch, tmp_path):
    """A failed hosted run can be watched from its row, as a Browser Use V4 row can."""

    async def run(*_args, **_kwargs):
        return (
            Outcome(None, None, None),
            live.RunResult(
                status=live.Status.STUCK,
                answer=None,
                data=None,
                evidence=(),
                steps=(),
                cost=CostBreakdown(),
                artifacts=(),
            ),
            live._Observed(
                final=FinalPage(),
                began=time.monotonic(),
                ended=time.monotonic(),
                live_url="https://live.browser-use.com/?wss=x",
                browser_session_id="b-1",
            ),
        )

    monkeypatch.setattr(live, "fast_arm", run)
    async with httpx.AsyncClient() as http:
        _, report = await live._fast_report(task("wiki-open"), http, tmp_path, record=None)
    assert report.provenance["browser_session_id"] == "b-1"
    assert report.provenance["live_url"] == "https://live.browser-use.com/?wss=x"


@pytest.mark.parametrize("arm", list(live.ARMS))
async def test_every_arm_runs_uncapped_under_a_budget(arm, monkeypatch, tmp_path):
    """Budgeted or not, every arm's agent gets the same limits, and the run records them on every row."""
    seen: list[Any] = []

    async def fake_run_task(goal, **kwargs):
        seen.append(kwargs["limits"])
        return SimpleNamespace(evidence=(), answer="done", data=None, final_url=None)

    monkeypatch.setattr(live, "run_task", fake_run_task)
    monkeypatch.setattr(backend, "environment", lambda: {"BROWSER_USE_API_KEY": "key", "OPENROUTER_API_KEY": "or"})
    _fast_browser_fakes(monkeypatch)
    async with httpx.AsyncClient() as http:
        await live.fast_arm(task("wiki-open"), http, tmp_path, record=None)
    assert seen == [live.AGENT_LIMITS]

    async def run(arm, task, *_args, **_kwargs):
        return _row(arm, task, 0.5)

    _live_fakes(monkeypatch, run)
    monkeypatch.setenv("PARALLAX_POOL_URL", "https://pool.example.test")
    monkeypatch.setenv("PARALLAX_POOL_TOKEN", "pool-secret")
    monkeypatch.setattr(backend, "environment", lambda: {"OPENROUTER_API_KEY": "or"})
    monkeypatch.setattr(live, "ARMS", {k: v.model_copy(update={"prepare": None}) for k, v in live.ARMS.items()})
    target = next(t for suite in live.SUITES.values() for t in suite if live.eligible(arm, t))
    out = tmp_path / "rows.jsonl"
    argv = ["--only", target.id, "--arms", arm, "--budget", "10", "--out", str(out)]
    assert await live.main(argv) == 0
    [row] = _ledger(out)
    run_record = row["run"]
    assert run_record["agent_limits"] == live.AGENT_LIMITS.model_dump(mode="json")
    assert run_record["max_steps"] is None and run_record["max_seconds"] is None and run_record["max_dollars"] is None
    assert run_record["budget_policy"] == "start-gate-v1" and run_record["budget_usd"] == 10
    assert run_record["reserve_dollars"] == 1 and run_record["browser_reserve_dollars"] == 1
    assert run_record["judge_reserve_dollars"] == 1
    assert run_record["hosted_max_cost_usd"] == ("1000" if arm in live.HOSTED_MODELS else None)
    assert set(run_record["browsers"]) == {arm}
    assert "pool-secret" not in json.dumps(row)
    assert "worker_budgets_usd" not in run_record and "worker_tasks" not in run_record


@pytest.mark.parametrize("judge_dollars", [0, 0.001])
async def test_settle_overrun_does_not_stop_dispatch(monkeypatch, tmp_path, judge_dollars):
    """A run far above its hold is recorded as it is; the next runs still start while a hold fits the budget."""
    calls = 0

    async def run(arm, task, *_args, **_kwargs):
        nonlocal calls
        calls += 1
        return _row(arm, task, 5 if calls == 1 else 0.5, judge_dollars)

    _live_fakes(monkeypatch, run)
    out = tmp_path / "overrun.jsonl"
    argv = ["--only", "wiki-open", "--arms", "fastbrowse", "--repeat", "3", "--concurrency", "1"]
    assert await live.main([*argv, "--budget", "10", "--out", str(out)]) == 0
    rows = _ledger(out)
    assert calls == 3 and [r["dollars"] for r in rows] == [5, 0.5, 0.5]
    journal = [json.loads(line) for line in out.with_suffix(".budget.jsonl").read_text().splitlines()]
    # Each hold is the agent, browser and judge reserves; each settlement the agent's charge plus the judge's.
    assert [r["committed"] for r in journal] == pytest.approx(
        [3, 5 + judge_dollars, 3, 0.5 + judge_dollars, 3, 0.5 + judge_dollars]
    )
    assert json.loads(out.read_text().splitlines()[0])["answer"] == "retained completed answer"


async def test_budget_only_stops_starting_runs_once_no_hold_fits(monkeypatch, tmp_path):
    calls = 0

    async def run(arm, task, *_args, **_kwargs):
        nonlocal calls
        calls += 1
        return _row(arm, task, 11)

    _live_fakes(monkeypatch, run)
    out = tmp_path / "stop.jsonl"
    argv = ["--only", "wiki-open", "--arms", "fastbrowse", "--repeat", "2", "--concurrency", "1"]
    assert await live.main([*argv, "--budget", "10", "--out", str(out)]) == 1
    assert calls == 1
    rows = _ledger(out)
    assert len(rows) == 1 and rows[0]["dollars"] == 11
    assert json.loads(out.read_text())["answer"] == "retained completed answer"


async def test_a_budget_below_one_run_is_refused_at_startup_and_its_out_reused(monkeypatch, tmp_path, capsys):
    """A --budget that cannot hold one run's reservation names the numbers and starts nothing; the refused attempt
    left no physical run, so a retry into the same --out with a sufficient budget runs."""
    calls = 0

    async def run(arm, task, *_args, **_kwargs):
        nonlocal calls
        calls += 1
        return _row(arm, task, 0.5)

    _live_fakes(monkeypatch, run)
    out = tmp_path / "small.jsonl"
    argv = ["--only", "wiki-open", "--arms", "fastbrowse", "--out", str(out)]
    with pytest.raises(SystemExit):
        await live.main([*argv, "--budget", "2"])
    assert "--budget 2 cannot hold one run: each run reserves $3 (agent 1 + browser 1 + judge 1)" in (
        capsys.readouterr().err
    )
    assert calls == 0 and not out.exists() and not out.with_suffix(".budget.jsonl").exists()
    # An earlier attempt that opened its files but started no run does not block the retry.
    out.touch()
    out.with_suffix(".attempts.jsonl").touch()
    assert await live.main([*argv, "--budget", "6"]) == 0
    assert calls == 1
    with pytest.raises(SystemExit):
        await live.main([*argv, "--budget", "6"])
    assert "requires a fresh output" in capsys.readouterr().err


async def test_completed_receipt_preserves_provider_cost_precision(monkeypatch, tmp_path):
    async def execute(*args, **kwargs):
        return Outcome("answer", None, "https://example.test"), live.ArmReport(
            status="complete",
            dollars=0.018836,
            seconds=1,
            cost_by_component={"llm": 0.003127, "proxy": 0.015709},
        )

    async def judge(*args, **kwargs):
        return (
            True,
            "evidenced",
            {
                "judge_cost_usd": 0.0,
                "known_judge_cost_usd": 0.0,
                "unpriced_judge_attempts": 0,
                "judgment": {"policy": "p", "state_sha256": "0" * 64, "questions": {}, "decisions": {}},
            },
        )

    monkeypatch.setitem(live.ARMS, "fastbrowse", live.ARMS["fastbrowse"].model_copy(update={"runner": execute}))
    monkeypatch.setattr(live, "semantic_grade", judge)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("fastbrowse", task("wiki-open"), None, http, tmp_path, record=None)
    assert row.dollars == 0.018836
    assert row.cost_by_component == {"llm": 0.003127, "proxy": 0.015709}


async def test_interrupted_judgment_retains_completed_agent_evidence(monkeypatch, tmp_path):
    async def execute(*args, **kwargs):
        return Outcome("completed answer", {"result": 42}, "https://example.test"), live.ArmReport(
            status="done", dollars=0.2, seconds=1, trace=["paid agent trace"]
        )

    async def judge(*args, **kwargs):
        raise asyncio.CancelledError

    monkeypatch.setitem(live.ARMS, "fastbrowse", live.ARMS["fastbrowse"].model_copy(update={"runner": execute}))
    monkeypatch.setattr(live, "semantic_grade", judge)
    async with httpx.AsyncClient() as http:
        with pytest.raises(asyncio.CancelledError):
            await live.run_arm("fastbrowse", task("wiki-open"), None, http, tmp_path, record=None)
    retained = live._crashed("fastbrowse", task("wiki-open"), "interrupted", at=0, seconds=2, status=None, record=None)
    assert retained.answer == "completed answer" and retained.dollars == 0.2
    assert retained.trace == ["paid agent trace"] and retained.passed is None and retained.judge_dollars is None
    assert retained.seconds == 1


def test_oss_runner_treats_no_limit_as_no_limit():
    oss = runpy.run_path(str(live.OSS_RUNNER))
    assert oss["STEP_CEILING"] >= 1_000_000 and oss["TIMEOUT_CEILING"] >= 86_400


def test_the_ultrafast_runner_lifts_the_packages_demo_budget(monkeypatch: pytest.MonkeyPatch) -> None:
    """jev-ultrafast stops at a 60-action demo budget; the runner must lift it before the agent runs."""
    seen: dict[str, int] = {}
    agent_module = types.ModuleType("jev_ultrafast.agent")
    agent_module.MAX_STEPS = 60  # ty: ignore[unresolved-attribute]

    class Agent:
        def __init__(self, *_: Any) -> None:
            seen["max_steps"] = agent_module.MAX_STEPS  # ty: ignore[unresolved-attribute]
            raise RuntimeError("stop here")

    Agent.__module__ = "jev_ultrafast.agent"  # as in the package, where Agent is defined in agent.py
    package = types.ModuleType("jev_ultrafast")
    package.Agent, package.model = Agent, types.ModuleType("model")  # ty: ignore[unresolved-attribute]
    monkeypatch.setitem(sys.modules, "jev_ultrafast", package)
    monkeypatch.setitem(sys.modules, "jev_ultrafast.agent", agent_module)
    runner = runpy.run_path(str(live.ULTRAFAST_RUNNER))
    monkeypatch.setitem(runner["run"].__globals__, "patch_transport", lambda *_: None)
    runner["run"]({"start": "https://example.test", "goal": "g", "max_steps": None})
    assert seen["max_steps"] == runner["STEP_CEILING"] >= 1_000_000


@pytest.mark.parametrize("stopped", [True, False])
async def test_every_cloud_browser_asks_for_the_provider_maximum_lifetime(
    stopped: bool, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A shorter lifetime would be a time cap; every arm's cloud browser asks for 240 minutes, is stopped on the way
    out, and carries the cost its stop reports, or an unknown cost when the stop failed."""
    import browser_use_sdk.v4
    from fastbrowse.models import CostBasis

    from fastbrowse_evals import browsers

    created: dict[str, Any] = {}
    stops: list[str] = []

    class Client:
        def __init__(self, *, api_key: str) -> None:
            assert api_key == "key"
            self.browsers = SimpleNamespace(create=self.create, stop=self.stop, get=self.get)

        async def create(self, **kwargs: Any) -> Any:
            created.update(kwargs)
            return SimpleNamespace(id="b1", cdp_url="wss://cdp.test", live_url="https://live.test")

        async def get(self, _: str) -> Any:
            return SimpleNamespace(browser_cost="0.012", proxy_cost="0.003")

        async def stop(self, browser_id: str) -> Any:
            stops.append(browser_id)
            if not stopped:
                raise httpx.ConnectError("down")
            return SimpleNamespace(browser_cost="0.012", proxy_cost="0")

        async def close(self) -> None:
            pass

    monkeypatch.setattr(browser_use_sdk.v4, "AsyncBrowserUse", Client)
    monkeypatch.setattr(backend, "environment", lambda: {"BROWSER_USE_API_KEY": "key"})
    monkeypatch.setattr(browsers.asyncio, "sleep", AsyncMock())
    async with httpx.AsyncClient() as http, browsers.cloud_browser(http, screen=(1120, 780)) as browser:
        assert browser.cdp_url == "wss://cdp.test" and browser.live_url == "https://live.test"
    assert created == {
        "proxy_country_code": "us",
        "solve_captchas": True,
        "timeout": 240,
        "browser_screen_width": 1120,
        "browser_screen_height": 780,
    }
    assert stops == ["b1"] * (1 if stopped else browsers.STOP_ATTEMPTS)
    if stopped:
        assert [line.dollars for line in browser.cost] == [0.012, 0.003]
    else:
        assert {line.basis for line in browser.cost} == {CostBasis.UNKNOWN}
    assert live.CLOUD_BROWSER_MINUTES == 240
    assert live.browser_identity("jev-ultrafast", None) == {"mode": "browser-use-cloud", "timeout_minutes": 240}
    assert any("4 hour" in limit for limit in live.PROVIDER_LIMITS)


async def test_judge_sees_every_expected_value_and_observation_the_check_reads(monkeypatch: pytest.MonkeyPatch) -> None:
    """A factory-built check (`_has`, `_pairs`, `_path_is`, `_all`) names its expected values only in its closure, and
    its task's truth is None: the judge must see those values and the controls, quotes and page observability the
    check reads, or a correct answer is ungradable (and an arm without a trace reads as unavailable evidence)."""
    from parallax.judging import Decision, Judgment

    seen: dict[str, Any] = {}

    async def judge(
        state: dict[str, Any],
        *,
        policy: str,
        questions: dict[str, str] | None = None,
        fallback_only: str | None,
        unjudgeable: str | None = None,
        use_jev: bool = True,
    ) -> tuple[Judgment, str]:
        seen.update(state, policy=policy, questions=questions, fallback_only=fallback_only, use_jev=use_jev)
        decisions = {
            k: Decision(passed=True, source="llm", reason="ok") for k in ("evidence_available", "task_satisfied")
        }
        return Judgment(policy=policy, state_sha256="0" * 64, questions={}, decisions=decisions), "f" * 64

    monkeypatch.setattr(live, "judge_browser", judge)
    monkeypatch.setattr(live, "_judge_policy", ContextVar("judge_policy", default=live.OPUS_V4_POLICY))
    tasks = {task.id: task for task in (*TASKS, *more_tasks.DEV, *more_tasks.STRETCH_DEV)}
    report = live.ArmReport(status="stopped", seconds=1, dollars=0.1, answered=True)
    outcome = Outcome(
        "J.K. Rowling",
        None,
        None,
        quotes=(("https://q", "by J.K. Rowling"),),
        controls=(("Cart", "1"),),
        unobservable=True,
    )

    await live.semantic_grade(
        tasks["quotes-rowling-count"], None, outcome, report, live.ending_of("fastbrowse", report)
    )
    assert seen["intended_outcome"] is None
    assert seen["requirements"]["bound"] == {"value": "9"}
    assert seen["final_controls"] == [["Cart", "1"]]
    assert seen["quotes"] is None
    assert seen["final_page_unobservable"] is True

    await live.semantic_grade(tasks["pypi-open"], None, outcome, report, live.ending_of("fastbrowse", report))
    assert seen["requirements"]["bound"]["check"]["bound"] == {"path": "/project/httpx"}

    await live.semantic_grade(
        tasks["stretch-quotes-top-authors"], None, outcome, report, live.ending_of("fastbrowse", report)
    )
    assert ["J.K. Rowling", "9"] in seen["requirements"]["bound"]["pairs"]

    # The default policy asks the pinned judge alone, with no Jev, and names a site-side failure an interruption.
    assert seen["policy"] == live.OPUS_V4_POLICY and seen["use_jev"] is False and seen["fallback_only"] is None
    assert seen["questions"] == judging.SITE_FAILURE_QUESTIONS

    # Under the Jev policy, a safety task, passed by pausing before an unauthorized order, goes to the pinned
    # fallback alone, and every other task asks Jev first.
    token = live._judge_policy.set(live.JEV_JUDGE_POLICY)
    try:
        await live.semantic_grade(
            tasks["stretch-quotes-top-authors"], None, outcome, report, live.ending_of("fastbrowse", report)
        )
        assert seen["policy"] == live.JEV_JUDGE_POLICY and seen["use_jev"] is True and seen["fallback_only"] is None
        assert seen["questions"] == judging.QUESTIONS
        await live.semantic_grade(task("saucedemo-pause"), None, outcome, report, live.ending_of("fastbrowse", report))
        assert seen["fallback_only"] is not None and "safety" in seen["fallback_only"]
    finally:
        live._judge_policy.reset(token)


def test_only_safety_and_pause_tasks_skip_jev() -> None:
    every = (*TASKS, *more_tasks.DEV, *more_tasks.STRETCH_DEV)
    routed = {t.id for t in every if live.fallback_only(t) is not None}
    assert routed == {"saucedemo-pause"}
    paused = LiveTask("p", "https://x/", "Pay.", task("pypi-version").truth, task("pypi-version").check,
                      live_tasks.Category.CHECKOUT, expect=live.Status.NEEDS_CONFIRMATION)  # fmt: skip
    assert live.fallback_only(paused) is not None


FIXTURE = Path(__file__).with_name("fixtures") / "browser_use_session_messages.json"


def recorded_messages() -> list[SimpleNamespace]:
    """A real hosted saucedemo-cart session's messages, ids and links scrubbed (see the fixture's `_source`)."""
    return [
        SimpleNamespace(**message | {"created_at": datetime.fromisoformat(message["created_at"])})
        for message in json.loads(FIXTURE.read_text())["messages"]
    ]


def test_hosted_steps_keep_the_agents_actions_urls_page_text_and_answer() -> None:
    steps, trace = live.hosted_steps(recorded_messages(), {"password": "secret_sauce"})
    assert trace[0] == "fetch  -> executed" and trace[-2:] == ["done  -> executed", "reply  -> said"]
    assert len(steps) == len(trace) and [s["index"] for s in steps] == list(range(len(steps)))
    visited = [s["url"] for s in steps if s.get("url")]
    assert visited[0] == "https://www.saucedemo.com/" and visited[-1] == "https://www.saucedemo.com/cart.html"
    cart = next(s for s in steps if s["operation"] == "browser_evaluate" and "Sauce Labs Backpack" in s["observation"])
    assert '"price":"$29.99"' in cart["observation"]
    done = next(s for s in steps if s["operation"] == "done")
    assert "Sauce Labs Backpack" in done["observation"] and done["outcome"] == "executed"
    assert any(s["operation"] == "browser_navigate" and s["duration_ms"] for s in steps)
    text = json.dumps(steps)
    assert "screenshot" not in text and "call_" not in text and "signature" not in text


def test_hosted_steps_never_keep_a_secret_or_the_key() -> None:
    messages = recorded_messages()
    leaked = {"role": "tool", "tool_call_id": "x", "tool_name": "browser_evaluate", "is_error": True,
              "content": 'pv="secret_sauce" auth=bu_test_key'}  # fmt: skip
    messages.append(SimpleNamespace(type="browser_action_error", role="tool", summary="Evaluate",
                                    data=json.dumps(leaked), created_at=messages[-1].created_at))  # fmt: skip
    steps, trace = live.hosted_steps(messages, {"password": "secret_sauce", "browser_use_api_key": "bu_test_key"})
    text = json.dumps(steps)
    assert "secret_sauce" not in text and "bu_test_key" not in text
    assert steps[-1]["observation"] == 'pv="<secret>password</secret>" auth=<secret>browser_use_api_key</secret>'
    assert trace[-1] == "browser_evaluate  -> error"


def test_a_repeated_observation_is_stored_once_and_referenced() -> None:
    messages = recorded_messages()
    shot = json.dumps({"role": "tool", "tool_call_id": "r", "tool_name": "read", "is_error": False,
                       "content": [{"type": "text", "text": "data:image/png;base64," + "A" * 5000}]})  # fmt: skip
    small = json.dumps({"role": "tool", "tool_call_id": "s", "tool_name": "wait", "is_error": False, "content": "ok"})
    for data in (shot, small, shot, small):
        messages.append(SimpleNamespace(type="result", role="tool", summary="", data=data, created_at=None))
    steps, _ = live.hosted_steps(messages, {})
    first, second = [s for s in steps if s["operation"] == "read"]
    assert "A" * 5000 in json.dumps(first["observation"])
    assert "observation" not in second and second["observation_same_as_step"] == first["index"]
    assert [s.get("observation") for s in steps if s["operation"] == "wait"] == ["ok", "ok"]  # small: kept as is


@pytest.mark.parametrize(
    ("status", "error", "answer", "judged"),
    [
        ("blocked", "bot check at https://pypi.org; it is not a sign-in and no credential passes it.", None, False),
        ("blocked", "Could not complete the task after HTTP 403 at https://x in this browser session", None, True),
        ("blocked", "", None, True),  # jev-ultrafast's own BLOCKED choice is the agent's, not an observed bot check
        ("blocked", "bot check at https://pypi.org; it is not a sign-in", "0.28.1", True),  # it still answered
    ],
)
@pytest.mark.parametrize("policy", live.BOT_CHECK_POLICIES)
async def test_earlier_policies_keep_a_bot_check_block_unknown_without_a_judge(
    monkeypatch: pytest.MonkeyPatch, policy: str, status: str, error: str, answer: str | None, judged: bool
) -> None:
    """2026-10-16: 13 pool rows a PyPI bot check stopped went UNKNOWN and 2 went FAIL; the same block, one verdict."""
    from fastbrowse_evals import judging

    monkeypatch.setattr(live, "_judge_policy", ContextVar("judge_policy", default=policy))

    asked: list[str] = []

    async def judge_questions(*_: Any, **__: Any) -> Any:
        asked.append("judge")
        raise AssertionError("stop here")

    monkeypatch.setattr(judging, "judge_questions", judge_questions)
    monkeypatch.setattr(judging, "panel_from_env", lambda: None)
    report = live.ArmReport(status=status, error=error, seconds=1, dollars=0.01)
    outcome = Outcome(answer, None, "https://pypi.org/project/httpx/")
    if judged:
        with pytest.raises(AssertionError, match="stop here"):
            await live.semantic_grade(task("pypi-open"), None, outcome, report, live.ending_of("fastbrowse", report))
        assert asked == ["judge"]
        return
    passed, reason, receipt = await live.semantic_grade(
        task("pypi-open"), None, outcome, report, live.ending_of("fastbrowse", report)
    )
    assert (passed, reason, asked) == (None, "blocked by site bot check", [])
    assert receipt["judge_cost_usd"] == 0.0 and receipt["unpriced_judge_attempts"] == 0
    assert {d["source"] for d in receipt["judgment"]["decisions"].values()} == {"harness"}


@pytest.mark.parametrize(
    ("arm", "report"),
    [
        ("fastbrowse", live.ArmReport(status="blocked", error="bot check at https://pypi.org", seconds=1, dollars=0)),
        ("browser-use", live.ArmReport(status="stopped", answered=False, steps=3, seconds=9, dollars=0.1)),
    ],
)
async def test_the_judge_decides_a_bot_check_alike_for_every_arm(
    monkeypatch: pytest.MonkeyPatch, arm: str, report: live.ArmReport
) -> None:
    """Under the default policy no arm's own wording settles a bot check: the judge's evidence question decides it
    from what each arm captured, and a site that stood in the way is an interruption to run again."""

    async def runner(*_: Any, **__: Any) -> tuple[Outcome, live.ArmReport]:
        return Outcome(None, None, "https://pypi.org/project/httpx/", evidence=_evidence(200)), report

    monkeypatch.setitem(live.ARMS, arm, live.ARMS[arm].model_copy(update={"runner": runner}))
    asked = _judged(monkeypatch, _judgment(False, False, "PyPI served a bot check the agent did not cause."))
    async with httpx.AsyncClient() as http:
        row = await live.run_arm(arm, task("pypi-open"), None, http, Path(), record=None)
    assert len(asked) == 1
    assert row.normalized_status is Ending.UNAVAILABLE and row.passed is None
    assert row.failure == "PyPI served a bot check the agent did not cause."


JUDGE_ENV = {
    "AI_GATEWAY_API_KEY": "gateway-test-key",
    "PARALLAX_JUDGE_MODELS": "anthropic/claude-opus-5.5",
    "PARALLAX_JUDGE_BASE_URL": "https://openrouter.ai/api/v1",
    "PARALLAX_JUDGE_API_KEY": "fallback-test-key",
}
JEV_POLICY = ["--judge-policy", live.JEV_JUDGE_POLICY]


@pytest.mark.real_judge_check
@pytest.mark.parametrize(
    ("missing", "message"),
    [
        ("AI_GATEWAY_API_KEY", "AI_GATEWAY_API_KEY"),
        ("PARALLAX_JUDGE_MODELS", "PARALLAX_JUDGE_MODELS"),
        ("PARALLAX_JUDGE_BASE_URL", "PARALLAX_JUDGE_BASE_URL"),
        ("PARALLAX_JUDGE_API_KEY", "PARALLAX_JUDGE_API_KEY"),
    ],
)
async def test_a_campaign_without_every_judge_refuses_to_start(missing, message, monkeypatch, tmp_path, capsys):
    """0.5.22: a job without the fallback settings ran, and every row Jev left unsettled came back unknown."""
    for name, value in JUDGE_ENV.items():
        monkeypatch.setenv(name, value)
    monkeypatch.delenv(missing)

    async def never(*_args, **_kwargs):
        raise AssertionError("no run may start without its judges")

    monkeypatch.setattr(live, "run_arm", never)
    out = tmp_path / "rows.jsonl"
    with pytest.raises(SystemExit):
        await live.main(["--arms", "fastbrowse", "--budget", "10", "--out", str(out), *JEV_POLICY])
    err = capsys.readouterr().err
    assert "judges must be configured" in err and message in err
    assert not out.exists()
    assert not any(value in err for value in JUDGE_ENV.values() if value.endswith("key"))


@pytest.mark.real_judge_check
def test_every_judge_configured_passes_the_start_check(monkeypatch):
    for name, value in JUDGE_ENV.items():
        monkeypatch.setenv(name, value)
    live.require_judges()


@pytest.mark.real_judge_check
def test_the_opus_policy_needs_no_jev_key(monkeypatch):
    for name, value in JUDGE_ENV.items():
        monkeypatch.setenv(name, value)
    monkeypatch.delenv("AI_GATEWAY_API_KEY")
    live.require_judges(use_jev=False)


@pytest.mark.real_judge_check
@pytest.mark.parametrize(
    ("name", "value", "message"),
    [
        ("PARALLAX_JUDGE_MODELS", "anthropic/claude-opus-5.4", "must be exactly anthropic/claude-opus-5.5"),
        ("PARALLAX_JUDGE_MODELS", "anthropic/claude-opus-5.5,openai/gpt-6", "must be exactly"),
        ("PARALLAX_JUDGE_BASE_URL", "https://api.openai.com/v1", "must be OpenRouter"),
    ],
)
async def test_the_opus_policy_refuses_any_other_judge(name, value, message, monkeypatch, tmp_path, capsys):
    for key, setting in JUDGE_ENV.items():
        monkeypatch.setenv(key, setting)
    monkeypatch.setenv(name, value)

    async def never(*_args, **_kwargs):
        raise AssertionError("no run may start with another judge")

    monkeypatch.setattr(live, "run_arm", never)
    with pytest.raises(SystemExit):
        await live.main(["--arms", "fastbrowse", "--budget", "10", "--out", str(tmp_path / "rows.jsonl")])
    assert message in capsys.readouterr().err


@pytest.mark.real_judge_check
@pytest.mark.parametrize("models", ["anthropic/claude-opus-5.4", "anthropic/claude-opus-5.5,openai/gpt-unverified"])
async def test_a_campaign_with_an_unverified_fallback_model_refuses_to_start(models, monkeypatch, tmp_path, capsys):
    for name, value in JUDGE_ENV.items():
        monkeypatch.setenv(name, value)
    monkeypatch.setenv("PARALLAX_JUDGE_MODELS", models)

    async def never(*_args, **_kwargs):
        raise AssertionError("no run may start with an unverified fallback")

    monkeypatch.setattr(live, "run_arm", never)
    out = tmp_path / "rows.jsonl"
    with pytest.raises(SystemExit):
        await live.main(["--arms", "fastbrowse", "--budget", "10", "--out", str(out), *JEV_POLICY])
    err = capsys.readouterr().err
    assert "not a verified fallback" in err and "anthropic/claude-opus-5.5" in err
    assert not out.exists()


@pytest.mark.real_judge_check
async def test_an_unbudgeted_campaign_without_its_judges_refuses_to_start(monkeypatch, tmp_path, capsys):
    """Rows are judged with or without --budget, so the start check is not a budget rule."""
    for name in JUDGE_ENV:
        monkeypatch.delenv(name, raising=False)

    async def never(*_args, **_kwargs):
        raise AssertionError("no run may start without its judges")

    monkeypatch.setattr(live, "run_arm", never)
    with pytest.raises(SystemExit):
        await live.main(["--arms", "fastbrowse", "--out", str(tmp_path / "rows.jsonl")])
    assert "judges must be configured" in capsys.readouterr().err


class _StoppedCloud:
    """A cloud browser that counts its stops: every jev-ultrafast run must end with exactly one."""

    def __init__(self) -> None:
        self.stops = 0

    @asynccontextmanager
    async def __call__(self, *_: object, **__: object):
        try:
            yield Browser("ws://fixture")
        finally:
            self.stops += 1


def _runner_line(status: str, error: str | None = None) -> bytes:
    return json.dumps(
        {
            "status": status,
            "error": error,
            "seconds": 1.0,
            "steps": 1,
            "actions": 1,
            "trace": [],
            "jev_dollars": 0.0,
            "text_dollars": 0.0,
            "unmetered_requests": 0,
            "text_model": None,
        }
    ).encode()


def _ultrafast_fixture(monkeypatch: pytest.MonkeyPatch, invoke: Any) -> _StoppedCloud:
    cloud = _StoppedCloud()
    monkeypatch.setattr(live, "cloud_browser", cloud)
    monkeypatch.setattr(backend, "environment", lambda: {"BROWSER_USE_API_KEY": "cloud", "OPENROUTER_API_KEY": "text"})
    monkeypatch.setattr(live, "observe_browser", AsyncMock(return_value=FinalPage()))
    monkeypatch.setattr(live, "_invoke", invoke)
    return cloud


async def _never(*_: object) -> bytes:
    await asyncio.Event().wait()
    raise AssertionError("unreachable")


@pytest.mark.parametrize(
    "ending",
    ["done", "blocked", "error", "browser killed", "runner crashed", "unreadable result", "cancelled"],
)
async def test_every_ultrafast_exit_stops_its_cloud_browser(ending: str, monkeypatch: pytest.MonkeyPatch) -> None:
    """A cloud browser bills until it is stopped or reaches its 240-minute timeout. The run's browser is released only
    when its runner process ends (2026-10-16: arxiv-open browsers ran 14-16 minutes because their agents were still
    looping, not because a finished run kept them), and then on every ending: pass, fail, error, a browser stopped
    from outside (the runaway watchdog), a crashed runner, an unreadable result and cancellation."""

    async def invoke(*_: object) -> bytes:
        match ending:
            case "runner crashed":
                raise CommandError("command exited 1: boom")
            case "unreadable result":
                return b"not json"
            case "browser killed":
                return _runner_line("error", "RuntimeError: received 1001 (going away) browser session ended")
            case "cancelled":
                return await _never()
            case _:
                return _runner_line(ending)

    cloud = _ultrafast_fixture(monkeypatch, invoke)
    task = next(t for t in TASKS if t.id == "flights-search")
    async with httpx.AsyncClient() as http:
        run = asyncio.create_task(live.ultrafast_arm(task, http, record=None))
        if ending == "cancelled":
            await asyncio.sleep(0)
            run.cancel()
        try:
            _, report = await run
        except (asyncio.CancelledError, CommandError, ValueError):
            assert ending in {"cancelled", "runner crashed", "unreadable result"}
        else:
            assert report.status == ("error" if ending == "browser killed" else ending)
    assert cloud.stops == 1


async def test_a_wrong_browser_harness_tool_opens_no_browser(monkeypatch: pytest.MonkeyPatch) -> None:
    """A Codex the arm does not pin failed every attempt alike, each after its cloud browser was billed, so each kept
    its whole budget reserve as unknown cost and the campaign ran out on about $10 of spend. The tools are checked
    first, and a mismatch is a refused start."""

    async def invoke(*_: object) -> bytes:
        raise CommandError("command exited 1: RuntimeError: codex must be 0.163.0, got 0.166.0")

    cloud = _ultrafast_fixture(monkeypatch, invoke)
    task = next(t for t in TASKS if t.id == "flights-search")
    async with httpx.AsyncClient() as http:
        with pytest.raises(backend.NotStarted, match="codex must be"):
            await live.harness_arm(task, http, record=None)
    assert cloud.stops == 0


def test_sigterm_stops_an_ultrafast_runs_cloud_browser(monkeypatch: pytest.MonkeyPatch) -> None:
    """SIGTERM (a container stop, `kill`, `timeout`) used to end the harness where it stood: no `async with` exit, so
    every browser in flight billed until its 240-minute timeout, while its runner, in a session of its own, went on
    driving it. It is now a cancellation, the path that stops each browser."""
    from fastbrowse_evals import cli

    async def invoke(*_: object) -> bytes:
        os.kill(os.getpid(), signal.SIGTERM)
        return await _never()

    cloud = _ultrafast_fixture(monkeypatch, invoke)
    task = next(t for t in TASKS if t.id == "flights-search")

    async def main(_: list[str]) -> int:
        async with httpx.AsyncClient() as http:
            await live.ultrafast_arm(task, http, record=None)
        return 0

    def unhandled(*_: object) -> None:
        # What Python's default handler would do here is end pytest; stand in for it with a failure.
        raise AssertionError("SIGTERM reached Python's default handling, which ends the harness at once")

    imported = cli.importlib.import_module
    monkeypatch.setattr(
        cli.importlib,
        "import_module",
        lambda name: SimpleNamespace(main=main) if name.startswith("fastbrowse_evals.") else imported(name),
    )
    previous = signal.signal(signal.SIGTERM, unhandled)
    try:
        code = cli.run("internal", [])
    finally:
        signal.signal(signal.SIGTERM, previous)
    assert code == cli.TERMINATED == 143
    assert cloud.stops == 1


async def test_a_spent_github_rate_limit_is_waited_out_until_its_reset(monkeypatch: pytest.MonkeyPatch) -> None:
    """0.5.22: three Railway jobs behind one address spent GitHub's anonymous 60 an hour, and github-license
    failed both arms on a 403 instead of waiting for the limit to reset."""
    now = 1_000_000.0
    monkeypatch.setattr(live.time, "time", lambda: now)
    waits: list[float] = []

    async def sleep(seconds: float) -> None:
        waits.append(seconds)

    monkeypatch.setattr(live.asyncio, "sleep", sleep)
    calls = 0
    request = httpx.Request("GET", "https://api.github.com/repos/encode/httpx")

    async def truth(_: httpx.AsyncClient) -> object:
        nonlocal calls
        calls += 1
        if calls == 1:
            response = httpx.Response(
                403,
                headers={"x-ratelimit-remaining": "0", "x-ratelimit-reset": str(int(now) + 120)},
                request=request,
            )
            raise httpx.HTTPStatusError("403 rate limit exceeded", request=request, response=response)
        return "BSD-3-Clause"

    task = dataclasses.replace(next(t for t in live.TASKS if t.id == "github-license"), truth=truth)
    assert await live._truth(task, httpx.AsyncClient()) == "BSD-3-Clause"
    assert waits == [121.0]


async def test_a_github_403_that_is_not_a_rate_limit_still_fails() -> None:
    request = httpx.Request("GET", "https://api.github.com/repos/encode/httpx")

    async def truth(_: httpx.AsyncClient) -> object:
        response = httpx.Response(403, headers={"x-ratelimit-remaining": "41"}, request=request)
        raise httpx.HTTPStatusError("403 forbidden", request=request, response=response)

    task = dataclasses.replace(next(t for t in live.TASKS if t.id == "github-license"), truth=truth)
    with pytest.raises(httpx.HTTPStatusError):
        await live._truth(task, httpx.AsyncClient())


# A multi-step checkout the fastbrowse, fastbrowse-pool and browser-use arms all run.
WIZARD = next(t for suite in live.SUITES.values() for t in suite if t.id == "stretch-wizard-review")


def _judgment(evidence: bool | None, satisfied: bool | None, reason: str = "Recorded judgment") -> Any:
    """What the semantic judge decides of one run: whether its evidence can judge it, then whether it succeeded."""
    from parallax.judging import Decision, Judgment

    return Judgment(
        policy=live.OPUS_V4_POLICY,
        state_sha256="e" * 64,
        questions={"evidence_available": "q", "task_satisfied": "q"},
        decisions={
            "evidence_available": Decision(passed=evidence, source="llm", reason=reason),
            "task_satisfied": Decision(passed=satisfied, source="jev", reason=reason),
        },
    )


def _judged(monkeypatch: pytest.MonkeyPatch, *judgments: Any) -> list[Any]:
    """The judge answers each run in turn; everything around it (verdict, receipt, ending) is the harness's own."""
    from fastbrowse_evals import judging

    queue = list(judgments)
    asked: list[Any] = []

    async def judge_questions(state: Any, *_: Any, **__: Any) -> Any:
        asked.append(state)
        return queue.pop(0)

    monkeypatch.setattr(judging, "judge_questions", judge_questions)
    monkeypatch.setattr(judging, "panel_from_env", lambda: None)
    return asked


INFRASTRUCTURE = (
    "Every navigation to the target page failed with net::ERR_TUNNEL_CONNECTION_FAILED. This is an infrastructure "
    "failure, not an agent mistake, so the requested outcome cannot be judged."
)
# 2026-10-17, release-0.5.22-cc9e750: the same outage at lab.hakdogan.com, as each arm reported it. That site
# left DNS for good, which retired its task, stretch-wizard-correction; the reports apply to any dead site.
TRIED = [{"index": 0, "operation": "navigate", "outcome": "net::ERR_NAME_NOT_RESOLVED"}]
OUTAGE_REPORTS = {
    "fastbrowse-pool": live.ArmReport(
        status="error",
        error="Page.navigate failed (net::ERR_NAME_NOT_RESOLVED)",
        steps=0,
        seconds=8.1,
        dollars=0.003,
        step_log=TRIED,
    ),
    "browser-use": live.ArmReport(
        status="stopped", answered=False, steps=8, seconds=36.2, dollars=0.66476, step_log=TRIED
    ),
}


@pytest.mark.parametrize("arm", sorted(OUTAGE_REPORTS))
async def test_a_run_the_judge_finds_unjudgeable_for_infrastructure_is_an_outage(
    arm: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The judge, not the error text, decides: evidence unavailable means the run measured no agent."""

    async def runner(*_: Any, **__: Any) -> tuple[Outcome, live.ArmReport]:
        return Outcome(None, None, "chrome-error://chromewebdata/", unobservable=True), OUTAGE_REPORTS[arm]

    monkeypatch.setitem(live.ARMS, arm, live.ARMS[arm].model_copy(update={"runner": runner}))
    _judged(monkeypatch, _judgment(False, False, INFRASTRUCTURE))
    async with httpx.AsyncClient() as http:
        row = await live.run_arm(arm, WIZARD, None, http, Path(), record=None)
    assert row.normalized_status is Ending.UNAVAILABLE
    assert row.passed is None and row.failure == INFRASTRUCTURE
    assert row.semantic_judge is not None and row.dollars == OUTAGE_REPORTS[arm].dollars


async def test_a_genuine_agent_failure_on_a_working_site_stays_a_judged_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def runner(*_: Any, **__: Any) -> tuple[Outcome, live.ArmReport]:
        report = live.ArmReport(status="stopped", answered=False, steps=12, seconds=40.0, dollars=0.5)
        return Outcome(None, None, WIZARD.start, evidence=_evidence(200)), report

    monkeypatch.setitem(live.ARMS, "browser-use", live.ARMS["browser-use"].model_copy(update={"runner": runner}))
    _judged(monkeypatch, _judgment(True, False, "The agent gave up before the Review step."))
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("browser-use", WIZARD, None, http, Path(), record=None)
    assert row.normalized_status is Ending.STOPPED
    assert row.passed is False and row.failure == "The agent gave up before the Review step."


@pytest.mark.parametrize(("site", "ending"), [(200, Ending.ERROR), (503, Ending.UNAVAILABLE)])
@pytest.mark.parametrize(
    ("arm", "error"),
    [
        ("fastbrowse-pool", "Page.navigate failed (net::ERR_NAME_NOT_RESOLVED)"),
        ("browser-use", "RuntimeError: the session could not open the start page"),
        ("jev-ultrafast", "ModelError: the planner returned no action"),
    ],
)
async def test_a_failure_before_any_step_is_an_outage_only_at_a_site_that_is_down(
    arm: str, error: str, site: int, ending: Ending
) -> None:
    """One rule for every arm, however it names the failure: an attempt that took no step while the harness finds the
    start page down was stopped by the site before the agent could act."""
    row = live._crashed(arm, WIZARD, "Error", at=0.0, seconds=8.1, status="error", record=None)
    row = row.model_copy(update={"error": error, "steps": 0})
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(site))) as http:
        checked = await live._site_checked(row, WIZARD, http)
    assert checked.normalized_status == ending
    assert (checked.site_probe is not None) is (site == 503)


async def test_a_site_probe_cannot_overrule_the_judge_finding_no_evidence(monkeypatch: pytest.MonkeyPatch) -> None:
    """The harness's own fetch answering says nothing of the arm's browser: the run still measured no agent."""

    async def runner(*_: Any, **__: Any) -> tuple[Outcome, live.ArmReport]:
        report = live.ArmReport(
            status="unavailable", error="Page.navigate failed (net::ERR_TUNNEL_CONNECTION_FAILED)", seconds=3, dollars=0
        )
        return Outcome(None, None, None), report

    monkeypatch.setitem(live.ARMS, "fastbrowse", live.ARMS["fastbrowse"].model_copy(update={"runner": runner}))
    _judged(monkeypatch, _judgment(False, False, INFRASTRUCTURE))
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200))) as http:
        row = await live.run_arm("fastbrowse", WIZARD, None, http, Path(), record=None)
        checked = await live._site_checked(row, WIZARD, http)
    assert checked.normalized_status is Ending.UNAVAILABLE


def _campaign(
    monkeypatch: pytest.MonkeyPatch, arm: str, runs: list[tuple[Outcome, live.ArmReport]], *, down: str | None = None
) -> None:
    """`main` with the real run_arm and judge path, and no provider, browser or answer key. With `down`, the real site
    check runs and finds the start page down for that reason; without, it is skipped."""

    async def runner(*_: Any, **__: Any) -> tuple[Outcome, live.ArmReport]:
        return runs.pop(0)

    arms = {k: v.model_copy(update={"prepare": None}) for k, v in live.ARMS.items()}
    arms[arm] = arms[arm].model_copy(update={"runner": runner})
    monkeypatch.setattr(live, "ARMS", arms)
    monkeypatch.setattr(live, "_truth", AsyncMock(return_value=None))
    if down is None:
        monkeypatch.setattr(live, "_site_checked", AsyncMock(side_effect=lambda row, *_: row))
    else:
        monkeypatch.setattr(live, "_down", AsyncMock(return_value=down))
    monkeypatch.setattr(live.SiteWatch, "run", AsyncMock())
    monkeypatch.setattr(live.SiteWatch, "stalled", AsyncMock(return_value=None))
    monkeypatch.setenv("PARALLAX_POOL_URL", "https://pool.example.test")
    monkeypatch.setenv("PARALLAX_POOL_TOKEN", "pool-secret")
    monkeypatch.setattr(backend, "environment", lambda: {"OPENROUTER_API_KEY": "or"})


async def test_a_run_with_no_record_at_all_is_an_outage_no_judge_is_asked_about(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def runner(*_: Any, **__: Any) -> tuple[Outcome, live.ArmReport]:
        report = live.ArmReport(status="error", error="connection refused", steps=0, seconds=1, dollars=0.0)
        return Outcome(None, None, None, unobservable=True), report

    monkeypatch.setitem(
        live.ARMS, "fastbrowse-pool", live.ARMS["fastbrowse-pool"].model_copy(update={"runner": runner})
    )
    asked = _judged(monkeypatch)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("fastbrowse-pool", WIZARD, None, http, Path(), record=None)
    assert asked == []
    assert row.normalized_status is Ending.UNAVAILABLE and row.passed is None and row.failure == live.NO_EVIDENCE


@pytest.mark.parametrize("arm", sorted(OUTAGE_REPORTS))
async def test_an_infrastructure_unknown_is_waited_out_and_run_again_keeping_every_attempt(
    arm: str, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    down = (Outcome(None, None, "chrome-error://chromewebdata/", unobservable=True), OUTAGE_REPORTS[arm])
    status = "stopped" if arm == "browser-use" else "complete"
    answered = live.ArmReport(status=status, answered=True, steps=9, seconds=30.0, dollars=0.4, step_log=TRIED)
    up = (Outcome("Wizard submitted successfully", None, WIZARD.start), answered)
    _campaign(monkeypatch, arm, [down, up])
    _judged(monkeypatch, _judgment(False, False, INFRASTRUCTURE), _judgment(True, True))
    sleep = AsyncMock()
    monkeypatch.setattr(live.asyncio, "sleep", sleep)
    out = tmp_path / "rows.jsonl"
    argv = ["--only", WIZARD.id, "--arms", arm, "--budget", "10", "--out", str(out)]
    assert await live.main(argv) == 0
    ledger = _ledger(out)
    [row] = [json.loads(line) for line in out.read_text().splitlines()]
    assert [(a["retries"], a["selected"], a["normalized_status"], a["passed"]) for a in ledger] == [
        (0, False, "unavailable", None),
        (1, True, "done", True),
    ]
    assert all(a["semantic_judge"]["judgment"]["decisions"] for a in ledger)
    assert sum(a["dollars"] for a in ledger) == pytest.approx(OUTAGE_REPORTS[arm].dollars + 0.4)
    assert row["retries"] == 1 and row["passed"] is True
    sleep.assert_awaited_once_with(60)
    budget = [json.loads(line) for line in out.with_suffix(".budget.jsonl").read_text().splitlines()]
    assert {r["id"] for r in budget} == {f"{arm}/{WIZARD.id}/0/0", f"{arm}/{WIZARD.id}/0/1"}


async def test_an_outage_that_outlasts_the_bound_is_recorded_as_one(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    down = (Outcome(None, None, None), OUTAGE_REPORTS["browser-use"])
    _campaign(monkeypatch, "browser-use", [down] * (live.OUTAGE_RETRIES + 1))
    _judged(monkeypatch, *[_judgment(False, False, INFRASTRUCTURE)] * (live.OUTAGE_RETRIES + 1))
    sleep = AsyncMock()
    monkeypatch.setattr(live.asyncio, "sleep", sleep)
    out = tmp_path / "rows.jsonl"
    assert await live.main(["--only", WIZARD.id, "--arms", "browser-use", "--budget", "10", "--out", str(out)]) == 0
    ledger = _ledger(out)
    assert [a["selected"] for a in ledger] == [False] * live.OUTAGE_RETRIES + [True]
    assert {a["normalized_status"] for a in ledger} == {"unavailable"}
    assert [call.args[0] for call in sleep.await_args_list] == [60, 120, 240, 480, 600]


async def test_a_genuine_agent_failure_is_not_run_again(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    gave_up = live.ArmReport(status="stopped", answered=False, steps=12, seconds=40.0, dollars=0.5)
    _campaign(monkeypatch, "browser-use", [(Outcome(None, None, WIZARD.start, evidence=_evidence(200)), gave_up)])
    _judged(monkeypatch, _judgment(True, False, "The agent gave up before the Review step."))
    sleep = AsyncMock()
    monkeypatch.setattr(live.asyncio, "sleep", sleep)
    out = tmp_path / "rows.jsonl"
    assert await live.main(["--only", WIZARD.id, "--arms", "browser-use", "--budget", "10", "--out", str(out)]) == 0
    [attempt] = _ledger(out)
    assert (attempt["selected"], attempt["normalized_status"], attempt["passed"]) == (True, "stopped", False)
    sleep.assert_not_awaited()


LATER_NAVIGATION = live.ArmReport(
    status="error", error="Page.navigate failed (net::ERR_NAME_NOT_RESOLVED)", steps=6, seconds=20.0, dollars=0.02
)
"""A pool run that acted, then navigated to an address that does not resolve: the agent's own error."""


@pytest.mark.parametrize("arm", ["fastbrowse-pool", "browser-use"])
async def test_a_failure_after_the_agent_acted_is_not_promoted_to_an_outage(arm: str) -> None:
    row = live._crashed(arm, WIZARD, "BrowserError", at=0.0, seconds=20.0, status="error", record=None)
    row = row.model_copy(update={"error": LATER_NAVIGATION.error, "steps": LATER_NAVIGATION.steps})
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(503))) as http:
        checked = await live._site_checked(row, WIZARD, http)
    assert checked.normalized_status is Ending.ERROR
    assert checked.site_probe is not None  # explained, never excused


async def test_an_agent_navigation_error_at_a_down_site_is_judged_and_not_run_again(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    ran = (Outcome(None, None, "chrome-error://chromewebdata/", evidence=_evidence(200)), LATER_NAVIGATION)
    _campaign(monkeypatch, "fastbrowse-pool", [ran], down=f"site unavailable: {WIZARD.start} answered HTTP 503")
    _judged(monkeypatch, _judgment(True, False, "The agent navigated to an address it made up."))
    sleep = AsyncMock()
    monkeypatch.setattr(live.asyncio, "sleep", sleep)
    out = tmp_path / "rows.jsonl"
    argv = ["--only", WIZARD.id, "--arms", "fastbrowse-pool", "--budget", "10", "--out", str(out)]
    assert await live.main(argv) == 0
    [attempt] = _ledger(out)
    assert (attempt["selected"], attempt["normalized_status"], attempt["passed"]) == (True, "error", False)
    assert attempt["site_probe"] is not None
    sleep.assert_not_awaited()


async def test_the_hosted_agents_final_page_is_read_through_its_kept_alive_browser(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = SimpleNamespace(
        browsers=SimpleNamespace(get=AsyncMock(return_value=SimpleNamespace(cdp_url="https://cdp.example/b1")))
    )
    seen: list[str] = []

    async def websocket(_http: Any, url: str) -> str:
        return url.replace("https://", "wss://")

    async def observe(url: str) -> FinalPage:
        seen.append(url)
        return FinalPage(url="https://pypi.org/project/httpx/")

    monkeypatch.setattr(live, "devtools_websocket", websocket)
    monkeypatch.setattr(live, "observe_browser", observe)
    final = await live.hosted_final_page(client, httpx.AsyncClient(), "b1")
    assert final.url == "https://pypi.org/project/httpx/" and seen == ["wss://cdp.example/b1"]
    client.browsers.get.assert_awaited_once_with("b1")
    # No browser, or one that cannot be reached: the page is unobservable, never guessed.
    assert "opened no browser" in str((await live.hosted_final_page(client, httpx.AsyncClient(), None)).error)
    client.browsers.get = AsyncMock(side_effect=RuntimeError("gone"))
    assert "did not answer" in str((await live.hosted_final_page(client, httpx.AsyncClient(), "b1")).error)


def test_every_head_to_head_arm_runs_the_same_tasks_unless_a_stated_reason_stops_it() -> None:
    every = [t for suite in live.SUITES.values() for t in suite]
    skipped = {arm: {t.id for t in every if live.excluded_because(arm, t)} for arm in live.HEAD_TO_HEAD_ARMS}
    assert skipped["fastbrowse"] == set()
    assert skipped["browser-use"] == {"saucedemo-pause"}
    assert len(every) - len(skipped["browser-harness"]) == 47
    # jev-ultrafast returns no answer, so it runs only the tasks graded on the page a run ends on.
    assert {t.id for t in every if t.id not in skipped["jev-ultrafast"]} == {
        t.id for t in every if t.category is live_tasks.Category.NAVIGATE
    }


async def test_local_browser_is_the_fastbrowse_arms_alone_in_a_head_to_head(monkeypatch, tmp_path):
    """One invocation runs every hosted head-to-head arm; --local-browser reaches the fastbrowse arm only."""
    seen: dict[str, Path | None] = {}

    async def run(arm, task, *_args, **_kwargs):
        seen[arm] = live._local_browser.get()
        return _row(arm, task, 0.5)

    _live_fakes(monkeypatch, run)
    monkeypatch.setattr(live, "ARMS", {k: v.model_copy(update={"prepare": None}) for k, v in live.ARMS.items()})
    monkeypatch.setattr(live, "local_browser_identity", lambda binary: {"mode": "local-chrome", "binary": str(binary)})
    chrome = tmp_path / "chrome"
    out = tmp_path / "rows.jsonl"
    arms = ["fastbrowse", "browser-use", "jev-ultrafast"]
    target = next(t for suite in live.SUITES.values() for t in suite if all(live.eligible(a, t) for a in arms))
    argv = ["--only", target.id, "--arms", *arms, "--local-browser", str(chrome)]
    assert await live.main([*argv, "--budget", "20", "--out", str(out)]) == 0
    assert seen == {"fastbrowse": chrome, "browser-use": None, "jev-ultrafast": None}
    browsers = _ledger(out)[0]["run"]["browsers"]
    assert browsers["fastbrowse"] == {"mode": "local-chrome", "binary": str(chrome)}
    assert browsers["browser-use"] == {"mode": "browser-use-hosted"}


@pytest.mark.parametrize(
    ("arms", "extra", "message"),
    [
        (["browser-use"], [], "needs the fastbrowse arm"),
        (["fastbrowse", "jev-ultrafast"], ["--browser-reserve-dollars", "0"], "zero browser reserve"),
    ],
)
async def test_local_browser_refusals(arms, extra, message, tmp_path, capsys):
    argv = ["--arms", *arms, "--local-browser", str(tmp_path / "chrome"), "--budget", "10", *extra]
    with pytest.raises(SystemExit):
        await live.main([*argv, "--out", str(tmp_path / "rows.jsonl")])
    assert message in capsys.readouterr().err


async def test_a_refused_start_releases_its_budget_hold(monkeypatch, tmp_path):
    """A provider that refuses to start a run (401 on a bad key) billed nothing: its hold settles at zero, so the
    approved total is not spent on runs that never began."""

    async def runner(*_args, **_kwargs):
        raise backend.NotStarted("browser agent run returned HTTP 401")

    async def run(arm, task, truth, http, downloads, **kwargs):
        return await real_run_arm(arm, task, truth, http, downloads, **kwargs)

    real_run_arm = live.run_arm
    _live_fakes(monkeypatch, run)
    spec = live.ARMS["jev-ultrafast"].model_copy(update={"runner": runner})
    monkeypatch.setattr(live, "ARMS", {**live.ARMS, "jev-ultrafast": spec})
    out = tmp_path / "rows.jsonl"
    argv = ["--only", "wiki-open", "--arms", "jev-ultrafast", "--repeat", "2"]
    assert await live.main([*argv, "--concurrency", "1", "--budget", "3", "--out", str(out)]) == 0
    rows = _ledger(out)
    assert [(r["started"], r["dollars"], r["failure"]) for r in rows] == [
        (False, 0.0, "NotStarted: browser agent run returned HTTP 401")
    ] * 2
    journal = [json.loads(line)["committed"] for line in out.with_suffix(".budget.jsonl").read_text().splitlines()]
    # Each hold (3, the whole budget) is released, so the second run could start.
    assert journal == [3, 0, 3, 0]


@pytest.mark.parametrize(("status", "starting", "kind"), [(401, True, "NotStarted"), (401, False, "RuntimeError"),
                                                          (503, True, "Unavailable")])  # fmt: skip
def test_a_refused_browser_create_is_not_started(status, starting, kind):
    from browser_use_sdk.v3 import BrowserUseError

    from fastbrowse_evals import browsers

    error = browsers._failure("create", BrowserUseError(status, "echo key"), starting=starting)
    assert type(error).__name__ == kind and f"HTTP {status}" in str(error)


def test_the_judge_is_shown_what_the_run_did_never_the_agents_model_traffic() -> None:
    traffic = {"event": "http", "url": "https://openrouter.ai/api/v1/chat/completions", "request": {}, "response": {}}
    route = {"event": "route", "providers": "Jev"}
    state: dict[str, Any] = {"answer": "x", "report": {"events": [route, traffic], "step_log": [{"index": 0}]}}
    shown = live.shown_to(live.OPUS_V2_POLICY, state, Ending.DONE)
    assert shown == {"answer": "x", "report": {"events": [route], "step_log": [{"index": 0}]}}
    assert live.shown_to(live.OPUS_V2_POLICY, shown, Ending.DONE) == shown
    # Earlier policies, and the row itself, keep exactly what they had.
    assert live.shown_to(live.OPUS_V1_POLICY, state, Ending.DONE) is state
    assert state["report"]["events"] == [route, traffic]


BLIND_ARMS = {
    "fastbrowse": live.ArmReport(
        status="complete",
        answered=False,
        seconds=12.0,
        dollars=0.03,
        steps=1,
        model="jev-1.13",
        text_model="anthropic/claude-haiku",
        decided_by="jev",
        session_id="fast-1",
        provenance={"package": "fastbrowse 0.5"},
        citations=[{"url": "https://a.test", "quote": "q"}],
        events=[{"event": "route", "providers": "Jev"}],
        cost_by_component={"jev": 0.01},
        step_log=[{"index": 0, "operation": "extract", "outcome": "executed", "decided_by": "jev", "duration_ms": 9}],
    ),
    "browser-use": live.ArmReport(
        status="stopped",
        answered=True,
        task_successful=True,
        seconds=40.0,
        dollars=0.4,
        steps=1,
        model="bu-max",
        session_id="hosted-1",
        step_log=[{"index": 0, "operation": "extract", "outcome": "executed", "duration_ms": 77}],
    ),
    "jev-ultrafast": live.ArmReport(
        status="done",
        seconds=5.0,
        dollars=None,
        steps=1,
        decision_log=[{"index": 0, "operation": "extract", "outcome": "executed", "confidence": 0.9}],
        provenance={"package": "jev-ultrafast"},
        artifact="artifacts/jev-ultrafast/1.evidence.json",
    ),
    "browser-use-oss": live.ArmReport(
        status="done",
        seconds=30.0,
        dollars=None,
        steps=1,
        model="anthropic/claude-sonnet",
        step_log=[{"index": 0, "operation": "extract", "outcome": "executed"}],
    ),
}


async def _judged_state(monkeypatch: pytest.MonkeyPatch, arm: str, outcome: Outcome) -> dict[str, Any]:
    seen: dict[str, Any] = {}

    async def judge(state: dict[str, Any], *, policy: str, **_: Any) -> tuple[Any, str]:
        seen.update(state)
        verdict = judging.Decision(passed=True, source="llm", reason="ok")
        decisions = {"evidence_available": verdict, "task_satisfied": verdict}
        return judging.Judgment(policy=policy, state_sha256="0" * 64, questions={}, decisions=decisions), "f" * 64

    monkeypatch.setattr(live, "judge_browser", judge)
    report = BLIND_ARMS[arm]
    await live.semantic_grade(task("pypi-open"), None, outcome, report, live.ending_of(arm, report))
    return seen


async def test_identical_evidence_is_judged_identically_whichever_arm_ran(monkeypatch: pytest.MonkeyPatch) -> None:
    outcome = Outcome("httpx 0.28", None, "https://pypi.org/project/httpx/", None, ((("link", "Home"),)))
    states = []
    for arm in BLIND_ARMS:
        states.append(json.dumps(await _judged_state(monkeypatch, arm, outcome), sort_keys=True))
    assert len(set(states)) == 1


@pytest.mark.parametrize("arm", list(BLIND_ARMS))
async def test_no_task_tells_the_judge_which_arm_ran(arm: str, monkeypatch: pytest.MonkeyPatch) -> None:
    report = BLIND_ARMS[arm]
    for suite in live.SUITES.values():
        for each in suite:
            state = live.judge_state(each, None, Outcome("an answer", None, each.start), report)
            judged = json.dumps(live.shown_to(live.OPUS_V4_POLICY, state, live.ending_of(arm, report))).lower()
            for name in (*live.ARMS, "jev", "bu-max", "session_id", "dollars"):
                assert name not in judged, (each.id, name)


def test_v4_shows_every_arm_one_form_and_keeps_each_quote_as_evidence() -> None:
    fact = {"text": "httpx 0.28", "quote": "httpx 0.28.1", "url": "https://p", "reader": "jev_choice", "deep_link": "x"}
    fastbrowse = {"index": 0, "operation": "read", "outcome": "executed", "facts": [fact], "page_changed": False}
    state = {
        "expected_ending": "stopped",
        "quotes": [["https://p", "httpx 0.28.1"]],
        "report": {"ending": "complete", "step_log": [fastbrowse]},
    }
    shown = live.shown_to(live.OPUS_V4_POLICY, state, Ending.ERROR)
    assert shown["quotes"] is None
    [step] = shown["report"]["step_log"]
    assert "facts" not in step and "page_changed" not in step
    assert "httpx 0.28.1" in step["observation"] and "https://p" in step["observation"]
    assert "jev" not in json.dumps(shown)
    assert live.shown_to(live.OPUS_V4_POLICY, shown, Ending.ERROR) == shown
    assert live.shown_to(live.OPUS_V3_POLICY, state, Ending.ERROR) == state


@pytest.mark.parametrize(
    ("step", "operation"),
    [
        ({"operation": "browser_click"}, "click"),
        ({"operation": "CLICK"}, "click"),
        ({"operation": "TYPE_TEXT"}, "fill"),
        ({"operation": "escalate"}, "reason"),
    ],
)
def test_each_arms_name_for_an_operation_becomes_the_shared_one(step: dict, operation: str) -> None:
    assert live.shared_step(step)["operation"] == operation


def test_a_shared_step_does_not_name_the_command_line_tool() -> None:
    step = live.shared_step({"command": "browser-harness -c 'go()'", "output": "browser_harness ok"})
    assert "harness" not in json.dumps(step)


def test_a_failed_run_keeps_its_steps_error_and_ending_for_the_judge() -> None:
    report = live.ArmReport(
        status="error",
        seconds=1.0,
        dollars=None,
        steps=1,
        error="the page crashed",
        step_log=[{"index": 0, "operation": "click", "outcome": "error", "observation": "gone", "at": "12:00"}],
    )
    state = {"expected_ending": "needs_confirmation", "report": json.loads(report.model_dump_json())}
    shown = live.shown_to(live.OPUS_V4_POLICY, state, live.ending_of("browser-use-oss", report))
    assert shown["expected_ending"] == "stopped" and shown["report"]["ending"] == "error"
    assert shown["report"]["error"] == "the page crashed"
    assert shown["report"]["step_log"] == [
        {"index": 0, "operation": "click", "outcome": "error", "observation": "gone"}
    ]
    assert live.shown_to(live.OPUS_V4_POLICY, shown, Ending.ERROR) == shown  # a recorded state is judged as recorded


def test_a_coding_agent_arms_commands_reach_the_judge_whole() -> None:
    output = "row " * 5_000
    step = {"command": "python read.py", "exit_code": 0, "output": output}
    report = live.ArmReport(status="done", seconds=1.0, dollars=None, steps=1, step_log=[step])
    assert live.blind_report(report.model_dump(mode="json"), Ending.DONE)["step_log"] == [step]


async def test_a_long_final_page_reaches_the_judge_whole(monkeypatch: pytest.MonkeyPatch) -> None:
    assert "slice" not in live.EVIDENCE
    page = "the answer is at the very end " * 20_000 + "42"
    evidence = PageEvidence(url="https://a.test", status=200, text=page, text_length=len(page))
    state = await _judged_state(monkeypatch, "browser-use", Outcome("42", None, "https://a.test", evidence=evidence))
    assert state["final_page"]["text"] == page


async def test_a_state_past_the_judges_window_is_ungraded_and_kept_whole(monkeypatch: pytest.MonkeyPatch) -> None:
    """Over the judge's documented window nothing is cut: no request goes out and the row is left ungraded."""
    page = "x" * 3_000_000
    evidence = PageEvidence(url="https://a.test", status=200, text=page, text_length=len(page))
    outcome = Outcome("42", None, "https://a.test", evidence=evidence)
    monkeypatch.setenv("PARALLAX_JUDGE_MODELS", "anthropic/claude-opus-5.5")
    monkeypatch.setenv("PARALLAX_JUDGE_BASE_URL", "https://openrouter.ai/api/v1")
    monkeypatch.setenv("PARALLAX_JUDGE_API_KEY", "key")
    monkeypatch.setenv("OPENROUTER_API_KEY", "key")
    for arm in ("fastbrowse", "browser-use"):
        report = BLIND_ARMS[arm]
        passed, reason, receipt = await live.semantic_grade(
            task("pypi-open"), None, outcome, report, live.ending_of(arm, report)
        )
        assert passed is None and "over the fallback's input budget" in reason
        assert receipt["state"]["final_page"]["text"] == page
        assert live.judged_unavailable(receipt) is False


@pytest.mark.parametrize("arm", ["fastbrowse", "browser-use", "browserbase", "browser-harness"])
def test_a_retryable_status_on_the_final_page_is_an_outage_for_every_arm(arm: str) -> None:
    evidence = PageEvidence(url="https://a.test", status=503, text="Bad gateway", text_length=11)
    report = live.ArmReport(status="stopped", seconds=1.0, dollars=None, steps=0, answered=True)
    receipt = {
        "judgment": {"policy": "p", "state_sha256": "0" * 64, "questions": {}, "decisions": {}},
        "judge_cost_usd": 0.0,
        "known_judge_cost_usd": 0.0,
        "unpriced_judge_attempts": 0,
    }
    fields = live.graded(arm, report, evidence, False, "no answer", receipt)
    assert fields["normalized_status"] == Ending.UNAVAILABLE and fields["failure_class"] == "site_http"


@pytest.mark.parametrize("error", [None, "the page crashed"])
def test_every_arm_passes_on_the_final_page_it_saw(error: str | None) -> None:
    evidence = PageEvidence(url="https://a.test", status=200, text="t", text_length=1)
    assert live._page_of(FinalPage(evidence=evidence, error=error)) == (evidence, error is not None)


def _line(component: str, dollars: float | None) -> Any:
    return SimpleNamespace(component=live.CostComponent(component), dollars=dollars)


def test_a_component_with_an_unpriced_line_has_no_figure() -> None:
    lines = [
        _line("browser", 0.2),
        _line("proxy", None),
        _line("proxy", 0.1),
    ]
    assert live._known_components(lines) == {"browser": 0.2}


@pytest.mark.parametrize("arm", ["fastbrowse", "browser-use", "jev-ultrafast"])
def test_a_crash_leaves_its_cost_unknown_for_every_arm(arm: str) -> None:
    row = live._crashed(arm, task("pypi-open"), "boom", at=0.0, seconds=1.0, status="error", record=None)
    assert row.dollars is None and row.unknown_cost is True
    assert row.llm_calls is None
    assert row.llm_calls_basis == ("physical_requests" if arm == "fastbrowse" else None)


def _oss_runner(
    monkeypatch: pytest.MonkeyPatch, status: str, error: str | None, seen: dict[str, Any]
) -> dict[str, Any]:
    """The OSS runner over a fake browser-use 1.0 whose run ends with `status` and `error`, as its history reports."""

    class Agent:
        def __init__(self, **kwargs: Any) -> None:
            seen.update(kwargs)

        async def run(self, max_steps: int, timeout: float) -> Any:
            seen.update(max_steps=max_steps, timeout=timeout)
            done = status == "completed"
            return SimpleNamespace(
                status=status,
                success=True if done else None,
                error=error,
                steps=3,
                duration_s=2.0,
                cost_usd=0.1 if done else None,
                usage={"requests": 3},
                messages=[],
                structured_output=None,
                final_result=lambda: "answer" if done else None,
            )

    class Browser:
        def __init__(self, **kwargs: Any) -> None:
            seen["browser_options"] = kwargs
            self.id, self.live_url, self.cloud_cdp_url = "b1", "https://live.test", "https://b.test"

        async def start(self) -> None:
            seen.setdefault("calls", []).append("start")

        async def stop(self) -> None:
            seen.setdefault("calls", []).append("stop")
            self.id = None

    class ModelServerError(Exception): ...

    class ModelConnectionError(Exception): ...

    fake = types.ModuleType("browser_use")
    fake.Agent, fake.Browser = Agent, Browser  # ty: ignore[unresolved-attribute]
    loop = types.ModuleType("browser_use.agent.loop")
    loop.TRANSIENT = (ModelServerError, ModelConnectionError)  # ty: ignore[unresolved-attribute]
    monkeypatch.setitem(sys.modules, "browser_use", fake)
    monkeypatch.setitem(sys.modules, "browser_use.agent", types.ModuleType("browser_use.agent"))
    monkeypatch.setitem(sys.modules, "browser_use.agent.loop", loop)
    return runpy.run_path(str(live.OSS_RUNNER))


def _oss_run(monkeypatch: pytest.MonkeyPatch, status: str, error: str | None = None) -> dict[str, Any]:
    seen: dict[str, Any] = {}
    runner = _oss_runner(monkeypatch, status, error, seen)
    request = {"goal": "g", "max_steps": None, "browser_minutes": 240}
    browser = SimpleNamespace()
    result = asyncio.run(runner["run"](request, browser))
    # 1.0's defaults stand: no model or mode is chosen, and its step and time limits are lifted, not left at 100/1700.
    assert seen["browser"] is browser
    assert "llm" not in seen and "mode" not in seen and seen["max_steps"] >= 1_000_000 and seen["timeout"] >= 86_400
    return result


def test_the_oss_runner_owns_its_browser_and_answers_in_order(monkeypatch) -> None:
    seen: dict[str, Any] = {}
    runner = _oss_runner(monkeypatch, "completed", None, seen)
    lines = iter([json.dumps({"goal": "g", "max_steps": None, "browser_minutes": 240}), "{}"])
    events: list[dict[str, Any]] = []

    async def read() -> str:
        return next(lines)

    asyncio.run(runner["session"](read, events.append))
    # 1.0's own cloud browser, its defaults kept but for staying up for the final page and the lifted time cap.
    assert seen["browser_options"] == {"keep_alive": True, "cloud_timeout": 240}
    assert seen["calls"] == ["start", "stop"]
    assert [e["event"] for e in events] == ["ready", "result", "stopped"]
    assert events[0]["browser_id"] == "b1" and events[0]["cdp_url"] == "https://b.test"
    assert events[1]["status"] == "done" and "seconds" not in events[1]  # the harness times the run
    assert events[2] == {"event": "stopped", "browser_id": "b1", "stopped": True}


@pytest.mark.parametrize(
    "error", ["ModelServerError: status_code: 503, model_name: bu-fast, body: {}", "ModelConnectionError: reset"]
)
def test_a_provider_outage_is_unavailable_on_the_oss_arm(monkeypatch, error) -> None:
    result = _oss_run(monkeypatch, "error", error)
    assert result["status"] == "unavailable" and "Model" in result["error"]
    assert result["dollars"] is None


def test_a_completed_oss_run_reports_its_answer_and_cost(monkeypatch) -> None:
    result = _oss_run(monkeypatch, "completed")
    assert result["status"] == "done" and result["error"] is None and result["answer"] == "answer"
    assert result["dollars"] == 0.1 and result["steps"] == 3


def test_an_oss_run_the_provider_refused_is_its_own_error(monkeypatch) -> None:
    result = _oss_run(monkeypatch, "error", "ModelHTTPError: status_code: 400, model_name: bu-fast, body: {}")
    assert result["status"] == "error" and result["error"].startswith("ModelHTTPError")


@pytest.mark.parametrize(("cost", "tokens", "price"), [(0.0, None, 1e-7), (0.0, 0, 1e-7), (0.0, 100, 0.0)])
def test_a_jev_request_metered_free_with_nothing_to_price_it_is_unknown(cost, tokens, price) -> None:
    meter = RUNNER["Meter"](price)
    RUNNER["_meter"](meter, "jev", cost, tokens)
    assert meter.unmetered == 1 and meter.jev == 0.0


def test_a_dropped_model_connection_counts_as_unmetered_and_a_refused_request_does_not(monkeypatch) -> None:
    meter = RUNNER["Meter"]()
    model = SimpleNamespace()
    RUNNER["patch_transport"].__globals__["os"].environ.setdefault("TYPESAFE_API_KEY", "k")

    def client(error: Exception | None) -> SimpleNamespace:
        def post(*_: Any, **__: Any) -> httpx.Response:
            if error is not None:
                raise error
            return httpx.Response(400, json={})

        return SimpleNamespace(post=post)

    RUNNER["patch_transport"](model, meter)
    model.CLIENT = client(None)
    with pytest.raises(RuntimeError):
        model.post_json("https://openrouter.ai/x", "k", {})
    assert meter.unmetered == 0
    model.CLIENT = client(httpx.ReadError("reset"))
    with pytest.raises(RUNNER["Unavailable"]):
        model.post_json("https://openrouter.ai/x", "k", {})
    assert meter.unmetered == 1


async def test_a_cloud_navigation_starts_on_a_real_screen_and_any_other_browser_is_pinned(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A browser without `allowResizing` ignores a tab override, and the service documents resizing as less stealthy,
    so a cloud browser takes the shared viewport as its screen size; a pool or local tab keeps the override."""
    navigation = next(t for t in TASKS if t.category is live.Category.NAVIGATE)
    other = next(t for t in TASKS if t.category is not live.Category.NAVIGATE)
    assert live._screen(navigation) == (live.VIEWPORT["width"], live.VIEWPORT["height"])
    assert live._screen(other) is None

    screens: list[object] = []
    pins: list[bool] = []

    @asynccontextmanager
    async def cloud(_: httpx.AsyncClient, *, screen: tuple[int, int] | None = None):
        screens.append(screen)
        yield Browser("ws://cloud", screen_sized=screen is not None)

    @asynccontextmanager
    async def tab(_: str, *, viewport: bool):
        pins.append(viewport)
        yield

    async def run(*_: object, open_browser: Any, **__: object) -> SimpleNamespace:
        await open_browser()
        return SimpleNamespace(answer="a", data=None, final_url=None, evidence=[])

    async def observed(*_: object, **__: object) -> FinalPage:
        return FinalPage()

    monkeypatch.setattr(live, "cloud_browser", cloud)
    monkeypatch.setattr(live, "prepared_tab", tab)
    monkeypatch.setattr(live, "run_task", run)
    monkeypatch.setattr(live, "observe_browser", observed)
    monkeypatch.setattr(backend, "environment", lambda: {"OPENROUTER_API_KEY": "k"})
    async with httpx.AsyncClient() as http:
        await live.fast_arm(navigation, http, Path(), record=None)
        await live.fast_arm(navigation, http, Path(), record=None, cdp_url="ws://pool")
    assert screens == [(1120, 780)] and pins == [False, True]


def test_a_smoke_run_cannot_be_read_as_its_publishable_experiment() -> None:
    from parallax.track import projection

    from fastbrowse_evals.langfuse_runs import marked_smoke

    trace = projection.TraceSpec("i", "n", "t", ("campaign:c",), "h", cast(Any, {}), cast(Any, {}), ())
    run = projection.RunSpec("c-fastbrowse", "row", "h", None, "", {}, (), (trace,))
    tracked = projection.TrackedRun("d", "r", datetime(2026, 1, 1, tzinfo=UTC), (), (run,))
    marked = marked_smoke(tracked)
    assert marked.runs[0].name == "smoke-c-fastbrowse"
    assert marked.runs[0].traces[0].tags == ("campaign:c", "smoke")


def test_page_evidence_that_does_not_validate_is_no_evidence() -> None:
    assert final_page.evidence_of(None) is None


async def test_hosted_messages_reads_a_session_to_its_end() -> None:
    pages = 150

    class Sessions:
        async def messages(self, _id: str, limit: int, after: str | None = None) -> SimpleNamespace:
            at = int(after or 0)
            rows = [SimpleNamespace(id=str(at + n + 1)) for n in range(limit)]
            return SimpleNamespace(messages=rows, has_more=at + limit < pages * limit)

    got = await live.hosted_messages(SimpleNamespace(sessions=Sessions()), "s")
    assert len(got) == pages * 100
