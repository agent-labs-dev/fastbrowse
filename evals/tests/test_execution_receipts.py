import copy
import hashlib
import json
from uuid import uuid4

import pytest

from fastbrowse_evals import execution_receipts as execution
from fastbrowse_evals import publication
from fastbrowse_evals.live import RUNNABLE, prompt
from test_publication import CATALOG, DIGEST, write_ledger

PROMPTS = {t.id: prompt(t) for tasks in RUNNABLE.values() for t in tasks}


def write(path, value):
    path.write_text(json.dumps(value))
    return {"path": path.name, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


@pytest.fixture
def case(tmp_path):
    def edit(rows):
        peers = [r for r in rows if r["task"] == "pypi-version"]
        for row in peers:
            row["provenance"] = {"browser_session_id": str(uuid4())}
            row["session_id"] = str(uuid4())
        first = peers[0]
        outage = copy.deepcopy(first)
        outage.update(
            retries=0,
            selected=False,
            started=True,
            dollars=None,
            unknown_cost=True,
            semantic_judge=None,
            passed=None,
            session_id=None,
            provenance={},
            known_judge_dollars=0,
            unpriced_judge_attempts=0,
        )
        first["retries"] = 1
        rows.insert(0, outage)

    results = write_ledger(tmp_path, "runs", ("browser-use",), edit)
    rows = publication.lines(results.with_suffix(".attempts.jsonl"))
    row = rows[0]
    browser, run_id, session_id = (str(uuid4()) for _ in range(3))

    def watch(bid):
        return f"https://live.browser-use.com?wss=wss%3A%2F%2F{bid}.cdp.browser-use.com"

    peers = [r for r in rows if r["task"] == row["task"]]
    stdout = tmp_path / "run.log"
    stdout.write_text(
        "".join(
            f"WATCH browser-use {row['task']} {watch((r.get('provenance') or {}).get('browser_session_id') or browser)}\n"
            for r in peers
        )
    )
    kinds = ["run.created", "run.dispatching", "browser.ready", "run.dispatch_failed", "worker.never_started"]
    events = [
        dict(id=i + 1, run_id=run_id, ts="2026-11-12T11:47:51Z", type=kind, data={}) for i, kind in enumerate(kinds)
    ]
    events[0]["data"] = {"session_id": session_id, "model": "test-model"}
    events[2]["data"] = {"browser_session_id": browser, "live_view_url": watch(browser)}
    events[-2]["data"] = {"error_code": "worker_never_started", "failure_stage": "worker_admission"}
    archive = dict(
        run=dict(
            id=run_id,
            session_id=session_id,
            model="test-model",
            status="failed",
            task=PROMPTS[row["task"]],
            result=None,
            output=None,
            total_input_tokens=0,
            total_output_tokens=0,
        ),
        pages=[dict(after=None, response=dict(events=events, next_after=5, has_more=False))],
        browser=dict(id=browser, status="stopped", finished_at="2026-11-12T11:47:52Z"),
        endpoints=dict(
            run=f"/api/v4/runs/{run_id}", events=f"/api/v4/runs/{run_id}/events", browser=f"/api/v4/browsers/{browser}"
        ),
    )

    def ref(p):
        return dict(path=p.name, sha256=hashlib.sha256(p.read_bytes()).hexdigest())

    sidecar = dict(
        kind="worker-never-started",
        provider="browser-use-v4",
        fetched_at="2026-11-12T12:00:00Z",
        sdk_version="3.12.1",
        fetched_by="test",
        results=ref(results),
        attempts=ref(results.with_suffix(".attempts.jsonl")),
        budget=ref(results.with_suffix(".budget.jsonl")),
        stdout=ref(stdout),
        provider_archive=write(tmp_path / "provider.json", archive),
        row_index=0,
        row_sha256=execution.row_digest(row),
        arm=row["arm"],
        task=row["task"],
        repeat=row["repeat"],
        retry=0,
        campaign_id=row["run"]["run_id"],
    )
    path = tmp_path / "receipt.json"
    write(path, sidecar)
    return results, path, sidecar, archive, rows


def test_archived_nonexecution_preserves_raw_hold_and_selected_grade(case):
    results, path, _, _, rows = case
    before = {
        p: p.read_bytes()
        for p in (results, results.with_suffix(".attempts.jsonl"), results.with_suffix(".budget.jsonl"))
    }
    receipts = execution.load([path], results, PROMPTS)
    execution.coverage({execution.key(r): r for r in rows}, receipts)
    physical = {execution.key(r): r for r in rows}
    publication.start_gate(
        publication.lines(results.with_suffix(".budget.jsonl")), physical, rows[0]["run"], "test", receipts
    )
    collected = publication.internal_group("internal-hosted", results, CATALOG, DIGEST, [path])
    assert all(row.dollars is None for row in collected.group.rows)
    [tracked] = collected.tracked
    assert any("execution_receipt" in r for group in tracked.receipts.values() for r in group)
    assert all(p.read_bytes() == content for p, content in before.items())
    assert all(row["arm"] == "browser-use" for row in rows)
    public = collected.group.model_dump_json()
    assert json.loads((path.parent / "provider.json").read_text())["run"]["id"] not in public
    with pytest.raises(ValueError, match="coverage"):
        execution.coverage(physical, {})


@pytest.mark.parametrize(
    "change",
    [
        "executed",
        "missing-usage",
        "unfinished-pages",
        "unknown-event",
        "wrong-browser",
        "not-stopped",
        "wrong-task",
        "wrong-session",
        "repeated-event",
        "wrong-endpoint",
    ],
)
def test_provider_contradictions_reject(case, change):
    results, path, sidecar, archive, _ = case
    events = archive["pages"][0]["response"]["events"]
    if change == "executed":
        archive["run"]["total_input_tokens"] = 1
    if change == "missing-usage":
        del archive["run"]["total_output_tokens"]
    if change == "unfinished-pages":
        archive["pages"][0]["response"]["has_more"] = True
    if change == "unknown-event":
        events[1]["type"] = "llm.called"
    if change == "wrong-browser":
        archive["browser"]["id"] = str(uuid4())
    if change == "not-stopped":
        archive["browser"]["status"] = "running"
    if change == "wrong-task":
        archive["run"]["task"] += " different"
    if change == "wrong-session":
        events[0]["data"]["session_id"] = str(uuid4())
    if change == "repeated-event":
        events[-1]["id"] = events[-2]["id"]
    if change == "wrong-endpoint":
        archive["endpoints"]["events"] += "/other"
    sidecar["provider_archive"] = write(path.parent / "provider.json", archive)
    write(path, sidecar)
    with pytest.raises(ValueError):
        execution.load([path], results, PROMPTS)


@pytest.mark.parametrize(
    "change",
    [
        "hash",
        "row-hash",
        "selected",
        "missing-watch",
        "duplicate-watch",
        "two-unlinked",
        "no-later-grade",
        "dollar-field",
    ],
)
def test_identity_and_receipt_negatives(case, change):
    results, path, sidecar, _, rows = case
    if change == "hash":
        sidecar["attempts"]["sha256"] = "0" * 64
    if change == "row-hash":
        sidecar["row_sha256"] = "0" * 64
    if change in {"selected", "two-unlinked", "no-later-grade"}:
        if change == "selected":
            rows[0]["selected"] = True
        if change == "two-unlinked":
            next(
                r
                for r in rows
                if r["task"] == rows[0]["task"] and (r.get("provenance") or {}).get("browser_session_id")
            )["provenance"] = {}
        if change == "no-later-grade":
            rows[1]["semantic_judge"] = None
        ledger = results.with_suffix(".attempts.jsonl")
        ledger.write_text("".join(json.dumps(r) + "\n" for r in rows))
        sidecar["attempts"]["sha256"] = hashlib.sha256(ledger.read_bytes()).hexdigest()
        sidecar["row_sha256"] = execution.row_digest(rows[0])
    if change in {"missing-watch", "duplicate-watch"}:
        log = path.parent / "run.log"
        lines = log.read_text().splitlines(keepends=True)
        log.write_text("".join(lines[1:] if change == "missing-watch" else [*lines, lines[0]]))
        sidecar["stdout"]["sha256"] = hashlib.sha256(log.read_bytes()).hexdigest()
    if change == "dollar-field":
        sidecar["dollars"] = 0
    write(path, sidecar)
    with pytest.raises(ValueError):
        execution.load([path], results, PROMPTS)


def test_duplicate_receipt_and_reduced_hold_reject(case):
    results, path, _, _, rows = case
    with pytest.raises(ValueError, match="duplicate"):
        execution.load([path, path], results, PROMPTS)
    receipts = execution.load([path], results, PROMPTS)
    journal = publication.lines(results.with_suffix(".budget.jsonl"))
    journal[1]["committed"] = 0
    with pytest.raises(ValueError, match="settled"):
        publication.start_gate(journal, {execution.key(r): r for r in rows}, rows[0]["run"], "test", receipts)


def test_terminal_cursor_and_multiple_pages(case):
    results, path, sidecar, archive, _ = case
    response = archive["pages"][0]["response"]
    events = response["events"]
    archive["pages"] = [
        {"after": None, "response": {"events": events[:3], "next_after": 3, "has_more": True}},
        {"after": 3, "response": {"events": events[3:], "next_after": 5, "has_more": False}},
    ]
    sidecar["provider_archive"] = write(path.parent / "provider.json", archive)
    write(path, sidecar)
    assert execution.load([path], results, PROMPTS)
    archive["pages"][1]["after"] = 2
    sidecar["provider_archive"] = write(path.parent / "provider.json", archive)
    write(path, sidecar)
    with pytest.raises(ValueError, match="cursor"):
        execution.load([path], results, PROMPTS)


def test_sidecar_changes_private_identity_and_retains_ungraded_detail(case):
    results, path, sidecar, _, _ = case
    [first] = publication.internal_group("internal-hosted", results, CATALOG, DIGEST, [path]).tracked
    sidecar["fetched_by"] = "second archive observer"
    write(path, sidecar)
    [second] = publication.internal_group("internal-hosted", results, CATALOG, DIGEST, [path]).tracked
    assert first.runs[0].metadata["projection_sha256"] != second.runs[0].metadata["projection_sha256"]
    repeats = [detail for trace in first.runs[0].traces for detail in trace.output.repeats]
    nonexecution = [detail for detail in repeats if detail.invalid_reason and "never started" in detail.invalid_reason]
    assert len(nonexecution) == 1
    assert nonexecution[0].verdict.value == "ungraded"


@pytest.mark.parametrize("change", ["missing-browser-time", "naive-time", "backwards-time", "empty-pages"])
def test_malformed_provider_observations_fail_closed(case, change):
    results, path, sidecar, archive, _ = case
    if change == "missing-browser-time":
        del archive["browser"]["finished_at"]
    if change == "naive-time":
        archive["browser"]["finished_at"] = "2026-11-12T11:47:52"
    if change == "backwards-time":
        archive["pages"][0]["response"]["events"][-1]["ts"] = "2026-11-12T11:00:00Z"
    if change == "empty-pages":
        archive["pages"] = []
    sidecar["provider_archive"] = write(path.parent / "provider.json", archive)
    write(path, sidecar)
    with pytest.raises(ValueError, match="malformed|timezone|backwards|empty"):
        execution.load([path], results, PROMPTS)


def test_source_changes_and_path_escape_reject(case):
    results, path, sidecar, _, _ = case
    original = results.read_bytes()
    results.write_bytes(original + b"\n")
    with pytest.raises(ValueError, match="changed"):
        execution.load([path], results, PROMPTS)
    results.write_bytes(original)
    sidecar["stdout"]["path"] = str((path.parent / "run.log").resolve())
    write(path, sidecar)
    with pytest.raises(ValueError, match="inside"):
        execution.load([path], results, PROMPTS)


def test_ordinary_missing_judge_blocks_publication_without_sidecar(case):
    results, _, _, _, _ = case
    with pytest.raises(publication.PublicationError, match="complete semantic judge receipt"):
        publication.internal_group("internal-hosted", results, CATALOG, DIGEST)


def test_cross_receipt_provider_run_reuse_rejects(case):
    results, path, sidecar, archive, rows = case
    extra = copy.deepcopy([r for r in rows if r["task"] == rows[0]["task"]])
    browser = str(uuid4())
    for row in extra:
        row["arm"] = "browser-use-ultrafast"
        if row.get("provenance"):
            row["provenance"]["browser_session_id"] = str(uuid4())
            row["session_id"] = str(uuid4())
    offset = len(rows)
    rows.extend(extra)
    ledger = results.with_suffix(".attempts.jsonl")
    ledger.write_text("".join(json.dumps(r) + "\n" for r in rows))
    sidecar["attempts"]["sha256"] = hashlib.sha256(ledger.read_bytes()).hexdigest()
    log = path.parent / "run.log"
    with log.open("a") as stream:
        for row in extra:
            bid = (row.get("provenance") or {}).get("browser_session_id") or browser
            stream.write(
                f"WATCH browser-use-ultrafast {row['task']} https://live.browser-use.com?wss=wss%3A%2F%2F{bid}.cdp.browser-use.com\n"
            )
    sidecar["stdout"]["sha256"] = hashlib.sha256(log.read_bytes()).hexdigest()
    write(path, sidecar)
    second = copy.deepcopy(sidecar)
    second.update(arm="browser-use-ultrafast", row_index=offset, row_sha256=execution.row_digest(extra[0]))
    archive["browser"]["id"] = browser
    ready = archive["pages"][0]["response"]["events"][2]["data"]
    ready.update(
        browser_session_id=browser,
        live_view_url=f"https://live.browser-use.com?wss=wss%3A%2F%2F{browser}.cdp.browser-use.com",
    )
    archive["endpoints"]["browser"] = f"/api/v4/browsers/{browser}"
    second["provider_archive"] = write(path.parent / "provider-second.json", archive)
    second_path = path.parent / "receipt-second.json"
    write(second_path, second)
    with pytest.raises(ValueError, match="reuse a provider run"):
        execution.load([path, second_path], results, PROMPTS)


def test_unmatched_browser_cannot_be_shared_with_another_task(case):
    results, path, sidecar, archive, _ = case
    log = path.parent / "run.log"
    ready = archive["pages"][0]["response"]["events"][2]["data"]
    with log.open("a") as stream:
        stream.write(f"WATCH browser-use other-task {ready['live_view_url']}\n")
    sidecar["stdout"]["sha256"] = hashlib.sha256(log.read_bytes()).hexdigest()
    write(path, sidecar)
    with pytest.raises(ValueError, match="identity is reused"):
        execution.load([path], results, PROMPTS)


def test_validated_nonexecution_keeps_elapsed_under_distinct_public_clock(case):
    results, path, _, _, rows = case
    receipts = execution.load([path], results, PROMPTS)
    slot = [
        r for r in rows if (r["arm"], r["task"], r["repeat"]) == (rows[0]["arm"], rows[0]["task"], rows[0]["repeat"])
    ]
    slot[0]["seconds"], slot[1]["seconds"] = 329.1, 11.2
    slot[1].update(
        timing_note="Provider connects inside its first step", session_open_seconds=0.71, session_close_seconds=0.12
    )
    fields = publication.timing_fields(slot, receipts)
    assert fields == dict(
        timing_basis="physical-attempt-wall-time",
        timing_note=publication.PHYSICAL_TIMING_NOTE,
        session_open_seconds=None,
        session_close_seconds=None,
    )
    assert sum(r["seconds"] for r in slot) == 340.3
    assert slot[0]["seconds"] == 329.1 and slot[1]["session_open_seconds"] == 0.71
    with pytest.raises(ValueError, match="different browser setup"):
        publication.timing_fields(slot)
