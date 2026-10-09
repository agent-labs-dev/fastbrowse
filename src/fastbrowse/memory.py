"""Small evidence-backed memory with stable citation ids and explicit truncation."""

import hashlib
import json
import re
from collections.abc import Iterable, Mapping
from datetime import datetime
from typing import Self
from urllib.parse import urlsplit

from pydantic import model_validator

from fastbrowse.models import Evidence, FactReader, Frozen
from fastbrowse.page import Capture
from fastbrowse.planner import Plan, Requirement


class Tally(Frozen):
    requirement_id: str
    key: str
    records: tuple[str, ...]
    """Citation ids of distinct records assigned to this key, never a model-written total."""

    @property
    def count(self) -> int:
        return len(set(self.records))


class Fact(Frozen):
    requirement_id: str | None = None
    tally: Tally | None = None
    text: str
    evidence: Evidence | None
    """The span the fact was read from; None when a repeated choice, count, total or winner rests on its basis."""
    basis: tuple[str, ...] = ()
    """Fact ids of the sources this choice, count or comparison rests on."""
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


def shows(text: str, quote: str) -> bool:
    """Whether `text` still shows `quote` as whole words: "Priya Sharma" is inside the corrected "Priya Sharman"."""
    return re.search(rf"(?<!\w){re.escape(quote)}(?!\w)", text) is not None


def _address(url: str) -> str:
    return urlsplit(url)._replace(fragment="").geturl()


def evidence_id(evidence: Evidence) -> str:
    # Equal text on different pages must not inherit the first page's citation or record identity.
    address = hashlib.sha256(_address(evidence.url).encode()).hexdigest()[:16]
    identity = f"{evidence.capture_sha256}:{evidence.start}:{evidence.end}:{address}"
    if evidence.control_context is not None:
        # Equal captions on different controls must not inherit one another's structural association.
        context = hashlib.sha256(
            json.dumps([evidence.frame_id, evidence.source_id, evidence.control_context.model_dump()]).encode()
        ).hexdigest()[:16]
        identity += f":{context}"
    return identity


def fact_id(fact: Fact) -> str:
    """A read fact is keyed by its span; a derived one by what it concludes from which facts."""
    if fact.tally is not None:
        digest = hashlib.sha256(json.dumps([fact.tally.requirement_id, fact.tally.key]).encode()).hexdigest()
        return f"tally:{digest[:16]}"
    if fact.evidence is not None:
        return evidence_id(fact.evidence)
    digest = hashlib.sha256(json.dumps([fact.text, sorted(fact.basis)]).encode()).hexdigest()
    return f"derived:{digest[:16]}"


class CapturedPage(Frozen):
    url: str
    title: str


