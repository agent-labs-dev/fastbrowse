"""Validate archived provider claims without changing an attempt or inventing a judgment."""

import hashlib
import json
from collections.abc import Mapping, Sequence
from datetime import datetime
from pathlib import Path
from typing import Any, Literal
from urllib.parse import parse_qs, urlsplit
from uuid import UUID

from pydantic import BaseModel, ConfigDict, Field


class Closed(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)


class Archive(Closed):
    path: str
    sha256: str = Field(pattern=r"^[a-f0-9]{64}$")

    def read(self, root: Path) -> bytes:
        source = (root / self.path).resolve()
        if Path(self.path).is_absolute() or not source.is_relative_to(root.resolve()):
            raise ValueError("execution archive must stay inside its sidecar directory")
        content = source.read_bytes()
        if hashlib.sha256(content).hexdigest() != self.sha256:
            raise ValueError("execution archive changed")
        return content


class ExecutionReceipt(Closed):
    schema_version: Literal[1] = 1
    kind: Literal["worker-never-started"]
    provider: Literal["browser-use-v4"]
    fetched_at: str
    sdk_version: str
    fetched_by: str
    results: Archive
    attempts: Archive
    budget: Archive
    stdout: Archive
    provider_archive: Archive
    row_index: int = Field(ge=0)
    row_sha256: str = Field(pattern=r"^[a-f0-9]{64}$")
    arm: str
    task: str
    repeat: int = Field(ge=0)
    retry: int = Field(ge=0)
    campaign_id: str


class ValidatedExecution(Closed):
    receipt: ExecutionReceipt
    sidecar_sha256: str
    provider_evidence: dict[str, Any]


