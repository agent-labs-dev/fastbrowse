"""Recovered answers bind each requested field to its own claims and source scope."""

import json
from collections.abc import Callable, Mapping
from unittest.mock import AsyncMock, Mock

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent, _follow_recovery, _recovery_routes, _Stop
from fastbrowse.config import Thresholds
from fastbrowse.jev import Answer, Evaluation, NoulAnswer, NoulQuestion, Question
from fastbrowse.memory import Comparison, Fact, Notes, fact_id
from fastbrowse.models import FactReader, Operation, Status
from fastbrowse.page import BlockKind, Control, Page
from fastbrowse.planner import OutputTarget, Plan, Requirement, RequirementKind, partition_plan
from fastbrowse.policy import Decision, Reduction
from fastbrowse.retrieval import OUTPUT_MISSING, AnswerCorrection, compose
from fastbrowse.verification import check_answer_outputs, check_claims
from tests.test_agent import open_outputs, run_state
from tests.test_answer_repair import RoutingJev
from tests.test_policy import FREE, ScriptedJev, observation
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


class FieldWriter(ScriptedLLM):
    async def generate(self, purpose, messages, schema, **kwargs):
        if schema.__name__ == "_AnswerDraft" and self.responses and isinstance(self.responses[0], dict):
            outputs = self.responses.pop(0)
            assert isinstance(outputs, dict)
            self.responses.insert(
                0, {"claims": [claim for claims in outputs.values() if isinstance(claims, list) for claim in claims]}
            )
        return await super().generate(purpose, messages, schema, **kwargs)


