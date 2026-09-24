"""Small evidence-backed memory with stable citation ids and explicit truncation."""

import hashlib
import json
from collections.abc import Iterable
from typing import Self

from pydantic import model_validator

from fastbrowse.models import Evidence, FactReader, Frozen
from fastbrowse.planner import Plan, Requirement


class Fact(Frozen):
    requirement_id: str | None = None
    text: str
    evidence: Evidence | None
    """The span the fact was read from; None for a count, total or winner concluded from its basis alone."""
    basis: tuple[str, ...] = ()
    """Fact ids of the facts this conclusion counts or compares."""
    reader: FactReader
    """Which reader produced the fact; citations are built from it."""

    @model_validator(mode="after")
    def grounded(self) -> Self:
        if self.evidence is None and not self.basis:
            raise ValueError("a fact with no evidence must be derived from a basis")
        return self


class NotesTooLarge(RuntimeError):
    """A verdict cannot fit its requirement evidence without losing facts."""


class RenderedNotes(Frozen):
    text: str
    evidence_ids: tuple[str, ...]


def evidence_id(evidence: Evidence) -> str:
    return f"{evidence.capture_sha256}:{evidence.start}:{evidence.end}"


def fact_id(fact: Fact) -> str:
    """A read fact is keyed by its span; a derived one by what it concludes from which facts."""
    if fact.evidence is not None:
        return evidence_id(fact.evidence)
    digest = hashlib.sha256(json.dumps([fact.text, sorted(fact.basis)]).encode()).hexdigest()
    return f"derived:{digest[:16]}"


class Notes:
    def __init__(self, facts: Iterable[Fact] = ()) -> None:
        self._facts: dict[str, Fact] = {}
        self._requirements: dict[str, set[str]] = {}
        for fact in facts:
            self.add(fact)

    @property
    def facts(self) -> tuple[Fact, ...]:
        return tuple(self._facts.values())

    @property
    def evidence(self) -> dict[str, Evidence]:
        return {key: fact.evidence for key, fact in self._facts.items() if fact.evidence is not None}

    def derived(self, key: str) -> bool:
        return key in self._facts and self._facts[key].evidence is None

    def add(self, fact: Fact) -> bool:
        """Return whether a new span was added; reused spans still evidence other requirements."""
        key = fact_id(fact)
        requirements = self._requirements.setdefault(key, set())
        if fact.requirement_id is not None:
            requirements.add(fact.requirement_id)
        if key in self._facts:
            previous = self._facts[key]
            basis = tuple(dict.fromkeys((*previous.basis, *fact.basis)))
            # A winner can quote a row already collected as context; its answer must survive that reuse.
            kept = fact if previous.requirement_id is None and fact.requirement_id is not None else previous
            # One block can answer two requirements (a card's title and its price): the draft answers each
            # requirement with this fact's text, so every requirement's claim must survive the reuse.
            text = kept.text
            if fact is not kept and fact.requirement_id is not None and fact.text not in text:
                text = f"{text}\n{fact.text}"
            self._facts[key] = kept.model_copy(update={"basis": basis, "text": text})
            return False
        self._facts[key] = fact
        return True

    def evidenced(self, requirement_id: str) -> bool:
        return any(requirement_id in requirements for requirements in self._requirements.values())

    def unevidence(self, requirement_ids: Iterable[str]) -> None:
        """Reopen requirements whose facts were read off the wrong page, keeping the facts as context.

        A price quoted from a summary a guessed address opened is still worth seeing, but left as the answer it
        turned every later click into DONE and every DONE into the same refusal.
        """
        dropped = set(requirement_ids)
        for requirements in self._requirements.values():
            requirements -= dropped

    def supporting(self, requirement_id: str) -> tuple[tuple[str, Fact], ...]:
        """The facts citing a requirement, keyed by evidence id, in the order they were read."""
        return tuple((key, self._facts[key]) for key, ids in self._requirements.items() if requirement_id in ids)

    def expand_evidence_ids(self, keys: Iterable[str]) -> tuple[str, ...]:
        """Cited facts and their transitive basis, once each in read order.

        An id no fact has is kept, after the known ones: dropping it would pass a claim whose citation the claim
        check must see fail.
        """
        cited = tuple(dict.fromkeys(keys))
        pending = list(cited)
        seen: set[str] = set()
        while pending:
            key = pending.pop()
            if key in seen:
                continue
            seen.add(key)
            if key in self._facts:
                pending.extend(self._facts[key].basis)
        return (*(key for key in self._facts if key in seen), *(key for key in cited if key not in self._facts))

    def unresolved(self, plan: Plan) -> tuple[Requirement, ...]:
        return tuple(requirement for requirement in plan.requirements if not self.evidenced(requirement.id))

    def render(self, max_chars: int, *, preserve_requirements: bool = False, json_encoded: bool = False) -> str:
        return self.render_with_ids(
            max_chars, preserve_requirements=preserve_requirements, json_encoded=json_encoded
        ).text

    def render_with_ids(
        self, max_chars: int, *, preserve_requirements: bool = False, json_encoded: bool = False
    ) -> RenderedNotes:
        """Every fact in read order when they all fit; otherwise unrelated context is dropped before requirement
        evidence and its basis, each group kept in read order.

        Read order is what the composer weighs: listing requirement facts first put a reader's one-quote
        conclusion ("X is the most expensive") above the prices it compared, and the answer cited only that.
        Verdicts must fail when requirement evidence cannot fit, rather than decide without it.
        """
        if max_chars < 0:
            raise ValueError("max_chars must be nonnegative")

        def line(key: str, fact: Fact) -> str:
            source = (
                "derived"
                if fact.evidence is None
                else f"source={json.dumps(fact.evidence.source_id)} url={json.dumps(fact.evidence.url)} "
                f"quote={json.dumps(fact.evidence.quote, ensure_ascii=False)}"
            )
            return (
                f"[{key}] {json.dumps(fact.text, ensure_ascii=False)} "
                f"requirements={','.join(sorted(self._requirements[key])) or '-'} {source}"
                + (f" basis={json.dumps(fact.basis)}" if fact.basis else "")
            )

        def size(text: str) -> int:
            # A JSON state escapes quotes and newlines; its notes budget must count those extra characters.
            return len(json.dumps(text)) - len('""') if json_encoded else len(text)

        complete = "\n".join(line(key, fact) for key, fact in self._facts.items())
        if size(complete) <= max_chars:
            return RenderedNotes(text=complete, evidence_ids=tuple(self._facts))
        required = set(self.expand_evidence_ids(key for key, ids in self._requirements.items() if ids))
        ordered = sorted(self._facts.items(), key=lambda item: item[0] not in required)
        lines = [line(key, fact) for key, fact in ordered]
        keys = tuple(key for key, _ in ordered)
        for count in range(len(lines) - 1, -1, -1):
            if preserve_requirements and count < len(required):
                raise NotesTooLarge(f"Requirement evidence exceeds the {max_chars} character notes budget")
            kept = set(keys[:count])
            if any(set(fact.basis) - kept for _, fact in ordered[:count]):
                continue
            result = "\n".join([*lines[:count], f"[{len(lines) - count} facts omitted]"])
            if size(result) <= max_chars:
                return RenderedNotes(text=result, evidence_ids=keys[:count])
        if preserve_requirements:
            raise NotesTooLarge(f"The {max_chars} character notes budget cannot report omitted facts")
        raise ValueError("max_chars is too small to report omitted citations")