def row_digest(row: Mapping[str, Any]) -> str:
    return hashlib.sha256(json.dumps(dict(row), sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def key(row: Mapping[str, Any]) -> str:
    return f"{row['arm']}/{row['task']}/{row['repeat']}/{row['retries']}"


def browser_id(url: str) -> str:
    address = parse_qs(urlsplit(url).query).get("wss", [])
    if len(address) != 1:
        raise ValueError("WATCH has no unique browser address")
    host = urlsplit(address[0]).hostname or ""
    suffix = ".cdp.browser-use.com"
    if not host.endswith(suffix):
        raise ValueError("WATCH browser host is not a provider browser")
    return str(UUID(host.removesuffix(suffix)))


def timestamp(value: str) -> datetime:
    parsed = datetime.fromisoformat(value)
    if parsed.tzinfo is None:
        raise ValueError("execution evidence timestamp lacks its timezone")
    return parsed


def validate_provider(archive: dict[str, Any], expected_browser: str, prompt: str) -> None:
    try:
        _validate_provider(archive, expected_browser, prompt)
    except (KeyError, TypeError, IndexError, AttributeError, OSError) as exc:
        raise ValueError("provider archive is incomplete or malformed") from exc


def _validate_provider(archive: dict[str, Any], expected_browser: str, prompt: str) -> None:
    run, pages, browser = archive["run"], archive["pages"], archive["browser"]
    run_id = str(UUID(run["id"]))
    session_id = str(UUID(run["session_id"]))
    if (
        run.get("status") != "failed"
        or run.get("task") != prompt
        or run.get("result") is not None
        or run.get("output") is not None
        or any(type(run.get(k)) is not int or run[k] != 0 for k in ("total_input_tokens", "total_output_tokens"))
    ):
        raise ValueError("provider run does not establish non-execution")
    if archive.get("endpoints") != {
        "run": f"/api/v4/runs/{run_id}",
        "events": f"/api/v4/runs/{run_id}/events",
        "browser": f"/api/v4/browsers/{expected_browser}",
    }:
        raise ValueError("provider archive endpoint identity differs")
    events: list[dict[str, Any]] = []
    after = None
    if not pages:
        raise ValueError("provider event archive is empty")
    for index, page in enumerate(pages):
        response = page["response"]
        batch = response["events"]
        if page["after"] != after or not batch or type(response.get("has_more")) is not bool:
            raise ValueError("incomplete provider event cursor chain")
        if response["has_more"] != (index < len(pages) - 1):
            raise ValueError("provider events were not drained after termination")
        for event in batch:
            if event.get("run_id") != run_id or type(event.get("id")) is not int:
                raise ValueError("provider event identity differs")
            if events and event["id"] <= events[-1]["id"]:
                raise ValueError("provider event order repeats or goes backwards")
            if events and timestamp(event["ts"]) < timestamp(events[-1]["ts"]):
                raise ValueError("provider event time goes backwards")
            events.append(event)
        after = response["next_after"]
        if after != events[-1]["id"]:
            raise ValueError("provider event cursor does not cover its page")
    kinds = [event["type"] for event in events]
    # Any unfamiliar event may represent execution, so only this complete admission lifecycle is accepted.
    if (
        kinds[:3] != ["run.created", "run.dispatching", "browser.ready"]
        or kinds[-2:] != ["run.dispatch_failed", "worker.never_started"]
        or any(kind != "run.dispatch_pending" for kind in kinds[3:-2])
        or len(kinds) < 5
    ):
        raise ValueError("provider event history does not establish non-execution")
    if events[0]["data"].get("model") != run.get("model") or not run.get("model"):
        raise ValueError("provider model identity differs")
    if events[0]["data"].get("session_id") != session_id:
        raise ValueError("provider session identity differs")
    ready = events[2]["data"]
    if ready.get("browser_session_id") != expected_browser or browser_id(ready["live_view_url"]) != expected_browser:
        raise ValueError("provider browser identity differs")
    failure = events[-2]["data"]
    if failure.get("error_code") != "worker_never_started" or failure.get("failure_stage") != "worker_admission":
        raise ValueError("provider failed for a reason other than non-execution")
    if (
        browser.get("id") != expected_browser
        or browser.get("status") != "stopped"
        or timestamp(browser["finished_at"]) < timestamp(events[-1]["ts"])
    ):
        raise ValueError("provider browser has no stopped-after-failure receipt")


def _load(paths: Sequence[Path], results: Path, prompts: Mapping[str, str]) -> dict[str, ValidatedExecution]:
    """Return validated private receipts for this exact invocation; unrelated sidecars supply no coverage."""
    validated = {}
    provider_runs: set[str] = set()
    provider_browsers: set[str] = set()
    for path in paths:
        receipt = ExecutionReceipt.model_validate_json(path.read_bytes())
        root = path.parent
        if (root / receipt.results.path).resolve() != results.resolve():
            continue
        timestamp(receipt.fetched_at)
        if not receipt.sdk_version or not receipt.fetched_by:
            raise ValueError("execution archive lacks collection provenance")
        receipt.results.read(root)
        raw = receipt.attempts.read(root)
        if (root / receipt.attempts.path).resolve() != results.with_suffix(".attempts.jsonl").resolve():
            raise ValueError("execution receipt names another physical ledger")
        if (root / receipt.budget.path).resolve() != results.with_suffix(".budget.jsonl").resolve():
            raise ValueError("execution receipt names another budget journal")
        receipt.budget.read(root)
        rows = [json.loads(line) for line in raw.splitlines() if line.strip()]
        if receipt.row_index >= len(rows):
            raise ValueError("execution receipt physical row is absent")
        row = rows[receipt.row_index]
        identity = (row["arm"], row["task"], row["repeat"], row["retries"], row["run"]["run_id"])
        expected = (receipt.arm, receipt.task, receipt.repeat, receipt.retry, receipt.campaign_id)
        if identity != expected or row_digest(row) != receipt.row_sha256:
            raise ValueError("execution receipt physical row identity differs")
        if (
            row.get("selected") is not False
            or row.get("started") is not True
            or row.get("semantic_judge") is not None
            or row.get("unknown_cost") is not True
            or row.get("dollars") is not None
            or row.get("passed") is not None
            or any(
                row.get(k)
                for k in (
                    "answer",
                    "data",
                    "events",
                    "trace",
                    "steps",
                    "input_tokens",
                    "output_tokens",
                    "artifact",
                    "step_log",
                )
            )
            or row.get("unpriced_judge_attempts") != 0
            or row.get("known_judge_dollars") != 0
        ):
            raise ValueError("execution receipt cannot cover a selected, judged or executed attempt")
        slot = [r for r in rows if (r["arm"], r["task"], r["repeat"]) == identity[:3]]
        selected = [r for r in slot if r.get("selected") is True]
        if (
            len(selected) != 1
            or selected[0]["retries"] <= receipt.retry
            or rows.index(selected[0]) <= receipt.row_index
            or not selected[0].get("semantic_judge")
            or not isinstance(selected[0].get("passed"), bool)
        ):
            raise ValueError("non-execution needs a later genuinely graded selected retry")
        peers = [r for r in rows if (r["arm"], r["task"]) == identity[:2]]
        watchers = []
        watch_lines = receipt.stdout.read(root).decode().splitlines()
        for line in watch_lines:
            parts = line.split()
            if parts[:3] == ["WATCH", receipt.arm, receipt.task]:
                if len(parts) != 4:
                    raise ValueError("WATCH identity line is malformed")
                watchers.append(browser_id(parts[3]))
        all_linked = [
            r["provenance"]["browser_session_id"] for r in rows if (r.get("provenance") or {}).get("browser_session_id")
        ]
        if len(set(all_linked)) != len(all_linked):
            raise ValueError("physical attempts share a provider browser identity")
        linked = [
            r["provenance"]["browser_session_id"]
            for r in peers
            if (r.get("provenance") or {}).get("browser_session_id")
        ]
        missing = [r for r in peers if not (r.get("provenance") or {}).get("browser_session_id")]
        if (
            len(watchers) != len(peers)
            or len(set(watchers)) != len(watchers)
            or len(set(linked)) != len(linked)
            or len(missing) != 1
            or missing[0] is not row
            or not set(linked) <= set(watchers)
        ):
            raise ValueError("WATCH and physical attempts do not give an exact unique identity join")
        unmatched = set(watchers) - set(linked)
        if len(unmatched) != 1:
            raise ValueError("provider browser identity is ambiguous")
        archive = json.loads(receipt.provider_archive.read(root))
        matched_browser = unmatched.pop()
        all_watchers = [
            browser_id(parts[3]) for line in watch_lines if len(parts := line.split()) == 4 and parts[0] == "WATCH"
        ]
        if matched_browser in all_linked or all_watchers.count(matched_browser) != 1:
            raise ValueError("unmatched WATCH browser identity is reused")
        validate_provider(archive, matched_browser, prompts[receipt.task])
        identifier = key(row)
        if identifier in validated:
            raise ValueError("duplicate execution receipt")
        if archive["run"]["id"] in provider_runs or matched_browser in provider_browsers:
            raise ValueError("execution receipts reuse a provider run or browser")
        provider_runs.add(archive["run"]["id"])
        provider_browsers.add(matched_browser)
        validated[identifier] = ValidatedExecution(
            receipt=receipt, sidecar_sha256=hashlib.sha256(path.read_bytes()).hexdigest(), provider_evidence=archive
        )
    return validated


def load(paths: Sequence[Path], results: Path, prompts: Mapping[str, str]) -> dict[str, ValidatedExecution]:
    try:
        return _load(paths, results, prompts)
    except (KeyError, TypeError, IndexError, AttributeError, OSError) as exc:
        raise ValueError("execution receipt or archive is incomplete or malformed") from exc


def coverage(rows: Mapping[str, dict[str, Any]], execution: Mapping[str, Any]) -> None:
    """A missing judge is covered only by a validated non-execution receipt, never by an absent verdict."""
    if not execution.keys() <= rows.keys():
        raise ValueError("execution receipt has no physical attempt")
    for identifier, row in rows.items():
        if bool(row.get("semantic_judge")) == (identifier in execution):
            raise ValueError("every physical attempt requires semantic XOR non-execution coverage")
