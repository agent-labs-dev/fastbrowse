"""Publish a measured eval run to the private Langfuse project, attributed to the build that produced it.

The publication gate in `publication.py` decides what a number may claim. This module does not touch that: it
keeps the run behind a number reviewable, by uploading one rows file as one Langfuse trace per task attempt to
the dedicated `fastbrowse-evals` project, and reading every trace back before it reports success.

    uv run python -m fastbrowse.evals.tracking --rows artifacts/evals/nightly.jsonl

Three properties make a published trace usable as evidence, and each is enforced rather than assumed:

- Every trace names the build. The rows must all come from one clean commit, and that commit, the run id and
  the task and suite versions go on the trace, so a figure traces back to the code that produced it.
- A re-run maps to the same traces. A trace id is a digest of the campaign and the attempt, so a retried job
  overwrites its own traces instead of publishing a second copy of one measurement; read-back therefore checks
  the run id and output on the trace, because an id that already existed is not proof this upload landed.
- Upload is not approval. Nothing here writes `docs/results` or a benchmark feed, and fixture rows are never
  a comparison figure.

Upload uses the OTLP endpoint, the path Langfuse serves for live data, and asks for real-time ingestion: without
that header Langfuse may take many minutes to expose a direct export through the read API. Read-back reads the v2
observation index, the read path Langfuse keeps: the trace endpoint it once fell back to is retired on 2026-11-16,
so a span that is not in the index yet is waited for rather than looked up somewhere that will not exist.
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

OTLP_HEADERS = {"x-langfuse-ingestion-version": "4"}
"""Ask for real-time ingestion: a direct exporter without this can take up to fifteen minutes to be readable."""

SCOPE = "fastbrowse.evals"
"""The instrumentation scope a reviewer sees on the trace beside the build."""

VERIFY_ATTEMPTS = 60
"""How often to look for a trace before calling it unreadable: ingestion is queued, not synchronous."""

VERIFY_DELAY_SECONDS = 5.0
"""Seconds between read-backs. Langfuse queued an upload for over four minutes when this was measured, so the
deadline is generous on purpose: a run that is simply still queueing is not a failed upload."""

READ_FIELDS = "core,basic,io,metadata"
"""The v2 field groups read-back asks for. `io` carries the measured output, and `metadata` carries the run id: the
index omits both unless they are named, so requesting less would read every row as somebody else's upload."""

PROVENANCE = ("run_id", "git_sha", "fastbrowse_version", "run_started", "git_dirty", "providers")
"""What identifies the run. `run` also carries per-suite agent limits, which differ between the local and mock
rows of one run, so the whole object cannot be the identity: a normal nightly file would read as many runs."""

OUTPUT = ("passed", "status", "normalized_status", "seconds", "dollars", "unknown_cost", "error")
"""The measured values read-back compares, so an old trace under a reused id cannot pass as this upload."""


class RowsError(ValueError):
    """The rows cannot be published as one attributable run, so nothing is uploaded."""


class RateLimited(Exception):
    """Langfuse refused a read for now; the caller waits the time it named instead of guessing."""

    def __init__(self, retry_after: float) -> None:
        super().__init__(f"rate limited for {retry_after}s")
        self.retry_after = retry_after


def read_rows(path: Path) -> list[dict[str, Any]]:
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    if not rows:
        raise RowsError(f"{path}: no rows to publish")
    return rows


