"""Where a campaign's Jev calls went, read from the requests its fastbrowse rows traced.

`python -m fastbrowse_evals.jev_calls LEDGER.jsonl ...` prints a markdown table of Jev calls per run by decision
purpose, with their dollars, the requests a run sent twice, the requests that resent a state it had already sent,
and the steps that sent the most. A ledger row or a trace row qualifies when it carries `events`, which hold every
Jev request a run sent beside the `call` span that names its purpose.
"""

from __future__ import annotations

import hashlib
import json
import statistics
import sys
from collections import Counter
from collections.abc import Iterable, Iterator, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass
class Request:
    purpose: str
    step: int
    """The step this request decided or read for: the count of pages the run had recorded before it."""
    dollars: float | None
    """What the provider reported charging, or None for a route whose response reports no cost."""
    state: str
    """A digest of the state sent, so requests resending one state can be told apart from those that changed it."""
    request: str
    """A digest of the whole request, state and questions."""


@dataclass
class Run:
    task: str
    requests: list[Request] = field(default_factory=list)


def _digest(value: object) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def requests(events: Iterable[object]) -> list[Request]:
    """Each Jev request in `events`, named by the `call` span it was sent inside."""
    traced = [event for event in events if isinstance(event, Mapping)]
    spans = [event for event in traced if event.get("event") == "call" and event.get("call") == "jev"]
    found: list[Request] = []
    pages = 0
    for event in traced:
        if event.get("event") == "page":
            pages += 1
        sent = event.get("request")
        if event.get("event") != "http" or not isinstance(sent, Mapping) or "questions" not in sent:
            continue
        span = next((s for s in spans if s["began"] <= event["began"] and event["ended"] <= s["ended"]), None)
        purpose = "unnamed" if span is None else str(span.get("decision_purpose") or "unnamed")
        # A response that was not JSON, such as a plain-text 503, is traced as its text and reports no cost.
        response = event.get("response")
        usage = response.get("usage") if isinstance(response, Mapping) else None
        cost = usage.get("cost") if isinstance(usage, Mapping) else None
        found.append(
            Request(
                purpose=purpose,
                step=pages,
                dollars=None if cost is None else float(cost),
                state=_digest(sent.get("state")),
                request=_digest(sent),
            )
        )
    return found


def runs(rows: Iterable[Mapping[str, Any]]) -> Iterator[Run]:
    """fastbrowse's runs among `rows`, each with the Jev requests it traced."""
    for row in rows:
        if row.get("arm", "fastbrowse") != "fastbrowse" or not row.get("events"):
            continue
        yield Run(task=str(row.get("task") or row.get("id")), requests=requests(row["events"]))


def _repeats(sent: list[Request], key: str) -> Counter[str]:
    """Requests per purpose that repeat an earlier request's `key` in the same run."""
    seen: set[str] = set()
    repeated: Counter[str] = Counter()
    for request in sent:
        value = getattr(request, key)
        if value in seen:
            repeated[request.purpose] += 1
        seen.add(value)
    return repeated


def report(measured: list[Run], *, top: int = 10) -> str:
    """The markdown table for `measured`."""
    if not measured:
        return "No fastbrowse run with traced events."
    count = len(measured)
    per_run = [len(run.requests) for run in measured]
    purposes = Counter(request.purpose for run in measured for request in run.requests)
    exact: Counter[str] = Counter()
    resent: Counter[str] = Counter()
    for run in measured:
        exact.update(_repeats(run.requests, "request"))
        resent.update(_repeats(run.requests, "state"))
    total = sum(per_run)
    lines = [
        f"{count} runs, {total} Jev calls: median {statistics.median(per_run):g} and mean {total / count:.2f} a run.",
        "",
        "| Purpose | Calls a run (mean) | Calls a run (median) | Share | Dollars a run | Exact repeats a run "
        "| Same-state requests a run |",
        "|---|---|---|---|---|---|---|",
    ]
    for purpose, calls in purposes.most_common():
        mine = [sum(1 for r in run.requests if r.purpose == purpose) for run in measured]
        priced = [r.dollars for run in measured for r in run.requests if r.purpose == purpose]
        unpriced = sum(1 for dollars in priced if dollars is None)
        # An unpriced request makes the purpose's dollars a floor, never a total that counts it free.
        dollars = f"${sum(d for d in priced if d is not None) / count:.5f}" + (
            f" (+{unpriced} unpriced)" if unpriced else ""
        )
        lines.append(
            f"| {purpose} | {calls / count:.2f} | {statistics.median(mine):g} | {calls / total:.0%} "
            f"| {dollars} | {exact[purpose] / count:.2f} | {resent[purpose] / count:.2f} |"
        )
    steps = Counter((run.task, r.purpose, r.step) for run in measured for r in run.requests)
    lines += [
        "",
        f"The {top} task steps that sent the most requests of one purpose, summed over the task's runs:",
        "",
        "| Task | Purpose | Step | Requests |",
        "|---|---|---|---|",
    ]
    lines += [f"| {task} | {purpose} | {step} | {n} |" for (task, purpose, step), n in steps.most_common(top)]
    return "\n".join(lines)


def read(paths: Iterable[Path]) -> Iterator[Mapping[str, Any]]:
    for path in paths:
        with path.open() as rows:
            yield from (json.loads(line) for line in rows if line.strip())


def main(argv: list[str] | None = None) -> None:
    paths = [Path(arg) for arg in (sys.argv[1:] if argv is None else argv)]
    if not paths:
        raise SystemExit("usage: python -m fastbrowse_evals.jev_calls LEDGER.jsonl ...")
    print(report(list(runs(read(paths)))))


if __name__ == "__main__":
    main()
