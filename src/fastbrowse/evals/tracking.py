"""Publish a measured eval run to the private Langfuse project, attributed to the build that produced it.

The publication gate in `publication.py` decides what a number may claim. This module does not touch that: it
keeps the run behind a number reviewable, by uploading one rows file as one Langfuse trace per task attempt to
the dedicated `fastbrowse-evals` project, and reading every trace back before it reports success.

    uv run python -m fastbrowse.evals.tracking --rows artifacts/evals/nightly.jsonl

Three properties make a published trace usable as evidence, and each is enforced rather than assumed:

- Every trace names the build. The rows must all come from one clean commit, and that commit, the run id and
  the task and suite versions go on the trace, so a figure traces back to the code that produced it.
- A re-run maps to the same traces. A trace id is a digest of the campaign and the attempt, so a retried job
  overwrites its own traces instead of publishing a second copy of one measurement.
- Upload is not approval. Nothing here writes `docs/results` or a benchmark feed, and fixture rows are never
  a comparison figure.

Upload uses the OTLP endpoint, the path Langfuse serves for live data; its v1 ingestion API is closing. Read-back
asks the v2 observation index first and falls back to the trace itself, because that index does not yet return
every span ingested through OTLP.
"""

import argparse
import hashlib
import json
import sys
import time
from collections.abc import Mapping, Sequence
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import httpx

from fastbrowse.evals.storage import connection

OTLP_PATH = "/api/public/otel/v1/traces"
"""The endpoint Langfuse ingests live spans through."""

SCOPE = "fastbrowse.evals"
"""The instrumentation scope a reviewer sees on the trace beside the build."""

VERIFY_ATTEMPTS = 12
"""How often to look for a trace before calling it unreadable: ingestion is queued, not synchronous."""

VERIFY_DELAY_SECONDS = 5.0
"""Seconds between read-backs, the wait `storage` already gives an upload before it gives up."""


class RowsError(ValueError):
    """The rows cannot be published as one attributable run, so nothing is uploaded."""


def read_rows(path: Path) -> list[dict[str, Any]]:
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    if not rows:
        raise RowsError(f"{path}: no rows to publish")
    return rows


