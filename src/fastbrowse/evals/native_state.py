"""A declarative contract for judging an attempt from observations of the site's own state, never from its answer.

A supplement is data: ordered witnesses, each a named moment (`before`, `during`, `after`) with checks over what an
independent observer read then. A check is an equality, a minimum or maximum, or a count of records matching a
partial record. The module names no task, site or field; those belong to the supplement file a reviewer pins by
digest. `before` and `during` are read by a trusted observer that the harness runs beside the attempt, and `after`
is read by the grader once the attempt has ended. Nothing the agent produced is ever a reading: its answer and its
`outcome.data` are model text. A required witness that is absent leaves the attempt ungraded, because a sequence
(an appointment booked, then cancelled) cannot be inferred from the state it ended in.
"""

from __future__ import annotations

import hashlib
import json
import math
from collections.abc import Iterator, Mapping, Sequence
from contextlib import contextmanager
from contextvars import ContextVar
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, JsonValue, StrictInt, field_validator, model_validator

Status = Literal["pass", "fail", "ungraded"]
Source = Literal["runner", "grade"]
Phase = Literal["before", "during", "after"]

_HEX = r"^[a-f0-9]{64}$"


class _Model(BaseModel):
    # An unknown key is refused: silently dropping a term a reviewer wrote would let a wrong attempt pass.
    model_config = ConfigDict(extra="forbid", frozen=True)


class Equals(_Model):
    kind: Literal["equals"]
    path: str = ""
    value: JsonValue


def _finite_number(value: object) -> object:
    # A bool is an int to Python, so `true` would read as the bound 1; NaN and infinity make every comparison false
    # or true by accident.
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise ValueError("a bound must be a finite number, not a boolean")
    return value


class Minimum(_Model):
    kind: Literal["minimum"]
    path: str = ""
    value: float

    _finite = field_validator("value", mode="before")(_finite_number)


class Maximum(_Model):
    kind: Literal["maximum"]
    path: str = ""
    value: float

    _finite = field_validator("value", mode="before")(_finite_number)


class Records(_Model):
    """The list at `path` holds between `minimum` and `maximum` records that contain every key of `match`."""

    kind: Literal["records"]
    path: str = ""
    match: dict[str, JsonValue]
    minimum: StrictInt = Field(default=1, ge=0)
    maximum: StrictInt | None = Field(default=None, ge=0)
    casefold: bool = False
    """Compare strings without case, for an identity such as an email that a site may store in another case."""

    @model_validator(mode="after")
    def _bounds_ordered(self) -> Records:
        if self.maximum is not None and self.maximum < self.minimum:
            raise ValueError(f"records maximum {self.maximum} is below minimum {self.minimum}")
        return self


Check = Annotated[Equals | Minimum | Maximum | Records, Field(discriminator="kind")]


class Probe(_Model):
    """A named read through the site's own state probe. Checks reach its result as `<name>.<path>`."""

    kind: Literal["probe"]
    name: str = Field(min_length=1)
    probe: str = Field(min_length=1)
    args: dict[str, JsonValue] = {}


class Http(_Model):
    """A named GET of a path on the site's own origin. The grader supplies the origin and the credential."""

    kind: Literal["http"]
    name: str = Field(min_length=1)
    path: str = Field(pattern=r"^/[^?#\s]*$")
    query: dict[str, str] = {}


Observer = Annotated[Probe | Http, Field(discriminator="kind")]


class Witness(_Model):
    label: Phase
    checks: tuple[Check, ...] = Field(min_length=1)
    source: Source = "runner"
    """`runner`: the harness's own observer sampled it while the attempt ran (`before` or `during`). `grade`: the
    grader reads it once the attempt has ended, which is only honest for `after`, the state it ended in."""
    observe: tuple[Observer, ...] = Field(min_length=1)
    """What is read at that moment. The observer that reads it is never the agent, and the names are the first
    path segment a check follows."""
    required: bool = True
    note: str | None = None

    @model_validator(mode="after")
    def _moment_matches_source(self) -> Witness:
        if (self.source == "grade") != (self.label == "after"):
            raise ValueError(f"{self.label}: only `after` is read at grade time, and `after` is never sampled earlier")
        if len({observer.name for observer in self.observe}) != len(self.observe):
            raise ValueError(f"{self.label}: observer names must be distinct")
        return self


