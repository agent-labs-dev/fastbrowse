"""The Jev call report: requests named by their span, counted per purpose and step, and repeats found."""

from fastbrowse_evals.jev_calls import report, requests, runs


def span(purpose: str, began: float, ended: float) -> dict[str, object]:
    return {"event": "call", "call": "jev", "decision_purpose": purpose, "began": began, "ended": ended}


def sent(state: object, questions: dict[str, object], at: float, cost: float = 0.001) -> dict[str, object]:
    return {
        "event": "http",
        "url": "https://openrouter.ai/api/v1/systemone",
        "request": {"model": "jev-1.14", "state": state, "questions": questions},
        "response": {"usage": {"cost": cost}},
        "began": at,
        "ended": at + 0.1,
    }


EVENTS = [
    span("action", 0.0, 1.0),
    sent({"page": 1}, {"operation": {}}, 0.1),
    {"event": "page", "step": 0},
    span("verify", 2.0, 3.0),
    sent({"notes": 1}, {"t0/n0": {}}, 2.1),
    span("verify", 4.0, 5.0),
    sent({"notes": 1}, {"t0/n0": {}}, 4.1),
    span("verify", 6.0, 7.0),
    sent({"notes": 1}, {"t0/n1": {}}, 6.1),
    {"event": "http", "url": "https://example.test/", "request": "GET", "began": 6.5, "ended": 6.6},
]


def test_each_request_is_named_by_the_span_it_was_sent_in_and_placed_at_its_step() -> None:
    found = requests(EVENTS)

    assert [(r.purpose, r.step) for r in found] == [("action", 0), ("verify", 1), ("verify", 1), ("verify", 1)]
    assert found[1].request == found[2].request
    assert found[2].state == found[3].state != found[0].state


def test_the_report_counts_calls_by_purpose_and_the_requests_a_run_repeated() -> None:
    rows = [
        {"arm": "fastbrowse", "task": "pypi-newer", "events": EVENTS},
        {"arm": "browser-use", "task": "pypi-newer", "events": EVENTS},
        {"arm": "fastbrowse", "task": "untraced"},
    ]

    table = report(list(runs(rows)))

    assert table.startswith("1 runs, 4 Jev calls: median 4 and mean 4.00 a run.")
    assert "| verify | 3.00 | 3 | 75% | $0.00300 | 1.00 | 2.00 |" in table
    assert "| action | 1.00 | 1 | 25% | $0.00100 | 0.00 | 0.00 |" in table
    assert "| pypi-newer | verify | 1 | 3 |" in table
