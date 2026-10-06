import asyncio
import json
import logging
import os
import runpy
import sys
import time
from base64 import urlsafe_b64decode, urlsafe_b64encode
from datetime import UTC, date, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock

import httpx
import pytest
from pydantic import SecretStr

from fastbrowse.clients.environment import JevSource, Settings
from fastbrowse.clients.validation import RETRYABLE_STATUS, TRANSIENT_TRANSPORT
from fastbrowse.evals import live, live_tasks, more_tasks
from fastbrowse.evals.live_tasks import TASKS, LiveTask, Outcome, PageEvidence, page_defect
from fastbrowse.evals.status import Ending
from fastbrowse.models import Unavailable
from fastbrowse.telemetry import TRACE, trace, traced

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


@pytest.mark.parametrize("cancel_first", [False, True])
async def test_concurrent_traces_keep_only_their_runs_events(
    cancel_first: bool, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(TRACE, "level", logging.WARNING)
    TRACE.setLevel(logging.WARNING)
    handlers = tuple(TRACE.handlers)
    started, overlapping, finish_first, finish_second = (asyncio.Event() for _ in range(4))
    collected: dict[str, list[object]] = {}

    async def child(name: str) -> None:
        trace("child", run=name)

    async def run(name: str, ready: asyncio.Event, finish: asyncio.Event) -> None:
        with traced() as events:
            collected[name] = events
            trace("start", run=name)
            await asyncio.create_task(child(name))
            ready.set()
            await finish.wait()
            trace("end", run=name)

    first = asyncio.create_task(run("first", started, finish_first))
    await started.wait()
    second = asyncio.create_task(run("second", overlapping, finish_second))
    try:
        await overlapping.wait()
        trace("outside")
        if cancel_first:
            first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first
        else:
            finish_first.set()
            await first
        assert TRACE.level == logging.DEBUG
        finish_second.set()
        await second
        assert collected["first"] == [
            {"event": event, "run": "first"}
            for event in (("start", "child") if cancel_first else ("start", "child", "end"))
        ]
        assert collected["second"] == [{"event": event, "run": "second"} for event in ("start", "child", "end")]
        assert TRACE.level == logging.WARNING and tuple(TRACE.handlers) == handlers
    finally:
        first.cancel()
        second.cancel()
        await asyncio.gather(first, second, return_exceptions=True)


def test_videos_count_up_past_names_already_claimed(tmp_path: Path) -> None:
    # Repeats are allocated before any of them records, so a name must be taken the moment it is handed out.
    first = live.video_path(tmp_path, "fastbrowse", task("hn-top"))
    assert first == tmp_path.resolve() / "fastbrowse" / "hn-top-1.mp4"
    assert live.video_path(tmp_path, "fastbrowse", task("hn-top")).name == "hn-top-2.mp4"
    assert live.video_path(tmp_path, "jev-ultrafast", task("hn-top")).name == "hn-top-1.mp4"


@pytest.mark.parametrize(("status", "passed"), [("done", True), ("blocked", False)])
async def test_ultrafast_passes_only_on_a_correct_outcome_it_called_done(
    monkeypatch: pytest.MonkeyPatch, status: str, passed: bool
) -> None:
    arxiv = task("arxiv-title")

    async def ultrafast_arm(
        _: LiveTask, __: httpx.AsyncClient, *, record: Path | None
    ) -> tuple[Outcome, live.ArmReport]:
        # jev-ultrafast has no answer; an outcome that has one stands in for a grader that needs none.
        outcome = Outcome("Attention Is All You Need", None, "https://arxiv.org/abs/1706.03762")
        return outcome, live.ArmReport(status=status, dollars=0.001, seconds=3.0)

    monkeypatch.setattr(live, "ultrafast_arm", ultrafast_arm)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm(
            "jev-ultrafast", arxiv, "Attention Is All You Need", http, Path(), bitwarden=False, record=None
        )
    assert row.correct is True
    assert row.passed is passed
    assert row.seconds == 3.0


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
        outcome = Outcome(answer, None, "https://arxiv.org/abs/1706.03762")
        return outcome, live.ArmReport(status=status, dollars=0.001, seconds=12.0, **telemetry)

    monkeypatch.setattr(live, "_fast_report", fast_report)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm(
            "fastbrowse", task("arxiv-title"), "Attention Is All You Need", http, Path(), bitwarden=False, record=None
        )
    assert row.normalized_status == (Ending.DONE if passed else Ending.STOPPED)
    assert row.passed is passed
    assert row.correct is passed
    assert row.seconds == 12.0
    assert row.dollars == 0.001
    assert (row.failure is None) is passed


async def test_an_attempt_of_any_arm_still_running_at_the_cap_is_an_outage(monkeypatch: pytest.MonkeyPatch) -> None:
    """Two of one day's Browser Use sessions sat unfinished for over half an hour; the cap holds every arm alike."""

    async def ultrafast_arm(_: LiveTask, __: httpx.AsyncClient, *, record: Path | None) -> Any:
        await asyncio.Event().wait()

    monkeypatch.setattr(live, "ultrafast_arm", ultrafast_arm)
    monkeypatch.setattr(live, "STUCK_SECONDS", 0.05)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("jev-ultrafast", task("arxiv-title"), None, http, Path(), bitwarden=False, record=None)
    assert row.normalized_status == Ending.UNAVAILABLE


@pytest.mark.parametrize(("site", "ending"), [(200, Ending.ERROR), (503, Ending.UNAVAILABLE)])
async def test_a_first_page_that_never_loaded_is_an_outage_only_at_a_site_that_is_down(
    site: int, ending: Ending
) -> None:
    """fastbrowse's own browser failing to load a site that is up is its failure, as it would be any other arm's."""
    row = live._crashed(
        "fastbrowse",
        task("pypi-newer"),
        "SiteUnreachable",
        at=0.0,
        seconds=1.0,
        status=Ending.UNAVAILABLE.value,
        record=None,
    ).model_copy(update={"error": "Page.navigate failed (net::ERR_EMPTY_RESPONSE)"})
    transport = httpx.MockTransport(lambda _: httpx.Response(site))
    async with httpx.AsyncClient(transport=transport) as http:
        checked = await live._site_checked(row, task("pypi-newer"), http)
    assert checked.normalized_status == ending


async def test_a_grader_that_raises_fails_only_its_own_row(monkeypatch: pytest.MonkeyPatch) -> None:
    """The agent chooses where a run ends, and urlparse raises on a bracket in the host: one such run once
    discarded 59 of 63 runs in a live suite."""

    async def ultrafast_arm(
        _: LiveTask, __: httpx.AsyncClient, *, record: Path | None
    ) -> tuple[Outcome, live.ArmReport]:
        return Outcome("Done.", None, "http://[not-an-address/page"), live.ArmReport(
            status="done", dollars=0.001, seconds=3.0
        )

    monkeypatch.setattr(live, "ultrafast_arm", ultrafast_arm)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("jev-ultrafast", task("wiki-open"), None, http, Path(), bitwarden=False, record=None)
    assert row.correct is False
    assert row.failure is not None
    assert row.failure.startswith("check raised ValueError")


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
    assert rows["pypi-open"].passed is True
    assert rows["wiki-open"].passed is False
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
        (None, "https://api.typesafe.ai/v1/systemone", "ts"),
        (JevSource.OPENROUTER, "https://openrouter.ai/api/v1/systemone", "or"),
        (JevSource.TYPESAFE, "https://api.typesafe.ai/v1/systemone", "ts"),
        (JevSource.GATEWAY, "https://ai-gateway.vercel.sh/v4/ai/evaluation-model", "gw"),
    ],
)
@pytest.mark.parametrize("pinned", [False, True])
def test_ultrafast_uses_the_selected_jev_route_and_meters_its_cost(
    monkeypatch: pytest.MonkeyPatch, source: JevSource | None, url: str, key: str, pinned: bool
) -> None:
    configured = Settings.model_construct(
        openrouter_api_key=SecretStr("or"),
        typesafe_api_key=SecretStr("ts"),
        ai_gateway_api_key=SecretStr("gw"),
        jev_source=source,
        jev_base_url="https://proxy.test/" if pinned else None,
        jev_model="pinned-jev" if pinned else None,
    )
    monkeypatch.setattr(live, "load_settings", lambda: configured)
    env = live._ultrafast_env("wss://browser.test", "/tmp/ultrafast-test")
    for name in ("TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY", "FASTBROWSE_JEV_BASE_URL", "FASTBROWSE_JEV_MODEL"):
        monkeypatch.delenv(name, raising=False)
        if name in env:
            monkeypatch.setenv(name, env[name])

    def post(_model: Any, endpoint: str, headers: dict[str, str], body: dict[str, Any]) -> dict[str, Any]:
        path = "/v4/ai/evaluation-model" if source is JevSource.GATEWAY else "/v1/systemone"
        assert endpoint == (f"https://proxy.test{path}" if pinned else url)
        assert headers["Authorization"] == f"Bearer {key}"
        if source is JevSource.GATEWAY:
            assert "model" not in body
            return {"answers": {}, "usage": {"inputTokens": 100}, "providerMetadata": {"gateway": {"cost": "0.001"}}}
        assert body["model"] == (
            "pinned-jev" if pinned else "jev-1.13.0" if source in (None, JevSource.TYPESAFE) else "jev-1.13"
        )
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
            assert "browser-use" not in live_task.arms
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


