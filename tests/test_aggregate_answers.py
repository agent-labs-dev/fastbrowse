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
from fastbrowse.llm import Generation
from fastbrowse.memory import Comparison, Fact, Notes, Tally, fact_id
from fastbrowse.models import CostBasis, CostComponent, CostLine, Evidence, FactReader
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


def _urls(notes: Notes, requirement_id: str) -> list[str]:
    return [read.url for read in notes.collection_pages(requirement_id)]


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
        notes.add_collection_page(AUTHORS.id, item["url"], f"sha-{item['url']}")
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
    claim = Claim(
        text="J.K. Rowling has the second most quotes, 9.",
        evidence_ids=(fact_id(tallies["J.K. Rowling"]),),
        ranks=True,
    )
    context = _output_context(assemble_answer((claim,), notes, (AUTHORS,)), notes)

    assert context is not None
    (count,) = context.claims[0].counted_records
    assert count.standing is not None
    assert count.standing.rank == 2
    assert count.standing.complete is complete
    # The leading groups and two either side of second place; the long tail of single quotes is left out.
    assert [(group.rank, group.group, group.count) for group in count.standing.groups] == [
        (1, "Albert Einstein", 10),
        (2, "J.K. Rowling", 9),
        (3, "Marilyn Monroe", 7),
        (4, "Dr. Seuss", 6),
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
    claim = Claim(
        text="J.K. Rowling ranks second with 9 quotes.", evidence_ids=(fact_id(tallies["J.K. Rowling"]),), ranks=True
    )
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
        "rank": 1,
        "group": "Albert Einstein",
        "count": 10,
        "complete": complete,
    }


