"""A ranking, count or comparison computed across pages can be stated and verified from the records it rests on.

The 0.5.22 evals read every quote on quotes.toscrape.com's ten pages and counted them by author, then ended stuck:
a claim that J.K. Rowling ranks second cited her own tally, so the output audit saw her nine quotes and nothing
they beat. On the Nonfiction category's six pages, the audit refused a comparison code had closed because page 4
held no five-star book and so no cited record. The records here are the ones those runs read.
"""

import json
import re
from datetime import UTC, datetime
from pathlib import Path

import pytest

from fastbrowse.config import Config
from fastbrowse.memory import Comparison, Fact, Notes, Tally, fact_id
from fastbrowse.models import Evidence, FactReader
from fastbrowse.planner import Requirement, RequirementKind
from fastbrowse.retrieval import Claim, assemble_answer, claim_check_questions, partial_answer
from fastbrowse.verification import _output_context, check_answer_outputs
from tests.test_answer_repair import RoutingJev
from tests.test_retrieval import ScriptedLLM

QUOTES = json.loads((Path(__file__).parent / "data" / "quotes_toscrape_records.json").read_text())["records"]
AUTHORS = Requirement(
    id="top_authors",
    text="Find the three authors with the most quotes across every page of the site and how many each has.",
    kind=RequirementKind.INFORMATION,
)
NONFICTION = "https://books.toscrape.com/catalogue/category/books/nonfiction_13/"
# Every five-star record the 0.5.22 run read, by page; page-4.html showed only its heading.
FIVE_STAR = {
    "index.html": ["Worlds Elsewhere: Journeys Around ...\n\n£40.30", "#HigherSelfie: Wake Up Your ...\n\n£23.11"],
    "page-2.html": ["Agnostic: A Spirited Manifesto\n\n£12.51", "Let It Out: A ...\n\n£26.79"],
    "page-3.html": ["Mother, Can You Not?\n\n£16.89", "Smarter Faster Better: The ...\n\n£38.89"],
    "page-4.html": [],
    "page-5.html": ["Disrupted: My Misadventure in ...\n\n£15.28", "Blink: The Power of ...\n\n£21.74"],
    "page-6.html": ["H is for Hawk\n\n£57.42", "The Tumor\n\n£41.56"],
}
CHEAPEST = Requirement(
    id="cheapest",
    text="Find the three cheapest five-star-rated books across every page of the Nonfiction category.",
    kind=RequirementKind.INFORMATION,
)


_OFFSETS: dict[str, int] = {}


def _record(url: str, quote: str) -> Fact:
    """A quote at its own span of its page's capture: two quotes of one length on a page are still two records."""
    start = _OFFSETS.get(url, 0)
    _OFFSETS[url] = start + len(quote) + 2
    span = Evidence(
        source_id=f"s{start}",
        url=url,
        frame_id=None,
        captured_at=datetime(2026, 10, 1, tzinfo=UTC),
        capture_sha256=f"sha-{url}",
        start=start,
        end=start + len(quote),
        quote=quote,
    )
    return Fact(text=quote, evidence=span, reader=FactReader.LLM)


def _author(quote: str) -> str:
    match = re.search(r"\nby (.+?) \(about\)", quote)
    assert match is not None
    return match[1]


def counted_authors(*, complete: bool, continued: bool = True) -> tuple[Notes, dict[str, Fact]]:
    """The quote records as the run's tally reader grouped them, page by page."""
    _OFFSETS.clear()
    notes = Notes()
    groups: dict[str, list[str]] = {}
    for item in QUOTES:
        record = _record(item["url"], item["quote"])
        notes.add(record)
        if continued:
            notes.add_continuation(AUTHORS.id, fact_id(record))
        groups.setdefault(_author(item["quote"]), []).append(fact_id(record))
        notes.add_collection_page(AUTHORS.id, item["url"])
    tallies = {
        author: notes.add_tally(Tally(requirement_id=AUTHORS.id, key=author, records=tuple(records)))
        for author, records in groups.items()
    }
    if complete:
        notes.complete_tallies(AUTHORS.id)
    return notes, tallies


def test_the_fixture_is_the_sites_whole_collection() -> None:
    authors = [_author(item["quote"]) for item in QUOTES]
    assert len(QUOTES) == 100
    assert len({item["url"] for item in QUOTES}) == 10
    assert [authors.count(name) for name in ("Albert Einstein", "J.K. Rowling", "Marilyn Monroe")] == [10, 9, 7]
    notes, tallies = counted_authors(complete=True)
    # Every record keeps its own identity, so each tally holds only its author's quotes.
    assert len(notes.evidence) == 100
    for author, tally in tallies.items():
        assert {_author(notes.evidence[key].quote) for key in tally.basis} == {author}