async def test_a_hosted_session_whose_output_fails_the_schema_keeps_its_cost(monkeypatch: pytest.MonkeyPatch) -> None:
    import browser_use_sdk.v3  # an optional extra

    session = SimpleNamespace(
        id="s1",
        output="[Session cost limit reached]",
        total_cost_usd=0.37,
        status=SimpleNamespace(value="stopped"),
        is_task_successful=False,
        model="claude-opus-4.7",
        step_count=4,
        live_url="https://live",
    )

    class Run:
        session_id = "s1"

        def __await__(self) -> Any:
            async def fail() -> None:
                raise ValueError("Invalid JSON")

            return fail().__await__()

    class Client:
        def __init__(self, **_: object) -> None:
            self.sessions = SimpleNamespace(
                get=AsyncMock(return_value=session),
                messages=AsyncMock(return_value=SimpleNamespace(messages=[], has_more=False)),
            )

        def run(self, *_: object, **__: object) -> Run:
            return Run()

    monkeypatch.setattr(browser_use_sdk.v3, "AsyncBrowserUse", Client)
    monkeypatch.setattr(live, "load_settings", lambda: SimpleNamespace(browser_key=lambda: "key"))
    outcome, report = await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None)
    assert outcome.answer == "[Session cost limit reached]"
    assert report.dollars == 0.37 and report.status == "stopped"
    # Browser Use ending the session itself is its outage, retried like a 503, not its agent's failure.
    session.output, session.status = "Task ended unexpectedly.", SimpleNamespace(value="error")
    with pytest.raises(Unavailable):
        await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None)

    # Any other error ending is its agent's failure, scored like one.
    session.output = "Failed to complete the task"
    outcome, report = await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None)
    assert report.status == "error"

    # A session the attempt's cap cancels is stopped, not left running and billing with nothing waiting for it.
    class Stuck(Run):
        def __await__(self) -> Any:
            return asyncio.Event().wait().__await__()

    stop = AsyncMock()
    monkeypatch.setattr(Client, "run", lambda *_, **__: Stuck())
    monkeypatch.setattr(
        Client,
        "__init__",
        lambda self, **_: setattr(self, "sessions", SimpleNamespace(get=AsyncMock(return_value=session), stop=stop)),
    )
    with pytest.raises(TimeoutError):
        async with asyncio.timeout(0.05):
            await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None)
    stop.assert_awaited_once_with("s1")


