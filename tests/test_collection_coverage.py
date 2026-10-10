"""Collection coverage comes from counted page records, independent of reader claims."""

from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent
from fastbrowse.memory import CollectionFact, Notes
from fastbrowse.page import BlockKind, Page
from fastbrowse.retrieval import read
from tests.test_agent import run_state
from tests.test_partitioned_finish import project_notes
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM, capture


@pytest.mark.parametrize("missing", [False, True])
async def test_counted_collection_is_retained_without_reader_completion(missing):
    page = capture(
        (BlockKind.HEADING, "Birch"),
        (BlockKind.HEADING, "Reviews (2)"),
        (BlockKind.LIST_ITEM, "Elm: Thank you."),
        *([] if missing else [(BlockKind.LIST_ITEM, "Oak: Nice work.")]),
    )
    notes = Notes()
    await read(
        ScriptedLLM([{"claims": [], "answered": False, "collections": []}]),
        page,
        "Report author replies for Birch.",
        ["r"],
        notes,
        preserve_collections=True,
        field_outputs=True,
    )
    collections = [f for f in notes.facts if isinstance(f, CollectionFact)]
    assert bool(collections) is (not missing)
    if collections:
        assert collections[0].comparison.complete
        assert len(collections[0].comparison.records) == 3


async def test_large_read_sources_enable_field_recovery_before_an_early_rejection(monkeypatch):
    state = await run_state()
    state.ready_plan, state.notes = project_notes(100)
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    from fastbrowse.retrieval import ReadOutcome

    monkeypatch.setattr(
        "fastbrowse.agent.read",
        AsyncMock(return_value=ReadOutcome(facts=(), coverage=(), rejected_claims=0, cost_lines=())),
    )
    for index in range(2):
        await agent._read(state, capture((BlockKind.PARAGRAPH, "source " * 6000), url=f"https://test/{index}"))
    state.open_answer_outputs = ("Report author replies.",)
    state.rejected_answer_evidence = frozenset()
    partition = AsyncMock(return_value=())
    monkeypatch.setattr("fastbrowse.agent.partition_plan", partition)
    monkeypatch.setattr(agent, "_finish_partitioned", AsyncMock(return_value=None))
    await agent._finish(state, None, None)
    assert state.oversized_answer
    assert partition.call_args.kwargs["independent_fields"]


async def test_explicit_empty_state_is_retained_without_reader_claims():
    page = capture(
        (BlockKind.HEADING, "Birch"),
        (BlockKind.HEADING, "Comments"),
        (BlockKind.PARAGRAPH, "Be the first to post a comment."),
    )
    notes = Notes()
    await read(
        ScriptedLLM([{"claims": [], "answered": False, "collections": []}]),
        page,
        "Report author replies for Birch.",
        ["r"],
        notes,
        preserve_collections=True,
        field_outputs=True,
    )
    collections = [f for f in notes.facts if isinstance(f, CollectionFact)]
    assert len(collections) == 1 and collections[0].comparison.complete
    assert any("Be the first" in notes.evidence[key].quote for key in collections[0].basis)


async def test_collection_keeps_nonadjacent_blocks_and_refuses_an_unread_tail():
    from fastbrowse.retrieval import _counted_collections

    page = capture(
        (BlockKind.HEADING, "Reviews (2)"),
        (BlockKind.PARAGRAPH, "Elm: Nice."),
        (BlockKind.OBSERVATION, "Reply field: [empty]"),
        (BlockKind.HEADING, "Author Ash"),
        (BlockKind.PARAGRAPH, "Please use the new version."),
    )
    page = page.model_copy(
        update={
            "blocks": tuple(
                block.model_copy(update={"list_id": "list", "list_count": 2}) if i in {1, 3, 4} else block
                for i, block in enumerate(page.blocks)
            )
        }
    )
    assert not _counted_collections(page, ["r"], page.blocks[1].end)
    facts = _counted_collections(page, ["r"], len(page.text))
    collection = next(f for f in facts if isinstance(f, CollectionFact))
    assert collection.comparison.complete
    assert len(collection.comparison.records) == 2