class Notes:
    def __init__(self, facts: Iterable[Fact] = ()) -> None:
        self._facts: dict[str, Fact] = {}
        self._requirements: dict[str, set[str]] = {}
        self._tally_records: set[str] = set()
        self._record_ids: dict[tuple[str, str, str], list[dict[str, str]]] = {}
        self._continuation_records: dict[str, set[str]] = {}
        self._pages: dict[tuple[str, str, datetime], CapturedPage] = {}
        for fact in facts:
            self.add(fact)

    def remember_capture(self, capture: Capture) -> None:
        # A title can change while the quoted body does not; recapturing must not rename older evidence.
        self._pages.setdefault(
            (capture.sha256, capture.url, capture.captured_at), CapturedPage(url=capture.url, title=capture.title)
        )

    def captured_page(self, evidence: Evidence) -> CapturedPage | None:
        return self._pages.get((evidence.capture_sha256, evidence.url, evidence.captured_at))

    @property
    def facts(self) -> tuple[Fact, ...]:
        return tuple(self._facts.values())

    @property
    def evidence(self) -> dict[str, Evidence]:
        return {key: fact.evidence for key, fact in self._facts.items() if fact.evidence is not None}

    def current_evidence(self, capture: Capture | None = None) -> dict[str, Evidence]:
        """The evidence a later read has not superseded, and a quote the given capture still shows on its address.

        A superseded fact stays as context but is not a current value, so offering it would let the extraction
        copy a price the final page corrected; a quote from another page still stands.
        """
        current = {
            key: fact.evidence for key, fact in self._facts.items() if fact.evidence is not None and self.current(key)
        }
        if capture is None:
            return current
        return {
            key: evidence
            for key, evidence in current.items()
            if _address(evidence.url) != _address(capture.url) or shows(capture.text, evidence.quote)
        }

    def derived(self, key: str) -> bool:
        return key in self._facts and self._facts[key].evidence is None

    def current(self, key: str) -> bool:
        """Retired facts remain history, including derived facts whose original quotes remain in context."""
        fact = self._facts.get(key)
        return fact is not None and (fact.requirement_id is None or bool(self._requirements[key]))

    @property
    def tallies(self) -> tuple[Tally, ...]:
        return tuple(
            sorted(
                (fact.tally for fact in self._facts.values() if fact.tally is not None),
                key=lambda tally: (tally.requirement_id, -tally.count, tally.key),
            )
        )

    def add_tally(self, tally: Tally) -> Fact:
        records: list[str] = []
        for key in tally.records:
            fact = self._facts[key]
            if fact.evidence is None:
                raise ValueError("a tally record must cite a captured span")
            evidence = fact.evidence
            # Equal quotes on different addresses can be distinct rows; only recaptures share an identity.
            identity = (tally.requirement_id, _address(evidence.url), " ".join(evidence.quote.split()))
            occurrences = self._record_ids.setdefault(identity, [])
            sha = evidence.capture_sha256
            occurrence = next((item for item in occurrences if item.get(sha) == key), None)
            if occurrence is None:
                occurrence = next((item for item in occurrences if sha not in item), None)
            if occurrence is None:
                occurrence = {}
                occurrences.append(occurrence)
            occurrence[sha] = key
            canonical = next(iter(occurrence.values()))
            records.append(canonical)
            self._tally_records.add(key)
        fact = Fact(text="", evidence=None, basis=tuple(records), tally=tally, reader=FactReader.LLM)
        key = fact_id(fact)
        previous = self._facts.get(key)
        records = list(dict.fromkeys((*(previous.basis if previous else ()), *records)))
        tally = tally.model_copy(update={"records": tuple(records)})
        fact = fact.model_copy(update={"text": f"{tally.key}: {tally.count}", "basis": tuple(records), "tally": tally})
        self.unevidence((tally.requirement_id,))
        self._facts[key] = fact
        self._requirements.setdefault(key, set())
        return fact

    def add_continuation(self, requirement_id: str, record_id: str) -> None:
        self._continuation_records.setdefault(requirement_id, set()).add(record_id)

    def comparison_records(self, requirement_id: str | None = None) -> tuple[str, ...]:
        records = (
            set().union(*self._continuation_records.values())
            if requirement_id is None
            else self._continuation_records.get(requirement_id, set())
        )
        return tuple(key for key in self._facts if key in records)

    def has_untallied_records(self, requirement_id: str) -> bool:
        tallied = {
            key
            for identity, occurrences in self._record_ids.items()
            if identity[0] == requirement_id
            for occurrence in occurrences
            for key in occurrence.values()
        }
        return bool(self._continuation_records.get(requirement_id, set()) - tallied)

    def complete_tallies(self, requirement_id: str) -> None:
        for key, fact in self._facts.items():
            if fact.tally is not None and fact.tally.requirement_id == requirement_id:
                self._requirements[key].add(requirement_id)
                self._facts[key] = fact.model_copy(update={"requirement_id": requirement_id})

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

    def fact_requirements(self, key: str) -> tuple[str, ...]:
        return tuple(sorted(self._requirements.get(key, ())))

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

    def supersede(self, requirement_id: str, url: str, sha256: str, text: str) -> None:
        """Once a read of `url` evidences a requirement, earlier reads of that address whose quote the page no
        longer shows stop evidencing it, and stay as context.

        After a date picker's choice was corrected from 31/10 to 28/11, a new read quoted the new date, but the
        answer still quoted the old one, since both facts evidenced the requirement."""
        supporting = self.supporting(requirement_id)
        fresh = tuple(
            evidence for evidence in self.supporting_evidence(requirement_id) if evidence.capture_sha256 == sha256
        )
        if not fresh:
            return
        # A capture includes child frames whose quotes belong to their own addresses, not the parent page's.
        addresses = {_address(url)} | {_address(span.url) for span in fresh}
        evidence = self.evidence
        for key, fact in supporting:
            sources = (
                (evidence.get(source) for source in self.expand_evidence_ids((key,)))
                if fact.basis and fact.tally is None
                else (fact.evidence,)
            )
            if any(
                source is not None
                and source.capture_sha256 != sha256
                and _address(source.url) in addresses
                and not shows(text, source.quote)
                for source in sources
            ):
                # A vanished quote cannot remain a current value through another requirement sharing its span.
                self._requirements[key].clear()

    def read_for(self, requirement_id: str) -> tuple[Evidence, ...]:
        """The spans read as evidence of a requirement, including those a later read superseded."""
        supporting = {key for key, _ in self.supporting(requirement_id)} | {
            key for key, fact in self._facts.items() if fact.requirement_id == requirement_id
        }
        sources = set(self.expand_evidence_ids(supporting))
        return tuple(fact.evidence for key, fact in self._facts.items() if fact.evidence is not None and key in sources)

    def supporting(self, requirement_id: str) -> tuple[tuple[str, Fact], ...]:
        """A tally is ranked by code; other facts keep the order they were read in."""
        supporting = [(key, self._facts[key]) for key, ids in self._requirements.items() if requirement_id in ids]
        return tuple(sorted(supporting, key=lambda item: -item[1].tally.count if item[1].tally else 0))

    def supporting_evidence(self, requirement_id: str) -> tuple[Evidence, ...]:
        keys = self.expand_evidence_ids(key for key, _ in self.supporting(requirement_id))
        evidence = self.evidence
        return tuple(evidence[key] for key in keys if key in evidence)

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

    def render_for_navigation(self, max_chars: int) -> str:
        """Recent claims for picking the next action; completion checks use full quoted evidence."""
        if max_chars < 0:
            raise ValueError("max_chars must be nonnegative")
        marker = f"[{len(self._facts)} facts omitted]"
        lines: list[str] = []
        sources: dict[str, int] = {}
        used = len(marker)
        # A long source quote can consume the whole action budget and hide which items were already checked.
        # Actions need progress claims; readers and verification retain their separate quoted view.
        for key, fact in reversed(self._facts.items()):
            source = ""
            if fact.evidence is not None:
                address = fact.evidence.url
                source = f" source={sources.get(address, len(sources) + 1)}"
                if address not in sources:
                    # Tracking addresses hid previously checked entities from the action chooser.
                    source += (
                        f" url={json.dumps(address)}"
                        if len(address) <= 256
                        else f" url_prefix={json.dumps(address[:256])}"
                    )
            line = (
                f"{json.dumps(fact.text, ensure_ascii=False)} "
                f"requirements={','.join(sorted(self._requirements[key])) or '-'}{source}"
            )
            if used + len(line) + 1 <= max_chars:
                lines.append(line)
                used += len(line) + 1
                if fact.evidence is not None:
                    sources.setdefault(fact.evidence.url, len(sources) + 1)
        omitted = len(self._facts) - len(lines)
        if omitted:
            lines.append(f"[{omitted} facts omitted]")
        result = "\n".join(lines)
        if len(result) > max_chars:
            raise ValueError("max_chars is too small to report omitted facts")
        return result

    def render(self, max_chars: int, *, preserve_requirements: bool = False, json_encoded: bool = False) -> str:
        return self.render_with_ids(
            max_chars, preserve_requirements=preserve_requirements, json_encoded=json_encoded
        ).text

    def render_with_ids(
        self,
        max_chars: int,
        *,
        preserve_requirements: bool = False,
        json_encoded: bool = False,
        labels: Mapping[str, str] | None = None,
    ) -> RenderedNotes:
        """Every fact in read order when they all fit; otherwise unrelated context is dropped before requirement
        evidence and its basis, each group kept in read order.

        Read order is what the composer weighs: listing requirement facts first put a reader's one-quote
        conclusion ("X is the most expensive") above the prices it compared, and the answer cited only that.
        Verdicts must fail when requirement evidence cannot fit, rather than decide without it.
        """
        if max_chars < 0:
            raise ValueError("max_chars must be nonnegative")

        aliases = {key: index for index, key in enumerate(self._facts, 1)}
        shown_ids = labels or {}
        evidence = self.evidence
        counted = {key for fact in self._facts.values() if fact.tally is not None for key in fact.basis}
        # A record can be counted and compared by price; its tally alone cannot evidence the price comparison.
        summarized = self._tally_records - set(self.comparison_records())

        def basis_text(fact: Fact) -> str:
            # A ranking can cite every counted record again; full span ids undo the tally's compact rendering.
            records = ",".join(str(aliases[key]) for key in fact.basis if key in counted)
            other = [shown_ids.get(key, key) for key in fact.basis if key not in counted]
            parts = [f"records({records})"] if records else []
            if other:
                parts.append(json.dumps(other))
            return " basis=" + " + ".join(parts) if parts else ""

        def line(key: str, fact: Fact) -> str:
            if fact.tally is not None:
                urls = tuple(dict.fromkeys(evidence[record].url for record in fact.basis))
                return (
                    f"[{shown_ids.get(key, key)}] {json.dumps(fact.text, ensure_ascii=False)} "
                    f"requirements={','.join(sorted(self._requirements[key])) or '-'} "
                    f"tally_for={fact.tally.requirement_id}{basis_text(fact)} urls={json.dumps(urls)}"
                )
            source = (
                "derived"
                if fact.evidence is None
                else f"source={json.dumps(fact.evidence.source_id)} url={json.dumps(fact.evidence.url)} "
                f"quote={json.dumps(fact.evidence.quote, ensure_ascii=False)}"
            )
            if fact.evidence is not None and fact.evidence.control_context is not None:
                source += f" control_context={fact.evidence.control_context.model_dump_json()}"
            # Quoted basis facts use the source text as their claim; sending it twice inflates every later read.
            text = (
                ""
                if fact.evidence is not None and fact.text == fact.evidence.quote
                else json.dumps(fact.text, ensure_ascii=False) + " "
            )
            return (
                f"[{shown_ids.get(key, key)}] {text}"
                f"requirements={','.join(sorted(self._requirements[key])) or '-'} {source}" + basis_text(fact)
            )

        def size(text: str) -> int:
            # A JSON state escapes quotes and newlines; its notes budget must count those extra characters.
            return len(json.dumps(text)) - len('""') if json_encoded else len(text)

        visible = {key: fact for key, fact in self._facts.items() if key not in summarized or self._requirements[key]}
        # Counts are ranked in code; the reader need only select the output the task asked for.
        ranked = sorted(
            visible.items(),
            key=lambda item: (0, -item[1].tally.count, item[1].tally.key) if item[1].tally else (1, 0, ""),
        )

        def render(entries: list[tuple[str, Fact]]) -> str:
            groups: dict[str, set[str]] = {}
            for _, fact in entries:
                if fact.tally is not None:
                    groups.setdefault(fact.tally.requirement_id, set()).update(fact.tally.records)
            totals = [
                f"Tally {key}: {len(records)} distinct records in shown groups, descending counts."
                for key, records in groups.items()
            ]
            return "\n".join([*totals, *(line(key, fact) for key, fact in entries)])

        complete = render(ranked)
        if size(complete) <= max_chars:
            return RenderedNotes(text=complete, evidence_ids=tuple(self._facts))
        required = set(self.expand_evidence_ids(key for key, ids in self._requirements.items() if ids))
        ordered = sorted(ranked, key=lambda item: item[0] not in required)
        keys = tuple(key for key, _ in ordered)
        for count in range(len(ordered) - 1, -1, -1):
            kept = set(keys[:count]) | (set(self.expand_evidence_ids(keys[:count])) & summarized)
            if preserve_requirements and required - kept:
                raise NotesTooLarge(f"Requirement evidence exceeds the {max_chars} character notes budget")
            if any(set(fact.basis) - kept - summarized for _, fact in ordered[:count]):
                continue
            result = "\n".join(filter(None, [render(ordered[:count]), f"[{len(ordered) - count} facts omitted]"]))
            if size(result) <= max_chars:
                return RenderedNotes(text=result, evidence_ids=tuple(key for key in self._facts if key in kept))
        if preserve_requirements:
            raise NotesTooLarge(f"The {max_chars} character notes budget cannot report omitted facts")
        raise ValueError("max_chars is too small to report omitted citations")
