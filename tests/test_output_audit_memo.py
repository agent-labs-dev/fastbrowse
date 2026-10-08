"""An unchanged audit reuses its verdict while each changed assertion is checked again."""

import json

import pytest

from fastbrowse.memory import Fact, Notes, fact_id
from fastbrowse.models import CostComponent, FactReader, Limits
from fastbrowse.page import BlockKind
from fastbrowse.retrieval import Claim, assemble_answer
from fastbrowse.telemetry import Ledger
from fastbrowse.verification import check_answer_outputs
from tests.test_answer_repair import RepairWriter, RoutingJev, price_notes
from tests.test_retrieval import ScriptedLLM, block_evidence, capture


async def test_repair_reuses_source_audit_but_checks_changed_assertion():
    notes, fact = price_notes()
    writer = RepairWriter()
    cache = {}
    rejected = assemble_answer((Claim(text="Price 12", evidence_ids=(fact_id(fact),)),), notes, ())
    corrected = assemble_answer((Claim(text="Member price 12", evidence_ids=(fact_id(fact),)),), notes, ())
    assert not await check_answer_outputs(
        RoutingJev(), writer, rejected, notes, ("Report the price.",), audit_cache=cache
    )
    assert await check_answer_outputs(RoutingJev(), writer, corrected, notes, ("Report the price.",), audit_cache=cache)
    sources = [json.loads(messages[-1].content) for _, messages in writer.calls]
    assert len(sources) == 3


class AuditWriter(ScriptedLLM):
    def __init__(self, verdict="yes"):
        super().__init__([])
        self.verdict = verdict

    async def generate(self, purpose, messages, schema, **kwargs):
        fields = json.loads(messages[-1].content)["criteria"]
        self.responses.append({"judgments": {key: self.verdict for key in fields}, "reason": "Stored audit reason."})
        return await super().generate(purpose, messages, schema, **kwargs)


@pytest.mark.parametrize("verdict", ["yes", "no", "uncertain"])
async def test_identical_audit_reuses_positive_and_negative_verdicts_without_billing(verdict):
    notes, fact = price_notes()
    answer = assemble_answer((Claim(text="Member price 12", evidence_ids=(fact_id(fact),)),), notes, ())
    writer, cache, ledger = AuditWriter(verdict), {}, Ledger(Limits())
    results = []
    for _ in range(2):
        missing, corrections = [], []
        result = await check_answer_outputs(
            RoutingJev(),
            writer,
            answer,
            notes,
            ("Report the price.",),
            audit_cache=cache,
            ledger=ledger,
            missing_outputs=missing,
            corrections=corrections,
        )
        results.append((result, missing, corrections))
    assert results[0] == results[1]
    assert results[0][0] is (verdict == "yes")
    assert len(writer.calls) == (2 if verdict == "yes" else 1)
    assert ledger.llm_calls == len(writer.calls)
    assert sum(line.component is CostComponent.LLM for line in ledger.lines) == len(writer.calls)


@pytest.mark.parametrize("change", ["task", "criterion", "quote", "url", "source", "frame", "client"])
async def test_changed_audit_context_cannot_reuse_a_previous_verdict(change):
    notes, fact = price_notes()
    answer = assemble_answer((Claim(text="Member price 12", evidence_ids=(fact_id(fact),)),), notes, ())
    writer, cache = AuditWriter(), {}
    assert await check_answer_outputs(RoutingJev(), writer, answer, notes, ("Report the price.",), audit_cache=cache)
    task, checks = "", ("Report the price.",)
    if change == "task":
        task = "Report the price for another entity."
    elif change == "criterion":
        checks = ("Report the price for another entity.",)
    elif change == "client":
        writer = AuditWriter()
    else:
        page = capture(
            (BlockKind.PARAGRAPH, "Member price 13" if change == "quote" else "Member price 12"),
            url="https://other.test" if change == "url" else "https://example.test",
        )
        evidence = block_evidence(page, "s0")
        if change in {"source", "frame"}:
            evidence = evidence.model_copy(update={"source_id" if change == "source" else "frame_id": "different"})
        fact = Fact(text="Price 12", evidence=evidence, reader=FactReader.LLM, requirement_id="r")
        notes = Notes((fact,))
        answer = assemble_answer((Claim(text="Member price 12", evidence_ids=(fact_id(fact),)),), notes, ())
    before = len(writer.calls)
    assert await check_answer_outputs(RoutingJev(), writer, answer, notes, checks, task=task, audit_cache=cache)
    assert len(writer.calls) - before == 2


async def test_repeated_rejected_assertion_replays_its_correction_and_reason():
    notes, fact = price_notes()
    answer = assemble_answer((Claim(text="Price 12", evidence_ids=(fact_id(fact),)),), notes, ())
    writer, cache, results = RepairWriter(), {}, []
    for _ in range(2):
        corrections = []
        assert not await check_answer_outputs(
            RoutingJev(), writer, answer, notes, ("Report the price.",), audit_cache=cache, corrections=corrections
        )
        results.append(corrections)
    assert results[0] == results[1]
    assert results[0][0].reason == "conditions omitted or extra detail unsupported"
    assert len(writer.calls) == 2


async def test_claim_audit_propagates_the_same_run_scoped_memo():
    from fastbrowse.config import Thresholds
    from fastbrowse.verification import check_claims

    notes, fact = price_notes()
    answer = assemble_answer((Claim(text="Member price 12", evidence_ids=(fact_id(fact),)),), notes, ())
    writer, cache = AuditWriter(), {}
    for _ in range(2):
        assert (
            await check_claims(
                RoutingJev(),
                answer,
                notes,
                Thresholds(),
                answer_checks=("Report the price.",),
                llm=writer,
                audit_cache=cache,
            )
            is not None
        )
    assert len(writer.calls) == 2


async def test_cached_verdict_cannot_supply_a_quote_missing_from_current_notes():
    notes, fact = price_notes()
    answer = assemble_answer((Claim(text="Member price 12", evidence_ids=(fact_id(fact),)),), notes, ())
    writer, cache = AuditWriter(), {}
    assert await check_answer_outputs(RoutingJev(), writer, answer, notes, ("Report the price.",), audit_cache=cache)
    assert not await check_answer_outputs(
        RoutingJev(), writer, answer, Notes(()), ("Report the price.",), audit_cache=cache
    )
    assert len(writer.calls) == 2
