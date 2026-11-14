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
from typing import Any, assert_never
from urllib.parse import urlsplit

import httpx
from fastbrowse.models import Status
from parallax.harnesses.browser import BrowserHarness
from parallax.harnesses.browser.site import Unavailable, retryable_status, transient_transport
from parallax.models import Capability, CostSource, HarnessInfo, HarnessKind, Isolation, Task, TurnInput

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
        failure = Unavailable if transient_transport(error) else RuntimeError
        return failure(f"fastbrowse-hosted {what} failed ({type(error).__name__})")
    failure = Unavailable if retryable_status(status) else RuntimeError
    return failure(f"fastbrowse-hosted {what} returned HTTP {status}")


async def _call(http: httpx.AsyncClient, method: str, url: str, what: str, **kwargs: Any) -> dict[str, Any]:
    try:
        response = await http.request(method, url, timeout=60, **kwargs)
    except httpx.HTTPError as error:
        raise _failure(what, error=error) from None
    if not response.is_success:
        raise _failure(what, response.status_code)
    return response.json()


def _start_url(task: Task) -> str:
    start_url = task.data.get("start_url")
    if not isinstance(start_url, str):
        raise ValueError(f"browser task {task.id} has no start_url")
    return start_url


def _run_secrets(task: Task) -> list[dict[str, Any]]:
    """The task's logins as the hosted API's per-run inline secrets, each typed only on the start origin."""
    parts = urlsplit(_start_url(task))
    origin = f"{parts.scheme}://{parts.netloc}"
    # The runner made the trial INVALID before this point if any of these variables is unset.
    return [{"name": alias, "value": os.environ[env], "origins": [origin]} for alias, env in task.secrets.items()]


async def submit_and_wait(http: httpx.AsyncClient, task: Task, prompt: str) -> tuple[dict[str, Any], float]:
    """Submit one source task and wait for its terminal API result."""
    base, key = settings()
    headers = {"Authorization": f"Bearer {key}"}
    began = time.monotonic()
    submit = asyncio.ensure_future(
        _call(
            http,
            "POST",
            f"{base}/v1/runs",
            "submit",
            headers=headers | {"Idempotency-Key": f"parallax-{task.id}-{uuid.uuid4().hex}"},
            json={
                "task": prompt,
                "start": _start_url(task),
                "output_schema": task.answer_schema,
                "secrets": _run_secrets(task),
                "authorize": Capability.AUTHORIZATION in task.meta.requires,
                "webhook_events": False,
                "metadata": {"source": "parallax"},
            },
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


def evidence_citations(run: dict[str, Any]) -> list[dict[str, str]]:
    """The run's citations as Parallax's browser evidence holds them: the page and the quote the answer rests on."""
    return [{"url": citation["url"], "quote": citation["quote"]} for citation in run.get("citations") or []]


def outcome_of(run: dict[str, Any]) -> dict[str, Any]:
    return {
        "answer": run.get("answer"),
        "data": run.get("data"),
        "final_url": run.get("final_url"),
        "citations": run.get("citations") or [],
    }


def status_of(run: dict[str, Any]) -> str:
    return str(run.get("status") or run.get("state") or "error")


def _product_status(state: str) -> Status:
    match state:
        case "complete" | "completed" | "idle" | "stopped" | "succeeded":
            return Status.COMPLETE
        case "expired":
            return Status.UNAVAILABLE
        case "failed":
            return Status.ERROR
        case "canceled":
            return Status.CANCELLED
        case _:
            return Status(state)


def _turn_status(status: Status) -> str:
    match status:
        case Status.UNAVAILABLE:
            return "unavailable"
        case Status.ERROR:
            return "error"
        case (
            Status.COMPLETE
            | Status.UNVERIFIED
            | Status.NEEDS_CONFIRMATION
            | Status.NEEDS_LOGIN
            | Status.BLOCKED
            | Status.NEEDS_INPUT
            | Status.STUCK
            | Status.BUDGET_EXCEEDED
            | Status.OBSERVATION_LIMIT
            | Status.CANCELLED
        ):
            return "done"
        case other:
            assert_never(other)


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
        "components": components,
        "provenance": {"api": host(), "run_id": run.get("id"), "simulator": test_key()},
    }


class FastbrowseHostedHarness(BrowserHarness):
    info = HarnessInfo(
        name="fastbrowse-hosted",
        kind=HarnessKind.PRODUCT,
        capabilities=frozenset({Capability.BROWSER, Capability.CREDENTIALS, Capability.AUTHORIZATION}),
        pins_model=True,
        cost_source=CostSource.HARNESS,
        isolation=Isolation.REMOTE,
        credential_env=("FASTBROWSE_HOSTED_API_KEY",),
        source_url="https://github.com/agent-labs-dev/fastbrowse",
    )

    def __init__(self, *, observer: str | None = None) -> None:
        super().__init__(observer=observer)
        self._task: Task | None = None
        self._product_status: Status | None = None

    def bind_task(self, task: Task) -> None:
        super().bind_task(task)
        self._task = task

    async def execute(self, turn: TurnInput) -> dict[str, Any]:
        if self._task is None:
            raise RuntimeError("fastbrowse-hosted harness has no task")
        async with httpx.AsyncClient() as http:
            run, wall = await submit_and_wait(http, self._task, turn.text)
        self._product_status = _product_status(status_of(run))
        fields = report_fields(run, wall)
        components = fields.get("components") or {}
        lines = [
            {"component": name, "basis": "metered", "dollars": data.get("dollars")}
            for name, data in components.items()
            if isinstance(data, dict)
        ]
        usage = [
            {
                "model": str(data.get("model") or name),
                "component": name,
                "input": int(data.get("input_tokens") or 0),
                "output": int(data.get("output_tokens") or 0),
                "requests": int(data.get("physical_requests") or 0),
                "cost_usd": data.get("dollars"),
                "source": "harness",
            }
            for name, data in components.items()
            if isinstance(data, dict) and name in {"llm", "jev"}
        ]
        return {
            "status": _turn_status(self._product_status),
            "answer": run.get("answer"),
            "error": fields.get("error"),
            "final_url": run.get("final_url"),
            "citations": evidence_citations(run),
            "step_log": run.get("steps") or [],
            "cost_lines": lines,
            "usage": usage,
            "actual_model": run.get("model"),
            "latency_ms": round(wall * 1000),
            "session_id": run.get("id"),
        }

    def trial_details(self) -> dict[str, Any]:
        details = super().trial_details()
        if self._product_status is not None:
            details["status"] = self._product_status.value
        return details


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