@pytest.mark.parametrize("known_source", [False, True])
async def test_new_source_or_collection_renews_large_task_recovery_once(known_source):
    from fastbrowse.agent import _Stop
    from fastbrowse.memory import Fact
    from fastbrowse.models import FactReader
    from tests.test_policy import observation
    from tests.test_retrieval import block_evidence

    state = await run_state()
    state.ready_plan, _ = project_notes(100)
    state.oversized_answer = True
    page = capture((BlockKind.PARAGRAPH, "Birch source: source.test/birch"))
    state.notes.add(Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM))
    if known_source:
        from fastbrowse.retrieval import _counted_collections

        state.recovered_sources.add(page.url)
        collection_page = capture((BlockKind.HEADING, "Reviews (1)"), (BlockKind.LIST_ITEM, "Elm: Nice."), url=page.url)
        for fact in _counted_collections(collection_page, ["r"], len(collection_page.text)):
            state.notes.add(fact)
    writer = ScriptedLLM(
        [{"diagnosis": "Read the next source.", "next_subgoal": "Go back.", "operation": "back", "give_up": False}]
    )
    agent = Agent(Mock(spec=Page), ScriptedJev({}), writer)
    agent._screenshots = AsyncMock(return_value=())
    agent._record_step = AsyncMock()
    state.recoveries = agent._config.stall.max_recoveries
    await agent._recover(state, observation(()), "Read another source")
    assert state.recoveries == 1
    state.recoveries = agent._config.stall.max_recoveries
    with pytest.raises(_Stop):
        await agent._recover(state, observation(()), "Repeated source")


async def test_reader_summary_cannot_complete_a_partial_counted_collection():
    page = capture(
        (BlockKind.HEADING, "Reviews (2)"),
        (BlockKind.LIST_ITEM, "Elm: Thank you."),
    )
    notes = Notes()
    await read(
        ScriptedLLM(
            [
                {
                    "claims": [
                        {
                            "requirement_id": "r",
                            "text": "No author replies.",
                            "cite": None,
                            "records": [{"first": "s0", "last": "s1"}],
                        }
                    ],
                    "answered": True,
                    "collections": [],
                }
            ]
        ),
        page,
        "Report author replies for Birch.",
        ["r"],
        notes,
        preserve_collections=True,
        field_outputs=True,
    )
    assert not any(f.comparison and f.comparison.complete for f in notes.facts)


async def test_unread_output_recovery_destination_is_read_before_uncertainty():
    from fastbrowse.agent import _code_decision
    from fastbrowse.models import Decider, Operation
    from tests.test_policy import observation

    state = await run_state()
    state.ready_plan, state.notes = project_notes(100)
    state.oversized_answer = True
    state.open_answer_outputs = ("Report links for Birch.",)
    state.owes_read = True
    observed = observation(())
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    agent._step = AsyncMock(return_value=False)
    assert await agent._read_before_interaction(state, observed, _code_decision(Operation.ESCALATE, None), Decider.JEV)
    assert agent._step.call_args.args[2].operation is Operation.READ
    state.reads.add((observed.document_key, "capture", ("r",), state.open_answer_outputs))
    assert not await agent._read_before_interaction(
        state, observed, _code_decision(Operation.ESCALATE, None), Decider.JEV
    )


async def test_ordinary_read_retains_counted_coverage_without_changing_its_notes():
    page = capture(
        (BlockKind.HEADING, "Reviews (1)"),
        (BlockKind.LIST_ITEM, "Elm: Thank you."),
    )
    notes = Notes()
    outcome = await read(ScriptedLLM([{"claims": [], "answered": False}]), page, "Read reviews.", ["r"], notes)
    assert not notes.facts
    assert any(isinstance(f, CollectionFact) and f.comparison.complete for f in outcome.collection_facts)


@pytest.mark.parametrize("changed", [False, True])
async def test_large_read_reuses_coverage_from_an_earlier_source(monkeypatch, changed):
    from fastbrowse.retrieval import ReadOutcome, _counted_collections

    state = await run_state()
    state.ready_plan, state.notes = project_notes(100)
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    page = capture(
        (BlockKind.HEADING, "Reviews (1)"),
        (BlockKind.LIST_ITEM, "Elm: Thank you."),
    )
    outcomes = [
        ReadOutcome(
            facts=(),
            coverage=(),
            rejected_claims=0,
            cost_lines=(),
            collection_facts=_counted_collections(page, ["r"], len(page.text)),
        ),
        ReadOutcome(facts=(), coverage=(), rejected_claims=0, cost_lines=()),
        ReadOutcome(facts=(), coverage=(), rejected_claims=0, cost_lines=()),
    ]
    monkeypatch.setattr("fastbrowse.agent.read", AsyncMock(side_effect=outcomes))
    await agent._read(state, page)
    reread = capture((BlockKind.HEADING, "Reviews (2)"), url=page.url) if changed else page
    await agent._read(state, reread)
    assert not any(isinstance(f, CollectionFact) for f in state.notes.facts)
    await agent._read(state, capture((BlockKind.PARAGRAPH, "source " * 18000), url="https://test/large"))
    assert state.oversized_answer
    assert any(isinstance(f, CollectionFact) and f.comparison.complete for f in state.notes.facts) is (not changed)