def test_the_claim_check_sees_a_tallys_standing() -> None:
    # Records that are not also a comparison's operands are shown to the claim check as their tally's line.
    notes, tallies = counted_authors(complete=True, continued=False)
    claim = Claim(
        text="Marilyn Monroe ranks third with 7 quotes.", evidence_ids=(fact_id(tallies["Marilyn Monroe"]),), ranks=True
    )
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
            notes.add_collection_page(CHEAPEST.id, url, f"sha-{url}")
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
    assert [(read.read, context.urls[read.url_ref]) for read in compared.pages_read] == (
        list(enumerate((NONFICTION + page for page in FIVE_STAR), 1)) if pages else []
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
    notes.add_collection_page("other", "https://example.test/", "sha")

    scoped = notes.for_requirements((CHEAPEST.id,))

    assert _urls(scoped, CHEAPEST.id) == [NONFICTION + page for page in FIVE_STAR]
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
    assert _urls(state.notes, CHEAPEST.id) == urls[:3]

    # A page the pipeline read ahead of the loop belongs to the same collection.
    async def nothing_matched() -> ReadOutcome:
        return ReadOutcome(facts=(), coverage=(), rejected_claims=0, cost_lines=())

    done = asyncio.create_task(nothing_matched())
    fourth = capture((BlockKind.PARAGRAPH, "110 results - showing 61 to 80."), url=urls[3])
    await agent._join_page(
        state, _PageRead(fourth, _at(urls[3]), done, time.monotonic(), None), [CHEAPEST], [CHEAPEST.id]
    )
    assert _urls(state.notes, CHEAPEST.id) == urls


def test_a_stopped_run_lists_every_count_before_a_quote_that_answers_another_requirement() -> None:
    """A run stopped mid-pagination has counts that are not complete, while a quote elsewhere already evidences
    another requirement. Ordering by what evidences a requirement put that quote first, and under a tight budget
    it left no room for a single count."""
    _OFFSETS.clear()
    quote = _record("https://quotes.toscrape.com/", QUOTES[0]["quote"] + " " + "word " * 300).model_copy(
        update={"requirement_id": "about_page"}
    )
    notes = Notes((quote,))
    for item in QUOTES[:4]:
        record = _record(item["url"], item["quote"])
        notes.add(record)
        notes.add_tally(Tally(requirement_id=AUTHORS.id, key=_author(item["quote"]), records=(fact_id(record),)))
    assert notes.fact_requirements(fact_id(quote)) == ("about_page",)
    assert not any(notes.fact_requirements(fact_id(f)) for f in notes.facts if f.tally is not None)

    quote_only = partial_answer(Notes((quote,)), 10**6).linked_answer
    # Room for the quote alone or for every count alone, never both.
    answer = partial_answer(notes, len(quote_only) + 10).answer

    counts = [line for line in answer.splitlines() if re.fullmatch(r".+: \d+", line)]
    assert len(counts) == len({_author(item["quote"]) for item in QUOTES[:4]})
    assert "word word" not in answer


async def test_pages_paged_for_one_requirement_are_not_credited_to_another_read_alongside() -> None:
    """Every unresolved requirement is read on each paged page. Crediting those pages to a requirement whose own
    list never continued told its audit they were part of its collection and held no uncited match."""
    from fastbrowse.planner import Requirement
    from tests.test_agent import _pipeline_fixture

    contact = Requirement(id="r2", text="The shop's contact email", kind=RequirementKind.INFORMATION)
    agent, state, _, observations, captures, llm = await _pipeline_fixture(also_wanted=(contact,))
    assert {r.id for r in state.notes.unresolved(state.plan)} == {"r1", "r2"}
    # The paged pages hold none of the contact requirement's records.
    for response in llm.responses[:2]:
        assert isinstance(response, dict)
        continues = response["continues"]
        assert isinstance(continues, list)
        response["continues"] = [*continues, {"requirement_id": "r2", "records": []}]

    await agent._pipeline_pages(state, observations[0])

    assert len(state.notes.collection_pages("r2")) == 0
    assert _urls(state.notes, "r1") == [c.url for c in captures]


async def test_a_list_that_loads_more_at_one_address_keeps_each_read() -> None:
    """Load more and in-place paging keep the URL. Keyed by address, every read after the first folded into it,
    so a state with no matching record was in neither the cited records nor the reads listed."""
    from unittest.mock import AsyncMock, Mock

    from fastbrowse.agent import Agent
    from fastbrowse.models import StepOutcome
    from fastbrowse.page import ActResult, BlockKind, Page
    from fastbrowse.planner import Plan
    from tests.test_agent import _at, run_state
    from tests.test_policy import ScriptedJev
    from tests.test_retrieval import capture

    state = await run_state()
    state.ready_plan = Plan(requirements=(CHEAPEST,), answer_expected=True)
    url = NONFICTION + "index.html"
    shown = [
        capture((BlockKind.RECORD, "Agnostic: A Spirited Manifesto \u00a312.51 (Five stars)"), url=url),
        capture((BlockKind.RECORD, "Reasons to Stay Alive \u00a326.41 (Two stars)"), url=url),
        capture((BlockKind.RECORD, "Mother, Can You Not? \u00a316.89 (Five stars)"), url=url),
    ]
    more = {"requirement_id": CHEAPEST.id, "records": [{"first": "s0", "last": "s0"}]}
    reads: list = [
        {"claims": [], "answered": False, "continues": [more]},
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
    # The list goes on below a Load more button, so the quote choice cannot settle it: the reader reads.
    agent = Agent(page, ScriptedJev({CHEAPEST.id: "synthesis"}), ScriptedLLM(reads))
    for captured in shown:
        await agent._read(state, captured, _at(url))

    reads_made = state.notes.collection_pages(CHEAPEST.id)
    assert len(reads_made) == 3
    assert _urls(state.notes, CHEAPEST.id) == [url] * 3

    # The audit is shown each read of the one address in order, the one with no match among them.
    notes, winner = cheapest_five_star(pages=False)
    for index in range(3):
        notes.add_collection_page(CHEAPEST.id, url, f"state-{index}")
    claim = Claim(text="Agnostic: A Spirited Manifesto is the cheapest at 12.51.", evidence_ids=(fact_id(winner),))
    context = _output_context(assemble_answer((claim,), notes, (CHEAPEST,)), notes)
    assert context is not None
    (compared,) = context.claims[0].compared_records
    assert [(read.read, context.urls[read.url_ref]) for read in compared.pages_read] == [(1, url), (2, url), (3, url)]


MANY = Requirement(
    id="per_group", text="Count the entries in every group and rank the groups.", kind=RequirementKind.INFORMATION
)


def many_groups() -> tuple[Notes, list[Fact]]:
    """Five hundred groups with long labels, a hundred at each count from five down to one."""
    _OFFSETS.clear()
    notes = Notes()
    tallies = []
    for index in range(500):
        label = f"Group {index:03d} " + "with a long descriptive label " * 10
        records = []
        for entry in range(5 - index // 100):
            record = _record(f"https://example.test/groups/{index // 50}", f"{label}: entry {entry}")
            notes.add(record)
            records.append(fact_id(record))
        tallies.append(notes.add_tally(Tally(requirement_id=MANY.id, key=label, records=tuple(records))))
    notes.complete_tallies(MANY.id)
    return notes, tallies


def test_a_low_ranked_groups_standing_stays_bounded() -> None:
    notes, tallies = many_groups()
    last = tallies[-1]
    claim = Claim(text="Group 499 ranks 401st with 1 entry.", evidence_ids=(fact_id(last),), ranks=True)
    context = _output_context(assemble_answer((claim,), notes, (MANY,)), notes)

    assert context is not None
    standing = context.claims[0].counted_records[0].standing
    assert standing is not None
    # Rank, ties and the group count stay exact while the list shown stays a fixed handful.
    assert (standing.rank, standing.tied, standing.group_count, standing.complete) == (401, 99, 500, True)
    assert [group.rank for group in standing.groups] == [1, 1, 1, 401, 401, 401]
    # The cited group's own label is kept whole; it is the claim's subject.
    assert last.tally is not None and standing.groups[-1].group == last.tally.key
    assert len(standing.model_dump_json()) < 3000
    question = claim_check_questions(assemble_answer((claim,), notes, (MANY,)), notes, check_omission=False)
    assert len(question["unsupported_0"].instructions) < 6000


@pytest.mark.parametrize(("stated", "passes"), [(401, True), (400, False)])
async def test_a_low_ranked_claim_is_judged_on_its_exact_rank(stated: int, passes: bool) -> None:
    notes, tallies = many_groups()
    claim = Claim(text=f"Group 499 ranks {stated} with 1 entry.", evidence_ids=(fact_id(tallies[-1]),), ranks=True)
    answer = assemble_answer((claim,), notes, (MANY,))
    check = "Report the rank of Group 499."

    class RankJudge(ScriptedLLM):
        """Answers as the audit's rules read: the stated rank must be the standing's code-computed rank."""

        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ != "_OutputIdentities":
                payload = json.loads(messages[-1].content)
                ((key, field),) = payload["criteria"].items()
                if "reported_claims" in field:
                    reported = field["reported_claims"][0]
                    standing = reported["counted_records"][0]["standing"]
                    holds = standing["complete"] and f"ranks {standing['rank']} " in reported["text"]
                else:
                    holds = field["sources"][0]["counted_records"][0]["standing"]["complete"]
                self.responses.append({"judgments": {key: "yes" if holds else "no"}, "reason": "Code rank."})
            return await super().generate(purpose, messages, schema, **kwargs)

    assert await check_answer_outputs(RoutingJev(), RankJudge([]), answer, notes, (check,)) is passes


def test_a_count_alone_carries_no_standing() -> None:
    notes, tallies = many_groups()
    claim = Claim(text="Group 499 has 1 entry.", evidence_ids=(fact_id(tallies[-1]),))
    answer = assemble_answer((claim,), notes, (MANY,))

    context = _output_context(answer, notes)
    assert context is not None
    assert "standing" not in context.claims[0].model_dump()["counted_records"][0]
    question = claim_check_questions(answer, notes, check_omission=False)["unsupported_0"].instructions
    assert '"standing"' not in question and '"count": 1' in question


async def test_identity_resolution_sees_only_the_source_keys_it_may_name() -> None:
    """A live resolver filed a correct quote under the cited record's page block id instead of its source key, and
    a right answer failed as an identity missing from its cited sources. The block id is no longer shown; a reply
    that still names something other than a key is refused, as before."""
    notes, tallies = many_groups()
    last = tallies[-1]
    assert last.tally is not None
    claim = Claim(text="Group 499 ranks 401 with 1 entry.", evidence_ids=(fact_id(last),), ranks=True)
    answer = assemble_answer((claim,), notes, (MANY,))
    block_ids = {notes.evidence[key].source_id for key in last.basis}
    shown: list[dict] = []

    class BlockIdResolver(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            payload = json.loads(messages[-1].content)
            if schema.__name__ == "_OutputIdentities":
                shown.append(payload["sources"])
                identities = [{"id": "i0", "source_ref": next(iter(block_ids)), "quote": "Group 499"}]
                bindings = {key: {"scope": "entities", "identity_ids": ["i0"]} for key in payload["criteria"]}
                return Generation(
                    data=schema.model_validate({"identities": identities, "bindings": bindings}),
                    cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.0),
                )
            self.responses.append({"judgments": {k: "yes" for k in payload["criteria"]}, "reason": "Read."})
            return await super().generate(purpose, messages, schema, **kwargs)

    missing: list[str] = []
    check = "Report the rank of Group 499."
    passed = await check_answer_outputs(
        RoutingJev(), BlockIdResolver([]), answer, notes, (check,), missing_outputs=missing
    )

    sources = shown[0]
    assert sources and all(ref.startswith("q") for ref in sources)
    assert not any("source_id" in source for source in sources.values())
    assert not block_ids & {value for source in sources.values() for value in source.values() if isinstance(value, str)}
    assert passed is False and missing == [check]


def test_a_count_and_a_page_read_off_a_refused_guess_take_no_part_in_a_ranking() -> None:
    """A group counted on a guessed page that was refused kept its count among the groups a rank is judged
    against, and the guessed page stayed listed as read, as if a page none of whose rows is counted were covered."""
    guess, own = "https://example.test/list?q=guess", "https://example.test/list?page=1"
    notes = Notes()
    groups = {"Ada": (own, 2), "Grace": (own, 1), "Edsger": (guess, 3)}
    tallies = {}
    for name, (url, count) in groups.items():
        rows = [_record(url, f"{name} row {n}") for n in range(count)]
        for row in rows:
            notes.add(row)
        tallies[name] = notes.add_tally(Tally(requirement_id="r", key=name, records=tuple(fact_id(r) for r in rows)))
        notes.add_collection_page("r", url, f"sha-{url}")
    for fact in tallies.values():
        notes.add(fact.model_copy(update={"requirement_id": "r"}))
    notes.discredit(("r",), lambda item: item.url == guess)
    ada = tallies["Ada"].tally
    assert ada is not None
    standing = notes.standing(ada)
    assert standing is not None and standing.rank == 1 and standing.group_count == 2
    assert [read.url for read in notes.collection_pages("r")] == [own]
