import runpy
from datetime import date, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock

import httpx
import pytest

from fastbrowse.evals import live
from fastbrowse.evals.live_tasks import TASKS, LiveTask, Outcome

RUNNER: dict[str, Any] = runpy.run_path(str(live.ULTRAFAST_RUNNER))


def task(task_id: str) -> LiveTask:
    return next(t for t in TASKS if t.id == task_id)


def test_videos_count_up_past_existing_files(tmp_path: Path) -> None:
    first = live.video_path(tmp_path, "fast", task("hn-top"))
    first.write_bytes(b"")
    assert first == tmp_path.resolve() / "fast" / "hn-top-1.mp4"
    assert live.video_path(tmp_path, "fast", task("hn-top")).name == "hn-top-2.mp4"
    assert live.video_path(tmp_path, "ultrafast", task("hn-top")).name == "hn-top-1.mp4"


@pytest.mark.parametrize(("status", "passed"), [("done", True), ("blocked", False)])
async def test_ultrafast_passes_only_on_a_correct_outcome_it_called_done(
    monkeypatch: pytest.MonkeyPatch, status: str, passed: bool
) -> None:
    arxiv = task("arxiv-title")

    async def ultrafast_arm(
        _: LiveTask, __: httpx.AsyncClient, *, record: Path | None
    ) -> tuple[Outcome, dict[str, object]]:
        # jev-ultrafast has no answer; an outcome that has one stands in for a grader that needs none.
        outcome = Outcome("Attention Is All You Need", None, "https://arxiv.org/abs/1706.03762")
        return outcome, {"status": status, "dollars": 0.001, "seconds": 3.0}

    monkeypatch.setattr(live, "ultrafast_arm", ultrafast_arm)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("ultrafast", arxiv, http, Path(), bitwarden=False, record=None)
    assert row["correct"] is True
    assert row["passed"] is passed
    assert row["seconds"] == 3.0


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
        if "ultrafast" in live_task.arms:
            # jev-ultrafast has no answer, so its tasks must be graded on the page alone.
            assert live_task.output_schema is None
            assert "hosted" not in live_task.arms
    assert {t.id for t in TASKS if "ultrafast" in t.arms} >= {"wiki-open", "flights-search"}


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
    # Hosted Browser Use reports no page, so only its answer is graded.
    assert check(Outcome("JetBlue, $846", None, None), None) is None


def test_a_repeated_label_passes_when_any_control_holds_the_value() -> None:
    day = date.today() + timedelta(days=28)
    page = (("Where from?", ""), *_flights_page(day))
    assert (
        task("flights-search").check(Outcome(None, None, "https://www.google.com/travel/flights", controls=page), None)
        is None
    )


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
    assert report["dollars"] == 0.37 and report["status"] == "stopped"