class Supplement(_Model):
    """A strict, separately labelled addition to an upstream predicate, bound to that predicate and task by digest."""

    id: str = Field(min_length=1)
    task_id: str = Field(min_length=1)
    version: str = Field(min_length=1)
    source: str = Field(min_length=1)
    revision: str = Field(pattern=r"^[a-f0-9]{40}$")
    dataset_sha256: str = Field(pattern=_HEX)
    site: str = Field(min_length=1)
    task_sha256: str = Field(pattern=_HEX)
    """SHA-256 of the task text the agent was given. A reworded task is a different task."""
    upstream_sha256: str = Field(pattern=_HEX)
    purpose: str = Field(min_length=1)
    witnesses: tuple[Witness, ...] = Field(min_length=1)
    unwitnessed: tuple[str, ...] = ()
    """Requirements of the task that no witness here can read. A supplement that lists any never passes: the
    attempt is reported limited and left ungraded, not rounded up to a pass on the strength of the end state."""

    @model_validator(mode="after")
    def _witnesses_distinct(self) -> Supplement:
        order = ("before", "during", "after")
        if len(set(self.labels)) != len(self.labels):
            raise ValueError(f"{self.id}: a witness label may appear once")
        # `evaluate` and the grader walk witnesses in declared order, so a backwards list would grade a sequence
        # (booked, then cancelled) as if it ran the other way.
        if list(self.labels) != sorted(self.labels, key=order.index):
            raise ValueError(f"{self.id}: witnesses must be declared in the order before, during, after")
        return self

    @property
    def labels(self) -> tuple[str, ...]:
        return tuple(witness.label for witness in self.witnesses)


class Observation(_Model):
    """One reading an observer took, named for the witness it answers."""

    label: str = Field(min_length=1)
    data: JsonValue


class Sample(_Model):
    """One reading by the harness's observer: every observer the supplement names for that moment, by name."""

    sequence: StrictInt = Field(ge=0)
    at: float
    phase: Phase
    data: dict[str, JsonValue]

    _finite = field_validator("at", mode="before")(_finite_number)


class TrustedObservations(_Model):
    """The harness's separate payload key `native_observations`, never a field of the agent's `outcome`.

    `observer_sha256` names the observer code that took the samples, so a swapped observer is a different grader."""

    observer_sha256: str = Field(pattern=_HEX)
    task_id: str = Field(min_length=1)
    samples: tuple[Sample, ...]

    @model_validator(mode="after")
    def _ordered(self) -> TrustedObservations:
        rank = {"before": 0, "during": 1, "after": 2}
        for index, sample in enumerate(self.samples):
            if sample.sequence != index:
                raise ValueError("samples must be numbered from 0 without gaps")
            if index and (
                sample.at < self.samples[index - 1].at or rank[sample.phase] < rank[self.samples[index - 1].phase]
            ):
                raise ValueError("samples must be in time and phase order")
        return self


_TRUSTED: ContextVar[TrustedObservations | None] = ContextVar("native_observations", default=None)


@contextmanager
def attached(observations: TrustedObservations | None) -> Iterator[None]:
    """Make the watcher's readings the ones the next `grade_request` carries, and only for this block.

    A ContextVar keeps the `Grader(ref, outcome)` seam unchanged and puts the readings out of the agent's reach:
    nothing on an `Outcome` can set it, and the token is reset when the grade ends, however it ends."""
    token = _TRUSTED.set(observations)
    try:
        yield
    finally:
        _TRUSTED.reset(token)


def attached_observations() -> TrustedObservations | None:
    return _TRUSTED.get()


class Verdict(_Model):
    status: Status
    failures: tuple[str, ...] = ()
    missing: tuple[str, ...] = ()
    checked: tuple[str, ...] = ()

    @property
    def passed(self) -> bool:
        return self.status == "pass"


def canonical_digest(value: object) -> str:
    """SHA-256 of the canonical JSON of `value`: the pin that binds a supplement to the predicate it extends.

    A value JSON cannot hold is refused rather than stringified, since two different objects could print alike."""
    try:
        body = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True, allow_nan=False)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"not canonical JSON: {exc}") from exc
    return hashlib.sha256(body.encode()).hexdigest()


def _lookup(data: object, path: str) -> tuple[bool, object]:
    """Follow a dotted path through objects and list indexes. The empty path is the value itself."""
    current = data
    for part in filter(None, path.split(".")):
        if isinstance(current, Mapping) and part in current:
            current = current[part]
        elif (
            isinstance(current, Sequence)
            and not isinstance(current, str)
            and part.isdigit()
            and int(part) < len(current)
        ):
            current = current[int(part)]
        else:
            return False, None
    return True, current


def _same(actual: object, expected: object, *, casefold: bool) -> bool:
    if isinstance(actual, str) and isinstance(expected, str):
        return actual.strip().casefold() == expected.strip().casefold() if casefold else actual == expected
    if isinstance(actual, Mapping) and isinstance(expected, Mapping):
        return actual.keys() == expected.keys() and all(
            _same(actual[key], expected[key], casefold=casefold) for key in actual
        )
    if isinstance(actual, list) and isinstance(expected, list):
        return len(actual) == len(expected) and all(
            _same(a, e, casefold=casefold) for a, e in zip(actual, expected, strict=True)
        )
    # Python's `True == 1`: a boolean on either side must meet a boolean, or `1` would accept a site's `true`.
    if isinstance(actual, bool) or isinstance(expected, bool):
        return type(actual) is type(expected) and actual == expected
    return actual == expected