async def test_a_hosted_run_is_timed_to_its_agents_answer_not_to_the_session_stopping(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """0.5.7: Browser Use reported sessions stopped up to 114s after its agent's `done`, and that wait was timed."""
    import browser_use_sdk.v3  # an optional extra

    created = datetime(2026, 9, 25, 22, 0, tzinfo=UTC)
    session = SimpleNamespace(
        id="s1",
        created_at=created,
        is_task_successful=False,
        total_cost_usd=0.1,
        status=SimpleNamespace(value="stopped"),
        model="m",
        step_count=2,
        live_url=None,
    )

    def message(kind: str, after: float, error: bool = False) -> SimpleNamespace:
        data = json.dumps({"tool_name": "done", "is_error": error})
        return SimpleNamespace(id=kind, type=kind, data=data, created_at=created + timedelta(seconds=after))

    messages = [message("assistant_message", 3), message("completion_result", 7), message("completion_result", 9, True)]

    class Run:
        session_id = "s1"

        def __await__(self) -> Any:
            async def finish() -> SimpleNamespace:
                await asyncio.sleep(0.05)
                return SimpleNamespace(session=session, output="0.28.1")

            return finish().__await__()

    class Client:
        def __init__(self, **_: object) -> None:
            self.sessions = SimpleNamespace(
                get=AsyncMock(return_value=session),
                messages=AsyncMock(return_value=SimpleNamespace(messages=messages, has_more=False)),
            )

        def run(self, *_: object, **__: object) -> Run:
            return Run()

    monkeypatch.setattr(browser_use_sdk.v3, "AsyncBrowserUse", Client)
    monkeypatch.setattr(live, "load_settings", lambda: SimpleNamespace(browser_key=lambda: "key"))
    _, report = await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None)
    assert report.answered is True and report.task_successful is False
    answered = await live.hosted_answer(Client(), "s1", "0.28.1")
    assert answered == created + timedelta(seconds=7)  # the `done` that was an error is not an answer
    # An agent that answered in a reply without calling `done` answered at that reply, if the session kept it.
    messages[1:] = []
    assert await live.hosted_answer(Client(), "s1", "0.28.1") == created + timedelta(seconds=3)
    assert await live.hosted_answer(Client(), "s1", None) is None
    # Half a second until the session existed, then 7s by its own clock; never past the session's own wall time.
    assert live.answer_seconds(0.5, session, answered, 120.0) == 7.5
    assert live.answer_seconds(0.5, session, answered, 3.0) == 3.0
    assert live.answer_seconds(0.5, session, None, 120.0) == 120.0


