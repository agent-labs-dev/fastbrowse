"""The fastbrowse-hosted arm: a task sent to the hosted product's public API, as a customer sends it.

This measures what a customer buys, not the agent alone: the API's queue, its worker's pinned fastbrowse build, its
own browser pool and its price. The run is submitted with `POST /v1/runs` and polled with `GET /v1/runs/{id}` until it
leaves `queued` and `running` (fastbrowse-hosted docs/api.md). The API returns the answer, its quotes and where the
browser ended, but no copy of that page, so the page half of a task is not graded for this arm (`unobservable`).

`dollars` is the customer charge (`usage.charged_micros`), not the metered provider cost the other fastbrowse arms
report: the two are different quantities, so this arm is a diagnostic, in no publication group.
"""

import asyncio
import logging
import os
import time
import uuid
from typing import Any
from urllib.parse import urlsplit

import httpx
from fastbrowse.models import Unavailable

from fastbrowse_evals.attempt_clock import SESSION_INCLUDED
from fastbrowse_evals.backend import RETRYABLE_STATUS, TRANSIENT_TRANSPORT
from fastbrowse_evals.live_tasks import LiveTask, Outcome

DEFAULT_URL = "https://api.fastbrowse.ai"
POLL_SECONDS = 2.0
_log = logging.getLogger("fastbrowse_evals.hosted")
RUNNING = frozenset({"queued", "running"})


def settings() -> tuple[str, str]:
    """The API's base URL and key. The key is never logged or recorded."""
    key = os.environ.get("FASTBROWSE_HOSTED_API_KEY")
    if not key:
        raise RuntimeError("fastbrowse-hosted needs FASTBROWSE_HOSTED_API_KEY")
    url = os.environ.get("FASTBROWSE_HOSTED_URL") or DEFAULT_URL
    parts = urlsplit(url)
    if parts.scheme != "https" and parts.hostname not in {"localhost", "127.0.0.1"}:
        raise RuntimeError("FASTBROWSE_HOSTED_URL must be https, or http on localhost")
    return url.rstrip("/"), key


def host() -> str | None:
    """The API's hostname alone, what provenance records of it."""
    return urlsplit(os.environ.get("FASTBROWSE_HOSTED_URL") or DEFAULT_URL).hostname


def test_key() -> bool:
    """A `fbh_test_` key runs the API's simulator: the same shapes, no browser, no charge."""
    return os.environ.get("FASTBROWSE_HOSTED_API_KEY", "").startswith("fbh_test_")


def _failure(what: str, status: int | None = None, error: Exception | None = None) -> Exception:
    """An API failure as an outage when a repeat can clear it, else as the harness's own error; never echoes a body,
    which can quote the request."""
    if error is not None:
        failed = Unavailable if isinstance(error, TRANSIENT_TRANSPORT) else RuntimeError
        return failed(f"fastbrowse-hosted {what} failed ({type(error).__name__})")
    failed = Unavailable if status in RETRYABLE_STATUS else RuntimeError
    return failed(f"fastbrowse-hosted {what} returned HTTP {status}")


async def _call(http: httpx.AsyncClient, method: str, url: str, what: str, **kwargs: Any) -> dict[str, Any]:
    try:
        response = await http.request(method, url, timeout=60, **kwargs)
    except httpx.HTTPError as error:
        raise _failure(what, error=error) from None
    if not response.is_success:
        raise _failure(what, response.status_code)
    return response.json()


async def submit_and_wait(http: httpx.AsyncClient, task: LiveTask) -> tuple[dict[str, Any], float]:
    """The finished run and the wall seconds from submitting it to seeing it finish. A run still going when the
    attempt is cancelled is cancelled too, so it is not left running and charging."""
    base, key = settings()
    headers = {"Authorization": f"Bearer {key}"}
    began = time.monotonic()
    run = await _call(
        http,
        "POST",
        f"{base}/v1/runs",
        "submit",
        headers=headers | {"Idempotency-Key": f"parallax-{task.id}-{uuid.uuid4().hex}"},
        json={"task": task.task, "start": task.start, "webhook_events": False, "metadata": {"source": "parallax"}},
    )
    run_url = f"{base}/v1/runs/{run['id']}"
    try:
        while run["state"] in RUNNING:
            await asyncio.sleep(POLL_SECONDS)
            run = await _call(http, "GET", run_url, "poll", headers=headers)
    except BaseException:
        if run["state"] in RUNNING:
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


def outcome_of(run: dict[str, Any]) -> Outcome:
    quotes = tuple((str(c["url"]), str(c["quote"])) for c in run.get("citations") or [] if c.get("quote"))
    return Outcome(run.get("answer"), run.get("data"), run.get("final_url"), quotes, None, None, True)


def status_of(run: dict[str, Any]) -> str:
    """The agent's own status when it finished; the API's state when the worker failed the run outside the agent
    (`failed`, `canceled`, `expired`), which the shared vocabulary reads as an error."""
    return run.get("status") or run["state"]


def report_fields(run: dict[str, Any], wall: float) -> dict[str, Any]:
    """The ArmReport fields this arm can fill. `seconds` is the worker's own duration of the final attempt; the wall
    time from submit includes the queue and the browser start, so it is kept apart as `session_seconds`."""
    usage = run.get("usage") or {}
    components = usage.get("components") or {}
    duration = usage.get("duration_ms")
    charged = usage.get("charged_micros")
    return {
        "status": status_of(run),
        "seconds": round(duration / 1000, 2) if duration is not None else round(wall, 2),
        "timing_basis": None if duration is not None else SESSION_INCLUDED,
        "session_seconds": round(wall, 2),
        "dollars": None if charged is None else charged / 1_000_000,
        "error": run.get("error"),
        "steps": usage.get("steps"),
        "citations": run.get("citations") or [],
        "step_log": run.get("steps") or [],
        "final_page_url": run.get("final_url"),
        "session_id": run["id"],
        "input_tokens": sum(c.get("input_tokens") or 0 for c in components.values()) or None,
        "output_tokens": sum(c.get("output_tokens") or 0 for c in components.values()) or None,
        "provenance": {"api": host(), "run_id": run["id"], "simulator": test_key()},
    }
