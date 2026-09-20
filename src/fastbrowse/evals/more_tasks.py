"""Tasks beyond the core suite, split into a dev set and a held-out set.

The core suite found the fixes it now scores well on, so it no longer says whether a change helps browsing in
general. These tasks cover skills it barely touches (pagination, frames, new windows, hover, script-rendered
pages, server-rendered forms) on sites the agent was never tuned against. Agent changes are iterated against
`DEV` only; `HELDOUT` is run before and after a round of changes and never debugged, so its score is the honest
measure of whether a round improved the agent or only its dev score. Each pair of tasks across the split
exercises the same skill.

Truth is fixed by a practice site, or fetched from a live API at run time, as in `live_tasks`.
"""

import re
from urllib.parse import unquote, urlparse

import httpx

from fastbrowse.evals.live_tasks import Category, Check, LiveTask, Outcome, Truth


async def _json(http: httpx.AsyncClient, url: str) -> object:
    response = await http.get(url, headers={"Accept": "application/json"})
    response.raise_for_status()
    return response.json()


async def _constant(value: object) -> object:
    return value


def _fixed(value: object) -> Truth:
    return lambda _: _constant(value)


async def _ruff_release(http: httpx.AsyncClient) -> object:
    body = await _json(http, "https://api.github.com/repos/astral-sh/ruff/releases/latest")
    assert isinstance(body, dict)
    return str(body["tag_name"])


async def _serde_version(http: httpx.AsyncClient) -> object:
    # crates.io refuses API requests that do not say who is asking.
    response = await http.get(
        "https://crates.io/api/v1/crates/serde", headers={"User-Agent": "fastbrowse-evals (github.com/agent-labs-dev)"}
    )
    response.raise_for_status()
    return str(response.json()["crate"]["max_stable_version"])


async def _httpx_requires(http: httpx.AsyncClient) -> object:
    body = await _json(http, "https://pypi.org/pypi/httpx/json")
    assert isinstance(body, dict)
    # ">=3.8" is answered as "3.8", "Python 3.8 or later", and so on: the version number is the fact.
    return str(body["info"]["requires_python"]).lstrip("<>=~! ")


def _flat(text: str) -> str:
    """Case, and the thousands separators and curly quotes a writer may add or drop, carry no meaning here."""
    text = text.casefold().translate({0x2019: "'", 0x201C: '"', 0x201D: '"'})
    return re.sub(r"(?<=\d),(?=\d{3})", "", text)


def _has(*needles: str) -> Check:
    """The answer names every one of the needles."""

    def check(outcome: Outcome, _: object) -> str | None:
        answer = _flat(outcome.answer or "")
        missing = [n for n in needles if _flat(n) not in answer]
        return f"answer lacks {missing}: {outcome.answer!r}" if missing else None

    return check


def _has_truth(outcome: Outcome, truth: object) -> str | None:
    return _has(str(truth))(outcome, truth)


def _exactly(needle: str) -> Check:
    """Case matters: the page's own capitals are the evidence that the answer was read, not echoed from the task."""

    def check(outcome: Outcome, _: object) -> str | None:
        return None if needle in (outcome.answer or "") else f"answer lacks {needle!r}: {outcome.answer!r}"

    return check


def _submitted(outcome: Outcome, truth: object) -> str | None:
    if outcome.final_url is not None and unquote(urlparse(outcome.final_url).path).rstrip("/") != "/post":
        return f"ended on {outcome.final_url}, not on the page the form posts to"
    return _has("large")(outcome, truth)


