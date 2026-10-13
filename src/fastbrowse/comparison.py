"""Numeric rankings over complete, cited records, with ambiguous fields left to the reader."""

import re
from decimal import Decimal
from typing import Literal

from pydantic import Field

from fastbrowse.memory import Comparison, Fact, Notes
from fastbrowse.models import FactReader, Frozen


class QuotedField(Frozen):
    prefix: str
    suffix: str

    def extract(self, quote: str) -> str | None:
        # Empty delimiters mean the boundary of the quote, never every position inside it.
        start = re.escape(self.prefix) if self.prefix else r"\A"
        end = re.escape(self.suffix) if self.suffix else r"\Z"
        # Adjacent fields can share a delimiter, so consuming a match would hide the next one.
        matches = list(re.finditer(r"(?=" + start + r"(.*?)" + end + r")", quote, re.DOTALL))
        if len(matches) != 1:
            return None
        value = matches[0][1].strip()
        return value if value and "\n" not in value else None


class NumericComparison(Frozen):
    order: Literal["lowest", "highest"]
    limit: int = Field(ge=1, le=60)
    label: QuotedField
    value: QuotedField


def complete_comparison(notes: Notes, requirement_id: str, comparison: NumericComparison) -> bool:
    records = notes.comparison_records(requirement_id)
    if not records:
        return False
    known = notes.evidence
    rows: list[tuple[Decimal, str, str]] = []
    units: set[str] = set()
    for key in records:
        evidence = known.get(key)
        if evidence is None:
            return False
        label = comparison.label.extract(evidence.quote)
        value = comparison.value.extract(evidence.quote)
        if label is None or value is None:
            return False
        # Commas, ranges, conversions and multiple units need interpretation, not a guessed number format.
        number = re.fullmatch(r"([$\u00a3\u20ac]?)(-?\d+(?:\.\d+)?)", value)
        if number is None:
            return False
        units.add(number[1])
        rows.append((Decimal(number[2]), label, value))
    if len(units) != 1 or len(rows) < comparison.limit or len({row[1] for row in rows}) != len(rows):
        return False
    rows.sort(key=lambda row: row[0], reverse=comparison.order == "highest")
    # A tied cutoff needs the task's tie rule; choosing the first captured row would silently invent one.
    if len(rows) > comparison.limit and rows[comparison.limit - 1][0] == rows[comparison.limit][0]:
        return False
    text = f"{comparison.order.capitalize()} {comparison.limit}:\n" + "\n".join(
        f"{label}: {value}" for _, label, value in rows[: comparison.limit]
    )
    # Context such as the active category or filter is part of the proof, even when it is outside a record.
    notes.add(
        Fact(
            requirement_id=requirement_id,
            comparison=Comparison(requirement_id=requirement_id, records=records, complete=True),
            text=text,
            evidence=None,
            basis=tuple(known),
            reader=FactReader.LLM,
        )
    )
    return True
