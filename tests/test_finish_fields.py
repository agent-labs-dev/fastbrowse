"""Recovered answers bind each requested field to its own claims and source scope."""

import json
from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent, _follow_recovery, _recovery_routes, _Stop
from fastbrowse.memory import Comparison, Fact, Notes, fact_id
from fastbrowse.models import FactReader, Operation, Status
from fastbrowse.page import BlockKind, Control, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind, partition_plan
from fastbrowse.policy import Decision, Reduction
from fastbrowse.retrieval import compose
from fastbrowse.verification import check_answer_outputs
from tests.test_agent import run_state
from tests.test_answer_repair import RoutingJev
from tests.test_policy import ScriptedJev, observation
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
    groups = await partition_plan(llm, "Read bugs and requests for Birch and Elm.", plan, ledger=state.ledger)
    assert [group.answer_checks for group in groups] == [
        (f"Report player bug reports for {name}.", f"Report player feature requests for {name}.")
        for name in ("Birch", "Elm")
    ]


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


class FieldJudge(ScriptedLLM):
    async def generate(self, purpose, messages, schema, **kwargs):
        if schema.__name__ != "_OutputIdentities":
            key, field = next(iter(json.loads(messages[-1].content)["criteria"].items()))
            claims = field.get("reported_claims", field.get("sources"))
            complete = any(c["complete"] for claim in claims for c in claim["compared_records"])
            explicit = "reported_claims" not in field or any("No author replies" in claim["text"] for claim in claims)
            self.responses.append(
                {
                    "judgments": {key: "yes" if complete and explicit else "no"},
                    "reason": "Only complete player comments support the bounded absence of author replies.",
                }
            )
        return await super().generate(purpose, messages, schema, **kwargs)


@pytest.mark.parametrize("complete", [True, False])
@pytest.mark.parametrize("thanks", [True, False])
async def test_field_claim_checks_author_identity_and_complete_absence(complete: bool, thanks: bool) -> None:
    plan, notes = comments(complete)
    text = "Player Oak: Thanks for the update." if thanks else "No author replies occur in Birch's two comments."
    writer = FieldWriter([{"output_0": [{"text": text, "evidence_ids": ["e3"]}]}])
    answer = (
        await compose(writer, plan.requirements[0].text, plan, notes, preserve_collections=True, field_outputs=True)
    ).data
    assert await check_answer_outputs(RoutingJev(), FieldJudge([]), answer, notes, plan.answer_checks) is (
        complete and not thanks
    )


async def test_missing_field_cannot_borrow_a_neighbor_claim_even_when_selector_accepts() -> None:
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
    assert not await check_answer_outputs(
        RoutingJev(),
        ScriptedLLM(
            [
                {"judgments": {f"output_{i}": "yes"}, "reason": "Selected claim accepted."}
                for _ in range(2)
                for i in range(2)
            ]
        ),
        answer,
        notes,
        plan.answer_checks,
        missing_outputs=missing,
    )
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

    class MissingLinksJudge(FieldJudge):
        async def generate(self, purpose, messages, schema, **kwargs):
            if schema.__name__ != "_OutputIdentities":
                key, field = next(iter(json.loads(messages[-1].content)["criteria"].items()))
                if "links" in field["criterion"]:
                    self.responses.append({"judgments": {key: "no"}, "reason": "No link is reported or quoted."})
                    return await ScriptedLLM.generate(self, purpose, messages, schema, **kwargs)
            return await super().generate(purpose, messages, schema, **kwargs)

    state = await run_state()
    state.ready_plan, state.notes, state.finish_partitions = plan, notes, (plan,)
    agent = Agent(Mock(spec=Page), RoutingJev(), MissingLinksJudge([]))
    assert await agent._holds(state, answer) is None
    assert requirement.text in state.open_answer_outputs


@pytest.mark.parametrize("irreversible", [False, True])
async def test_output_recovery_routes_to_observed_unread_section_and_keeps_authorization(irreversible: bool) -> None:
    state = await run_state()
    state.task = "Report Project Birch author replies and links."
    state.caller_start = "https://example.test/birch/comments"
    state.open_answer_outputs = ("Report links for Project Birch.",)
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
    prompt = llm.calls[0][1][-1].content
    assert '"label": "Description"' in prompt and '"read": false' in prompt
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
    state.open_answer_outputs = ("Report links for Project Birch.",)
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
