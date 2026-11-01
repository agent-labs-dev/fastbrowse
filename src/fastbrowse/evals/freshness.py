"""Tag every eval case with the build it was written against, and say when one has gone stale.

`versions.json` says what a case asks and how it is graded. It does not say which build the expectations in it
were earned on, and that is the part that goes stale: the agent ships, the judge model moves to a new release, and
a case keeps its version while the thing its numbers were measured on has been replaced. This module holds that
second tag, one entry per case, and compares it with the build in this checkout.

    uv run python -m fastbrowse.evals.freshness            # report stale cases, fail when one can be resynced
    uv run python -m fastbrowse.evals.freshness --resync   # re-label every movable case at this build

`--resync` only rewrites tags: it runs nothing, so it says a case's figures belong to this build without measuring them
on it. Re-measuring is a campaign, not this module.

A case listed under `immovable` is never re-stamped and never fails the check. It is reported with the reason it
cannot move, so a limit is named rather than the case quietly leaving the comparison.
"""

import argparse
import json
from dataclasses import dataclass
from importlib.metadata import PackageNotFoundError, version
from pathlib import Path

from fastbrowse.evals import judge
from fastbrowse.evals.mock_tasks import TASKS as MOCK_TASKS
from fastbrowse.evals.tasks import TASKS

PINNED = Path(__file__).with_name("freshness.json")
"""Where the tags live. A checkout reads and `--resync` rewrites this file, so a bump is one reviewable diff."""

FIELDS = ("driver", "judge")
"""What a case is tagged with: the fastbrowse release, and the judge model that graded it."""

TAG = "tag"
"""The finding field for a case with no entry at all, which is stale whatever build is running."""


@dataclass(frozen=True, slots=True)
class Finding:
    """One stale case: which tag moved, what it said, what this build says, and why it cannot move if it cannot."""

    case: str
    field: str
    written: str | None
    current: str | None
    reason: str | None = None

    @property
    def movable(self) -> bool:
        return self.reason is None

    def line(self) -> str:
        detail = "no tag" if self.field == TAG else f"{self.field} {self.written} -> {self.current}"
        if self.reason is None:
            return f"{self.case}: {detail}"
        return f"{self.case}: {detail} (immovable: {self.reason})"


def current() -> dict[str, str]:
    """The build running now: the package version, and the judge model the harness grades with.

    A case's answer key is a set of judgements, so the judge is part of what it was written against. The
    OpenRouter id is the tag because it is the one the harness falls back to and the one a published campaign
    records; the Typesafe id is the same release on the other route.
    """
    try:
        driver = version("fastbrowse")
    except PackageNotFoundError:
        driver = "unknown"
    return {"driver": driver, "judge": judge.OPENROUTER_MODEL}


def cases() -> dict[str, str]:
    """Every case the fixture suites run, mapped to the suite that runs it."""
    return {task.id: "local" for task in TASKS} | {task.id: "mock" for task in MOCK_TASKS}


def load(path: Path = PINNED) -> dict[str, object]:
    """The checked-in tags. A missing file is an empty book rather than an error, so a fresh checkout reports
    every case as untagged instead of failing to start."""
    if not path.is_file():
        return {"schema_version": 1, "cases": {}, "immovable": {}}
    return json.loads(path.read_text(encoding="utf-8"))


def check(
    record: dict[str, object], now: dict[str, str] | None = None, known: dict[str, str] | None = None
) -> list[Finding]:
    """Every case whose tag is not this build's, in case order. An untagged case is stale by definition."""
    now = current() if now is None else now
    known = cases() if known is None else known
    entries = record.get("cases") or {}
    reasons = record.get("immovable") or {}
    assert isinstance(entries, dict)
    assert isinstance(reasons, dict)
    findings: list[Finding] = []
    for case in sorted(known):
        reason = reasons.get(case)
        entry = entries.get(case)
        if not isinstance(entry, dict):
            findings.append(Finding(case, TAG, None, None, reason))
            continue
        for field in FIELDS:
            written = entry.get(field)
            if written != now[field]:
                findings.append(Finding(case, field, None if written is None else str(written), now[field], reason))
    return findings


def resync(
    record: dict[str, object], now: dict[str, str] | None = None, known: dict[str, str] | None = None
) -> tuple[dict[str, object], list[Finding]]:
    """Re-label every movable case at this build, and return what was refused. Nothing is re-measured.

    A case that no longer runs is dropped: a tag for a case nothing executes is a claim about nothing. An immovable
    case keeps its tag, or its lack of one, and its reason, so the next run reports it again rather than losing it.
    """
    now = current() if now is None else now
    known = cases() if known is None else known
    reasons = record.get("immovable")
    reasons = reasons if isinstance(reasons, dict) else {}
    refused = [finding for finding in check(record, now, known) if not finding.movable]
    written = record.get("cases")
    written = written if isinstance(written, dict) else {}
    entries: dict[str, dict[str, str]] = {}
    for case, suite in sorted(known.items()):
        entry = written.get(case)
        if case not in reasons:
            entries[case] = {"suite": suite, **now}
        elif isinstance(entry, dict):
            entries[case] = {key: str(value) for key, value in entry.items()}
    return {"schema_version": 1, "cases": entries, "immovable": reasons}, refused


def save(record: dict[str, object], path: Path = PINNED) -> None:
    path.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Report eval cases written against an older build than this one.")
    parser.add_argument(
        "--resync", action="store_true", help="re-label every movable case at this build; nothing is re-measured"
    )
    parser.add_argument("--immovable", nargs=2, metavar=("CASE", "REASON"), help="record why a case cannot be resynced")
    arguments = parser.parse_args(argv)

    record = load()
    now = current()
    if arguments.immovable:
        case, reason = arguments.immovable
        if case not in cases():
            parser.error(f"{case} is not a case the suites run")
        reasons = record.get("immovable")
        if not isinstance(reasons, dict):
            reasons = {}
            record["immovable"] = reasons
        reasons[case] = reason
        save(record)
        print(f"{case}: recorded as immovable, {reason}")
        return 0

    if arguments.resync:
        updated, refused = resync(record)
        save(updated)
        for finding in refused:
            print(finding.line())
        immovable = updated["immovable"]
        assert isinstance(immovable, dict)
        moved = len(cases().keys() - immovable.keys())
        print(f"re-labelled {moved} cases at {now['driver']}; nothing was re-measured")
        return 0

    findings = check(record, now)
    for finding in findings:
        print(finding.line())
    stale = [finding for finding in findings if finding.movable]
    if stale:
        print(f"{len(stale)} of {len(cases())} cases are written against another build")
        print("resync them with: uv run python -m fastbrowse.evals.freshness --resync")
        return 1
    print(f"every case is tagged at {now['driver']} and {now['judge']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