DEV: tuple[LiveTask, ...] = (
    LiveTask(
        "books-travel-priciest",
        "https://books.toscrape.com/",
        "Which is the most expensive book in the Travel category, and what does it cost?",
        _fixed(None),
        _has("A Year in Provence", "56.88"),
        Category.LOOKUP,
    ),
    LiveTask(
        "hockey-bruins-1990",
        "https://www.scrapethissite.com/pages/forms/",
        "How many games did the Boston Bruins win in the 1990 season?",
        _fixed(None),
        _has("44"),
        Category.LOOKUP,
    ),
    LiveTask(
        "oscars-2012",
        "https://www.scrapethissite.com/pages/ajax-javascript/",
        "Of the 2012 films listed here, which one won Best Picture?",
        _fixed(None),
        _has("Argo"),
        Category.LOOKUP,
    ),
    LiveTask(
        "dynamic-loading",
        "https://the-internet.herokuapp.com/dynamic_loading/2",
        "Start the example and tell me the text that appears when loading finishes.",
        _fixed(None),
        _has("Hello World"),
        Category.WIDGET,
    ),
    LiveTask(
        "nested-frames",
        "https://the-internet.herokuapp.com/nested_frames",
        "What text does the frame in the middle of the top row show?",
        _fixed(None),
        _exactly("MIDDLE"),
        Category.WIDGET,
    ),
    LiveTask(
        "hover-profile",
        "https://the-internet.herokuapp.com/hovers",
        "Which user name is revealed when you hover over the second profile picture?",
        _fixed(None),
        _has("user2"),
        Category.WIDGET,
    ),
    LiveTask(
        "ruff-release",
        "https://github.com/astral-sh/ruff",
        "What is the latest release of ruff on GitHub?",
        _ruff_release,
        _has_truth,
        Category.LOOKUP,
    ),
    LiveTask(
        "pizza-order",
        "https://httpbin.org/forms/post",
        "Order a large pizza with mushroom for Ada Lovelace, telephone 020 7946 0000, email ada@example.com, "
        "and submit it. Tell me which size the server received.",
        _fixed(None),
        _submitted,
        Category.CHECKOUT,
        authorize=True,
    ),
)

HELDOUT: tuple[LiveTask, ...] = (
    LiveTask(
        "books-mystery-cheapest",
        "https://books.toscrape.com/",
        "Which is the cheapest book in the Mystery category, and what does it cost?",
        _fixed(None),
        _has("Tastes Like Fear", "10.69"),
        Category.LOOKUP,
    ),
    LiveTask(
        "quotes-einstein-count",
        "https://quotes.toscrape.com/",
        "How many quotes by Albert Einstein are there across the whole site?",
        _fixed(None),
        _has("10"),
        Category.LOOKUP,
    ),
    LiveTask(
        "countries-mongolia",
        "https://www.scrapethissite.com/pages/simple/",
        "What population does this page list for Mongolia?",
        _fixed(None),
        _has("3086918"),
        Category.LOOKUP,
    ),
    LiveTask(
        "quotes-js-page2",
        "https://quotes.toscrape.com/js/",
        "Who wrote the first quote on the second page?",
        _fixed(None),
        _has("Marilyn Monroe"),
        Category.LOOKUP,
    ),
    LiveTask(
        "crates-serde",
        "https://crates.io/",
        "What is the latest stable version of the serde crate?",
        _serde_version,
        _has_truth,
        Category.LOOKUP,
    ),
    LiveTask(
        "new-window",
        "https://the-internet.herokuapp.com/windows",
        "Follow the link that opens a new window and tell me that window's heading.",
        _fixed(None),
        _has("New Window"),
        Category.WIDGET,
    ),
    LiveTask(
        "table-largest-due",
        "https://the-internet.herokuapp.com/tables",
        "In the first table, whose amount due is the largest?",
        _fixed(None),
        _has("Jason", "Doe"),
        Category.WIDGET,
    ),
    LiveTask(
        "httpx-requires-python",
        "https://pypi.org/",
        "What is the oldest Python version the latest httpx release supports?",
        _httpx_requires,
        _has_truth,
        Category.LOOKUP,
    ),
    LiveTask(
        "quotes-search",
        "https://quotes.toscrape.com/search.aspx",
        "Use the search form to find Albert Einstein's quote tagged success, and tell me what it says.",
        _fixed(None),
        _has("man of success", "man of value"),
        Category.LOOKUP,
    ),
)
