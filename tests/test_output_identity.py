import json

import pytest
from pydantic import ValidationError

from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import FactReader
from fastbrowse.page import BlockKind
from fastbrowse.retrieval import Claim, assemble_answer
from fastbrowse.verification import _QuotedIdentity, check_answer_outputs
from tests.test_answer_repair import RoutingJev
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


class IdentityWriter(ScriptedLLM):
    def __init__(self, scope="entities", quote="Adapter Beacon", ref="q1", url_ref="u1"):
        super().__init__([])
        self.scope, self.quote, self.ref, self.url_ref = scope, quote, ref, url_ref

    async def generate(self, purpose, messages, schema, **kwargs):
        payload = json.loads(messages[-1].content)
        if schema.__name__ == "_OutputIdentities":
            response = {
                "bindings": {
                    key: {
                        "scope": self.scope,
                        "identities": (
                            [{"source_ref": self.ref, "quote": self.quote}] if self.scope == "entities" else []
                        ),
                    }
                    for key in payload["criteria"]
                }
            }
            # This seam's identities are explicit test inputs, rather than the default field-audit fixture.
            self.calls.append((purpose, tuple(messages)))
            from fastbrowse.llm import Generation
            from fastbrowse.models import CostBasis, CostComponent, CostLine

            if kwargs.get("ledger") is not None:
                kwargs["ledger"].reserve(CostComponent.LLM)
            return Generation(
                data=schema.model_validate(response),
                cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose),
            )
        field = next(iter(payload["criteria"].values()))
        if self.scope == "entities" and "requested_entities" in field:
            assert field["requested_entities"] == [
                {
                    "reference": "Report Adapter Beacon exact port count.",
                    "url_ref": self.url_ref,
                    "quote": "Adapter Beacon",
                }
            ]
            assert "Actual reported value 999" not in json.dumps(field.get("requested_entities"))
        self.responses.append({"judgments": {key: "yes" for key in payload["criteria"]}, "reason": "Test verdict."})
        return await super().generate(purpose, messages, schema, **kwargs)


def two_entity_answer():
    facts = []
    for i, text in enumerate(("Adapter Atlas has 2 ports.", "Adapter Beacon has 3 ports.")):
        page = capture((BlockKind.PARAGRAPH, text)).model_copy(update={"url": f"https://example.test/{i}"})
        facts.append(Fact(text=text, evidence=block_evidence(page, "s0"), reader=FactReader.LLM))
    notes = Notes(facts)
    answer = assemble_answer(tuple(Claim(text=fact.text, evidence_ids=(fact_id(fact),)) for fact in facts), notes, ())
    return notes, answer


@pytest.mark.parametrize("ref,quote", [("q999", "Adapter Beacon"), ("q1", "Invented Beacon")])
async def test_identity_must_copy_an_offered_literal_source(ref, quote):
    notes, answer = two_entity_answer()
    writer = IdentityWriter(ref=ref, quote=quote)
    assert not await check_answer_outputs(
        RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",)
    )
    assert len(writer.calls) == 1


async def test_source_and_assertion_audits_share_literal_identity_without_reported_values():
    notes, answer = two_entity_answer()
    writer = IdentityWriter()
    assert await check_answer_outputs(RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",))
    assert len(writer.calls) == 4


@pytest.mark.parametrize("scope,expected", [("subjectless", True), ("unresolved", False)])
async def test_subjectless_outputs_and_unresolved_subjects_have_distinct_outcomes(scope, expected):
    notes, answer = two_entity_answer()
    assert (
        await check_answer_outputs(
            RoutingJev(), IdentityWriter(scope=scope), answer, notes, ("Use the requested answer format.",)
        )
        is expected
    )


def test_identity_cannot_inject_a_model_written_reference():
    with pytest.raises(ValidationError):
        _QuotedIdentity.model_validate({"source_ref": "q1", "quote": "Adapter Beacon", "reference": "Value is 999"})


async def test_single_compound_claim_does_not_skip_subject_binding():
    notes, _ = two_entity_answer()
    atlas = notes.facts[0]
    answer = assemble_answer(
        (
            Claim(
                text="Adapter Atlas has 2 ports and Adapter Beacon has 3 ports.",
                evidence_ids=(fact_id(atlas),),
            ),
        ),
        notes,
        (),
    )
    writer = IdentityWriter(quote="Adapter Beacon", ref="q0")
    assert not await check_answer_outputs(
        RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",)
    )
    assert len(writer.calls) == 1


@pytest.mark.parametrize("title,expected", [("Adapter Beacon", True), ("Unrelated adapter", False)])
async def test_identity_can_copy_only_an_offered_captured_page_title(title, expected):
    page = capture((BlockKind.PARAGRAPH, "3 ports.")).model_copy(update={"title": title})
    fact = Fact(text="3 ports.", evidence=block_evidence(page, "s0"), reader=FactReader.LLM)
    notes = Notes((fact,))
    notes.remember_capture(page)
    answer = assemble_answer((Claim(text="Adapter Beacon has 3 ports.", evidence_ids=(fact_id(fact),)),), notes, ())
    writer = IdentityWriter(ref="q0", url_ref="u0")
    assert (
        await check_answer_outputs(RoutingJev(), writer, answer, notes, ("Report Adapter Beacon exact port count.",))
        is expected
    )