@pytest.mark.parametrize("complete", [True, False])
def test_a_rank_by_count_is_shown_against_every_group_counted(complete: bool) -> None:
    notes, tallies = counted_authors(complete=complete)
    claim = Claim(text="J.K. Rowling has the second most quotes, 9.", evidence_ids=(fact_id(tallies["J.K. Rowling"]),))
    context = _output_context(assemble_answer((claim,), notes, (AUTHORS,)), notes)

    assert context is not None
    (count,) = context.claims[0].counted_records
    assert count.standing is not None
    assert count.standing.rank == 2
    assert count.standing.complete is complete
    # The groups at or above nine, then the next one down; the long tail of single quotes is left out.
    assert [(group.group, group.count) for group in count.standing.groups] == [
        ("Albert Einstein", 10),
        ("J.K. Rowling", 9),
        ("Marilyn Monroe", 7),
    ]
    assert count.standing.group_count == len({_author(item["quote"]) for item in QUOTES})
    # Only her own nine quotes are cited: the ranking does not need the other ninety-one.
    assert len(context.claims[0].cited_sources) == 9


def test_tied_counts_share_a_rank() -> None:
    notes, tallies = counted_authors(complete=True)
    for author in ("Dr. Seuss", "Mark Twain"):
        tally = tallies[author].tally
        assert tally is not None
        standing = notes.standing(tally)
        assert standing is not None and standing.rank == 4
        assert {group.group for group in standing.groups} >= {"Dr. Seuss", "Mark Twain"}


def test_a_single_group_has_no_standing() -> None:
    record = _record("https://quotes.toscrape.com/", QUOTES[0]["quote"])
    notes = Notes((record,))
    tally = notes.add_tally(Tally(requirement_id=AUTHORS.id, key="records", records=(fact_id(record),)))
    notes.complete_tallies(AUTHORS.id)
    claim = Claim(text="There is one quote.", evidence_ids=(fact_id(tally),))
    context = _output_context(assemble_answer((claim,), notes, (AUTHORS,)), notes)

    assert context is not None
    assert "standing" not in context.claims[0].model_dump()["counted_records"][0]


@pytest.mark.parametrize("complete", [True, False])
async def test_the_rank_audit_receives_code_counts_and_their_completeness(complete: bool) -> None:
    notes, tallies = counted_authors(complete=complete)
    check = "Report the author with the second most quotes and the count."
    claim = Claim(text="J.K. Rowling ranks second with 9 quotes.", evidence_ids=(fact_id(tallies["J.K. Rowling"]),))
    answer = assemble_answer((claim,), notes, (AUTHORS,))
    seen: list[dict] = []

    class RankJudge(ScriptedLLM):
        """Answers as the audit's rules read: a rank stands on complete code counts, never on an incomplete one."""

        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ != "_OutputIdentities":
                payload = json.loads(messages[-1].content)
                ((key, field),) = payload["criteria"].items()
                seen.append(field)
                claims = field.get("sources", field.get("reported_claims"))
                standing = claims[0]["counted_records"][0].get("standing")
                ranked = standing is not None and standing["complete"] and standing["rank"] == 2
                self.responses.append({"judgments": {key: "yes" if ranked else "no"}, "reason": "Code counts."})
            return await super().generate(purpose, messages, schema, **kwargs)

    missing: list[str] = []
    passed = await check_answer_outputs(RoutingJev(), RankJudge([]), answer, notes, (check,), missing_outputs=missing)

    assert passed is complete
    assert missing == ([] if complete else [check])
    standing = seen[0]["sources"][0]["counted_records"][0]["standing"]
    assert standing["group_count"] > 3 and standing["groups"][0] == {
        "group": "Albert Einstein",
        "count": 10,
        "complete": complete,
    }


def test_the_claim_check_sees_a_tallys_standing() -> None:
    # Records that are not also a comparison's operands are shown to the claim check as their tally's line.
    notes, tallies = counted_authors(complete=True, continued=False)
    claim = Claim(text="Marilyn Monroe ranks third with 7 quotes.", evidence_ids=(fact_id(tallies["Marilyn Monroe"]),))
    questions = claim_check_questions(assemble_answer((claim,), notes, (AUTHORS,)), notes, check_omission=False)

    assert '"standing": {"rank": 3' in questions["unsupported_0"].instructions


def test_a_partial_answer_states_every_counted_group_before_any_quote() -> None:
    notes, _ = counted_authors(complete=True)
    answer = partial_answer(notes, Config().observation.working_notes_chars).answer

    lines = [line for line in answer.splitlines() if line]
    assert lines[1:4] == ["Albert Einstein: 10", "J.K. Rowling: 9", "Marilyn Monroe: 7"]


def cheapest_five_star(*, pages: bool) -> tuple[Notes, Fact]:
    _OFFSETS.clear()
    notes = Notes()
    records = []
    for page, books in FIVE_STAR.items():
        url = NONFICTION + page
        for book in books:
            record = _record(url, f"{book}\n\nIn stock\n\nAdd to basket (Five stars)")
            notes.add(record)
            records.append(fact_id(record))
        if pages:
            notes.add_collection_page(CHEAPEST.id, url)
    winner = Fact(
        requirement_id=CHEAPEST.id,
        comparison=Comparison(requirement_id=CHEAPEST.id, records=tuple(records), complete=True),
        text="The three cheapest five-star books are Agnostic (12.51), Disrupted (15.28) and Mother (16.89).",
        evidence=None,
        basis=tuple(records),
        reader=FactReader.LLM,
    )
    notes.add(winner)
    return notes, winner


