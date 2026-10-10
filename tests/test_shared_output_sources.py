"""A repeated source fits once while every claim keeps its citation and checks."""

import json

import pytest

from fastbrowse.memory import CollectionFact, Comparison, Fact, Notes, fact_id
from fastbrowse.models import FactReader
from fastbrowse.page import BlockKind
from fastbrowse.planner import Requirement, RequirementKind
from fastbrowse.retrieval import Claim, FieldAnswer, assemble_answer
from fastbrowse.verification import check_answer_outputs
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


@pytest.mark.parametrize("failure", [None, "source", "assertion"])
async def test_repeated_collection_quote_keeps_every_claim_and_rejection(failure):
    quote = "Birch comments: " + "Full member context. " * 700
    page = capture((BlockKind.PARAGRAPH, quote))
    fact = Fact(text="Birch comments.", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    collection = CollectionFact(
        text="Birch comments.",
        scope="Birch comments",
        basis=(fact_id(fact),),
        evidence=None,
        reader=FactReader.LLM,
        comparison=Comparison(requirement_id="r", records=(fact_id(fact),), complete=True),
    )
    conflict = capture((BlockKind.PARAGRAPH, "Birch: conflicting comment."))
    counter = Fact(text=conflict.text, evidence=block_evidence(conflict, "s0"), reader=FactReader.LLM)
    notes = Notes((fact, collection, counter))
    requirement = Requirement(id="r", text="Report every Birch comment.", kind=RequirementKind.INFORMATION)
    claims = tuple(Claim(text=f"Comment {i} concerns Birch.", evidence_ids=(fact_id(collection),)) for i in range(9))
    base = assemble_answer(claims, notes, (requirement,))
    answer = FieldAnswer(**base.model_dump(), output_claims={requirement.text: tuple(range(9))})

    class Auditor(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            payload = json.loads(messages[-1].content)
            if schema.__name__ != "_OutputIdentities":
                field = payload["criteria"]["output_0"]
                stage = "assertion" if "reported_claims" in field else "source"
                records = field.get("reported_claims", field.get("sources"))
                assert len(records) == 9
                assert payload["shared_sources"][field["counterevidence"][0]["source_ref"]]["quote"] == conflict.text
                for record in records:
                    assert record["compared_records"] == [
                        {"scope": "Birch comments", "complete": True, "record_indices": [0]}
                    ]
                    assert payload["shared_sources"][record["cited_sources"][0]["source_ref"]]["quote"] == quote
                if stage == "assertion":
                    assert [record["text"] for record in records] == [claim.text for claim in claims]
                self.responses.append(
                    {"judgments": {"output_0": "no" if failure == stage else "yes"}, "reason": "Source audit."}
                )
            return await super().generate(purpose, messages, schema, **kwargs)

    missing = []
    accepted = await check_answer_outputs(
        ScriptedJev({}), Auditor([]), answer, notes, (requirement.text,), bounded=True, missing_outputs=missing
    )
    assert accepted is (failure is None)
    assert missing == ([] if failure is None else [requirement.text])
