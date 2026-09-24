"""The dev and held-out graders accept a right answer in the forms a writer uses and reject a wrong one."""

import pytest

from fastbrowse.evals.live_tasks import LiveTask, Outcome
from fastbrowse.evals.more_tasks import DEV, HELDOUT

RIGHT = {
    "books-travel-priciest": ("A Year in Provence (Provence #1), at £56.88.", None),
    "hockey-bruins-1990": ("The Boston Bruins won 44 games in 1990.", None),
    "oscars-2012": ("Argo won Best Picture.", None),
    "dynamic-loading": ("It shows \u201cHello World!\u201d.", None),
    "nested-frames": ("The middle frame shows MIDDLE.", None),
    "hover-profile": ("name: user2", None),
    "ruff-release": ("The latest release is 0.16.8.", "0.16.8"),
    "pizza-order": ("The server received size: large.", None),
    "books-mystery-cheapest": ("Tastes Like Fear (DI Marnie Rome #3) costs £10.69.", None),
    "quotes-einstein-count": ("There are 10 quotes by Albert Einstein.", None),
    "countries-mongolia": ("Mongolia's population is listed as 3,086,918.", None),
    "quotes-js-page2": ("Marilyn Monroe wrote it.", None),
    "crates-serde": ("serde 1.0.229", "1.0.229"),
    "new-window": ("The new window's heading is New Window.", None),
    "table-largest-due": ("Jason Doe, who owes $100.00.", None),
    "httpx-requires-python": ("Python 3.8 or later.", "3.8"),
    "quotes-search": ("\u201cTry not to become a man of success. Rather become a man of value.\u201d", None),
}


@pytest.mark.parametrize("task", [*DEV, *HELDOUT], ids=lambda t: t.id)
def test_right_answers_pass_and_wrong_ones_fail(task: LiveTask) -> None:
    answer, truth = RIGHT[task.id]
    final_url = "https://httpbin.org/post" if task.id == "pizza-order" else None
    assert task.check(Outcome(answer=answer, data=None, final_url=final_url), truth) is None
    assert task.check(Outcome(answer="I could not find it.", data=None, final_url=final_url), truth) is not None


def test_the_split_is_disjoint_and_covers_every_task() -> None:
    ids = [t.id for t in (*DEV, *HELDOUT)]
    assert len(ids) == len(set(ids)) == len(RIGHT)


def test_nested_frames_needs_the_page_capitals() -> None:
    (task,) = [t for t in DEV if t.id == "nested-frames"]
    assert task.check(Outcome(answer="The middle frame.", data=None, final_url=None), None) is not None


def test_an_order_that_never_reached_the_server_fails() -> None:
    (task,) = [t for t in DEV if t.id == "pizza-order"]
    outcome = Outcome(answer="Size large.", data=None, final_url="https://httpbin.org/forms/post")
    assert task.check(outcome, None) is not None