@pytest.mark.parametrize("missing_boundary", [False, True])
def test_literal_collection_citations_keep_their_proven_coverage(missing_boundary):
    from fastbrowse.memory import fact_id
    from fastbrowse.planner import Requirement, RequirementKind
    from fastbrowse.retrieval import Claim, _counted_collections, assemble_answer
    from fastbrowse.verification import _output_context

    page = capture(
        (BlockKind.HEADING, "Reviews (1)"),
        (BlockKind.LIST_ITEM, "Elm: Thank you."),
    )
    notes = Notes(_counted_collections(page, ["r"], len(page.text)))
    collection = next(f for f in notes.facts if isinstance(f, CollectionFact))
    claim = Claim(text="No author replies in the reviews.", evidence_ids=collection.basis[int(missing_boundary) :])
    requirement = Requirement(id="r", text="Report author replies.", kind=RequirementKind.INFORMATION)
    answer = assemble_answer((claim,), notes, (requirement,))
    assert fact_id(collection) not in claim.evidence_ids
    context = _output_context(answer, notes)
    assert context is not None
    assert bool(context.claims[0].compared_records) is (not missing_boundary)
    if not missing_boundary:
        assert context.claims[0].compared_records[0].complete
        assert context.claims[0].compared_records[0].record_indices == (0, 1)


def test_counted_collection_keeps_its_owning_entity_context():
    from fastbrowse.retrieval import _counted_collections

    page = capture(
        (BlockKind.HEADING, "Birch"),
        (BlockKind.PARAGRAPH, "By Ash"),
        (BlockKind.HEADING, "Reviews (1)"),
        (BlockKind.LIST_ITEM, "Elm: Nice work."),
    )
    page = page.model_copy(
        update={
            "blocks": tuple(
                block.model_copy(update={"heading_path": ("Birch",)}) if index == 2 else block
                for index, block in enumerate(page.blocks)
            )
        }
    )
    notes = Notes(_counted_collections(page, ["r"], len(page.text)))
    collection = next(f for f in notes.facts if isinstance(f, CollectionFact))
    assert any("By Ash" in notes.evidence[key].quote for key in collection.basis)
    assert any("Elm: Nice work." in notes.evidence[key].quote for key in collection.basis)


def _requirements(*texts: str):
    from fastbrowse.planner import Requirement, RequirementKind

    return [Requirement(id=f"r{i}", text=text, kind=RequirementKind.INFORMATION) for i, text in enumerate(texts)]


def test_one_collection_keeps_coverage_for_each_requirement_it_serves():
    from fastbrowse.retrieval import _counted_collections

    page = capture((BlockKind.HEADING, "Reviews (1)"), (BlockKind.LIST_ITEM, "Elm: Nice work."))
    notes = Notes(_counted_collections(page, ["r0"], len(page.text)))
    for fact in _counted_collections(page, ["r1"], len(page.text)):
        notes.add(fact)
    covered = {f.comparison.requirement_id for f in notes.facts if isinstance(f, CollectionFact)}
    assert covered == {"r0", "r1"}


@pytest.mark.parametrize(("heading", "covered"), [("Birch (1)", {"r0"}), ("Reviews (1)", set())])
def test_a_counted_heading_naming_one_target_covers_only_that_target(heading, covered):
    from fastbrowse.retrieval import _counted_collections

    page = capture((BlockKind.HEADING, heading), (BlockKind.LIST_ITEM, "Elm: Nice work."))
    requirements = _requirements("Report reviews for Birch.", "Report reviews for Rowan.")
    facts = _counted_collections(page, ["r0", "r1"], len(page.text), requirements)
    assert {f.comparison.requirement_id for f in facts if isinstance(f, CollectionFact)} == covered