def run_of(rows: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """One run, or a refusal: a mixed file or a dirty tree is not a measurement anyone can check."""
    runs = {json.dumps(row.get("run"), sort_keys=True) for row in rows}
    if len(runs) != 1:
        raise RowsError(f"rows mix {len(runs)} runs; one upload carries exactly one build")
    run = dict(rows[0]["run"])
    if not run.get("git_sha"):
        raise RowsError("rows carry no git_sha, so a trace could not be attributed to a build")
    if run.get("git_dirty"):
        raise RowsError("rows were measured from a dirty tree; commit the build before publishing")
    return run


def campaign(path: Path, run: Mapping[str, Any]) -> str:
    """The name every trace of this upload shares, and the release a reviewer filters on."""
    return f"fastbrowse/{run['fastbrowse_version']}@{run['git_sha'][:7]}/{path.stem}"


def _attempt(row: Mapping[str, Any]) -> str:
    return f"{row.get('suite')}/{row.get('task')}/r{row.get('repeat')}"


def trace_id(name: str, row: Mapping[str, Any]) -> str:
    """Stable and campaign-scoped: the same attempt maps to the same trace however often it is uploaded."""
    return hashlib.sha256(f"{name}|{_attempt(row)}|{row.get('run_id') or ''}".encode()).hexdigest()[:32]


def expected(path: Path, rows: Sequence[Mapping[str, Any]]) -> dict[str, str]:
    """Every trace the upload is answerable for: its id, and the name read-back must find."""
    name = campaign(path, run_of(rows))
    return {trace_id(name, row): f"{name}/{_attempt(row)}" for row in rows}


def _value(value: Any) -> dict[str, Any]:
    if isinstance(value, bool):
        return {"boolValue": value}
    if isinstance(value, int):
        return {"intValue": str(value)}
    return {"stringValue": str(value)}


def _attribute(key: str, value: Any) -> dict[str, Any]:
    return {"key": key, "value": _value(value)}


def _span(name: str, run: Mapping[str, Any], row: Mapping[str, Any], at: datetime) -> dict[str, Any]:
    """One task attempt as one OTLP span, carrying the build, the run and the numbers it produced."""
    trace = trace_id(name, row)
    attributes = [
        _attribute("langfuse.observation.type", "span"),
        _attribute("langfuse.trace.name", f"{name}/{_attempt(row)}"),
        _attribute("langfuse.release", name),
        _attribute("langfuse.version", run["fastbrowse_version"]),
        _attribute("langfuse.observation.input", json.dumps({"task": row.get("task"), "suite": row.get("suite")})),
        _attribute(
            "langfuse.observation.output",
            json.dumps(
                {
                    key: row.get(key)
                    for key in ("passed", "status", "normalized_status", "seconds", "dollars", "unknown_cost", "error")
                },
                sort_keys=True,
            ),
        ),
    ]
    for key, value in (
        ("build", run["git_sha"]),
        ("run_id", run.get("run_id")),
        ("providers", run.get("providers")),
        ("suite_version", row.get("suite_version")),
        ("task_version", row.get("task_version")),
        ("steps", row.get("steps")),
        ("llm_calls", row.get("llm_calls")),
        ("jev_calls", row.get("jev_calls")),
        ("tokens_in", row.get("tokens_in")),
        ("tokens_out", row.get("tokens_out")),
        ("tokens_per_second", row.get("tokens_per_second")),
    ):
        if value is not None:
            attributes.append(_attribute(f"langfuse.trace.metadata.{key}", value))
    seconds = float(row.get("seconds") or 0)
    return {
        "traceId": trace,
        "spanId": hashlib.sha256(f"{name}|{_attempt(row)}".encode()).hexdigest()[:16],
        "name": _attempt(row),
        "kind": 1,
        "startTimeUnixNano": str(int(at.timestamp() * 1_000_000_000)),
        "endTimeUnixNano": str(int((at + timedelta(seconds=seconds)).timestamp() * 1_000_000_000)),
        "attributes": attributes,
        "status": {"code": 1 if row.get("passed") else 2},
    }


def payload(path: Path, rows: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """The upload as one OTLP request: one resource, one scope, one span per attempt."""
    run = run_of(rows)
    name = campaign(path, run)
    # Each attempt starts where the ones before it finished, so the trace timeline reads like the run.
    at = datetime.fromisoformat(run["run_started"]).astimezone(UTC)
    spans = []
    for row in rows:
        spans.append(_span(name, run, row, at))
        at += timedelta(seconds=float(row.get("seconds") or 0))
    return {
        "resourceSpans": [
            {
                "resource": {
                    "attributes": [
                        _attribute("service.name", "fastbrowse-evals"),
                        _attribute("service.version", run["fastbrowse_version"]),
                        _attribute("deployment.commit", run["git_sha"]),
                    ]
                },
                "scopeSpans": [{"scope": {"name": SCOPE, "version": "1"}, "spans": spans}],
            }
        ]
    }


def _readable(client: httpx.Client, trace: str, name: str, since: datetime) -> bool:
    """Whether one trace is readable now, through the v2 index or, while that lags, the trace itself."""
    params: dict[str, Any] = {
        "traceId": trace,
        "fromStartTime": since.isoformat(),
        "fields": "core,basic",
        "limit": "10",
    }
    indexed = client.get("/api/public/v2/observations", params=params)
    if indexed.status_code == 200 and indexed.json().get("data"):
        return True
    detail = client.get(f"/api/public/traces/{trace}")
    if detail.status_code != 200:
        return False
    body = detail.json()
    return body.get("name") == name or any(o.get("name") == name for o in body.get("observations") or [])


def verify(
    client: httpx.Client,
    wanted: Mapping[str, str],
    *,
    since: datetime,
    attempts: int = VERIFY_ATTEMPTS,
    delay: float = VERIFY_DELAY_SECONDS,
) -> None:
    """Fail unless every trace is readable: an upload nobody can read back is not evidence."""
    for attempt in range(1, attempts + 1):
        missing = [trace for trace, name in wanted.items() if not _readable(client, trace, name, since)]
        if not missing:
            return
        if attempt < attempts:
            time.sleep(delay)
    raise RowsError(f"{len(missing)} of {len(wanted)} traces are not readable in Langfuse: {', '.join(missing[:5])}")


def publish(rows_path: Path, *, attempts: int = VERIFY_ATTEMPTS, delay: float = VERIFY_DELAY_SECONDS) -> dict[str, Any]:
    """Upload one rows file and read it back, so a caller can report what a reviewer will find."""
    rows = read_rows(rows_path)
    run = run_of(rows)
    wanted = expected(rows_path, rows)
    since = datetime.fromisoformat(run["run_started"]).astimezone(UTC) - timedelta(hours=1)
    with connection() as client:
        client.post(OTLP_PATH, json=payload(rows_path, rows)).raise_for_status()
        verify(client, wanted, since=since, attempts=attempts, delay=delay)
    return {
        "campaign": campaign(rows_path, run),
        "build": run["git_sha"],
        "run_id": run.get("run_id"),
        "traces": wanted,
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--rows", type=Path, required=True, help="rows file a fixture or campaign run wrote")
    parser.add_argument("--dry-run", action="store_true", help="print what would be published, without credentials")
    args = parser.parse_args(argv)
    try:
        rows = read_rows(args.rows)
    except (RowsError, json.JSONDecodeError) as exc:
        print(f"eval publishing refused: {exc}", file=sys.stderr)
        return 1
    if args.dry_run:
        run = run_of(rows)
        print(
            json.dumps({"campaign": campaign(args.rows, run), "build": run["git_sha"], "traces": len(rows)}, indent=1)
        )
        return 0
    try:
        published = publish(args.rows)
    except (RowsError, httpx.HTTPError, KeyError) as exc:
        # One named line, so a missing key or a refused project reads as itself in the job log.
        print(f"eval publishing failed: {exc}", file=sys.stderr)
        return 1
    print(json.dumps({**published, "traces": len(published["traces"])}, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
