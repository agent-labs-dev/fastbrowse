"""Write a tracked browser run to the private fastbrowse-evals Langfuse project, and read back whether it is there.

Parallax 2.0 writes a run with `parallax.track.sink.push`, which attaches an evidence zip to every item, and reads
experiments back only as Parallax runs (`list_runs`, `experiment_items`). A browser run is projected from a ledger,
not from a Parallax run directory, so the evidence zip and the completeness read are this module's.
"""

import io
import json
import time
import zipfile
from collections.abc import Sequence
from dataclasses import replace
from typing import Any

from langfuse import Langfuse
from parallax.track import sink, upload
from parallax.track.projection import RunSpec, TraceSpec, TrackedRun

PROJECT = "fastbrowse-evals"
_ZIP_TIME = (1980, 1, 1, 0, 0, 0)


def marked_smoke(tracked: TrackedRun) -> TrackedRun:
    """A smoke run's projection: its experiments named and its traces tagged as smoke, so it reads as a bug hunt and
    cannot stand in for the experiment a publication looks up by name. Parallax 2.0 has no smoke marking."""
    runs = tuple(
        replace(
            run,
            name=f"smoke-{run.name}",
            traces=tuple(replace(trace, tags=(*trace.tags, "smoke")) for trace in run.traces),
        )
        for run in tracked.runs
    )
    return replace(tracked, runs=runs)


def item_evidence(row: str, trace: TraceSpec, receipts: Sequence[dict[str, Any]] = ()) -> bytes:
    """The item's own projected input and output, and the semantic receipt of each attempt that graded it: the
    evidence a ledger projection has. A fixed timestamp keeps the bytes, and so a re-push, identical."""
    item = {"row": row, "input": trace.input.model_dump(mode="json"), "output": trace.output.model_dump(mode="json")}
    body = json.dumps(item | ({"receipts": list(receipts)} if receipts else {}), indent=2, sort_keys=True).encode()
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(zipfile.ZipInfo("item.json", _ZIP_TIME), body)
    return buffer.getvalue()


def push(tracked: TrackedRun) -> None:
    """Write `tracked` to the fastbrowse-evals project; safe to repeat, since every write lands on the same ids."""
    client, project = sink.connect(write=True)
    try:
        if project.name != PROJECT:
            raise SystemExit(f"refusing to write outside the dedicated {PROJECT} project")
        receipts = getattr(tracked, "receipts", {})

        def evidence(row: str, trace: TraceSpec) -> bytes:
            return item_evidence(row, trace, receipts.get((trace.version, trace.item_id), ()))

        experiments = sink.push(client, project, tracked, evidence)
        for name, experiment_id, url in experiments:
            print(json.dumps({"dataset": tracked.dataset, "experiment": name, "id": experiment_id, "url": url}))
        # `parallax compare` takes only a verified run as a baseline, as Parallax's own upload marks one: once Langfuse
        # reads back every metadata key, score, item, output and evidence zip exactly as written.
        deadline = time.monotonic() + upload.VISIBLE_WITHIN_S
        while problems := upload.mismatches(client, project, tracked, evidence):
            if time.monotonic() > deadline:
                raise SystemExit(f"Langfuse does not hold the run yet: {'; '.join(problems[:5])}")
            time.sleep(upload.POLL_S)
        sink.mark_verified(client, tracked, experiments)
    finally:
        client.shutdown()


def experiment_ids(client: Langfuse, dataset_id: str) -> dict[str, str]:
    """Every experiment on the dataset, by name, whatever wrote it: `sink.list_runs` keeps only those carrying
    Parallax's run metadata, and publication must find an experiment that lacks it to refuse it."""
    found: dict[str, str] = {}
    cursor: str | None = None
    while True:
        page = client.api.experiments.list(
            from_start_time=sink.EPOCH, dataset_id=dataset_id, limit=sink.PAGE, cursor=cursor
        )
        found.update({e.name: e.id for e in page.data})
        cursor = page.meta.cursor
        if not cursor or not page.data:
            return found


def recorded_in_full(client: Langfuse, experiment_id: str, run: RunSpec) -> bool:
    """Whether the experiment holds every item of `run` with each of its scores, and every run-level score."""
    items = {
        item.experiment_item_id: {s.name for s in item.scores or []}
        for item in sink.experiment_items(client, experiment_id, "core,scores")
    }
    listing = client.api.experiments.list(from_start_time=sink.EPOCH, id=experiment_id, fields="scores", limit=1)
    run_scores = {s.name for e in listing.data for s in e.scores or []}
    return run_scores >= {s.name for s in run.scores} and all(
        (have := items.get(t.item_id)) is not None and have >= {s.name for s in t.scores} for t in run.traces
    )
