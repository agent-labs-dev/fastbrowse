"""Durable physical-request reservations for the nonpublishable browser diagnostic.

This wallet does not establish provider prices. Its caller must supply a verified
bound for the exact request before sending it. Interrupted requests keep that bound.
"""

from __future__ import annotations

import fcntl
import os
import threading
from decimal import Decimal
from enum import StrEnum
from pathlib import Path
from typing import Annotated, Literal

from parallax.models import Model
from pydantic import Field, TypeAdapter, field_validator

POLICY = "paid-call-reservations-v1"
_SCALE = 10**18
Money = Annotated[Decimal, Field(ge=0, max_digits=30, decimal_places=18, allow_inf_nan=False)]


class SpendStop(RuntimeError):
    """The wallet cannot authorize another physical request."""


class _MoneyModel(Model):
    @field_validator("limit", "bound", "actual", mode="before", check_fields=False)
    @classmethod
    def _no_float(cls, value: object) -> object:
        if isinstance(value, float):
            raise ValueError("dollars must be exact decimal strings or Decimal values")
        return value


class _Header(_MoneyModel):
    kind: Literal["header"] = "header"
    policy: Literal["paid-call-reservations-v1"] = POLICY
    campaign: str = Field(min_length=1)
    limit: Money = Field(gt=0)


class _Reserve(_MoneyModel):
    kind: Literal["reserve"] = "reserve"
    request_id: str = Field(min_length=1)
    quote_id: str = Field(min_length=1)
    bound: Money = Field(gt=0)


class _Settle(_MoneyModel):
    kind: Literal["settle"] = "settle"
    request_id: str = Field(min_length=1)
    actual: Money | None


_EVENT = TypeAdapter(Annotated[_Header | _Reserve | _Settle, Field(discriminator="kind")])


class ChargeState(StrEnum):
    RESERVED = "reserved"
    UNKNOWN = "unknown"
    SETTLED = "settled"


class Charge(Model):
    request_id: str
    quote_id: str
    bound: Money
    actual: Money | None = None
    state: ChargeState = ChargeState.RESERVED


def _units(value: Decimal) -> int:
    # Decimal arithmetic inherits a caller's precision; integer accounting does not.
    sign, digits, exponent = value.as_tuple()
    assert isinstance(exponent, int)
    coefficient = int("".join(map(str, digits)))
    if coefficient == 0:
        return 0
    assert not sign
    while exponent < -18 and coefficient % 10 == 0:
        coefficient //= 10
        exponent += 1
    assert exponent >= -18
    return coefficient * 10 ** (exponent + 18)


def _dollars(units: int) -> Decimal:
    whole, fraction = divmod(units, _SCALE)
    return Decimal(f"{whole}.{fraction:018d}")


