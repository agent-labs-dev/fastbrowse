"""Gate the start of each physical run on the approved campaign total, and retain unknown charges across interruption.

The approved budget decides one thing only: whether a NEW physical run may start. It is never handed to a running
agent and never stops one. Before a run starts it holds `reservation` dollars, an accounting estimate of what one
run costs, and it may start only while everything already committed plus that hold still fits within the limit.
When the run ends its actual charge replaces the hold. A charge above its hold is recorded as it is and stops
nothing: a run that turned out dearer than the estimate simply leaves less room for the runs after it, and dispatch
continues while the next hold still fits. Unknown and interrupted charges keep their full hold, so a resumed ledger
never forgets money that may have been spent; a run its provider refused to start settles at zero.
"""

import asyncio
import fcntl
import json
import math
import os
from pathlib import Path

POLICY = "start-gate-v1"
"""The only budget policy: the campaign total gates starts; no running agent carries a step, time or dollar limit."""


class BudgetStop(RuntimeError):
    """Nothing is running and the approved total cannot hold another physical run."""


class CampaignBudget:
    def __init__(self, limit: float, reservation: float, path: Path) -> None:
        if not math.isfinite(limit) or limit <= 0 or not math.isfinite(reservation) or reservation <= 0:
            raise ValueError("campaign budget and per-run reservation must be positive finite dollars")
        path.parent.mkdir(parents=True, exist_ok=True)
        self.lock = path.with_suffix(".lock").open("a")
        try:
            fcntl.flock(self.lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            self.lock.close()
            raise ValueError("another process owns this campaign budget") from None
        self.active: set[str] = set()
        self.limit, self.reservation, self.path = limit, reservation, path
        self.committed = 0.0
        self.running = 0
        self.condition = asyncio.Condition()
        self.charges: dict[str, float] = {}
        try:
            if path.exists():
                for line in path.read_text().splitlines():
                    row = json.loads(line)
                    # A ledger written under any other policy reserved or capped differently; it cannot be resumed.
                    if row["limit"] != limit or row["reservation"] != reservation or row.get("policy") != POLICY:
                        raise ValueError("an existing spend ledger requires its original limits and policy")
                    if not math.isfinite(row["committed"]) or row["committed"] < 0:
                        raise ValueError("invalid charge in spend ledger")
                    self.charges[row["id"]] = row["committed"]
                self.committed = math.fsum(self.charges.values())
        except BaseException:
            self.lock.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self) -> None:
        self.lock.close()

    def write(self, key: str, committed: float) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        row = {
            "id": key,
            "limit": self.limit,
            "reservation": self.reservation,
            "committed": committed,
            "policy": POLICY,
        }
        descriptor = os.open(self.path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        try:
            os.write(descriptor, (json.dumps(row) + "\n").encode())
            os.fsync(descriptor)
        finally:
            os.close(descriptor)

    async def reserve(self, key: str) -> float:
        """Hold one run's reservation before it starts; waits while runs in flight may free room, else stops."""
        async with self.condition:
            while True:
                # Recheck after waiting: another waiter may already own this physical attempt.
                if key in self.charges:
                    raise ValueError("a physical attempt cannot reserve twice")
                if self.committed + self.reservation <= self.limit + 1e-9:
                    break
                if not self.running:
                    raise BudgetStop("the approved campaign budget cannot hold another run")
                await self.condition.wait()
            self.write(key, self.reservation)
            self.charges[key] = self.reservation
            self.committed += self.reservation
            self.running += 1
            self.active.add(key)
            return self.reservation

    async def settle(self, key: str, dollars: float | None) -> None:
        """Replace the hold with the run's actual charge; None (unknown) keeps the hold. An overrun never raises."""
        async with self.condition:
            if key not in self.active:
                raise ValueError("only an active reservation can settle")
            if dollars is not None and (not math.isfinite(dollars) or dollars < 0):
                raise ValueError("a reconciled charge must be nonnegative finite dollars")
            charge = self.charges[key] if dollars is None else dollars
            self.write(key, charge)
            self.committed += charge - self.charges[key]
            self.charges[key] = charge
            self.running -= 1
            self.active.remove(key)
            self.condition.notify_all()