async def test_partition_expands_bundled_fields_for_every_named_target() -> None:
    state = await run_state()
    plan = Plan(
        requirements=tuple(
            Requirement(id=name, text=f"Report bugs and requests for {name}.", kind=RequirementKind.INFORMATION)
            for name in ("Birch", "Elm")
        ),
        answer_expected=True,
        answer_checks=("Report bugs and requests for Birch.", "Report bugs and requests for Elm."),
    )

    class PartitionWriter(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ == "_OutputBindings":
                self.responses[0] = {
                    "bindings": [
                        {"check_index": i, "requirement_ids": [name]} for i, name in enumerate(("Birch", "Elm"))
                    ]
                }
            return await super().generate(purpose, messages, schema, **kwargs)

    llm = PartitionWriter(
        [
            {
                "groups": [
                    {
                        "subjects": [{"name": name, "requirement_ids": [name]} for name in ("Birch", "Elm")],
                        "fields": ["player bug reports", "player feature requests"],
                        "check_indices": [0, 1],
                    }
                ]
            },
        ]
    )
    groups, targets = await partition_plan(llm, "Read bugs and requests for Birch and Elm.", plan, ledger=state.ledger)
    assert [group.answer_checks for group in groups] == [
        (f"Report player bug reports for {name}.", f"Report player feature requests for {name}.")
        for name in ("Birch", "Elm")
    ]
    assert targets["Report player bug reports for Elm."] == OutputTarget(field="player bug reports", subject="Elm")


def comments(complete: bool):
    page = capture(
        (BlockKind.HEADING, "Project Birch comments: 2 comments. Author: Ash."),
        (BlockKind.RECORD, "Player Elm: Please add a compact view."),
        (BlockKind.RECORD, "Player Oak: Thanks for the update."),
    )
    sources = tuple(
        Fact(text=page.text[b.start : b.end], evidence=block_evidence(page, b.source_id), reader=FactReader.LLM)
        for b in page.blocks
    )
    collection = Fact(
        requirement_id="r",
        text="Birch has two player comments.",
        evidence=None,
        basis=tuple(fact_id(source) for source in sources),
        comparison=Comparison(requirement_id="r", records=tuple(fact_id(s) for s in sources), complete=complete),
        reader=FactReader.LLM,
    )
    plan = Plan(
        requirements=(Requirement(id="r", text="Report Birch author replies.", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("Report author replies for Project Birch.",),
    )
    return plan, Notes((*sources, collection))


class DoubtingJev(ScriptedJev):
    """Answers yes to a claim-check question exactly when `doubt` holds for its key and instructions."""

    def __init__(self, doubt: Callable[[str, str], bool]) -> None:
        super().__init__({})
        self.doubt = doubt

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        self.requests.append(questions)
        # An omission question reads the answer from the state, so the doubt sees both.
        shown = f"\n\n# Answer\n{state['answer']}" if isinstance(state, dict) and "answer" in state else ""
        answers: dict[str, Answer] = {
            key: NoulAnswer(probability=float(isinstance(q, NoulQuestion) and self.doubt(key, q.instructions + shown)))
            for key, q in questions.items()
        }
        return Evaluation(model="test", answers=answers, input_tokens=10, cost=FREE)


def unanswered(answer: str) -> Callable[[str, str], bool]:
    """Doubts only the output omission questions, and only when the answer lacks `answer`."""
    return lambda key, text: key.startswith(OUTPUT_MISSING) and answer not in text.split("# Answer", 1)[1]


@pytest.mark.parametrize("thanks", [True, False])
async def test_field_claim_that_does_not_report_author_replies_is_rejected_as_omitted(thanks: bool) -> None:
    plan, notes = comments(True)
    text = "Player Oak: Thanks for the update." if thanks else "No author replies occur in Birch's two comments."
    writer = FieldWriter([{"output_0": [{"text": text, "evidence_ids": ["e3"]}]}])
    answer = (
        await compose(writer, plan.requirements[0].text, plan, notes, preserve_collections=True, field_outputs=True)
    ).data
    assert check_answer_outputs(answer, notes, plan.answer_checks)
    jev = DoubtingJev(unanswered("No author replies"))
    missing: list[str] = []
    corrections: list[AnswerCorrection] = []
    held = await check_claims(
        jev,
        answer,
        notes,
        Thresholds(),
        answer_checks=plan.answer_checks,
        missing_outputs=missing,
        corrections=corrections,
    )
    assert (held is None) is thanks
    assert any(f"{OUTPUT_MISSING}0" in request for request in jev.requests)
    # A refusal names the check, so recovery and the composer know what to add.
    assert missing == (list(plan.answer_checks) if thanks else [])
    assert [(c.stage, c.criterion) for c in corrections] == ([("source", plan.answer_checks[0])] if thanks else [])


async def test_an_absence_over_an_unfinished_collection_is_rejected() -> None:
    plan, notes = comments(False)
    writer = FieldWriter(
        [{"output_0": [{"text": "No author replies occur in Birch's two comments.", "evidence_ids": ["e3"]}]}]
    )
    answer = (
        await compose(writer, plan.requirements[0].text, plan, notes, preserve_collections=True, field_outputs=True)
    ).data
    missing: list[str] = []
    corrections: list[AnswerCorrection] = []
    # Jev doubts nothing: code refuses the absence because the collection was not read to its end.
    held = await check_claims(
        DoubtingJev(lambda key, text: False),
        answer,
        notes,
        Thresholds(),
        answer_checks=plan.answer_checks,
        missing_outputs=missing,
        corrections=corrections,
    )
    assert held is None
    assert missing == list(plan.answer_checks)
    assert [(c.stage, c.criterion) for c in corrections] == [("source", plan.answer_checks[0])]
    assert "not read to its end" in corrections[0].reason


async def test_missing_field_cannot_borrow_a_neighbor_claim() -> None:
    page = capture((BlockKind.RECORD, "Project Birch: Player Elm reports a crash."))
    fact = Fact(text=page.text, requirement_id="r", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    plan = Plan(
        requirements=(
            Requirement(id="r", text="Report bugs and feature requests for Birch.", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=True,
        answer_checks=("Report player bug reports for Birch.", "Report feature requests for Birch."),
    )
    writer = FieldWriter(
        [{"output_0": [{"text": "Elm reports a crash in Birch.", "evidence_ids": ["e0"]}], "output_1": []}]
    )
    answer = (
        await compose(
            writer,
            plan.requirements[0].text,
            plan,
            notes,
            preserve_collections=True,
            field_outputs=True,
        )
    ).data
    missing = []
    assert not check_answer_outputs(answer, notes, plan.answer_checks, missing_outputs=missing)
    assert missing == [plan.answer_checks[1]]


async def test_reader_fallback_keeps_composed_absence_and_its_field_binding() -> None:
    plan, notes = comments(True)
    state = await run_state()
    state.ready_plan, state.notes, state.finish_partitions = plan, notes, (plan,)
    writer = FieldWriter(
        [{"output_0": [{"text": "No author replies occur in Birch's two comments.", "evidence_ids": ["e3"]}]}]
    )
    agent = Agent(Mock(spec=Page), RoutingJev(), writer)
    offered = []

    async def reject(state, answer):
        offered.append(answer)
        return None

    agent._holds = AsyncMock(side_effect=reject)
    await agent._answer(state, None)
    assert offered
    assert all("No author replies" in answer.answer for answer in offered)
    assert all(answer.output_claims[plan.answer_checks[0]] for answer in offered)


async def test_field_decomposition_cannot_discharge_an_omitted_original_obligation() -> None:
    plan, notes = comments(True)
    requirement = plan.requirements[0].model_copy(update={"text": "Report author replies and links for Birch."})
    plan = plan.model_copy(update={"requirements": (requirement,)})
    writer = FieldWriter(
        [{"output_0": [{"text": "No author replies occur in Birch's two comments.", "evidence_ids": ["e3"]}]}]
    )
    answer = (await compose(writer, requirement.text, plan, notes, preserve_collections=True, field_outputs=True)).data
    state = await run_state()
    state.ready_plan, state.notes, state.finish_partitions = plan, notes, (plan,)
    # Jev would refuse any omission question shown the original requirement while the answer has no link.
    jev = DoubtingJev(
        lambda key, text: (
            (key == "requirement_omitted" or key.startswith(OUTPUT_MISSING))
            and requirement.text in text
            and "link" not in answer.answer
        )
    )
    agent = Agent(Mock(spec=Page), jev, ScriptedLLM([]))
    assert await agent._holds(state, answer) is None
    assert requirement.text in state.open_answer_outputs


@pytest.mark.parametrize("irreversible", [False, True])
async def test_output_recovery_routes_to_observed_unread_section_and_keeps_authorization(irreversible: bool) -> None:
    state = await run_state()
    state.task = "Report Project Birch author replies and links."
    state.caller_start = "https://example.test/birch/comments"
    open_outputs(state, ("links", "Project Birch"))
    description = "https://example.test/birch"
    seen = observation(
        (
            Control(
                id="description",
                frame_id=None,
                role="link",
                label="Description",
                href=description,
                operations={Operation.CLICK},
            ),
        )
    ).model_copy(update={"url": state.caller_start, "title": "Project Birch comments"})
    Agent._note_effect(state, seen)
    page = capture((BlockKind.RECORD, "Project Birch: Player Oak says thanks."), url=state.caller_start)
    state.notes.remember_capture(page)
    state.notes.add(Fact(text=page.text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM))
    elsewhere = observation(()).model_copy(update={"url": "https://example.test/elm"})
    llm = ScriptedLLM(
        [
            {
                "diagnosis": "Links need the unread description.",
                "next_subgoal": "Read Birch's description.",
                "operation": "navigate",
                "url": description,
                "give_up": False,
                "needs_input": False,
            }
        ]
    )
    agent = Agent(Mock(spec=Page), RoutingJev(), llm)
    agent._screenshots = AsyncMock(return_value=())
    await agent._recover(state, elsewhere, "Missing links.")
    assert not llm.calls
    assert state.directed == (Operation.NAVIGATE, description)
    proposed = Decision(
        operation=Operation.READ,
        target=None,
        tab_id=None,
        operation_confidence=1,
        target_confidence=None,
        login_required=None,
        offered_controls=0,
        reduction=Reduction.NONE,
        cost=(),
        input_tokens=0,
    )
    decision = _follow_recovery(state, elsewhere, proposed, uncertain=False)
    assert decision is not None and decision.url == description
    agent._jev = ScriptedJev({}, noul=float(irreversible))
    if irreversible:
        with pytest.raises(_Stop) as stopped:
            await agent._action(state, elsewhere, decision, gate=True)
        assert stopped.value.status is Status.NEEDS_CONFIRMATION
    else:
        action = await agent._action(state, elsewhere, decision, gate=True)
        assert action.url == description


async def test_recovery_section_routes_do_not_reconstruct_display_only_external_addresses() -> None:
    state = await run_state()
    open_outputs(state, ("links", "Project Birch"))
    controls = (
        *tuple(
            Control(
                id=str(i),
                frame_id=None,
                role="link",
                label="Author resources",
                href=f"external{i}.test/resources",
                operations={Operation.CLICK},
            )
            for i in range(30)
        ),
        Control(
            id="description",
            frame_id=None,
            role="link",
            label="Description",
            href="/projects/birch",
            operations={Operation.CLICK},
        ),
    )
    seen = observation(controls).model_copy(
        update={"url": "https://example.test/projects/birch/comments", "title": "Project Birch comments"}
    )
    Agent._note_effect(state, seen)
    assert tuple(state.observed_links) == ("https://example.test/projects/birch",)
    routes = json.loads(_recovery_routes(state, 1000))
    assert routes[0]["label"] == "Description" and not routes[0]["read"]