async def test_a_hosted_outage_retries_without_quoting_the_key(monkeypatch: pytest.MonkeyPatch) -> None:
    import browser_use_sdk.v3  # an optional extra

    class Run:
        session_id = "s1"

        def __await__(self) -> Any:
            async def fail() -> None:
                raise browser_use_sdk.v3.BrowserUseError(503, "upstream echoed bu_secret_key")

            return fail().__await__()

    class Client:
        def __init__(self, **_: object) -> None:
            self.sessions = SimpleNamespace(get=AsyncMock())

        def run(self, *_: object, **__: object) -> Run:
            return Run()

    monkeypatch.setattr(browser_use_sdk.v3, "AsyncBrowserUse", Client)
    monkeypatch.setattr(live, "load_settings", lambda: SimpleNamespace(browser_key=lambda: "bu_secret_key"))
    with pytest.raises(Unavailable) as error:
        await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None)
    assert "bu_secret_key" not in str(error.value) and "HTTP 503" in str(error.value)


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


async def test_hosted_idle_session_stops_after_the_published_reply(monkeypatch: pytest.MonkeyPatch) -> None:
    import browser_use_sdk.v3

    stopped = asyncio.Event()
    session = SimpleNamespace(
        id="pause",
        output="Please confirm the prepared change",
        total_cost_usd=0.23,
        status=SimpleNamespace(value="stopped"),
        model="hosted",
        step_count=5,
        live_url=None,
    )
    seen = {}

    class Run:
        session_id = "pause"

        def __await__(self) -> Any:
            async def finish() -> Any:
                await stopped.wait()
                return SimpleNamespace(session=session, output=None)

            return finish().__await__()

    class Client:
        def __init__(self, **_: object) -> None:
            self.sessions = SimpleNamespace(
                get=AsyncMock(return_value=session),
                stop=AsyncMock(side_effect=lambda _: stopped.set()),
                messages=AsyncMock(return_value=SimpleNamespace(messages=[], has_more=False)),
            )

        def run(self, *_: object, **kwargs: object) -> Run:
            seen.update(kwargs)
            return Run()

    monkeypatch.setattr(browser_use_sdk.v3, "AsyncBrowserUse", Client)
    monkeypatch.setattr(live, "load_settings", lambda: SimpleNamespace(browser_key=lambda: "key"))
    async with httpx.AsyncClient() as http:
        outcome, report = await live.hosted_arm(
            task("pypi-newer"), http, record=None, max_dollars=2.0, stop_at_answer=True
        )
    assert stopped.is_set()
    assert outcome.answer == session.output and report.dollars == 0.23
    assert seen["max_cost_usd"] == 2.0


