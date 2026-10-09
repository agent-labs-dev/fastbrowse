import json

import pytest

from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import FactReader
from fastbrowse.page import BlockKind
from fastbrowse.retrieval import Claim, assemble_answer
from fastbrowse.verification import check_answer_outputs
from tests.test_answer_repair import RoutingJev
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


class ConflictWriter(ScriptedLLM):
    async def generate(self, purpose, messages, schema, **kwargs):
        if schema.__name__ != "_OutputIdentities":
            key, field = next(iter(json.loads(messages[-1].content)["criteria"].items()))
            conflict = any("combined output 64.5W" in source["quote"] for source in field.get("counterevidence", ()))
            self.responses.append({"judgments": {key: "no" if conflict else "yes"}, "reason": "Conflicting totals."})
        return await super().generate(purpose, messages, schema, **kwargs)


@pytest.mark.parametrize("location,expected", [("same", False), ("other_url", True), ("other_frame", True)])
async def test_uncited_retained_conflicting_value_reaches_the_output_audit(location: str, expected: bool) -> None:
    page = capture(
        (BlockKind.PARAGRAPH, "Adapter Beacon total wattage 70W Max."),
        (BlockKind.PARAGRAPH, "Adapter Beacon combined output 64.5W."),
    )
    facts = tuple(
        Fact(text=block.source_id, evidence=block_evidence(page, block.source_id), reader=FactReader.LLM)
        for block in page.blocks
    )
    if location != "same":
        peer = facts[1]
        assert peer.evidence is not None
        peer = peer.model_copy(
            update={
                "evidence": peer.evidence.model_copy(
                    update={"url": "https://other.test"} if location == "other_url" else {"frame_id": "child"}
                )
            }
        )
        facts = (facts[0], peer)
    notes = Notes(facts)
    answer = assemble_answer(
        (Claim(text="Adapter Beacon total output is 70W.", evidence_ids=(fact_id(facts[0]),)),), notes, ()
    )
    assert (
        await check_answer_outputs(
            RoutingJev(), ConflictWriter([]), answer, notes, ("Report Adapter Beacon total output.",)
        )
    ) is expected


async def test_new_counterevidence_invalidates_a_previously_passing_cached_audit() -> None:
    page = capture(
        (BlockKind.PARAGRAPH, "Adapter Beacon total wattage 70W Max."),
        (BlockKind.PARAGRAPH, "Adapter Beacon combined output 64.5W."),
    )
    first = Fact(text="Total output", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((first,))
    answer = assemble_answer(
        (Claim(text="Adapter Beacon total output is 70W.", evidence_ids=(fact_id(first),)),), notes, ()
    )
    writer = ConflictWriter([])
    cache = {}
    checks = ("Report Adapter Beacon total output.",)
    assert await check_answer_outputs(RoutingJev(), writer, answer, notes, checks, audit_cache=cache)
    notes.add(Fact(text="Combined output", evidence=block_evidence(page, "s1"), reader=FactReader.LLM))
    assert not await check_answer_outputs(RoutingJev(), writer, answer, notes, checks, audit_cache=cache)