def _contains(record: object, match: Mapping[str, JsonValue], *, casefold: bool) -> bool:
    if not isinstance(record, Mapping):
        return False
    for key, expected in match.items():
        if key not in record:
            return False
        actual = record[key]
        if isinstance(expected, Mapping):
            if not _contains(actual, expected, casefold=casefold):
                return False
        elif not _same(actual, expected, casefold=casefold):
            return False
    return True


def _number(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        return None
    return float(value)


def _describe(check: Check) -> str:
    where = check.path or "<root>"
    if isinstance(check, Records):
        bound = f"{check.minimum}..{'*' if check.maximum is None else check.maximum}"
        return f"{where}: records {bound} matching {json.dumps(check.match, sort_keys=True)}"
    return f"{where}: {check.kind} {json.dumps(check.value)}"


def run_check(check: Check, data: JsonValue) -> str | None:
    """Why `check` fails against `data`, or None when it holds. A value the path cannot reach always fails."""
    found, actual = _lookup(data, check.path)
    if not found:
        return f"{_describe(check)} (path not observed)"
    if isinstance(check, Equals):
        ok = _same(actual, check.value, casefold=False)
    elif isinstance(check, (Minimum, Maximum)):
        number = _number(actual)
        ok = number is not None and (number >= check.value if isinstance(check, Minimum) else number <= check.value)
    else:
        if not isinstance(actual, Sequence) or isinstance(actual, str):
            return f"{_describe(check)} (not a list)"
        count = sum(_contains(item, check.match, casefold=check.casefold) for item in actual)
        ok = count >= check.minimum and (check.maximum is None or count <= check.maximum)
        if not ok:
            return f"{_describe(check)} (found {count})"
    return None if ok else f"{_describe(check)} (observed {json.dumps(actual)[:200]})"


def select_observations(supplement: Supplement, trusted: TrustedObservations) -> list[Observation]:
    """The `before` and `during` readings out of the observer's samples, as `Observation`s in witness order.

    `before` is the last sample taken before the attempt began. `during` is the first sample whose checks hold: a
    sampler can miss a short-lived state, so a sequence it never caught is left absent (ungraded), never a pass."""
    found: list[Observation] = []
    for witness in supplement.witnesses:
        if witness.source != "runner":
            continue
        pool = [sample for sample in trusted.samples if sample.phase == witness.label]
        if witness.label == "before":
            pool = pool[-1:]
        else:
            pool = [sample for sample in pool if not any(run_check(check, sample.data) for check in witness.checks)][:1]
        found.extend(Observation(label=witness.label, data=sample.data) for sample in pool)
    return found


def evaluate(supplement: Supplement, observations: Sequence[Observation]) -> Verdict:
    """Judge `observations` against the supplement's witnesses, in the order the supplement declares.

    A failing check is a fail, even when another witness is absent: one contradicted witness already refutes the
    attempt. Otherwise a missing required witness leaves it ungraded, and only a full set of passing witnesses
    passes. Observations must arrive in witness order; a reading out of order is not a reading of that sequence."""
    by_label: dict[str, tuple[int, Observation]] = {}
    for position, observation in enumerate(observations):
        if observation.label in by_label:
            return Verdict(status="fail", failures=(f"{observation.label}: observed more than once",))
        by_label[observation.label] = (position, observation)
    failures: list[str] = []
    missing: list[str] = []
    checked: list[str] = []
    last = -1
    for witness in supplement.witnesses:
        seen = by_label.get(witness.label)
        if seen is None:
            if witness.required:
                missing.append(witness.label)
            continue
        position, observation = seen
        if position < last:
            failures.append(f"{witness.label}: observed out of sequence")
            continue
        last = position
        checked.append(witness.label)
        failures.extend(
            f"{witness.label}: {reason}" for check in witness.checks if (reason := run_check(check, observation.data))
        )
    unknown = sorted(set(by_label) - set(supplement.labels))
    failures.extend(f"{label}: no witness declares this observation" for label in unknown)
    if failures:
        return Verdict(status="fail", failures=tuple(failures), missing=tuple(missing), checked=tuple(checked))
    status: Status = "ungraded" if missing else "pass"
    return Verdict(status=status, missing=tuple(missing), checked=tuple(checked))


def parse_supplements(raw: object) -> dict[str, Supplement]:
    """`task id -> supplement` from a file's decoded JSON: `{"supplements": [...]}`. A duplicate task is refused."""
    if not isinstance(raw, Mapping) or not isinstance(raw.get("supplements"), list):
        raise ValueError('supplement file must be an object with a "supplements" list')
    found: dict[str, Supplement] = {}
    for item in raw["supplements"]:
        supplement = Supplement.model_validate(item)
        if supplement.task_id in found:
            raise ValueError(f"{supplement.task_id}: more than one supplement")
        found[supplement.task_id] = supplement
    return found
