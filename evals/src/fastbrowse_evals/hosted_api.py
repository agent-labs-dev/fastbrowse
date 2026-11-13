"""Parallax harness for the fastbrowse hosted API.

The API owns the browser session, so this harness reports the answer and
citations returned by the service. It does not invent final-page evidence when
the service did not expose the page to the caller.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import time
import uuid
from typing import Any
from urllib.parse import urlsplit

import httpx
from fastbrowse.models import Unavailable
from parallax.harnesses.browser import BrowserHarness
from parallax.models import Capability, CostSource, HarnessInfo, HarnessKind, Isolation, TurnInput

from fastbrowse_evals.backend import RETRYABLE_STATUS, TRANSIENT_TRANSPORT

DEFAULT_URL = "https://api.fastbrowse.ai"
POLL_SECONDS = 2.0
RUNNING = frozenset({"queued", "running"})
_log = logging.getLogger("fastbrowse_evals.hosted")


def settings() -> tuple[str, str]:
    key = os.environ.get("FASTBROWSE_HOSTED_API_KEY")
    if not key:
        raise RuntimeError("fastbrowse-hosted needs FASTBROWSE_HOSTED_API_KEY")
    url = os.environ.get("FASTBROWSE_HOSTED_URL") or DEFAULT_URL
    parts = urlsplit(url)
    if parts.scheme != "https" and parts.hostname not in {"localhost", "127.0.0.1"}:
        raise RuntimeError("FASTBROWSE_HOSTED_URL must be https, or http on localhost")
    return url.rstrip("/"), key


def host() -> str | None:
    return urlsplit(os.environ.get("FASTBROWSE_HOSTED_URL") or DEFAULT_URL).hostname


def test_key() -> bool:
    return os.environ.get("FASTBROWSE_HOSTED_API_KEY", "").startswith("fbh_test_")


def _failure(what: str, status: int | None = None, error: Exception | None = None) -> Exception:
    if error is not None:
        failure = Unavailable if isinstance(error, TRANSIENT_TRANSPORT) else RuntimeError
        return failure(f"fastbrowse-hosted {what} failed ({type(error).__name__})")
    failure = Unavailable if status in RETRYABLE_STATUS else RuntimeError
    return failure(f"fastbrowse-hosted {what} returned HTTP {status}")


async def _call(http: httpx.AsyncClient, method: str, url: str, what: str, **kwargs: Any) -> dict[str, Any]:
    try:
        response = await http.request(method, url, timeout=60, **kwargs)
    except httpx.HTTPError as error:
        raise _failure(what, error=error) from None
    if not response.is_success:
        raise _failure(what, response.status_code)
    return response.json()


def _task_value(task: object, name: str, default: object = None) -> object:
    value = getattr(task, name, default)
    if value is not default:
        return value
    data = getattr(task, "data", {})
    return data.get(name, default) if isinstance(data, dict) else default


async def submit_and_wait(http: httpx.AsyncClient, task: object) -> tuple[dict[str, Any], float]:
    """Submit one TaskRecord and wait for its terminal API result."""
    base, key = settings()
    headers = {"Authorization": f"Bearer {key}"}
    task_id = str(_task_value(task, "id", "task"))
    prompt = str(_task_value(task, "task", _task_value(task, "prompt", "")))
    start_url = str(_task_value(task, "start_url", ""))
    began = time.monotonic()
    submit = asyncio.ensure_future(
        _call(
            http,
            "POST",
            f"{base}/v1/runs",
            "submit",
            headers=headers | {"Idempotency-Key": f"parallax-{task_id}-{uuid.uuid4().hex}"},
            json={"task": prompt, "start": start_url, "webhook_events": False, "metadata": {"source": "parallax"}},
        )
    )
    try:
        run = await asyncio.shield(submit)
    except asyncio.CancelledError:
        with contextlib.suppress(Exception):
            accepted = await submit
            if accepted.get("state") in RUNNING:
                await _cancel(http, f"{base}/v1/runs/{accepted['id']}", headers)
        raise
    run_url = f"{base}/v1/runs/{run['id']}"
    try:
        while run.get("state") in RUNNING:
            await asyncio.sleep(POLL_SECONDS)
            run = await _call(http, "GET", run_url, "poll", headers=headers)
    except BaseException:
        if run.get("state") in RUNNING:
            await asyncio.shield(_cancel(http, run_url, headers))
        raise
    return run, time.monotonic() - began


async def _cancel(http: httpx.AsyncClient, run_url: str, headers: dict[str, str]) -> None:
    try:
        await http.post(f"{run_url}/cancel", headers=headers, timeout=30)
    except httpx.HTTPError as error:
        _log.warning(
            "fastbrowse-hosted run %s was not cancelled (%s)", run_url.rsplit("/", 1)[-1], type(error).__name__
        )


def outcome_of(run: dict[str, Any]) -> dict[str, Any]:
    return {
        "answer": run.get("answer"),
        "data": run.get("data"),
        "final_url": run.get("final_url"),
        "citations": run.get("citations") or [],
    }


def status_of(run: dict[str, Any]) -> str:
    return str(run.get("status") or run.get("state") or "error")


def report_fields(run: dict[str, Any], wall: float) -> dict[str, Any]:
    usage = run.get("usage") or {}
    components = usage.get("components") or {}
    duration = usage.get("duration_ms")
    charged = usage.get("charged_micros")
    return {
        "status": status_of(run),
        "seconds": round(duration / 1000, 2) if duration is not None else round(wall, 2),
        "timing_basis": "session-included" if duration is None else None,
        "session_seconds": round(wall, 2),
        "dollars": None if charged is None else charged / 1_000_000,
        "error": run.get("error"),
        "steps": usage.get("steps"),
        "citations": run.get("citations") or [],
        "step_log": run.get("steps") or [],
        "final_page_url": run.get("final_url"),
        "session_id": run.get("id"),
        "input_tokens": sum(c.get("input_tokens") or 0 for c in components.values()) or None,
        "output_tokens": sum(c.get("output_tokens") or 0 for c in components.values()) or None,
        "provenance": {"api": host(), "run_id": run.get("id"), "simulator": test_key()},
    }


class FastbrowseHostedHarness(BrowserHarness):
    info = HarnessInfo(
        name="fastbrowse-hosted",
        kind=HarnessKind.PRODUCT,
        capabilities=frozenset({Capability.BROWSER}),
        pins_model=True,
        cost_source=CostSource.HARNESS,
        isolation=Isolation.REMOTE,
        credential_env=("FASTBROWSE_HOSTED_API_KEY",),
        source_url="https://github.com/agent-labs-dev/fastbrowse",
    )

    def __init__(self, *, observer: str | None = None) -> None:
        super().__init__(observer=observer)
        self._task: object | None = None

    def bind_task(self, task: object) -> None:
        self._task = task

    async def execute(self, turn: TurnInput) -> dict[str, Any]:
        if self._task is None:
            raise RuntimeError("fastbrowse-hosted harness has no task")
        async with httpx.AsyncClient() as http:
            run, wall = await submit_and_wait(http, self._task)
        state = status_of(run)
        if state in {"complete", "completed", "idle", "stopped"}:
            result_status = "done"
        elif state in {"unavailable", "expired"}:
            result_status = "unavailable"
        else:
            result_status = "error"
        fields = report_fields(run, wall)
        dollars = fields.get("dollars")
        lines = [] if dollars is None else [{"component": "agent", "basis": "metered", "dollars": dollars}]
        return {
            "status": result_status,
            "answer": run.get("answer"),
            "error": fields.get("error"),
            "final_url": run.get("final_url"),
            "citations": run.get("citations") or [],
            "step_log": run.get("steps") or [],
            "cost_lines": lines,
            "actual_model": run.get("model"),
            "latency_ms": fields.get("seconds", wall) * 1000,
            "session_id": run.get("id"),
        }


__all__ = [
    "FastbrowseHostedHarness",
    "_call",
    "host",
    "outcome_of",
    "report_fields",
    "settings",
    "status_of",
    "submit_and_wait",
    "test_key",
]