async def test_hosted_poll_failure_stops_session_before_retry(monkeypatch: pytest.MonkeyPatch) -> None:
    import browser_use_sdk.v3

    cancelled = asyncio.Event()
    sessions = SimpleNamespace(
        get=AsyncMock(side_effect=[SimpleNamespace(live_url=None), httpx.ReadError("poll failed")]),
        stop=AsyncMock(),
    )

    class Run:
        session_id = "poll-failure"

        def __await__(self) -> Any:
            async def finish() -> None:
                try:
                    await asyncio.Event().wait()
                finally:
                    cancelled.set()

            return finish().__await__()

    client = SimpleNamespace(run=lambda *args, **kwargs: Run(), sessions=sessions)
    monkeypatch.setattr(browser_use_sdk.v3, "AsyncBrowserUse", lambda **kwargs: client)
    monkeypatch.setattr(live, "load_settings", lambda: SimpleNamespace(browser_key=lambda: "key"))
    async with httpx.AsyncClient() as http:
        with pytest.raises(live.Unavailable, match="ReadError"):
            await live.hosted_arm(task("pypi-newer"), http, record=None, stop_at_answer=True)
    assert cancelled.is_set()
    sessions.stop.assert_awaited_once_with("poll-failure")


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
    await started.wait()
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
    await started.wait()
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
    await started.wait()
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


@pytest.mark.parametrize(("evidence", "passed"), [(_evidence(), True), (_evidence(status=419), False), (None, False)])
def test_a_navigation_url_alone_does_not_pass(evidence: PageEvidence | None, passed: bool) -> None:
    wiki = task("wiki-open")
    url = "https://en.wikipedia.org/wiki/G%C3%B6del%27s_incompleteness_theorems"
    outcome = Outcome(None, None, url, evidence=evidence)
    report = live.ArmReport(status="done", dollars=0.0, seconds=1.0)
    correct, failure, _ = live.grade("jev-ultrafast", wiki, None, outcome, report)
    assert correct is passed
    assert (failure is None) is passed


def test_an_answer_task_is_not_gated_on_page_evidence() -> None:
    outcome = Outcome("Attention Is All You Need", None, "https://arxiv.org/abs/1706.03762")
    report = live.ArmReport(status="complete", dollars=0.0, seconds=1.0)
    assert live.grade("fastbrowse", task("arxiv-title"), "Attention Is All You Need", outcome, report)[0] is True


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
            self.state: dict[str, Any] = {"page": page, "decisions": []}

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
        row = await live.run_arm(arm, task("pypi-open"), None, http, Path(), bitwarden=False, record=None)
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
