"""The six tasks of the head-to-head, their graders, and a check of each expected answer against the live site.

Prompts and starts are copied verbatim from `src/fastbrowse/evals/public-tasks.json`; README.md says how the
set was chosen. A grader reads only the answer text and, for the checkout, the final URL.
"""

import html
import json
import re
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlsplit

CATALOG = Path(__file__).parents[2] / "src" / "fastbrowse" / "evals" / "public-tasks.json"
USER_AGENT = "fastbrowse-head-to-head-truth-check"


@dataclass(frozen=True)
class Task:
    id: str
    length: str
    needles: tuple[str, ...] = ()
    """Every one of these appears in the answer."""
    pairs: tuple[tuple[str, str], ...] = ()
    """Each value appears after a mention of its name, before the next name."""
    final_path: str | None = None
    secrets: dict[str, str] = field(default_factory=dict)
    authorize: bool = False


TASKS = (
    Task("books-travel-priciest", "short", needles=("A Year in Provence", "56.88")),
    Task("quotes-search", "short", needles=("man of success", "man of value")),
    Task("dynamic-loading", "short", needles=("Hello World",)),
    Task(
        "stretch-books-nonfiction-five-star",
        "long",
        pairs=(
            ("Agnostic: A Spirited Manifesto", "12.51"),
            ("Disrupted: My Misadventure in", "15.28"),
            ("Mother, Can You Not?", "16.89"),
        ),
    ),
    Task(
        "stretch-quotes-top-authors",
        "long",
        pairs=(("Albert Einstein", "10"), ("J.K. Rowling", "9"), ("Marilyn Monroe", "7")),
    ),
    Task(
        "saucedemo-checkout",
        "long",
        needles=("43.18",),
        final_path="/checkout-complete.html",
        # The practice site prints these two on its own sign-in page.
        secrets={"username": "standard_user", "password": "secret_sauce"},
        authorize=True,
    ),
)


def catalog_entry(task: Task) -> dict[str, str]:
    entries = json.loads(CATALOG.read_text(encoding="utf-8"))["tasks"]
    return next(entry for entry in entries if entry["id"] == task.id)


def goal(task: Task) -> tuple[str, str]:
    """The start URL and what both arms are asked, worded as the live evals word it (`live_tasks.prompt`)."""
    entry = catalog_entry(task)
    return entry["start"], f"Start at {entry['start']}. {entry['task']}"


def _flat(text: str) -> str:
    """Case, spacing, and the thousands separators and curly quotes a writer may add or drop, carry no meaning here."""
    text = text.casefold().translate({0x2019: "'", 0x201C: '"', 0x201D: '"'})
    return " ".join(re.sub(r"(?<=\d),(?=\d{3})", "", text).split())


def _prose(answer: str) -> str:
    """The answer without citations: a numbered link or a URL carries digits that are not the answer's."""
    text = re.sub(r"\[(\d+)\]\(<?[^)>]*>?\)|\[\d+\]|https?://\S+", " ", answer)
    return _flat(re.sub(r"\[([^\]]*)\]\(<?[^)>]*>?\)", r"\1", text).replace("*", ""))


def _states(value: str, text: str) -> bool:
    """The value as a whole number, with only zero cents allowed: 399 is not 1399 or 399.99."""
    return re.search(rf"(?<![\d.]){re.escape(value)}(?:\.0+)?(?!\.?\d)", text) is not None


def grade(task: Task, *, ended: bool, answer: str | None, final_url: str | None) -> str | None:
    """Why this run failed, or None when it passed. A run passes only by ending normally with the right answer."""
    if not ended:
        return "the run did not end normally"
    if not answer:
        return "no answer"
    prose = _prose(answer)
    if missing := [needle for needle in task.needles if _flat(needle) not in prose]:
        return f"answer lacks {missing}"
    names = [_flat(name) for name, _ in task.pairs]
    for name, value in task.pairs:
        spans = []
        for match in re.finditer(re.escape(_flat(name)), prose):
            later = [at for other in names if (at := prose.find(other, match.end())) >= 0]
            spans.append(prose[match.end() : min(later, default=len(prose))])
        if not spans:
            return f"answer does not name {name!r}"
        if not any(_states(value, span) for span in spans):
            return f"answer does not give {value} for {name!r}"
    if task.final_path is not None and urlsplit(final_url or "").path != task.final_path:
        return f"ended on {final_url!r}, not {task.final_path}"
    return None


def _get(url: str) -> str:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=30) as response:
        return response.read().decode("utf-8")


def _books(category: str) -> list[tuple[str, str, float]]:
    """(title, star rating, price) of every book in a books.toscrape.com category, from its plain HTML."""
    base = f"https://books.toscrape.com/catalogue/category/books/{category}/"
    books, page = [], "index.html"
    while page:
        body = _get(base + page)
        for card in re.findall(r"<article class=\"product_pod\">.*?</article>", body, re.S):
            rating = re.search(r"star-rating (\w+)", card)
            title = re.search(r'title="([^"]+)"', card.split("<h3>")[1])
            price = re.search(r'price_color">[^0-9]*([0-9.]+)', card)
            assert rating and title and price
            books.append((html.unescape(title.group(1)), rating.group(1), float(price.group(1))))
        following = re.search(r'<li class="next"><a href="([^"]+)"', body)
        page = following.group(1) if following else ""
    return books


def verify_truth() -> dict[str, str]:
    """The expected answers as the sites serve them now, without a browser or a model; raises when one moved.

    The two tasks that need a browser to reach their answer are recorded as what they are.
    """
    checked: dict[str, str] = {}
    travel = max(_books("travel_2"), key=lambda book: book[2])
    assert travel[0].startswith("A Year in Provence") and travel[2] == 56.88, travel
    checked["books-travel-priciest"] = f"{travel[0]} {travel[2]}"

    five = sorted((b for b in _books("nonfiction_13") if b[1] == "Five"), key=lambda book: book[2])[:3]
    assert [b[2] for b in five] == [12.51, 15.28, 16.89], five
    assert [b[0].startswith(p[0]) for b, p in zip(five, TASKS[3].pairs, strict=True)] == [True] * 3, five
    checked["stretch-books-nonfiction-five-star"] = "; ".join(f"{b[0]} {b[2]}" for b in five)

    authors: dict[str, int] = {}
    page = 1
    while True:
        body = _get(f"https://quotes.toscrape.com/page/{page}/")
        for author in re.findall(r'<small class="author"[^>]*>([^<]+)</small>', body):
            name = html.unescape(author)
            authors[name] = authors.get(name, 0) + 1
        if 'class="next"' not in body:
            break
        page += 1
    top = sorted(authors.items(), key=lambda item: -item[1])[:3]
    assert top == [("Albert Einstein", 10), ("J.K. Rowling", 9), ("Marilyn Monroe", 7)], top
    checked["stretch-quotes-top-authors"] = f"{top} over {page} pages"

    checked["quotes-search"] = "unverified here: the answer sits behind the site's search form"
    checked["dynamic-loading"] = "unverified here: the text appears only after the page's script runs"
    checked["saucedemo-checkout"] = "fixed by the site: (29.99 + 9.99) with 8% tax = 43.18"
    return checked


if __name__ == "__main__":
    print(json.dumps(verify_truth(), indent=2))
