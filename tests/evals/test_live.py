import asyncio
import logging
import runpy
from base64 import urlsafe_b64decode, urlsafe_b64encode
from datetime import date, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock

import httpx
import pytest

from fastbrowse.evals import live, live_tasks, more_tasks
from fastbrowse.evals.live_tasks import TASKS, LiveTask, Outcome
from fastbrowse.models import Unavailable
from fastbrowse.telemetry import TRACE, trace, traced

RUNNER: dict[str, Any] = runpy.run_path(str(live.ULTRAFAST_RUNNER))


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
        outcome = Outcome("Done.", None, "https://pypi.org/project/httpx/")
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
    # The Browser Use agent reports no page, so only its answer is graded.
    assert check(Outcome("JetBlue, $846", None, None), None) is None


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
            self.sessions = SimpleNamespace(get=AsyncMock(return_value=session))

        def run(self, *_: object, **__: object) -> Run:
            return Run()

    monkeypatch.setattr(browser_use_sdk.v3, "AsyncBrowserUse", Client)
    monkeypatch.setattr(live, "load_settings", lambda: SimpleNamespace(browser_key=lambda: "key"))
    outcome, report = await live.hosted_arm(task("pypi-newer"), httpx.AsyncClient(), record=None)
    assert outcome.answer == "[Session cost limit reached]"
    assert report.dollars == 0.37 and report.status == "stopped"


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


def test_the_first_friday_grader_takes_the_date_as_the_page_shows_it() -> None:
    truth = {"date": "2026-10-02", "mdy": "10/02/2026"}
    shown = "The date shown is 10/02/2026. The day shown is Friday. The month shown is October."
    assert more_tasks._first_friday_check(Outcome(shown, None, None), truth) is None
    assert more_tasks._first_friday_check(Outcome("Friday 2 October 2026", None, None), truth) is None
    assert more_tasks._first_friday_check(Outcome("It shows 10/09/2026, Friday, October.", None, None), truth)
    assert more_tasks._first_friday_check(Outcome("It shows 10/02/2026.", None, None), truth)