class RequestWallet:
    """One process owns a journal; threads share one admission lock.

    A failed write poisons this instance. Recovery refuses a torn journal instead
    of guessing whether an upstream request was sent after its last durable row.
    """

    def __init__(self, path: Path, *, campaign: str, limit: Decimal) -> None:
        self._header = _Header(campaign=campaign, limit=limit)
        self._mutex = threading.RLock()
        self._owner_pid = os.getpid()
        self._closed = False
        self._poisoned = False
        self._charges: dict[str, Charge] = {}
        path.parent.mkdir(parents=True, exist_ok=True)
        self._lock_fd = os.open(path.with_suffix(path.suffix + ".lock"), os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        self._fd = -1
        try:
            try:
                fcntl.flock(self._lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise SpendStop("another process owns this request wallet") from None
            try:
                self._fd = os.open(path, os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            except FileExistsError:
                self._fd = os.open(path, os.O_RDWR | os.O_APPEND | os.O_NOFOLLOW)
                self._restore()
            else:
                self._append(self._header)
                directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(directory)
                finally:
                    os.close(directory)
        except BaseException:
            self.close()
            raise

    def __enter__(self) -> RequestWallet:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def close(self) -> None:
        with self._mutex:
            if not self._closed:
                self._closed = True
                if self._fd >= 0:
                    os.close(self._fd)
                os.close(self._lock_fd)

    @property
    def charges(self) -> tuple[Charge, ...]:
        with self._mutex:
            return tuple(self._charges.values())

    @property
    def committed(self) -> Decimal:
        with self._mutex:
            return _dollars(self._committed_units())

    def _committed_units(self) -> int:
        return sum(_units(c.actual if c.actual is not None else c.bound) for c in self._charges.values())

    def _check_open(self) -> None:
        if os.getpid() != self._owner_pid:
            raise SpendStop("a forked process cannot use its parent's request wallet")
        if self._closed or self._poisoned:
            raise SpendStop("request wallet is closed or has an unresolved journal failure")

    def _check_reserve(self, event: _Reserve) -> None:
        if event.request_id in self._charges:
            raise ValueError("a physical request cannot reserve twice")
        if any(c.actual is not None and c.actual > c.bound for c in self._charges.values()):
            raise SpendStop("a provider charge exceeded its verified bound")
        if self._committed_units() + _units(event.bound) > _units(self._header.limit):
            raise SpendStop("the approved total cannot hold this physical request")

    def _apply(self, event: _Reserve | _Settle) -> None:
        if isinstance(event, _Reserve):
            self._check_reserve(event)
            self._charges[event.request_id] = Charge(**event.model_dump(exclude={"kind"}))
        else:
            charge = self._charges.get(event.request_id)
            if charge is None or charge.state == ChargeState.SETTLED:
                raise ValueError("only an unresolved reservation can settle")
            self._charges[event.request_id] = charge.model_copy(
                update={
                    "actual": event.actual,
                    "state": ChargeState.UNKNOWN if event.actual is None else ChargeState.SETTLED,
                }
            )

    def _restore(self) -> None:
        with os.fdopen(os.dup(self._fd), "rb") as source:
            rows = source.readlines()
        if not rows or any(not row.endswith(b"\n") for row in rows):
            raise ValueError("request wallet has an empty or torn journal")
        header = _EVENT.validate_json(rows[0])
        if header != self._header:
            raise ValueError("request wallet requires its original campaign, policy and limit")
        for row in rows[1:]:
            event = _EVENT.validate_json(row)
            if isinstance(event, _Header):
                raise ValueError("request wallet contains a duplicate header")
            self._apply(event)

    def _append(self, event: _Header | _Reserve | _Settle) -> None:
        pending = memoryview((event.model_dump_json() + "\n").encode())
        try:
            while pending:
                written = os.write(self._fd, pending)
                if written <= 0:
                    raise OSError("request wallet write made no progress")
                pending = pending[written:]
            os.fsync(self._fd)
        except BaseException:
            self._poisoned = True
            raise

    def reserve(self, request_id: str, *, quote_id: str, bound: Decimal) -> Charge:
        event = _Reserve(request_id=request_id, quote_id=quote_id, bound=bound)
        with self._mutex:
            self._check_open()
            self._check_reserve(event)
            try:
                self._append(event)
                self._apply(event)
            except BaseException:
                self._poisoned = True
                raise
            return self._charges[request_id]

    def settle(self, request_id: str, actual: Decimal | None) -> Charge:
        event = _Settle(request_id=request_id, actual=actual)
        with self._mutex:
            self._check_open()
            charge = self._charges.get(request_id)
            if charge is None or charge.state == ChargeState.SETTLED:
                raise ValueError("only an unresolved reservation can settle")
            try:
                self._append(event)
                self._apply(event)
            except BaseException:
                self._poisoned = True
                raise
            if actual is not None and actual > charge.bound:
                raise SpendStop("a provider charge exceeded its verified bound; actual charge retained")
            return self._charges[request_id]