def run_of(rows: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """One run, or a refusal: a mixed file or a dirty tree is not a measurement anyone can check."""
    identities = {tuple(str((row.get("run") or {}).get(key)) for key in PROVENANCE) for row in rows}
    if len(identities) != 1:
        raise RowsError(f"rows mix {len(identities)} runs; one upload carries exactly one build")
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
    return hashlib.sha256(f"{name}|{_attempt(row)}".encode()).hexdigest()[:32]


def _output(row: Mapping[str, Any]) -> dict[str, Any]:
    return {key: row.get(key) for key in OUTPUT}


def expected(path: Path, rows: Sequence[Mapping[str, Any]]) -> dict[str, dict[str, Any]]:
    """Every trace the upload is answerable for: its id, the name and run id it must carry, and its output."""
    run = run_of(rows)
    name = campaign(path, run)
    return {
        trace_id(name, row): {"name": f"{name}/{_attempt(row)}", "run_id": run.get("run_id"), "output": _output(row)}
        for row in rows
    }


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
    attributes = [
        _attribute("langfuse.observation.type", "span"),
        _attribute("langfuse.trace.name", f"{name}/{_attempt(row)}"),
        _attribute("langfuse.release", name),
        _attribute("langfuse.version", run["fastbrowse_version"]),
        _attribute("langfuse.observation.input", json.dumps({"task": row.get("task"), "suite": row.get("suite")})),
        _attribute("langfuse.observation.output", json.dumps(_output(row), sort_keys=True)),
    ]
    for key, value in (
        ("run_id", run.get("run_id")),
        ("build", run["git_sha"]),
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
        "traceId": trace_id(name, row),
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


def _retry_after(response: httpx.Response) -> float:
    try:
        return float(response.headers.get("retry-after", "5"))
    except ValueError:
        return 5.0


def _matches(observation: Mapping[str, Any], spec: Mapping[str, Any]) -> bool:
    """Whether this stored observation is the one this upload wrote, not an older one under the same name."""
    output = observation.get("output")
    if isinstance(output, str):
        try:
            output = json.loads(output)
        except json.JSONDecodeError:
            return False
    metadata = observation.get("metadata") or {}
    recorded = metadata.get("run_id") if isinstance(metadata, Mapping) else None
    return recorded == spec["run_id"] and output == spec["output"]


def _readable(client: httpx.Client, trace: str, spec: Mapping[str, Any], since: datetime) -> bool:
    """Whether this upload's version of one trace is readable now, in the observation index."""
    params: dict[str, Any] = {
        "traceId": trace,
        "fromStartTime": since.isoformat(),
        "fields": READ_FIELDS,
        "limit": "10",
    }
    index = client.get("/api/public/v2/observations", params=params)
    if index.status_code == 429:
        raise RateLimited(_retry_after(index))
    if index.status_code != 200:
        return False
    return any(_matches(row, spec) for row in index.json().get("data") or [])


def verify(
    client: httpx.Client,
    wanted: Mapping[str, Mapping[str, Any]],
    *,
    since: datetime,
    attempts: int = VERIFY_ATTEMPTS,
    delay: float = VERIFY_DELAY_SECONDS,
) -> None:
    """Fail unless every trace is readable: an upload nobody can read back is not evidence."""
    seen: set[str] = set()
    for attempt in range(1, attempts + 1):
        for trace in [trace for trace in wanted if trace not in seen]:
            try:
                if _readable(client, trace, wanted[trace], since):
                    seen.add(trace)
            except RateLimited as exc:
                # The read API is limited per minute, and a big upload would otherwise spend the window on
                # retries it is refused; waiting what Langfuse asks keeps the tail of the run from starving.
                time.sleep(exc.retry_after)
        if len(seen) == len(wanted):
            return
        if attempt < attempts:
            time.sleep(delay)
    missing = [trace for trace in wanted if trace not in seen]
    raise RowsError(f"{len(missing)} of {len(wanted)} traces are not readable in Langfuse: {', '.join(missing[:5])}")


def publish(rows_path: Path, *, attempts: int = VERIFY_ATTEMPTS, delay: float = VERIFY_DELAY_SECONDS) -> dict[str, Any]:
    """Upload one rows file and read it back, so a caller can report what a reviewer will find."""
    rows = read_rows(rows_path)
    run = run_of(rows)
    wanted = expected(rows_path, rows)
    since = datetime.fromisoformat(run["run_started"]).astimezone(UTC) - timedelta(hours=1)
    with connection() as client:
        client.post(OTLP_PATH, json=payload(rows_path, rows), headers=OTLP_HEADERS).raise_for_status()
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
