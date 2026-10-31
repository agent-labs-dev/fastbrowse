"""Each arm's own ending, as publication reads it."""

import pytest

from fastbrowse.evals.status import Ending, normalize


@pytest.mark.parametrize(
    ("status", "ending"),
    [
        ("done", Ending.DONE),
        ("budget_exceeded", Ending.BUDGET),
        ("blocked", Ending.BLOCKED),
        ("stalled", Ending.STALLED),
        ("unavailable", Ending.UNAVAILABLE),
    ],
)
def test_an_arms_own_status_keeps_its_ending(status: str, ending: Ending) -> None:
    assert normalize(status) == ending


def test_a_stalled_run_is_a_measured_row_and_not_an_outage() -> None:
    """The Ultrafast arm stops itself in a decision loop (`stalled`), and campaign 41133404129 lost six rows because
    that loop ended in a dropped connection instead, which read as an outage the harness retried forever. A stalled
    ending is the arm's own result: published as a row failure, never re-run as a provider outage."""
    assert normalize("stalled") is not Ending.UNAVAILABLE
    assert normalize("Stalled") is Ending.STALLED
    assert Ending.STALLED.value == "stalled"


def test_an_unknown_status_is_still_an_error() -> None:
    assert normalize("something-new") is Ending.ERROR
    assert normalize(None) is Ending.ERROR