@pytest.mark.parametrize("pages", [True, False])
async def test_a_page_with_no_matching_record_still_counts_as_read(pages: bool) -> None:
    notes, winner = cheapest_five_star(pages=pages)
    claim = Claim(text="Agnostic: A Spirited Manifesto is the cheapest at 12.51.", evidence_ids=(fact_id(winner),))
    answer = assemble_answer((claim,), notes, (CHEAPEST,))
    context = _output_context(answer, notes)

    assert context is not None
    (compared,) = context.claims[0].compared_records
    cited = {context.urls[source.url_ref] for source in context.claims[0].cited_sources}
    assert NONFICTION + "page-4.html" not in cited
    assert [context.urls[ref] for ref in compared.pages_read] == (
        [NONFICTION + page for page in FIVE_STAR] if pages else []
    )

    sent: list[dict] = []

    class Recording(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ != "_OutputIdentities":
                payload = json.loads(messages[-1].content)
                sent.append(payload)
                self.responses.append({"judgments": {k: "yes" for k in payload["criteria"]}, "reason": "Read."})
            return await super().generate(purpose, messages, schema, **kwargs)

    await check_answer_outputs(RoutingJev(), Recording([]), answer, notes, ("Report the cheapest five-star book.",))
    # The audit resolves the page refs to addresses, the page without a match among them.
    assert (NONFICTION + "page-4.html" in sent[0]["urls"].values()) is pages


def test_scoped_notes_keep_their_requirements_pages() -> None:
    notes, _ = cheapest_five_star(pages=True)
    notes.add_collection_page("other", "https://example.test/")

    scoped = notes.for_requirements((CHEAPEST.id,))

    assert scoped.collection_pages(CHEAPEST.id) == tuple(NONFICTION + page for page in FIVE_STAR)
    assert scoped.collection_pages("other") == ()


async def test_every_page_of_a_continuing_list_is_recorded_whether_or_not_it_matched() -> None:
    import asyncio
    import time
    from unittest.mock import AsyncMock, Mock

    from fastbrowse.agent import Agent, _PageRead
    from fastbrowse.models import StepOutcome
    from fastbrowse.page import ActResult, BlockKind, Page
    from fastbrowse.planner import Plan
    from fastbrowse.retrieval import ReadOutcome
    from tests.test_agent import _at, _link, run_state
    from tests.test_policy import ScriptedJev
    from tests.test_retrieval import capture

    state = await run_state()
    state.ready_plan = Plan(requirements=(CHEAPEST,), answer_expected=True)
    urls = [NONFICTION + page for page in ("index.html", "page-2.html", "page-3.html", "page-4.html")]
    pages = [
        capture((BlockKind.RECORD, "Agnostic: A Spirited Manifesto £12.51 (Five stars)"), url=urls[0]),
        # Page 2 of the run's category held no five-star book: nothing on it is quoted.
        capture((BlockKind.RECORD, "Reasons to Stay Alive £26.41 (Two stars)"), url=urls[1]),
        capture((BlockKind.RECORD, "Mother, Can You Not? £16.89 (Five stars)"), url=urls[2]),
    ]
    reads: list = [
        {
            "claims": [],
            "answered": False,
            "continues": [{"requirement_id": CHEAPEST.id, "records": [{"first": "s0", "last": "s0"}]}],
        },
        {"claims": [], "answered": False, "continues": [{"requirement_id": CHEAPEST.id, "records": []}]},
        {
            "claims": [
                {
                    "requirement_id": CHEAPEST.id,
                    "text": "Agnostic: A Spirited Manifesto is the cheapest five-star book at 12.51.",
                    "cite": {"first": "s0", "last": "s0"},
                }
            ],
            "answered": True,
        },
    ]
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    agent = Agent(page, ScriptedJev({}), ScriptedLLM(reads))
    for index, captured in enumerate(pages):
        following = (_link("next", "next", urls[index + 1]),) if index < 2 else ()
        await agent._read(state, captured, _at(captured.url, *following))
    assert state.notes.collection_pages(CHEAPEST.id) == tuple(urls[:3])

    # A page the pipeline read ahead of the loop belongs to the same collection.
    async def nothing_matched() -> ReadOutcome:
        return ReadOutcome(facts=(), coverage=(), rejected_claims=0, cost_lines=())

    done = asyncio.create_task(nothing_matched())
    fourth = capture((BlockKind.PARAGRAPH, "110 results - showing 61 to 80."), url=urls[3])
    await agent._join_page(state, _PageRead(fourth, _at(urls[3]), done, time.monotonic(), None), [CHEAPEST])
    assert state.notes.collection_pages(CHEAPEST.id) == tuple(urls)
